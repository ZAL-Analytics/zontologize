pub mod classify;
pub mod config;
pub mod db;
pub mod embed;
pub mod export;
pub mod facts;
pub mod fit;
pub mod graph;
pub mod llm;
pub mod ontology;
pub mod resolve;
pub mod review;
pub mod rules;
pub mod scan;

use std::path::Path;
use std::process::ExitCode;

use anyhow::{Context, Result};

pub async fn db_migrate(config_dir: Option<&Path>) -> Result<ExitCode> {
    let config = config::load(config_dir)?;
    let schema = &config.database.schema;
    let mut conn = db::connect(database_url().as_deref(), schema).await?;
    let api = &config.database.api_schema;
    let result = async {
        db::runs::take_lock(&mut conn, schema).await?;
        let applied = db::migrate::run(&mut conn, schema).await?;
        db::api::create(&mut conn, schema, api).await?;
        anyhow::Ok(applied)
    }
    .await;
    db::close(conn).await;
    let applied = result?;
    if applied.is_empty() {
        println!("schema {schema}: nothing to do");
    }
    for version in applied {
        println!("schema {schema}: applied {version:04}");
    }
    println!("schema {api}: views up to date");
    Ok(ExitCode::SUCCESS)
}

pub async fn db_check(config_dir: Option<&Path>) -> Result<ExitCode> {
    let config = config::load(config_dir)?;
    let schema = &config.database.schema;
    let mut conn = db::connect(database_url().as_deref(), schema).await?;
    let result = check(&mut conn, schema).await;
    db::close(conn).await;
    let complete = result?;
    Ok(if complete {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    })
}

fn database_url() -> Option<String> {
    std::env::var("DATABASE_URL").ok()
}

async fn check(conn: &mut sqlx::PgConnection, schema: &str) -> Result<bool> {
    let mut complete = true;
    println!("server: {}", db::server_version(conn).await?);
    for name in db::EXTENSIONS {
        match db::extension_version(conn, name).await? {
            Some(version) => println!("extension {name}: {version}"),
            None => {
                complete = false;
                println!(
                    "extension {name}: missing, run as database owner: {}",
                    db::migrate::create_extension_sql(name)
                );
            }
        }
    }
    if db::schema_exists(conn, schema).await? {
        println!("schema {schema}: present");
    } else {
        complete = false;
        println!("schema {schema}: missing, run zontologize db migrate");
    }
    for state in db::migrate::states(conn, schema).await? {
        let status = if state.applied { "applied" } else { "pending" };
        if !state.applied {
            complete = false;
        }
        println!(
            "migration {:04} {}: {status}",
            state.version, state.description
        );
    }
    Ok(complete)
}

pub fn ontology_check(config_dir: Option<&Path>) -> Result<ExitCode> {
    let config = config::load(config_dir)?;
    if config.ontology.files.is_empty() {
        println!("error: [ontology] files is empty");
        return Ok(ExitCode::FAILURE);
    }
    let loaded = ontology::load(config_dir, &config.ontology)?;
    let mut findings = loaded.findings.clone();
    findings.sort_by_key(|f| f.severity);
    for finding in &findings {
        let level = match finding.severity {
            ontology::Severity::Error => "error",
            ontology::Severity::Warning => "warning",
        };
        println!("{level}: {}", finding.message);
    }
    let errors = findings
        .iter()
        .filter(|f| f.severity == ontology::Severity::Error)
        .count();
    let onto = &loaded.ontology;
    if errors == 0 {
        for (name, iri) in config.ontology.include.iter().zip(&onto.roots) {
            let (classes, properties) = onto.selected_under(iri);
            println!("root {name}: {classes} classes, {properties} properties");
        }
    }
    println!(
        "ontology: {} files, {} classes ({} selected), {} properties ({} selected), {errors} errors, {} warnings",
        onto.files.len(),
        onto.classes.len(),
        onto.classes.values().filter(|c| c.selected).count(),
        onto.properties.len(),
        onto.properties.values().filter(|p| p.selected).count(),
        findings.len() - errors
    );
    Ok(if errors == 0 {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    })
}

pub const PAUSED_EXIT_CODE: u8 = 2;

