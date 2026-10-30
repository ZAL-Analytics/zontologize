use anyhow::Result;
use sqlx::{FromRow, PgPool};

use super::copy_row;
use crate::scan::chunk::Chunk;
use crate::scan::extract::Page;

#[derive(Debug, Clone, FromRow)]
pub struct Pending {
    pub id: i64,
    pub path: String,
    pub file: Option<String>,
    pub source_id: i64,
}

const HAS_DOCUMENT: &str = "exists (select 1 from documents d where d.content_id = c.id \
                            and d.status = 'present' and d.source_id = any($2))";

pub async fn count_to_extract(pool: &PgPool, key: &str, sources: &[i64]) -> Result<i64> {
    let sql = format!(
        "select count(*) from contents c where c.extract_key is distinct from $1 and {HAS_DOCUMENT}"
    );
    let count = sqlx::query_scalar(sqlx::AssertSqlSafe(sql))
        .bind(key)
        .bind(sources)
        .fetch_one(pool)
        .await?;
    Ok(count)
}

pub async fn next_to_extract(
    pool: &PgPool,
    key: &str,
    sources: &[i64],
    after: i64,
    limit: i64,
) -> Result<Vec<Pending>> {
    let rows = sqlx::query_as(
        "select c.id, d.path, d.file, d.source_id from contents c \
         join lateral (select path, file, source_id from documents d where d.content_id = c.id \
         and d.status = 'present' and d.source_id = any($2) order by d.id limit 1) d on true \
         where c.extract_key is distinct from $1 and c.id > $3 order by c.id limit $4",
    )
    .bind(key)
    .bind(sources)
    .bind(after)
    .bind(limit)
    .fetch_all(pool)
    .await?;
    Ok(rows)
}

pub async fn write_pages(
    pool: &PgPool,
    content_id: i64,
    key: &str,
    pages: &[Page],
    seconds: f64,
) -> Result<()> {
    let mut tx = pool.begin().await?;
    sqlx::query("delete from pages where content_id = $1")
        .bind(content_id)
        .execute(&mut *tx)
        .await?;
    let mut data = String::new();
    let id = content_id.to_string();
    let mut seen = std::collections::HashSet::new();
    for page in pages {
        if !seen.insert(page.page_no) {
            continue;
        }
        let page_no = page.page_no.to_string();
        let confidence = page.ocr_confidence.map(|c| c.to_string());
        copy_row(
            &mut data,
            &[
                Some(&id),
                Some(&page_no),
                Some(page.method.as_str()),
                confidence.as_deref(),
                Some(&page.text),
            ],
        );
    }
    if !data.is_empty() {
        let mut copy = tx
            .copy_in_raw(
                "copy pages (content_id, page_no, method, ocr_confidence, text) from stdin",
            )
            .await?;
        copy.send(data.into_bytes()).await?;
        copy.finish().await?;
    }
    sqlx::query(
        "update contents set status = 'done', error = null, extract_key = $2, \
         extract_seconds = $3, chunk_key = null, rules_key = null where id = $1",
    )
    .bind(content_id)
    .bind(key)
    .bind(seconds)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(())
}

pub async fn write_error(
    pool: &PgPool,
    content_id: i64,
    key: &str,
    message: &str,
    seconds: f64,
) -> Result<()> {
    let mut tx = pool.begin().await?;
    sqlx::query("delete from pages where content_id = $1")
        .bind(content_id)
        .execute(&mut *tx)
        .await?;
    sqlx::query("delete from chunks where content_id = $1")
        .bind(content_id)
        .execute(&mut *tx)
        .await?;
    sqlx::query(
        "update contents set status = 'error', error = $3, extract_key = $2, \
         extract_seconds = $4, chunk_key = null, rules_key = null where id = $1",
    )
    .bind(content_id)
    .bind(key)
    .bind(message.replace('\0', ""))
    .bind(seconds)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(())
}

pub async fn count_to_chunk(pool: &PgPool, key: &str, sources: &[i64]) -> Result<i64> {
    let sql = format!(
        "select count(*) from contents c where c.status = 'done' \
         and c.chunk_key is distinct from $1 and {HAS_DOCUMENT}"
    );
    let count = sqlx::query_scalar(sqlx::AssertSqlSafe(sql))
        .bind(key)
        .bind(sources)
        .fetch_one(pool)
        .await?;
    Ok(count)
}

pub async fn next_to_chunk(
    pool: &PgPool,
    key: &str,
    sources: &[i64],
    after: i64,
    limit: i64,
) -> Result<Vec<i64>> {
    let sql = format!(
        "select c.id from contents c where c.status = 'done' \
         and c.chunk_key is distinct from $1 and {HAS_DOCUMENT} and c.id > $3 \
         order by c.id limit $4"
    );
    let ids = sqlx::query_scalar(sqlx::AssertSqlSafe(sql))
        .bind(key)
        .bind(sources)
        .bind(after)
        .bind(limit)
        .fetch_all(pool)
        .await?;
    Ok(ids)
}

pub async fn pages_of(pool: &PgPool, ids: &[i64]) -> Result<Vec<(i64, i32, String)>> {
    let rows = sqlx::query_as(
        "select content_id, page_no, text from pages where content_id = any($1) \
         order by content_id, page_no",
    )
    .bind(ids)
    .fetch_all(pool)
    .await?;
    Ok(rows)
}

pub async fn write_chunks(
    pool: &PgPool,
    content_id: i64,
    key: &str,
    ts_config: &str,
    chunks: &[Chunk],
) -> Result<()> {
    let mut tx = pool.begin().await?;
    sqlx::query("delete from chunks where content_id = $1")
        .bind(content_id)
        .execute(&mut *tx)
        .await?;
    if !chunks.is_empty() {
        let id = content_id.to_string();
        let mut data = String::new();
        for chunk in chunks {
            let ord = chunk.ord.to_string();
            let from = chunk.page_from.map(|p| p.to_string());
            let to = chunk.page_to.map(|p| p.to_string());
            copy_row(
                &mut data,
                &[
                    Some(&id),
                    Some(&ord),
                    from.as_deref(),
                    to.as_deref(),
                    Some(&chunk.text),
                    Some(ts_config),
                ],
            );
        }
        let mut copy = tx
            .copy_in_raw(
                "copy chunks (content_id, ord, page_from, page_to, text, ts_config) from stdin",
            )
            .await?;
        copy.send(data.into_bytes()).await?;
        copy.finish().await?;
    }
    sqlx::query("update contents set chunk_key = $2, rules_key = null where id = $1")
        .bind(content_id)
        .bind(key)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(())
}

pub async fn check_text_search(pool: &PgPool, name: &str) -> Result<bool> {
    let exists = sqlx::query_scalar("select exists(select 1 from pg_ts_config where cfgname = $1)")
        .bind(name)
        .fetch_one(pool)
        .await?;
    Ok(exists)
}
