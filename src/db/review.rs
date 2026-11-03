use anyhow::Result;
use sqlx::{FromRow, PgPool};

#[derive(Debug, Clone, FromRow)]
pub struct Fact {
    pub kind: String,
    pub id: i64,
    pub class_iri: String,
    pub property_iri: Option<String>,
    pub method: String,
    pub confidence: f64,
    pub subject: String,
    pub object: Option<String>,
    pub text: String,
    pub span_start: Option<i32>,
    pub span_end: Option<i32>,
    pub page_from: Option<i32>,
    pub path: String,
}

pub async fn sample(
    pool: &PgPool,
    method_prefix: &str,
    iri: &str,
    seed: &str,
    limit: i64,
) -> Result<Vec<Fact>> {
    let rows = sqlx::query_as(
        "with facts as (select 'mention' as kind, m.id, e.class_iri, null::text as property_iri, \
         m.method, m.confidence, e.name as subject, null::text as object, \
         coalesce(k.text, d0.path) as text, m.span_start, m.span_end, \
         coalesce(k.content_id, d0.content_id) as content_id, k.page_from \
         from mentions m join entities e on e.id = m.entity_id \
         left join chunks k on k.id = m.chunk_id left join documents d0 on d0.id = m.document_id \
         union all \
         select 'relation', r.id, s.class_iri, r.property_iri, r.method, r.confidence, s.name, \
         t.name, coalesce(k.text, d0.path), r.span_start, r.span_end, \
         coalesce(k.content_id, d0.content_id), k.page_from \
         from relations r join entities s on s.id = r.src_id join entities t on t.id = r.dst_id \
         left join chunks k on k.id = r.chunk_id left join documents d0 on d0.id = r.document_id) \
         select f.kind, f.id, f.class_iri, f.property_iri, f.method, f.confidence, f.subject, \
         f.object, f.text, f.span_start, f.span_end, f.page_from, d.path from facts f \
         join lateral (select path from documents d where d.content_id = f.content_id \
         and d.status = 'present' order by d.id limit 1) d on true \
         where f.method like $1 || '%' and ($2 = '' or f.class_iri = $2 or f.property_iri = $2) \
         and not exists (select 1 from fact_reviews v where v.kind = f.kind and v.fact_id = f.id) \
         order by md5($3 || f.kind || f.id::text) limit $4",
    )
    .bind(method_prefix)
    .bind(iri)
    .bind(seed)
    .bind(limit)
    .fetch_all(pool)
    .await?;
    Ok(rows)
}

pub async fn record(pool: &PgPool, fact: &Fact, verdict: &str, snapshot: &str) -> Result<()> {
    sqlx::query(
        "insert into fact_reviews (kind, fact_id, class_iri, property_iri, method, verdict, \
         snapshot) values ($1, $2, $3, $4, $5, $6, $7)",
    )
    .bind(&fact.kind)
    .bind(fact.id)
    .bind(&fact.class_iri)
    .bind(&fact.property_iri)
    .bind(&fact.method)
    .bind(verdict)
    .bind(snapshot)
    .execute(pool)
    .await?;
    Ok(())
}

#[derive(Debug, Clone, FromRow)]
pub struct Precision {
    pub iri: String,
    pub method: String,
    pub right: i64,
    pub total: i64,
}

pub async fn precision(pool: &PgPool) -> Result<Vec<Precision>> {
    let rows = sqlx::query_as(
        "select coalesce(property_iri, class_iri) as iri, method, \
         count(*) filter (where verdict = 'right') as right, count(*) as total \
         from fact_reviews group by 1, 2 order by 2, 1",
    )
    .fetch_all(pool)
    .await?;
    Ok(rows)
}

#[derive(Debug, Clone, FromRow)]
pub struct Merge {
    pub id: i64,
    pub method: String,
    pub confidence: f64,
    pub a_id: i64,
    pub a_name: String,
    pub a_class: String,
    pub b_id: i64,
    pub b_name: String,
    pub b_class: String,
}

pub async fn proposed_merges(
    pool: &PgPool,
    class_iri: &str,
    seed: &str,
    n: i64,
) -> Result<Vec<Merge>> {
    let rows = sqlx::query_as(
        "select r.id, r.method, r.confidence, a.id as a_id, a.name as a_name, \
         a.class_iri as a_class, b.id as b_id, b.name as b_name, b.class_iri as b_class \
         from relations r join entities a on a.id = r.src_id join entities b on b.id = r.dst_id \
         where r.property_iri = $1 and r.status = 'proposed' \
         and ($2 = '' or a.class_iri = $2 or b.class_iri = $2) \
         order by md5($3 || r.id::text), r.id limit $4",
    )
    .bind(crate::db::resolve::SAME_AS)
    .bind(class_iri)
    .bind(seed)
    .bind(n)
    .fetch_all(pool)
    .await?;
    Ok(rows)
}

#[derive(Debug, Clone, FromRow)]
pub struct Example {
    pub path: String,
    pub page_from: Option<i32>,
    pub text: String,
    pub span_start: i32,
    pub span_end: i32,
}

pub async fn entity_card(
    pool: &PgPool,
    id: i64,
    examples: i64,
) -> Result<(Vec<String>, i64, Vec<Example>)> {
    let aliases =
        sqlx::query_scalar("select alias from aliases where entity_id = $1 order by alias")
            .bind(id)
            .fetch_all(pool)
            .await?;
    let mentions = sqlx::query_scalar("select count(*) from mentions where entity_id = $1")
        .bind(id)
        .fetch_one(pool)
        .await?;
    let rows = sqlx::query_as(
        "select d.path, k.page_from, k.text, m.span_start, m.span_end from mentions m \
         join chunks k on k.id = m.chunk_id \
         join lateral (select d.path from documents d where d.content_id = k.content_id \
         and d.status = 'present' order by d.id limit 1) d on true \
         where m.entity_id = $1 order by m.id limit $2",
    )
    .bind(id)
    .bind(examples)
    .fetch_all(pool)
    .await?;
    Ok((aliases, mentions, rows))
}

pub async fn set_merge_status(pool: &PgPool, id: i64, status: &str) -> Result<()> {
    sqlx::query("update relations set status = $2 where id = $1 and status = 'proposed'")
        .bind(id)
        .bind(status)
        .execute(pool)
        .await?;
    Ok(())
}