pub async fn ontology_export(config_dir: Option<&Path>, out: &Path) -> Result<ExitCode> {
    let text = export::run(config_dir, database_url().as_deref()).await?;
    std::fs::write(out, text).with_context(|| format!("cannot write {}", out.display()))?;
    println!("ontology export: written to {}", out.display());
    Ok(ExitCode::SUCCESS)
}

pub async fn graph_sync(config_dir: Option<&Path>) -> Result<ExitCode> {
    let stats = graph::sync(config_dir, database_url().as_deref()).await?;
    println!(
        "graph sync: {} vertices, {} edges, {} labels; {:.1} s",
        stats.vertices, stats.edges, stats.labels, stats.seconds
    );
    Ok(ExitCode::SUCCESS)
}

pub async fn ontology_fit(
    config_dir: Option<&Path>,
    options: &fit::run::Options,
) -> Result<ExitCode> {
    let outcome = fit::run::run(config_dir, database_url().as_deref(), options).await?;
    Ok(exit_code(outcome))
}

pub async fn extract(config_dir: Option<&Path>, options: &facts::run::Options) -> Result<ExitCode> {
    let outcome = facts::run::run(config_dir, database_url().as_deref(), options).await?;
    Ok(exit_code(outcome))
}

pub async fn classify(config_dir: Option<&Path>, options: &llm::job::Options) -> Result<ExitCode> {
    let outcome = classify::run(config_dir, database_url().as_deref(), options).await?;
    Ok(exit_code(outcome))
}

pub async fn review_sample(
    config_dir: Option<&Path>,
    options: &review::Options,
) -> Result<ExitCode> {
    let mut input = tokio::io::BufReader::new(tokio::io::stdin());
    review::sample(config_dir, database_url().as_deref(), options, &mut input).await?;
    Ok(ExitCode::SUCCESS)
}

pub async fn embed(config_dir: Option<&Path>, options: &embed::run::Options) -> Result<ExitCode> {
    let stats = embed::run::run(config_dir, database_url().as_deref(), options).await?;
    for (kind, counts) in [
        ("chunks", &stats.chunks),
        ("entities", &stats.entities),
        ("classes", &stats.classes),
    ] {
        println!(
            "embed: {kind}: {} eligible, {} embedded, {} copied from equal text, {} unchanged, {} removed, {} truncated",
            counts.eligible,
            counts.embedded,
            counts.copied,
            counts.kept,
            counts.deleted,
            counts.truncated
        );
    }
    let embedded = stats.chunks.embedded + stats.entities.embedded + stats.classes.embedded;
    if embedded > 0 {
        println!(
            "embed: {embedded} texts in {:.1} s of model time, {:.1}/s",
            stats.embed_seconds,
            embedded as f64 / stats.embed_seconds.max(1e-9)
        );
    }
    if stats.junk > 0 {
        println!("embed: {} chunks left out as unreadable", stats.junk);
    }
    if stats.index_seconds > 0.1 {
        println!("embed: index checked in {:.1} s", stats.index_seconds);
    }
    for name in &stats.pruned {
        println!("embed: removed {name}");
    }
    if stats.stopped {
        println!("embed: stopped by --minutes; run embed again to go on");
    }
    println!("embed: done in {:.1} s", stats.seconds);
    Ok(ExitCode::SUCCESS)
}

pub async fn eval_embed(
    config_dir: Option<&Path>,
    options: &embed::eval::Options,
) -> Result<ExitCode> {
    let path = embed::eval::run(config_dir, database_url().as_deref(), options).await?;
    println!("eval embed: report in {}", path.display());
    Ok(ExitCode::SUCCESS)
}

pub async fn resolve(config_dir: Option<&Path>) -> Result<ExitCode> {
    let stats = resolve::run(config_dir, database_url().as_deref()).await?;
    if let Some(reason) = &stats.embed_skipped {
        println!("resolve: embedding matches skipped, {reason}");
    }
    for (method, count) in &stats.added {
        if *count > 0 {
            println!("resolve: {method}: {count} links added");
        }
    }
    println!(
        "resolve: {} links kept, {} removed; {} clusters, {} canonical ids changed; {} aliases added, {} removed; {:.1} s",
        stats.kept,
        stats.removed,
        stats.clusters,
        stats.canonical_changed,
        stats.aliases_added,
        stats.aliases_removed,
        stats.seconds
    );
    if stats.written() == 0 {
        println!("resolve: nothing changed");
    }
    Ok(ExitCode::SUCCESS)
}

