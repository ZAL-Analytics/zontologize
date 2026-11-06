use std::path::Path;

use anyhow::Result;
use serde::Deserialize;
use sqlx::PgPool;

use crate::config::Config;
use crate::db;
use crate::embed::run::Counts;
use crate::ns::local_name;
use crate::scan::pool::Restarts;
use crate::scan::{OcrIdStats, StageStats};

const STATUS_RUNS: i64 = 5;
const STATUS_ERRORS: i64 = 20;

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct RunSummary {
    walk: Option<StageStats>,
    extract: Option<StageStats>,
    chunk: Option<StageStats>,
    rules: Option<StageStats>,
    embed: Option<StageStats>,
    workers: Restarts,
    ocr_ids: OcrIdStats,
    extract_left: i64,
}

impl RunSummary {
    fn parse(stats: &serde_json::Value) -> RunSummary {
        serde_json::from_value(stats.clone()).unwrap_or_default()
    }

    fn stages(&self) -> [(&'static str, Option<&StageStats>); 5] {
        [
            ("walk", self.walk.as_ref()),
            ("extract", self.extract.as_ref()),
            ("chunk", self.chunk.as_ref()),
            ("rules", self.rules.as_ref()),
            ("embed", self.embed.as_ref()),
        ]
    }
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct EmbedSummary {
    chunks: Option<Counts>,
    entities: Option<Counts>,
    classes: Option<Counts>,
}

pub async fn run(config_dir: Option<&Path>, url: Option<&str>, all_errors: bool) -> Result<()> {
    let config = crate::config::load(config_dir)?;
    let pool = db::pool::connect(url, &config.database.schema, 2).await?;
    let result = print_status(&pool, config_dir, &config, all_errors).await;
    pool.close().await;
    result
}

fn run_details(run: &RunSummary) -> Vec<String> {
    let workers = &run.workers;
    let mut details = Vec::new();
    let restarts: Vec<String> = [
        ("timeout", workers.timeout),
        ("memory", workers.memory),
        ("crash", workers.crash),
        ("panic", workers.panic),
        ("files", workers.files),
    ]
    .iter()
    .filter(|(_, count)| *count > 0)
    .map(|(reason, count)| format!("{count} {reason}"))
    .collect();
    if !restarts.is_empty() {
        details.push(format!("worker restarts: {}", restarts.join(", ")));
    }
    let accepted = run.ocr_ids.accepted;
    let rejected = run.ocr_ids.rejected;
    if accepted + rejected > 0 {
        details.push(format!(
            "ids in OCR text: {accepted} matched a known key, {rejected} rejected"
        ));
    }
    if run.extract_left > 0 {
        details.push(format!(
            "stopped by --extract-minutes with {} left",
            run.extract_left
        ));
    }
    details
}

fn stage_parts(run: &RunSummary) -> Vec<String> {
    let mut parts = Vec::new();
    for (stage, stats) in run.stages() {
        let Some(stats) = stats else {
            continue;
        };
        let (seconds, items) = (stats.seconds, stats.items);
        let rate = if seconds > 0.0 && items > 0 {
            format!(", {:.1}/s", items as f64 / seconds)
        } else {
            String::new()
        };
        parts.push(format!("{stage} {items} in {seconds:.1} s{rate}"));
    }
    parts
}

struct Keys {
    extract: String,
    chunk: String,
    rules: String,
}

fn keys(config_dir: Option<&Path>, config: &Config) -> Result<Keys> {
    let ontology = crate::ontology::load_checked(config_dir, &config.ontology)?.ontology;
    Ok(Keys {
        extract: crate::scan::extract::extract_key(&config.extract, &config.ocr),
        chunk: crate::scan::chunk::ChunkSettings::new(&config.chunk, config.extract.format).key,
        rules: crate::rules::key(config_dir, &ontology)?,
    })
}

async fn print_waiting(pool: &PgPool, config_dir: Option<&Path>, config: &Config) -> Result<()> {
    let keys = match keys(config_dir, config) {
        Ok(keys) => keys,
        Err(error) => {
            println!("waiting: unknown, {error:#}");
            return Ok(());
        }
    };
    let names: Vec<String> = config.sources.iter().map(|s| s.name.clone()).collect();
    let ids = db::documents::source_ids(pool, &names).await?;
    let extract = db::contents::count_to_extract(pool, &keys.extract, &ids).await?;
    let chunk = db::contents::count_to_chunk(pool, &keys.chunk, &ids).await?;
    let documents = db::entities::count_documents_to_rule(pool, &keys.rules, &ids).await?;
    let contents =
        db::entities::count_contents_to_rule(pool, &keys.rules, &ids, &keys.chunk).await?;
    println!(
        "waiting: {extract} to extract, {chunk} to chunk, {documents} documents and {contents} contents to rule"
    );
    Ok(())
}

async fn print_status(
    pool: &PgPool,
    config_dir: Option<&Path>,
    config: &Config,
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
    print_waiting(pool, config_dir, config).await?;
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
        let summary = RunSummary::parse(&run.stats.0);
        println!(
            "  run {} {} {}, {wall}: {}",
            run.id,
            run.started_at.format("%Y-%m-%d %H:%M UTC"),
            run.status,
            stage_parts(&summary).join("; ")
        );
        let extra = run_details(&summary);
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
    print_models(pool).await?;
    print_model_facts(pool, config).await?;
    print_embeddings(pool).await?;
    print_graph(pool, &config.database.api_schema).await
}

async fn print_models(pool: &PgPool) -> Result<()> {
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
    Ok(())
}

async fn print_graph(pool: &PgPool, api_schema: &str) -> Result<()> {
    if let Some(sha) = db::status::ontology_version(pool).await? {
        println!("ontology: version {}", &sha[..sha.len().min(12)]);
    }
    if let Some((stats, finished)) = db::status::last_graph(pool).await? {
        let stats: crate::graph::Stats = serde_json::from_value(stats.0).unwrap_or_default();
        println!(
            "graph: {} vertices, {} edges, synced {}",
            stats.vertices,
            stats.edges,
            finished.format("%Y-%m-%d %H:%M UTC")
        );
    }
    match db::status::contract_version(pool, api_schema).await? {
        Some(version) => println!("views: {api_schema} contract {version}"),
        None => println!("views: missing, run zontologize db migrate"),
    }
    Ok(())
}

fn eligible(counts: Option<&Counts>) -> String {
    counts.map_or("?".to_string(), |c| c.eligible.to_string())
}

async fn print_embeddings(pool: &PgPool) -> Result<()> {
    for row in db::embed::counts(pool).await? {
        let last: EmbedSummary = db::status::last_embed(pool, row.id)
            .await?
            .map(|stats| serde_json::from_value(stats).unwrap_or_default())
            .unwrap_or_default();
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
            eligible(last.chunks.as_ref()),
            row.entities,
            eligible(last.entities.as_ref()),
            row.classes,
            eligible(last.classes.as_ref()),
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

async fn print_model_facts(pool: &PgPool, config: &Config) -> Result<()> {
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
    let sent = db::attributes::sent_chunks(pool).await?;
    if sent > 0 {
        let parts: Vec<String> = db::attributes::counts(pool)
            .await?
            .into_iter()
            .map(|(iri, count)| format!("{count} {}", local_name(&iri)))
            .collect();
        println!(
            "attributes: {sent} chunks answered; {}",
            if parts.is_empty() {
                "no values".to_string()
            } else {
                parts.join(", ")
            }
        );
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
            local_name(&row.iri),
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
        assert!(run_details(&RunSummary::parse(&serde_json::json!({}))).is_empty());
        let stats = serde_json::json!({
            "workers": {"timeout": 2, "memory": 0, "crash": 1, "files": 0},
            "ocr_ids": {"accepted": 3, "rejected": 4},
            "extract_left": 5
        });
        assert_eq!(
            run_details(&RunSummary::parse(&stats)),
            [
                "worker restarts: 2 timeout, 1 crash",
                "ids in OCR text: 3 matched a known key, 4 rejected",
                "stopped by --extract-minutes with 5 left"
            ]
        );
    }

    #[test]
    fn stage_parts_skip_missing_stages_and_read_embed_runs() {
        let scan = serde_json::json!({
            "walk": {"items": 3, "errors": 0, "seconds": 1.5},
            "chunk": {"items": 0, "errors": 0, "seconds": 2.25},
            "error": "ignored"
        });
        assert_eq!(
            stage_parts(&RunSummary::parse(&scan)),
            ["walk 3 in 1.5 s, 2.0/s", "chunk 0 in 2.2 s"]
        );
        let embed = serde_json::json!({
            "embed": {"seconds": 30.0, "items": 42, "model_id": 1, "chunks": {"eligible": 12}}
        });
        assert_eq!(
            stage_parts(&RunSummary::parse(&embed)),
            ["embed 42 in 30.0 s, 1.4/s"]
        );
        assert!(stage_parts(&RunSummary::parse(&serde_json::json!({"error": "x"}))).is_empty());
    }
}
