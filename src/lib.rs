pub mod config;
pub mod db;
pub mod ontology;
pub mod rules;
pub mod scan;

use std::path::Path;
use std::process::ExitCode;

use anyhow::Result;

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

pub async fn scan(config_dir: Option<&Path>, sources: &[String]) -> Result<ExitCode> {
    scan::run(config_dir, sources, database_url().as_deref()).await?;
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