pub async fn review_merges(
    config_dir: Option<&Path>,
    options: &review::MergeOptions,
) -> Result<ExitCode> {
    let mut input = tokio::io::BufReader::new(tokio::io::stdin());
    review::merges(config_dir, database_url().as_deref(), options, &mut input).await?;
    Ok(ExitCode::SUCCESS)
}

fn exit_code(outcome: llm::job::Outcome) -> ExitCode {
    match outcome {
        llm::job::Outcome::Paused => ExitCode::from(PAUSED_EXIT_CODE),
        _ => ExitCode::SUCCESS,
    }
}

pub async fn models_check(
    config_dir: Option<&Path>,
    claude_program: std::path::PathBuf,
) -> Result<ExitCode> {
    let config = config::load(config_dir)?;
    let tasks = [("fit", &config.models.fit)];
    let pool = db::pool::connect(database_url().as_deref(), &config.database.schema, 1)
        .await
        .ok();
    let mut failed = false;
    for (task, model) in tasks {
        if model.is_empty() {
            println!("{task}: no model configured");
            continue;
        }
        let name =
            llm::ModelName::parse(model).map_err(|e| anyhow::anyhow!("models.{task}: {e}"))?;
        match check_model(&config, &name, claude_program.clone(), pool.as_ref()).await {
            Ok(lines) => {
                for line in lines {
                    println!("{task} {name}: {line}");
                }
            }
            Err(error) => {
                failed = true;
                println!("{task} {name}: failed, {error:#}");
            }
        }
    }
    if let Some(pool) = pool {
        pool.close().await;
    }
    Ok(if failed {
        ExitCode::FAILURE
    } else {
        ExitCode::SUCCESS
    })
}

const CHECK_SYSTEM: &str = "Answer with the JSON schema.";
const CHECK_INPUT: &str = "Say ok.";

async fn check_model(
    config: &config::Config,
    name: &llm::ModelName,
    claude_program: std::path::PathBuf,
    pool: Option<&sqlx::PgPool>,
) -> Result<Vec<String>> {
    let provider = llm::Provider::new(name, config, claude_program)?;
    match &provider {
        llm::Provider::ClaudeCode(_) => {
            let request = llm::Request {
                task: "check".to_string(),
                system: CHECK_SYSTEM.to_string(),
                user: CHECK_INPUT.to_string(),
                schema: serde_json::json!({
                    "type": "object",
                    "additionalProperties": false,
                    "required": ["answer"],
                    "properties": {"answer": {"type": "string"}},
                }),
            };
            let started = std::time::Instant::now();
            let reply = provider
                .send(&request)
                .await
                .map_err(|error| anyhow::anyhow!("{error}"))?;
            if let Some(pool) = pool {
                let call = db::llm::Call {
                    job_id: None,
                    provider: name.provider.as_str(),
                    model: &name.model,
                    task: "check",
                    cache_key: None,
                    input_chars: request.chars() as i64,
                    usage: reply.usage,
                    subscription: reply.subscription,
                    windows: reply.windows,
                    seconds: started.elapsed().as_secs_f64(),
                    status: "ok",
                    error: None,
                };
                let _ = db::llm::record(pool, &call, None, None).await;
            }
            let billing = if reply.subscription {
                "subscription"
            } else {
                "API key, USD caps apply"
            };
            let mut lines = vec![format!(
                "reachable ({billing}), {} input tokens per request at the least",
                reply.usage.input_tokens
            )];
            for (label, window) in [
                ("5-hour", reply.windows.five_hour),
                ("7-day", reply.windows.seven_day),
            ] {
                if let Some(window) = window {
                    lines.push(format!(
                        "{label} window {:.0}% used, resets {}",
                        window.used * 100.0,
                        window.resets_at.format("%Y-%m-%d %H:%M UTC")
                    ));
                }
            }
            Ok(lines)
        }
        llm::Provider::OpenRouter(openrouter) => {
            let prices = openrouter.prices().await?;
            let credit = openrouter.credit().await?;
            let left = credit
                .remaining_usd
                .map_or("no limit on the key".to_string(), |r| {
                    format!("${r:.4} left")
                });
            Ok(vec![format!(
                "reachable, ${:.2} and ${:.2} per million input and output tokens, {left}",
                prices.input * 1e6,
                prices.output * 1e6
            )])
        }
        llm::Provider::Ollama(ollama) => {
            if !ollama.has_model().await? {
                anyhow::bail!(
                    "the model is not pulled on {}; run ollama pull {}",
                    llm::ollama::host(),
                    name.model
                );
            }
            Ok(vec![format!(
                "reachable at {}, model pulled",
                llm::ollama::host()
            )])
        }
    }
}

