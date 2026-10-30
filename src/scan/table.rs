use std::collections::HashMap;
use std::path::PathBuf;

use anyhow::{Result, bail};
use sqlx::PgPool;
use sqlx::types::chrono::{DateTime, Utc};

use super::{Found, Planned, Prepared, SourceStats, apply_plans, planned, walk};
use crate::config::SourceConfig;
use crate::db::documents::{self, Stored};
use crate::db::quote_identifier;

pub const COLUMNS: [&str; 6] = ["id", "path", "file", "sha256", "size", "modified"];

type ViewRow = (
    String,
    String,
    Option<String>,
    Option<String>,
    i64,
    DateTime<Utc>,
);

pub async fn check_view(pool: &PgPool, view: &str) -> Result<String> {
    let Some((schema, name)) = view.split_once('.') else {
        bail!("view {view:?} must be schema.view");
    };
    let columns: Vec<String> = sqlx::query_scalar(
        "select column_name::text from information_schema.columns \
         where table_schema = $1 and table_name = $2",
    )
    .bind(schema)
    .bind(name)
    .fetch_all(pool)
    .await?;
    if columns.is_empty() {
        bail!("view {view} does not exist or is not readable");
    }
    for column in COLUMNS {
        if !columns.iter().any(|c| c == column) {
            bail!("view {view} has no column {column}");
        }
    }
    Ok(format!(
        "{}.{}",
        quote_identifier(schema),
        quote_identifier(name)
    ))
}

pub async fn scan_table(
    pool: &PgPool,
    source_id: i64,
    run_id: i64,
    source: &SourceConfig,
    prepared: &Prepared,
    kinds: &walk::Kinds,
) -> Result<SourceStats> {
    let scan = &prepared.config.scan;
    let qualified = check_view(pool, &source.view)
        .await
        .map_err(|e| anyhow::anyhow!("sources.{}: {e:#}", source.name))?;
    let sql = format!(
        "select id::text, path::text, file::text, sha256::text, size::bigint, \
         modified::timestamptz from {qualified} where id::text > $1 order by id::text limit $2"
    );
    let mut patterns = scan.ignore.clone();
    patterns.extend(source.ignore.iter().cloned());
    let ignore = walk::globs(&patterns)?;
    let root = PathBuf::from(&source.root);
    let mut stats = SourceStats {
        complete: true,
        ..SourceStats::default()
    };
    let mut after = String::new();
    loop {
        let rows: Vec<ViewRow> = sqlx::query_as(sqlx::AssertSqlSafe(sql.clone()))
            .bind(&after)
            .bind(scan.batch_size as i64)
            .fetch_all(pool)
            .await?;
        let Some(last) = rows.last() else {
            break;
        };
        after = last.0.clone();
        let rows: Vec<ViewRow> = rows
            .into_iter()
            .map(|mut row| {
                row.1 = row.1.trim_start_matches('/').to_string();
                row
            })
            .filter(|row| !ignore.is_match(&row.1))
            .collect();
        stats.files += rows.len() as u64;
        let ids: Vec<String> = rows.iter().map(|r| r.0.clone()).collect();
        let stored: HashMap<String, Stored> = documents::by_external_ids(pool, source_id, &ids)
            .await?
            .into_iter()
            .filter_map(|s| s.external_id.clone().map(|id| (id, s)))
            .collect();
        let plans: Vec<Planned> = rows
            .into_iter()
            .map(|(id, path, file, sha256, size, modified)| {
                let found = Found {
                    path,
                    external_id: Some(id),
                    file,
                    size,
                    mtime: walk::mtime(modified.into()),
                    sha256: sha256.map(|s| s.to_lowercase()),
                };
                let decision = kinds.decide(&found.path, found.size.max(0) as u64);
                let existing = found.external_id.as_ref().and_then(|id| stored.get(id));
                planned(existing, &found, &decision)
            })
            .collect();
        apply_plans(
            pool,
            source_id,
            run_id,
            &root,
            plans,
            scan.hash_threads,
            &mut stats,
        )
        .await?;
    }
    stats.deleted = documents::mark_deleted(pool, source_id, run_id).await?;
    Ok(stats)
}
