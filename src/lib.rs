pub mod config;
pub mod db;
pub mod ontology;
pub mod rules;
pub mod scan;

use std::path::Path;
use std::process::ExitCode;

use anyhow::{Context, Result};

pub async fn db_migrate(config_dir: Option<&Path>) -> Result<ExitCode> {
    let config = config::load(config_dir)?;
    let schema = &config.database.schema;
    let mut conn = db::connect(database_url().as_deref(), schema).await?;
    let result = db::migrate::run(&mut conn, schema).await;
    db::close(conn).await;
    let applied = result?;
    if applied.is_empty() {
        println!("schema {schema}: nothing to do");
    }
    for version in applied {
        println!("schema {schema}: applied {version:04}");
    }
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
const STAGES: [&str; 4] = ["walk", "extract", "chunk", "rules"];

fn run_details(stats: &serde_json::Value) -> Vec<String> {
    let number = |path: [&str; 2]| stats[path[0]][path[1]].as_u64().unwrap_or(0);
    let mut details = Vec::new();
    let restarts: Vec<String> = ["timeout", "memory", "crash", "files"]
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
