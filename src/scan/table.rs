use std::collections::HashMap;
use std::path::PathBuf;

use anyhow::Result;
use sqlx::PgPool;

use super::{Found, Planned, Prepared, SourceStats, apply_plans, planned, walk};
use crate::config::SourceConfig;
use crate::db::documents::{self, Stored};
use crate::db::table::{self, ViewRow};

pub async fn scan_table(
    pool: &PgPool,
    source_id: i64,
    run_id: i64,
    source: &SourceConfig,
    prepared: &Prepared,
    kinds: &walk::Kinds,
) -> Result<SourceStats> {
    let scan = &prepared.config.scan;
    let qualified = table::check_view(pool, &source.view)
        .await
        .map_err(|e| anyhow::anyhow!("sources.{}: {e:#}", source.name))?;
    let mut patterns = scan.ignore.clone();
    patterns.extend(source.ignore.iter().cloned());
    let ignore = walk::globs(&patterns)?;
    let root = PathBuf::from(&source.root);
    let mut stats = SourceStats {
        complete: true,
        ..SourceStats::default()
    };
    let mut view = table::open(pool, &qualified, scan.batch_size).await?;
    let result = async {
        loop {
            let rows = view.next().await?;
            if rows.is_empty() {
                break;
            }
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
                &prepared.hash_pool,
                &mut stats,
            )
            .await?;
        }
        Ok::<(), anyhow::Error>(())
    }
    .await;
    view.close().await;
    result?;
    stats.deleted = documents::mark_deleted(pool, source_id, run_id).await?;
    Ok(stats)
}
