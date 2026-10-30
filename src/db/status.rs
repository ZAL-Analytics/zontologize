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