pub async fn scan(
    config_dir: Option<&Path>,
    sources: &[String],
    extract_minutes: Option<u64>,
) -> Result<ExitCode> {
    let program = std::env::current_exe().context("cannot find the zontologize binary")?;
    let mut options = scan::Options::new(program);
    options.extract_minutes = extract_minutes;
    scan::run(config_dir, sources, database_url().as_deref(), &options).await?;
    Ok(ExitCode::SUCCESS)
}

pub async fn extract_worker() -> Result<ExitCode> {
    let input = tokio::io::BufReader::new(tokio::io::stdin());
    scan::worker::serve(input, tokio::io::stdout()).await?;
    Ok(ExitCode::SUCCESS)
}

pub async fn status(config_dir: Option<&Path>, all_errors: bool) -> Result<ExitCode> {
    let config = config::load(config_dir)?;
    let pool = db::pool::connect(database_url().as_deref(), &config.database.schema, 2).await?;
    let result = print_status(&pool, config_dir, &config, all_errors).await;
    pool.close().await;
    result?;
    Ok(ExitCode::SUCCESS)
}

const STATUS_RUNS: i64 = 5;
const STATUS_ERRORS: i64 = 20;
const STAGES: [&str; 5] = ["walk", "extract", "chunk", "rules", "embed"];

fn run_details(stats: &serde_json::Value) -> Vec<String> {
    let number = |path: [&str; 2]| stats[path[0]][path[1]].as_u64().unwrap_or(0);
    let mut details = Vec::new();
    let restarts: Vec<String> = ["timeout", "memory", "crash", "panic", "files"]
        .iter()
        .map(|reason| (reason, number(["workers", reason])))
        .filter(|(_, count)| *count > 0)
        .map(|(reason, count)| format!("{count} {reason}"))
        .collect();
    if !restarts.is_empty() {
        details.push(format!("worker restarts: {}", restarts.join(", ")));
    }
    let accepted = number(["ocr_ids", "accepted"]);
    let rejected = number(["ocr_ids", "rejected"]);
    if accepted + rejected > 0 {
        details.push(format!(
            "ids in OCR text: {accepted} matched a known key, {rejected} rejected"
        ));
    }
    let left = stats["extract_left"].as_u64().unwrap_or(0);
    if left > 0 {
        details.push(format!("stopped by --extract-minutes with {left} left"));
    }
    details
}

