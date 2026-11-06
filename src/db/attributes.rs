use std::collections::HashMap;

use anyhow::Result;
use sqlx::{FromRow, PgPool};

use super::OCR_CONFIDENCE;
use crate::attributes::check::Checked;

const SEGMENT_METHOD: &str = super::entities::SEGMENT_METHOD;

#[derive(Debug, Clone, FromRow)]
pub struct Eligible {
    pub id: i64,
    pub attr_key: Option<String>,
    pub source: String,
    pub document_id: i64,
    pub doc_type: Option<String>,
    pub path: String,
    pub text: String,
    pub context: String,
    pub ocr_confidence: Option<f64>,
    pub matched: bool,
}

#[derive(Debug, Clone, FromRow)]
pub struct KnownRow {
    pub chunk_id: i64,
    pub entity_id: i64,
    pub class_iri: String,
    pub name: String,
}

pub struct Filter<'a> {
    pub sources: &'a [String],
    pub doc_types: &'a [String],
    pub min_chars: i32,
    pub extracted: bool,
    pub context_chars: i32,
}

pub async fn eligible(
    pool: &PgPool,
    filter: &Filter<'_>,
    after: i64,
    limit: i64,
) -> Result<Vec<Eligible>> {
    let rows = sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "select k.id, k.attr_key, d.source, d.id as document_id, d.doc_type, d.path, k.text, \
         coalesce((select right(p.text, $6) from chunks p where p.content_id = k.content_id \
         and p.ord = k.ord - 1), '') as context, \
         {OCR_CONFIDENCE} as ocr_confidence, \
         exists (select 1 from class_mentions m where m.chunk_id = k.id) as matched \
         from chunks k \
         join lateral (select d.id, d.doc_type, d.path, s.name as source from documents d \
         join sources s on s.id = d.source_id \
         where d.content_id = k.content_id and d.status = 'present' \
         and (cardinality($1::text[]) = 0 or s.name = any($1)) \
         and (cardinality($2::text[]) = 0 or d.doc_type = any($2)) \
         order by d.id limit 1) d on true \
         where length(k.text) >= $3 and k.id > $4 and (not $7 or k.llm_key is not null) \
         order by k.id limit $5"
    )))
    .bind(filter.sources)
    .bind(filter.doc_types)
    .bind(filter.min_chars)
    .bind(after)
    .bind(limit)
    .bind(filter.context_chars)
    .bind(filter.extracted)
    .fetch_all(pool)
    .await?;
    Ok(rows)
}

pub async fn known(pool: &PgPool, ids: &[i64]) -> Result<Vec<KnownRow>> {
    let rows = sqlx::query_as(
        "select distinct m.chunk_id, e.id as entity_id, e.class_iri, e.name from mentions m \
         join entities e on e.id = m.entity_id where m.chunk_id = any($1) \
         and m.method <> $2 order by m.chunk_id, e.id",
    )
    .bind(ids)
    .bind(SEGMENT_METHOD)
    .fetch_all(pool)
    .await?;
    Ok(rows)
}

pub async fn contexts(pool: &PgPool, ids: &[i64], chars: i32) -> Result<HashMap<i64, String>> {
    let rows: Vec<(i64, String)> = sqlx::query_as(
        "select k.id, coalesce((select right(p.text, $2) from chunks p \
         where p.content_id = k.content_id and p.ord = k.ord - 1), '') \
         from chunks k where k.id = any($1)",
    )
    .bind(ids)
    .bind(chars)
    .fetch_all(pool)
    .await?;
    Ok(rows.into_iter().collect())
}

pub struct ChunkValues {
    pub chunk_id: i64,
    pub key: String,
    pub scale: f64,
    pub checked: Checked,
}

