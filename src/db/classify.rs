use anyhow::Result;
use sqlx::{FromRow, PgPool};

#[derive(Debug, Clone, FromRow)]
pub struct Unit {
    pub content_id: i64,
    pub page_from: i32,
    pub page_to: i32,
    pub path: String,
    pub cache_key: Option<String>,
}

pub async fn units(pool: &PgPool) -> Result<Vec<Unit>> {
    let rows = sqlx::query_as(
        "with units as (select s.content_id, s.page_from, s.page_to, s.doc_type, \
         count(*) over (partition by s.content_id) as n from segments s \
         union all \
         select p.content_id, min(p.page_no), max(p.page_no), null, 1 from pages p \
         where not exists (select 1 from segments s where s.content_id = p.content_id) \
         group by p.content_id) \
         select u.content_id, u.page_from, u.page_to, d.path, t.cache_key from units u \
         join lateral (select d.path from documents d where d.content_id = u.content_id \
         and d.status = 'present' order by d.id limit 1) d on true \
         left join llm_types t on t.content_id = u.content_id and t.page_from = u.page_from \
         where u.doc_type is null and (u.n > 1 or exists (select 1 from documents x \
         where x.content_id = u.content_id and x.status = 'present' \
         and (x.doc_type is null or x.type_method like 'llm:%'))) \
         order by u.content_id, u.page_from",
    )
    .fetch_all(pool)
    .await?;
    Ok(rows)
}

pub async fn heads(
    pool: &PgPool,
    units: &[(i64, i32, i32)],
    head_chars: i32,
) -> Result<Vec<(i64, i32, String)>> {
    let contents: Vec<i64> = units.iter().map(|u| u.0).collect();
    let froms: Vec<i32> = units.iter().map(|u| u.1).collect();
    let tos: Vec<i32> = units.iter().map(|u| u.2).collect();
    let rows = sqlx::query_as(
        "select u.content_id, u.page_from, string_agg(p.text, E'\\n' order by p.page_no) \
         from unnest($1::bigint[], $2::int[], $3::int[]) as u(content_id, page_from, page_to) \
         join lateral (select x.text, x.page_no from (select text, page_no, \
         coalesce(sum(length(text)) over (order by page_no rows between unbounded preceding \
         and 1 preceding), 0) as before from pages where content_id = u.content_id \
         and page_no between u.page_from and u.page_to) x where x.before < $4) p on true \
         group by 1, 2",
    )
    .bind(&contents)
    .bind(&froms)
    .bind(&tos)
    .bind(head_chars)
    .fetch_all(pool)
    .await?;
    Ok(rows)
}

pub struct TypeRow<'a> {
    pub content_id: i64,
    pub page_from: i32,
    pub page_to: i32,
    pub doc_type: Option<&'a str>,
    pub key: &'a str,
}

pub async fn write(
    pool: &PgPool,
    rows: &[TypeRow<'_>],
    method: &str,
    confidence: f64,
) -> Result<()> {
    if rows.is_empty() {
        return Ok(());
    }
    let contents: Vec<i64> = rows.iter().map(|r| r.content_id).collect();
    let froms: Vec<i32> = rows.iter().map(|r| r.page_from).collect();
    let tos: Vec<i32> = rows.iter().map(|r| r.page_to).collect();
    let types: Vec<Option<&str>> = rows.iter().map(|r| r.doc_type).collect();
    let keys: Vec<&str> = rows.iter().map(|r| r.key).collect();
    let mut tx = pool.begin().await?;
    sqlx::query(
        "insert into llm_types (content_id, page_from, page_to, doc_type, method, confidence, \
         cache_key) select u.content_id, u.page_from, u.page_to, u.doc_type, $6, $7, u.key \
         from unnest($1::bigint[], $2::int[], $3::int[], $4::text[], $5::text[]) \
         as u(content_id, page_from, page_to, doc_type, key) \
         on conflict (content_id, page_from) do update set page_to = excluded.page_to, \
         doc_type = excluded.doc_type, method = excluded.method, \
         confidence = excluded.confidence, cache_key = excluded.cache_key",
    )
    .bind(&contents)
    .bind(&froms)
    .bind(&tos)
    .bind(&types)
    .bind(&keys)
    .bind(method)
    .bind(confidence)
    .execute(&mut *tx)
    .await?;
    sqlx::query("update documents set rules_key = null where content_id = any($1)")
        .bind(&contents)
        .execute(&mut *tx)
        .await?;
    sqlx::query("update contents set rules_key = null where id = any($1)")
        .bind(&contents)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(())
}

pub async fn replace_items(
    pool: &PgPool,
    job: i64,
    units: &[(i64, i32)],
    keys: &[String],
) -> Result<Vec<i32>> {
    let contents: Vec<i64> = units.iter().map(|u| u.0).collect();
    let froms: Vec<i32> = units.iter().map(|u| u.1).collect();
    let mut tx = pool.begin().await?;
    sqlx::query("delete from llm_job_items where job_id = $1 and status in ('pending', 'error')")
        .bind(job)
        .execute(&mut *tx)
        .await?;
    let ords = sqlx::query_scalar(
        "insert into llm_job_items (job_id, ord, content_id, page_from, cache_key) \
         select $1, (select coalesce(max(ord), 0) from llm_job_items where job_id = $1) \
         + t.n::integer, t.content_id, t.page_from, t.key \
         from unnest($2::bigint[], $3::int[], $4::text[]) with ordinality \
         as t(content_id, page_from, key, n) order by t.n returning ord",
    )
    .bind(job)
    .bind(&contents)
    .bind(&froms)
    .bind(keys)
    .fetch_all(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(ords)
}

pub async fn counts(pool: &PgPool) -> Result<Vec<(Option<String>, i64)>> {
    let rows =
        sqlx::query_as("select doc_type, count(*) from llm_types group by 1 order by 2 desc, 1")
            .fetch_all(pool)
            .await?;
    Ok(rows)
}