async fn print_status(
    pool: &sqlx::PgPool,
    config_dir: Option<&Path>,
    config: &config::Config,
    all_errors: bool,
) -> Result<()> {
    println!("documents:");
    for row in db::status::document_counts(pool).await? {
        println!(
            "  {} {} {}: {}",
            row.source, row.kind, row.status, row.count
        );
    }
    let totals: Vec<String> = db::status::totals(pool)
        .await?
        .into_iter()
        .map(|(name, count)| format!("{count} {name}"))
        .collect();
    println!("totals: {}", totals.join(", "));
    for (source, pages, confidence) in db::status::ocr_pages(pool).await? {
        let mean = confidence.map_or("-".to_string(), |c| format!("{c:.3}"));
        println!("ocr pages: {source}: {pages}, mean confidence {mean}");
    }
    let (segments, split) = db::status::segments(pool).await?;
    if !segments.is_empty() {
        let per_type: Vec<String> = segments
            .iter()
            .map(|(doc_type, count)| format!("{count} {doc_type}"))
            .collect();
        println!(
            "segments: {}; {split} contents split into several documents",
            per_type.join(", ")
        );
    }
    match scan::prepare(config_dir, &[]) {
        Ok(prepared) => {
            let names: Vec<String> = config.sources.iter().map(|s| s.name.clone()).collect();
            let ids = db::documents::source_ids(pool, &names).await?;
            let extract = db::contents::count_to_extract(pool, &prepared.extract.key, &ids).await?;
            let chunk = db::contents::count_to_chunk(pool, &prepared.chunk.key, &ids).await?;
            let documents =
                db::entities::count_documents_to_rule(pool, &prepared.rules.key, &ids).await?;
            let contents = db::entities::count_contents_to_rule(
                pool,
                &prepared.rules.key,
                &ids,
                &prepared.chunk.key,
            )
            .await?;
            println!(
                "waiting: {extract} to extract, {chunk} to chunk, {documents} documents and {contents} contents to rule"
            );
        }
        Err(error) => println!("waiting: unknown, {error:#}"),
    }
    println!("last runs:");
    for run in db::status::last_runs(pool, STATUS_RUNS).await? {
        let wall = run
            .finished_at
            .map(|f| {
                format!(
                    "{:.1} s",
                    (f - run.started_at).num_milliseconds() as f64 / 1000.0
                )
            })
            .unwrap_or_else(|| "-".to_string());
        let mut parts = Vec::new();
        for stage in STAGES {
            let stats = &run.stats.0[stage];
            if stats.is_null() {
                continue;
            }
            let seconds = stats["seconds"].as_f64().unwrap_or(0.0);
            let items = stats["items"].as_u64().unwrap_or(0);
            let rate = if seconds > 0.0 && items > 0 {
                format!(", {:.1}/s", items as f64 / seconds)
            } else {
                String::new()
            };
            parts.push(format!("{stage} {items} in {seconds:.1} s{rate}"));
        }
        println!(
            "  run {} {} {}, {wall}: {}",
            run.id,
            run.started_at.format("%Y-%m-%d %H:%M UTC"),
            run.status,
            parts.join("; ")
        );
        let extra = run_details(&run.stats.0);
        if !extra.is_empty() {
            println!("    {}", extra.join("; "));
        }
    }
    let limit = (!all_errors).then_some(STATUS_ERRORS);
    let (total, errors) = db::status::errors(pool, limit).await?;
    println!("errors: {total}");
    for error in &errors {
        println!(
            "  {} {} {}: {}",
            error.source, error.stage, error.path, error.message
        );
    }
    if (errors.len() as i64) < total {
        println!(
            "  ... {} more, run zontologize status --errors",
            total - errors.len() as i64
        );
    }
    let usage = db::llm::usage(pool).await?;
    if !usage.is_empty() {
        println!("models:");
    }
    for row in usage {
        println!(
            "  {}:{}: {} calls, {} input and {} output tokens, ${:.4}; last 7 days {} calls, ${:.4}",
            row.provider,
            row.model,
            row.calls,
            row.input_tokens,
            row.output_tokens,
            row.cost_usd,
            row.week_calls,
            row.week_cost_usd
        );
    }
    let jobs = db::llm::jobs(pool, STATUS_RUNS).await?;
    if !jobs.is_empty() {
        println!("model jobs:");
    }
    for job in jobs {
        let mut line = format!(
            "  {} {} {}:{} {}: {} done, {} left, {} invalid, {} errors",
            job.task,
            job.id,
            job.provider,
            job.model,
            job.status,
            job.done,
            job.pending,
            job.invalid,
            job.error
        );
        if let Some(reason) = &job.pause_reason {
            line.push_str(&format!("; {reason}"));
        }
        if let Some(at) = job.resume_after {
            line.push_str(&format!(
                "; goes on after {}",
                at.format("%Y-%m-%d %H:%M UTC")
            ));
        }
        println!("{line}");
    }
    print_model_facts(pool, config).await?;
    print_embeddings(pool).await?;
    print_graph(pool, &config.database.api_schema).await
}