pub async fn write(
    pool: &PgPool,
    chunks: &[ChunkValues],
    method: &str,
    confidence: &crate::db::facts::Confidence,
) -> Result<()> {
    if chunks.is_empty() {
        return Ok(());
    }
    let ids: Vec<i64> = chunks.iter().map(|c| c.chunk_id).collect();
    let keys: Vec<&str> = chunks.iter().map(|c| c.key.as_str()).collect();
    let mut entity = Vec::new();
    let mut property = Vec::new();
    let mut value_text = Vec::new();
    let mut unit_text = Vec::new();
    let mut unit_iri = Vec::new();
    let mut low = Vec::new();
    let mut high = Vec::new();
    let mut si_low = Vec::new();
    let mut si_high = Vec::new();
    let mut dimension = Vec::new();
    let mut chunk = Vec::new();
    let mut start = Vec::new();
    let mut end = Vec::new();
    let mut conf = Vec::new();
    for item in chunks {
        for value in &item.checked.values {
            entity.push(value.entity_id);
            property.push(value.property.clone());
            value_text.push(value.value_text.clone());
            unit_text.push(value.unit_text.clone());
            unit_iri.push(value.unit_iri.clone());
            low.push(value.low);
            high.push(value.high);
            si_low.push(value.si_low);
            si_high.push(value.si_high);
            dimension.push(value.dimension.clone());
            chunk.push(item.chunk_id);
            start.push(value.start as i32);
            end.push(value.end as i32);
            conf.push(confidence.of(&value.property) * item.scale);
        }
    }
    let mut tx = pool.begin().await?;
    sqlx::query("delete from attributes where chunk_id = any($1)")
        .bind(&ids)
        .execute(&mut *tx)
        .await?;
    if !entity.is_empty() {
        sqlx::query(
            "insert into attributes (entity_id, property_iri, value_text, unit_text, unit_iri, \
             low, high, si_low, si_high, dimension, chunk_id, span_start, span_end, method, \
             confidence) \
             select e, p, v, ut, ui, l, h, sl, sh, d, c, s, x, $14, cf from unnest($1::bigint[], \
             $2::text[], $3::text[], $4::text[], $5::text[], $6::float8[], $7::float8[], \
             $8::float8[], $9::float8[], $10::text[], $11::bigint[], $12::int[], $13::int[], \
             $15::float8[]) as t(e, p, v, ut, ui, l, h, sl, sh, d, c, s, x, cf) \
             where exists (select 1 from entities n where n.id = t.e)",
        )
        .bind(&entity)
        .bind(&property)
        .bind(&value_text)
        .bind(&unit_text)
        .bind(&unit_iri)
        .bind(&low)
        .bind(&high)
        .bind(&si_low)
        .bind(&si_high)
        .bind(&dimension)
        .bind(&chunk)
        .bind(&start)
        .bind(&end)
        .bind(method)
        .bind(&conf)
        .execute(&mut *tx)
        .await?;
    }
    sqlx::query(
        "update chunks k set attr_key = t.key from unnest($1::bigint[], $2::text[]) as t(id, key) \
         where k.id = t.id",
    )
    .bind(&ids)
    .bind(&keys)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(())
}

pub async fn clear(pool: &PgPool, ids: &[i64]) -> Result<()> {
    let mut tx = pool.begin().await?;
    sqlx::query("delete from attributes where chunk_id = any($1)")
        .bind(ids)
        .execute(&mut *tx)
        .await?;
    sqlx::query("update chunks set attr_key = null where id = any($1)")
        .bind(ids)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(())
}

#[derive(Debug, Clone, FromRow)]
pub struct PropertyRow {
    pub property_iri: String,
    pub unit: String,
    pub values: i64,
    pub chunks: i64,
    pub example: String,
}

pub async fn property_counts(pool: &PgPool) -> Result<Vec<PropertyRow>> {
    let rows = sqlx::query_as(
        "select a.property_iri, coalesce(a.unit_iri, '') as unit, count(*) as values, \
         count(distinct a.chunk_id) as chunks, min(d.path || ' p. ' || \
         coalesce(k.page_from::text, '-')) as example from attributes a \
         join chunks k on k.id = a.chunk_id \
         join lateral (select d.path from documents d where d.content_id = k.content_id \
         and d.status = 'present' order by d.id limit 1) d on true \
         group by 1, 2 order by 1, 3 desc",
    )
    .fetch_all(pool)
    .await?;
    Ok(rows)
}

pub async fn sent_chunks(pool: &PgPool) -> Result<i64> {
    let count = sqlx::query_scalar("select count(*) from chunks where attr_key is not null")
        .fetch_one(pool)
        .await?;
    Ok(count)
}

pub async fn counts(pool: &PgPool) -> Result<Vec<(String, i64)>> {
    let rows = sqlx::query_as(
        "select property_iri, count(*) from attributes group by 1 order by 2 desc, 1",
    )
    .fetch_all(pool)
    .await?;
    Ok(rows)
}
