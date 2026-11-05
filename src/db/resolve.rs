use anyhow::Result;
use sqlx::{FromRow, PgPool};

pub const SAME_AS: &str = "http://www.w3.org/2002/07/owl#sameAs";
pub const REFERENCES: &str = "http://purl.org/dc/terms/references";
pub const REPLACES: &str = "http://purl.org/dc/terms/replaces";

#[derive(Debug, Clone, FromRow)]
pub struct EntityRow {
    pub id: i64,
    pub class_iri: String,
    pub key: String,
    pub name: String,
    pub document_id: Option<i64>,
    pub canonical_id: Option<i64>,
}

#[derive(Debug, Clone, FromRow, PartialEq)]
pub struct Link {
    pub id: i64,
    pub src_id: i64,
    pub dst_id: i64,
    pub method: String,
    pub status: String,
    pub document_id: Option<i64>,
    pub chunk_id: Option<i64>,
    pub span_start: Option<i32>,
    pub span_end: Option<i32>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct NewLink {
    pub src_id: i64,
    pub dst_id: i64,
    pub property: &'static str,
    pub method: String,
    pub status: &'static str,
    pub confidence: f64,
    pub props: serde_json::Value,
    pub document_id: Option<i64>,
    pub chunk_id: Option<i64>,
    pub span_start: Option<i32>,
    pub span_end: Option<i32>,
}

pub async fn entities(pool: &PgPool) -> Result<Vec<EntityRow>> {
    let rows = sqlx::query_as(
        "select id, class_iri, key, name, document_id, canonical_id from entities order by id",
    )
    .fetch_all(pool)
    .await?;
    Ok(rows)
}

pub async fn links(pool: &PgPool, property: &str) -> Result<Vec<Link>> {
    let rows = sqlx::query_as(
        "select id, src_id, dst_id, method, status, document_id, chunk_id, span_start, span_end \
         from relations where property_iri = $1 order by id",
    )
    .bind(property)
    .fetch_all(pool)
    .await?;
    Ok(rows)
}

pub async fn delete(pool: &PgPool, ids: &[i64]) -> Result<()> {
    if ids.is_empty() {
        return Ok(());
    }
    sqlx::query("delete from relations where id = any($1) and status in ('auto', 'proposed')")
        .bind(ids)
        .execute(pool)
        .await?;
    Ok(())
}

pub async fn insert(pool: &PgPool, links: &[NewLink]) -> Result<()> {
    if links.is_empty() {
        return Ok(());
    }
    let column = |f: fn(&NewLink) -> Option<i64>| links.iter().map(f).collect::<Vec<_>>();
    let span = |f: fn(&NewLink) -> Option<i32>| links.iter().map(f).collect::<Vec<_>>();
    sqlx::query(
        "insert into relations (src_id, property_iri, dst_id, props, method, status, confidence, \
         document_id, chunk_id, span_start, span_end) \
         select * from unnest($1::bigint[], $2::text[], $3::bigint[], $4::jsonb[], $5::text[], \
         $6::text[], $7::float8[], $8::bigint[], $9::bigint[], $10::int[], $11::int[])",
    )
    .bind(column(|l| Some(l.src_id)))
    .bind(links.iter().map(|l| l.property).collect::<Vec<_>>())
    .bind(column(|l| Some(l.dst_id)))
    .bind(links.iter().map(|l| l.props.clone()).collect::<Vec<_>>())
    .bind(links.iter().map(|l| l.method.clone()).collect::<Vec<_>>())
    .bind(links.iter().map(|l| l.status).collect::<Vec<_>>())
    .bind(links.iter().map(|l| l.confidence).collect::<Vec<_>>())
    .bind(column(|l| l.document_id))
    .bind(column(|l| l.chunk_id))
    .bind(span(|l| l.span_start))
    .bind(span(|l| l.span_end))
    .execute(pool)
    .await?;
    Ok(())
}

pub async fn current_model(pool: &PgPool) -> Result<Option<(i64, i32)>> {
    let row = sqlx::query_as("select id, dimensions from embedding_models where current")
        .fetch_optional(pool)
        .await?;
    Ok(row)
}

pub async fn entity_vectors(pool: &PgPool, model_id: i64) -> Result<i64> {
    let count = sqlx::query_scalar(
        "select count(*) from embeddings where model_id = $1 and owner_kind = 'entity'",
    )
    .bind(model_id)
    .fetch_one(pool)
    .await?;
    Ok(count)
}

pub async fn neighbours(
    pool: &PgPool,
    model_id: i64,
    dimensions: i32,
    ids: &[i64],
    k: i64,
) -> Result<Vec<(i64, i64, f64)>> {
    let cast = format!("halfvec({dimensions})");
    let sql = format!(
        "select a.id, n.owner_id, 1 - n.distance from entities a \
         join embeddings e on e.model_id = $1 and e.owner_kind = 'entity' and e.owner_id = a.id \
         cross join lateral (select x.owner_id, \
         (x.embedding::{cast} <=> e.embedding::{cast}) as distance from embeddings x \
         join entities b on b.id = x.owner_id \
         where x.model_id = $1 and x.owner_kind = 'entity' and b.class_iri = a.class_iri \
         and x.owner_id <> a.id order by x.embedding::{cast} <=> e.embedding::{cast} \
         limit $3) n where a.id = any($2)"
    );
    let mut tx = pool.begin().await?;
    sqlx::query("set local hnsw.iterative_scan = relaxed_order")
        .execute(&mut *tx)
        .await?;
    let rows = sqlx::query_as(sqlx::AssertSqlSafe(sql))
        .bind(model_id)
        .bind(ids)
        .bind(k)
        .fetch_all(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(rows)
}

pub async fn set_canonical(pool: &PgPool, ids: &[i64], canonical: &[i64]) -> Result<u64> {
    let done = sqlx::query(
        "update entities e set canonical_id = v.canonical \
         from unnest($1::bigint[], $2::bigint[]) as v(id, canonical) \
         where e.id = v.id and e.canonical_id is distinct from v.canonical",
    )
    .bind(ids)
    .bind(canonical)
    .execute(pool)
    .await?;
    Ok(done.rows_affected())
}

pub async fn resolve_aliases(pool: &PgPool, method: &str) -> Result<Vec<(i64, String)>> {
    let rows = sqlx::query_as(
        "select entity_id, alias from aliases where method = $1 order by entity_id, alias",
    )
    .bind(method)
    .fetch_all(pool)
    .await?;
    Ok(rows)
}

pub async fn add_aliases(pool: &PgPool, rows: &[(i64, String)], method: &str) -> Result<u64> {
    if rows.is_empty() {
        return Ok(0);
    }
    let done = sqlx::query(
        "insert into aliases (entity_id, alias, method) \
         select id, alias, $3 from unnest($1::bigint[], $2::text[]) as v(id, alias) \
         on conflict do nothing",
    )
    .bind(rows.iter().map(|r| r.0).collect::<Vec<_>>())
    .bind(rows.iter().map(|r| r.1.clone()).collect::<Vec<_>>())
    .bind(method)
    .execute(pool)
    .await?;
    Ok(done.rows_affected())
}

pub async fn remove_aliases(pool: &PgPool, rows: &[(i64, String)], method: &str) -> Result<()> {
    if rows.is_empty() {
        return Ok(());
    }
    sqlx::query(
        "delete from aliases a using unnest($1::bigint[], $2::text[]) as v(id, alias) \
         where a.entity_id = v.id and a.alias = v.alias and a.method = $3",
    )
    .bind(rows.iter().map(|r| r.0).collect::<Vec<_>>())
    .bind(rows.iter().map(|r| r.1.clone()).collect::<Vec<_>>())
    .bind(method)
    .execute(pool)
    .await?;
    Ok(())
}

#[derive(Debug, Clone, FromRow)]
pub struct Reference {
    pub src_id: i64,
    pub dst_id: i64,
    pub chunk_id: i64,
    pub span_start: i32,
    pub span_end: i32,
    pub confidence: f64,
}

pub async fn references(pool: &PgPool, classes: &[String]) -> Result<Vec<Reference>> {
    let rows = sqlx::query_as(
        "select distinct on (sa.id, sb.id) sa.id as src_id, sb.id as dst_id, m.chunk_id, \
         m.span_start, m.span_end, m.confidence from mentions m \
         join entities t on t.id = m.entity_id and t.document_id is null \
         and t.class_iri = any($1) \
         join chunks k on k.id = m.chunk_id \
         join documents da on da.content_id = k.content_id and da.status = 'present' \
         join entities sa on sa.document_id = da.id and sa.key = da.id::text \
         join mentions p on p.entity_id = t.id and p.document_id is not null \
         join documents db on db.id = p.document_id and db.status = 'present' \
         join entities sb on sb.document_id = db.id and sb.key = db.id::text \
         where m.chunk_id is not null and db.content_id <> da.content_id \
         order by sa.id, sb.id, m.chunk_id, m.span_start",
    )
    .bind(classes)
    .fetch_all(pool)
    .await?;
    Ok(rows)
}

#[derive(Debug, Clone, FromRow)]
pub struct DocumentEntity {
    pub document_id: i64,
    pub source_id: i64,
    pub path: String,
    pub entity_id: i64,
}

pub async fn document_entities(pool: &PgPool) -> Result<Vec<DocumentEntity>> {
    let rows = sqlx::query_as(
        "select d.id as document_id, d.source_id, d.path, e.id as entity_id from documents d \
         join entities e on e.document_id = d.id and e.key = d.id::text \
         where d.status = 'present' order by d.id",
    )
    .fetch_all(pool)
    .await?;
    Ok(rows)
}
