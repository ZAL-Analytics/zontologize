use anyhow::Result;
use sqlx::{FromRow, PgPool};

#[derive(Debug, Clone, FromRow)]
pub struct Chunk {
    pub id: i64,
    pub content_id: i64,
    pub text: String,
    pub page_from: Option<i32>,
    pub page_to: Option<i32>,
    pub path: String,
}

pub async fn ontology_version(pool: &PgPool, sha256: &str) -> Result<Option<i64>> {
    let id = sqlx::query_scalar("select id from ontology_versions where sha256 = $1")
        .bind(sha256)
        .fetch_optional(pool)
        .await?;
    Ok(id)
}

pub async fn sample(
    pool: &PgPool,
    seed: &str,
    min_chars: i32,
    matched: bool,
    limit: i64,
) -> Result<Vec<i64>> {
    let ids = sqlx::query_scalar(
        "with eligible as (select c.id, d.source_id, d.kind, \
         case when position('/' in d.path) > 0 then split_part(d.path, '/', 1) else '' end \
         as folder from chunks c \
         join lateral (select source_id, kind, path from documents d \
         where d.content_id = c.content_id and d.status = 'present' order by d.id limit 1) d \
         on true \
         where length(c.text) >= $2 \
         and exists (select 1 from class_mentions m where m.chunk_id = c.id) = $3), \
         ranked as (select id, md5($1 || id::text) as h, row_number() over \
         (partition by source_id, kind, folder order by md5($1 || id::text)) as rn \
         from eligible) \
         select id from ranked order by rn, h limit $4",
    )
    .bind(seed)
    .bind(min_chars)
    .bind(matched)
    .bind(limit)
    .fetch_all(pool)
    .await?;
    Ok(ids)
}

pub async fn chunks(pool: &PgPool, ids: &[i64]) -> Result<Vec<Chunk>> {
    let rows = sqlx::query_as(
        "select c.id, c.content_id, c.text, c.page_from, c.page_to, d.path from chunks c \
         join lateral (select path from documents d where d.content_id = c.content_id \
         and d.status = 'present' order by d.id limit 1) d on true where c.id = any($1)",
    )
    .bind(ids)
    .fetch_all(pool)
    .await?;
    Ok(rows)
}

pub async fn class_counts(pool: &PgPool, ids: &[i64]) -> Result<Vec<(i64, String, i64)>> {
    let rows = sqlx::query_as(
        "select chunk_id, class_iri, count(*) from class_mentions where chunk_id = any($1) \
         group by 1, 2",
    )
    .bind(ids)
    .fetch_all(pool)
    .await?;
    Ok(rows)
}