async fn print_graph(pool: &sqlx::PgPool, api_schema: &str) -> Result<()> {
    if let Some(sha) = db::status::ontology_version(pool).await? {
        println!("ontology: version {}", &sha[..sha.len().min(12)]);
    }
    if let Some((stats, finished)) = db::status::last_graph(pool).await? {
        println!(
            "graph: {} vertices, {} edges, synced {}",
            stats["vertices"],
            stats["edges"],
            finished.format("%Y-%m-%d %H:%M UTC")
        );
    }
    match db::status::contract_version(pool, api_schema).await? {
        Some(version) => println!("views: {api_schema} contract {version}"),
        None => println!("views: missing, run zontologize db migrate"),
    }
    Ok(())
}

async fn print_embeddings(pool: &sqlx::PgPool) -> Result<()> {
    for row in db::embed::counts(pool).await? {
        let last = db::status::last_embed(pool, row.id).await?;
        let eligible = |kind: &str| {
            last.as_ref()
                .and_then(|l| l[kind]["eligible"].as_u64())
                .map_or("?".to_string(), |n| n.to_string())
        };
        let mut indexes = Vec::new();
        for kind in [db::embed::CHUNK, db::embed::ENTITY] {
            if db::embed::index_exists(pool, row.id, kind).await? {
                indexes.push(kind);
            }
        }
        let index = if indexes.is_empty() {
            "no index".to_string()
        } else {
            format!("index on {}", indexes.join(" and "))
        };
        let current = if row.current { ", current" } else { "" };
        let embedded = row.chunks + row.entities;
        println!(
            "embeddings {}{current}: {} of {} chunks, {} of {} entities, {} of {} classes; {:.1}% truncated; {index}",
            row.name,
            row.chunks,
            eligible("chunks"),
            row.entities,
            eligible("entities"),
            row.classes,
            eligible("classes"),
            100.0 * row.truncated as f64 / embedded.max(1) as f64
        );
    }
    let links = db::status::resolve_links(pool).await?;
    if !links.is_empty() {
        let parts: Vec<String> = links
            .iter()
            .map(|(method, status, count)| format!("{count} {method} {status}"))
            .collect();
        println!("links: {}", parts.join(", "));
    }
    let (clusters, members) = db::status::clusters(pool).await?;
    if clusters > 0 {
        println!("canonical: {clusters} entities with duplicates, {members} entities in them");
    }
    Ok(())
}

async fn print_model_facts(pool: &sqlx::PgPool, config: &config::Config) -> Result<()> {
    let extract = &config.llm_extract;
    let eligible = db::facts::eligible_count(
        pool,
        &extract.sources,
        &extract.doc_types,
        extract.min_chars as i32,
    )
    .await?;
    let extracted = db::facts::extracted_chunks(pool).await?;
    if extracted > 0 {
        println!("extract: {extracted} chunks extracted, {eligible} eligible");
    }
    for (method, mentions, relations) in db::facts::method_counts(pool).await? {
        println!("model facts: {method}: {mentions} mentions, {relations} relations");
    }
    let types: Vec<String> = db::classify::counts(pool)
        .await?
        .into_iter()
        .map(|(doc_type, count)| format!("{count} {}", doc_type.as_deref().unwrap_or("none")))
        .collect();
    if !types.is_empty() {
        println!("classify: {}", types.join(", "));
    }
    let precision = db::review::precision(pool).await?;
    if !precision.is_empty() {
        println!("review precision:");
    }
    for row in precision {
        println!(
            "  {} {}: {} of {} right ({:.0}%)",
            row.method,
            llm::shortlist::local_name(&row.iri),
            row.right,
            row.total,
            100.0 * row.right as f64 / row.total.max(1) as f64
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn run_details_list_only_what_happened() {
        assert!(run_details(&serde_json::json!({})).is_empty());
        let stats = serde_json::json!({
            "workers": {"timeout": 2, "memory": 0, "crash": 1, "files": 0},
            "ocr_ids": {"accepted": 3, "rejected": 4},
            "extract_left": 5
        });
        assert_eq!(
            run_details(&stats),
            [
                "worker restarts: 2 timeout, 1 crash",
                "ids in OCR text: 3 matched a known key, 4 rejected",
                "stopped by --extract-minutes with 5 left"
            ]
        );
    }
}
