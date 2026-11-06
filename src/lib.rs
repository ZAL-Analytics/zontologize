pub mod attributes;
pub mod classify;
pub mod config;
pub mod cpu;
pub mod db;
pub mod embed;
pub mod export;
pub mod facts;
pub mod fit;
pub mod graph;
pub mod hash;
pub mod llm;
pub mod ns;
pub mod ontology;
pub mod resolve;
pub mod review;
pub mod rules;
pub mod scan;
pub mod status;
pub mod text;
pub mod units;

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
    let _ = db::runs::unlock(&mut conn, schema).await;
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
        for property in onto.properties.values().filter(|p| p.is_attribute()) {
            match property.value {
                Some(ontology::ValueKind::Number) if property.quantity_kind.is_some() => println!(
                    "attribute {}: number, {} units fit",
                    property.iri,
                    onto.fitting_units(property).len()
                ),
                Some(kind) => println!("attribute {}: {}", property.iri, kind.as_str()),
                None => {}
            }
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
    if stats.unchanged {
        println!("graph sync: entities, relations and labels unchanged since the last sync");
    }
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

pub async fn attributes(
    config_dir: Option<&Path>,
    options: &llm::job::Options,
) -> Result<ExitCode> {
    let outcome = attributes::run::run(config_dir, database_url().as_deref(), options).await?;
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
    let pool = db::pool::connect(database_url().as_deref(), &config.database.schema, 1)
        .await
        .ok();
    let mut failed = false;
    let mut checked: Vec<(&str, &str)> = Vec::new();
    for (task, model) in config.models.llm_tasks() {
        if model.is_empty() {
            println!("{task}: no model configured");
            continue;
        }
        if let Some((first, _)) = checked.iter().find(|(_, m)| *m == model) {
            println!("{task} {model}: same model as {first}");
            continue;
        }
        checked.push((task, model));
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

const CHECK_SYSTEM: &str = include_str!("../prompts/check-system.md");
const CHECK_USER: &str = include_str!("../prompts/check-user.md");
const CHECK_SCHEMA: &str = include_str!("../prompts/check-schema.json");

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
                system: CHECK_SYSTEM.trim_end().to_string(),
                user: CHECK_USER.trim_end().to_string(),
                schema: serde_json::from_str(CHECK_SCHEMA)
                    .context("built-in prompts/check-schema.json is not valid JSON")?,
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
                if let Err(error) = db::llm::record(pool, &call, None, None).await {
                    eprintln!("models check: the call was not recorded: {error:#}");
                }
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
    status::run(config_dir, database_url().as_deref(), all_errors).await?;
    Ok(ExitCode::SUCCESS)
}
