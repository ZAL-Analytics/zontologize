use anyhow::Result;
use sqlx::types::chrono::{DateTime, Utc};
use sqlx::{FromRow, PgPool};

#[derive(Debug, FromRow)]
pub struct DocumentCount {
    pub source: String,
    pub kind: String,
    pub status: String,
    pub count: i64,
}

#[derive(Debug, FromRow)]
pub struct Run {
    pub id: i64,
    pub started_at: DateTime<Utc>,
    pub finished_at: Option<DateTime<Utc>>,
    pub status: String,
    pub stats: sqlx::types::Json<serde_json::Value>,
}

#[derive(Debug, FromRow)]
pub struct Error {
    pub source: String,
    pub path: String,
    pub stage: String,
    pub message: String,
}

pub async fn document_counts(pool: &PgPool) -> Result<Vec<DocumentCount>> {
    let rows = sqlx::query_as(
        "select s.name as source, coalesce(d.kind, '-') as kind, d.status, count(*) as count \
         from documents d join sources s on s.id = d.source_id group by 1, 2, 3 order by 1, 2, 3",
    )
    .fetch_all(pool)
    .await?;
    Ok(rows)
}

pub async fn totals(pool: &PgPool) -> Result<Vec<(String, i64)>> {
    let rows = sqlx::query_as(
        "select 'contents', count(*) from contents union all \
         select 'pages', count(*) from pages union all \
         select 'chunks', count(*) from chunks union all \
         select 'entities', count(*) from entities union all \
         select 'mentions', count(*) from mentions union all \
         select 'relations', count(*) from relations union all \
         select 'class mentions', count(*) from class_mentions",
    )
    .fetch_all(pool)
    .await?;
    Ok(rows)
}

pub async fn ocr_pages(pool: &PgPool) -> Result<Vec<(String, i64, Option<f64>)>> {
    let rows = sqlx::query_as(
        "select s.name, count(*), avg(p.ocr_confidence) from pages p \
         join lateral (select source_id from documents d where d.content_id = p.content_id \
         and d.status = 'present' order by d.id limit 1) d on true \
         join sources s on s.id = d.source_id where p.method = 'ocr' group by 1 order by 1",
    )
    .fetch_all(pool)
    .await?;
    Ok(rows)
}

pub async fn segments(pool: &PgPool) -> Result<(Vec<(String, i64)>, i64)> {
    let per_type = sqlx::query_as(
        "select coalesce(doc_type, '-'), count(*) from segments group by 1 order by 1",
    )
    .fetch_all(pool)
    .await?;
    let split = sqlx::query_scalar(
        "select count(*) from (select content_id from segments group by 1 having count(*) > 1) s",
    )
    .fetch_one(pool)
    .await?;
    Ok((per_type, split))
}

pub async fn last_runs(pool: &PgPool, limit: i64) -> Result<Vec<Run>> {
    let rows = sqlx::query_as(
        "select id, started_at, finished_at, status, stats from runs order by id desc limit $1",
    )
    .bind(limit)
    .fetch_all(pool)
    .await?;
    Ok(rows)
}

pub async fn errors(pool: &PgPool, limit: Option<i64>) -> Result<(i64, Vec<Error>)> {
    let query = "select s.name as source, d.path, 'walk' as stage, coalesce(d.reason, '') as message \
         from documents d join sources s on s.id = d.source_id where d.status = 'error' \
         union all \
         select s.name, d.path, 'extract', coalesce(c.error, '') from contents c \
         join lateral (select path, source_id from documents d where d.content_id = c.id \
         and d.status = 'present' order by d.id limit 1) d on true \
         join sources s on s.id = d.source_id where c.status = 'error'";
    let total: i64 = sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
        "select count(*) from ({query}) e"
    )))
    .fetch_one(pool)
    .await?;
    let rows = sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "select * from ({query}) e order by 1, 2 limit $1"
    )))
    .bind(limit)
    .fetch_all(pool)
    .await?;
    Ok((total, rows))
}

pub async fn resolve_links(pool: &PgPool) -> Result<Vec<(String, String, i64)>> {
    let rows = sqlx::query_as(
        "select method, status, count(*) from relations where method like 'resolve:%' \
         group by 1, 2 order by 1, 2",
    )
    .fetch_all(pool)
    .await?;
    Ok(rows)
}

pub async fn clusters(pool: &PgPool) -> Result<(i64, i64)> {
    let row = sqlx::query_as(
        "select count(*), coalesce(sum(n), 0)::bigint from (select count(*) as n from entities \
         where canonical_id is not null group by canonical_id having count(*) > 1) c",
    )
    .fetch_one(pool)
    .await?;
    Ok(row)
}

pub async fn last_embed(pool: &PgPool, model_id: i64) -> Result<Option<serde_json::Value>> {
    let row: Option<sqlx::types::Json<serde_json::Value>> = sqlx::query_scalar(
        "select stats -> 'embed' from runs where status = 'finished' \
         and (stats -> 'embed' ->> 'model_id')::bigint = $1 order by id desc limit 1",
    )
    .bind(model_id)
    .fetch_optional(pool)
    .await?;
    Ok(row.map(|j| j.0))
}
