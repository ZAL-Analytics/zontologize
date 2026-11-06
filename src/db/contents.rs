use std::collections::{HashMap, HashSet};

use anyhow::Result;
use sqlx::{FromRow, PgConnection, PgPool};

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
    retries: bool,
    after: i64,
    limit: i64,
) -> Result<Vec<Pending>> {
    let rows = sqlx::query_as(
        "select c.id, d.path, d.file, d.source_id from contents c \
         join lateral (select path, file, source_id from documents d where d.content_id = c.id \
         and d.status = 'present' and d.source_id = any($2) order by d.id limit 1) d on true \
         where c.extract_key is distinct from $1 and (c.attempts > 0) = $3 and c.id > $4 \
         order by c.id limit $5",
    )
    .bind(key)
    .bind(sources)
    .bind(retries)
    .bind(after)
    .bind(limit)
    .fetch_all(pool)
    .await?;
    Ok(rows)
}

pub async fn recover_unfinished(
    pool: &PgPool,
    key: &str,
    max_attempts: i32,
    message: &str,
) -> Result<(u64, u64)> {
    let failed = sqlx::query(
        "update contents set status = 'error', error = $3, extract_key = $1 \
         where status = 'running' and attempts >= $2",
    )
    .bind(key)
    .bind(max_attempts)
    .bind(message)
    .execute(pool)
    .await?
    .rows_affected();
    let retried = sqlx::query("update contents set status = 'pending' where status = 'running'")
        .execute(pool)
        .await?
        .rows_affected();
    Ok((failed, retried))
}

pub async fn start_attempt(pool: &PgPool, content_id: i64) -> Result<()> {
    sqlx::query("update contents set status = 'running', attempts = attempts + 1 where id = $1")
        .bind(content_id)
        .execute(pool)
        .await?;
    Ok(())
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
        "update contents set status = 'done', error = null, extract_key = $2, attempts = 0, \
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

const CHUNK_FACTS: [&str; 5] = [
    "mentions",
    "class_mentions",
    "relation_terms",
    "attributes",
    "llm_job_items",
];

pub async fn write_chunks(
    pool: &PgPool,
    content_id: i64,
    key: &str,
    ts_config: &str,
    chunks: &[Chunk],
) -> Result<()> {
    let mut tx = pool.begin().await?;
    let old: Vec<(i64, i32, String)> =
        sqlx::query_as("select id, ord, text from chunks where content_id = $1 order by ord")
            .bind(content_id)
            .fetch_all(&mut *tx)
            .await?;
    let old_by_ord: HashMap<i32, (i64, &str)> = old
        .iter()
        .map(|(id, ord, text)| (*ord, (*id, text.as_str())))
        .collect();
    let mut changed = Vec::new();
    let mut kept = Vec::new();
    let mut fresh = Vec::new();
    for chunk in chunks {
        match old_by_ord.get(&chunk.ord) {
            Some((id, text)) if *text == chunk.text => kept.push((*id, chunk)),
            Some((id, _)) => changed.push((*id, chunk)),
            None => fresh.push(chunk),
        }
    }
    let new_ords: HashSet<i32> = chunks.iter().map(|c| c.ord).collect();
    let removed: Vec<i64> = old
        .iter()
        .filter(|(_, ord, _)| !new_ords.contains(ord))
        .map(|(id, _, _)| *id)
        .collect();
    let changed_ids: Vec<i64> = changed.iter().map(|(id, _)| *id).collect();
    clear_chunk_facts(&mut tx, &changed_ids).await?;
    let keep_in = kept
        .iter()
        .chain(&changed)
        .max_by_key(|(_, chunk)| chunk.ord)
        .map(|(id, _)| *id);
    if let Some(target) = keep_in {
        sqlx::query(
            "update relations set chunk_id = $2, span_start = null, span_end = null \
             where chunk_id = any($1) and status in ('confirmed', 'rejected')",
        )
        .bind(&removed)
        .bind(target)
        .execute(&mut *tx)
        .await?;
    }
    sqlx::query("delete from chunks where id = any($1)")
        .bind(&removed)
        .execute(&mut *tx)
        .await?;
    update_chunks(&mut tx, &kept, ts_config, false).await?;
    update_chunks(&mut tx, &changed, ts_config, true).await?;
    insert_chunks(&mut tx, content_id, ts_config, &fresh).await?;
    sqlx::query("update contents set chunk_key = $2, rules_key = null where id = $1")
        .bind(content_id)
        .bind(key)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(())
}

async fn clear_chunk_facts(conn: &mut PgConnection, ids: &[i64]) -> Result<()> {
    if ids.is_empty() {
        return Ok(());
    }
    for table in CHUNK_FACTS {
        let sql = format!("delete from {table} where chunk_id = any($1)");
        sqlx::query(sqlx::AssertSqlSafe(sql))
            .bind(ids)
            .execute(&mut *conn)
            .await?;
    }
    sqlx::query(
        "delete from relations where chunk_id = any($1) and status in ('auto', 'proposed')",
    )
    .bind(ids)
    .execute(&mut *conn)
    .await?;
    sqlx::query("update relations set span_start = null, span_end = null where chunk_id = any($1)")
        .bind(ids)
        .execute(&mut *conn)
        .await?;
    Ok(())
}

async fn update_chunks(
    conn: &mut PgConnection,
    rows: &[(i64, &Chunk)],
    ts_config: &str,
    reset: bool,
) -> Result<()> {
    if rows.is_empty() {
        return Ok(());
    }
    let ids: Vec<i64> = rows.iter().map(|(id, _)| *id).collect();
    let from: Vec<Option<i32>> = rows.iter().map(|(_, c)| c.page_from).collect();
    let to: Vec<Option<i32>> = rows.iter().map(|(_, c)| c.page_to).collect();
    let texts: Vec<&str> = rows.iter().map(|(_, c)| c.text.as_str()).collect();
    sqlx::query(
        "update chunks k set page_from = u.page_from, page_to = u.page_to, text = u.text, \
         ts_config = $5::regconfig, \
         llm_key = case when $6 then null else k.llm_key end, \
         attr_key = case when $6 then null else k.attr_key end \
         from unnest($1::bigint[], $2::int[], $3::int[], $4::text[]) \
         as u(id, page_from, page_to, text) \
         where k.id = u.id and (k.text <> u.text or k.page_from is distinct from u.page_from \
         or k.page_to is distinct from u.page_to or k.ts_config <> $5::regconfig)",
    )
    .bind(&ids)
    .bind(&from)
    .bind(&to)
    .bind(&texts)
    .bind(ts_config)
    .bind(reset)
    .execute(&mut *conn)
    .await?;
    Ok(())
}

async fn insert_chunks(
    conn: &mut PgConnection,
    content_id: i64,
    ts_config: &str,
    chunks: &[&Chunk],
) -> Result<()> {
    if chunks.is_empty() {
        return Ok(());
    }
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
    let mut copy = conn
        .copy_in_raw(
            "copy chunks (content_id, ord, page_from, page_to, text, ts_config) from stdin",
        )
        .await?;
    copy.send(data.into_bytes()).await?;
    copy.finish().await?;
    Ok(())
}

pub async fn check_text_search(pool: &PgPool, name: &str) -> Result<bool> {
    let exists = sqlx::query_scalar("select exists(select 1 from pg_ts_config where cfgname = $1)")
        .bind(name)
        .fetch_one(pool)
        .await?;
    Ok(exists)
}
