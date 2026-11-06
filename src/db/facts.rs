use std::collections::{HashMap, HashSet};

use anyhow::Result;
use sqlx::{FromRow, PgConnection, PgPool};

use super::entities::{NewEntity, Owner, copy, mention_row, upsert_entities};
use super::{MODEL_METHODS, OCR_CONFIDENCE, copy_row};
use crate::facts::check::{Checked, Endpoint};

const SEGMENT_METHOD: &str = super::entities::SEGMENT_METHOD;

#[derive(Debug, Clone, FromRow)]
pub struct Eligible {
    pub id: i64,
    pub llm_key: Option<String>,
    pub source: String,
    pub document_id: i64,
    pub doc_type: Option<String>,
    pub path: String,
    pub text: String,
    pub ocr_confidence: Option<f64>,
    pub matched: bool,
}

#[derive(Debug, Clone, FromRow)]
pub struct ChunkRow {
    pub id: i64,
    pub text: String,
    pub page_from: Option<i32>,
    pub page_to: Option<i32>,
    pub path: String,
    pub dataset: String,
    pub ocr: bool,
    pub ocr_confidence: Option<f64>,
}

impl ChunkRow {
    pub fn ocr_scale(&self) -> f64 {
        if self.ocr {
            self.ocr_confidence.unwrap_or(1.0)
        } else {
            1.0
        }
    }

    pub fn pages(&self) -> String {
        match (self.page_from, self.page_to) {
            (Some(from), Some(to)) if from != to => format!("{from}-{to}"),
            (Some(from), _) => from.to_string(),
            _ => "-".to_string(),
        }
    }
}

#[derive(Debug, Clone, FromRow)]
pub struct KnownRow {
    pub chunk_id: i64,
    pub entity_id: i64,
    pub class_iri: String,
    pub name: String,
}

#[derive(Debug, Clone, FromRow)]
pub struct DocEntity {
    pub chunk_id: i64,
    pub entity_id: i64,
    pub class_iri: String,
}

pub struct ChunkFacts {
    pub chunk_id: i64,
    pub key: String,
    pub dataset: String,
    pub scale: f64,
    pub known: Vec<i64>,
    pub checked: Checked,
}

pub struct Confidence {
    pub base: f64,
    pub measured: HashMap<String, f64>,
}

impl Confidence {
    pub fn of(&self, iri: &str) -> f64 {
        self.measured.get(iri).copied().unwrap_or(self.base)
    }
}

pub async fn eligible(
    pool: &PgPool,
    sources: &[String],
    doc_types: &[String],
    min_chars: i32,
    after: i64,
    limit: i64,
) -> Result<Vec<Eligible>> {
    let rows = sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "select k.id, k.llm_key, d.source, d.id as document_id, d.doc_type, d.path, k.text, \
         {OCR_CONFIDENCE} as ocr_confidence, \
         exists (select 1 from class_mentions m where m.chunk_id = k.id) as matched \
         from chunks k \
         join lateral (select d.id, d.doc_type, d.path, s.name as source from documents d \
         join sources s on s.id = d.source_id \
         where d.content_id = k.content_id and d.status = 'present' \
         and (cardinality($1::text[]) = 0 or s.name = any($1)) \
         and (cardinality($2::text[]) = 0 or d.doc_type = any($2)) \
         order by d.id limit 1) d on true \
         where length(k.text) >= $3 and k.id > $4 order by k.id limit $5"
    )))
    .bind(sources)
    .bind(doc_types)
    .bind(min_chars)
    .bind(after)
    .bind(limit)
    .fetch_all(pool)
    .await?;
    Ok(rows)
}

pub async fn chunks(pool: &PgPool, ids: &[i64]) -> Result<Vec<ChunkRow>> {
    let rows = sqlx::query_as(
        "select k.id, k.text, k.page_from, k.page_to, d.path, d.dataset, o.ocr, o.ocr_confidence \
         from chunks k \
         join lateral (select d.path, s.dataset from documents d \
         join sources s on s.id = d.source_id \
         where d.content_id = k.content_id and d.status = 'present' order by d.id limit 1) d \
         on true \
         cross join lateral (select count(*) > 0 as ocr, min(p.ocr_confidence) as ocr_confidence \
         from pages p where p.content_id = k.content_id and p.method = 'ocr' \
         and p.page_no between coalesce(k.page_from, 0) and coalesce(k.page_to, 2147483647)) o \
         where k.id = any($1) order by k.id",
    )
    .bind(ids)
    .fetch_all(pool)
    .await?;
    Ok(rows)
}

pub async fn known(pool: &PgPool, ids: &[i64]) -> Result<Vec<KnownRow>> {
    let rows = sqlx::query_as(
        "select distinct m.chunk_id, e.id as entity_id, e.class_iri, e.name from mentions m \
         join entities e on e.id = m.entity_id where m.chunk_id = any($1) \
         and m.method not like $2 and m.method <> $3 order by m.chunk_id, e.id",
    )
    .bind(ids)
    .bind(MODEL_METHODS)
    .bind(SEGMENT_METHOD)
    .fetch_all(pool)
    .await?;
    Ok(rows)
}

const DOC_ENTITIES: &str = "with segment as (select k.id as chunk_id, e.id as entity_id, \
    e.class_iri from chunks k join contents c on c.id = k.content_id \
    join segments s on s.content_id = k.content_id \
    and coalesce(k.page_from, 1) between s.page_from and s.page_to \
    join entities e on e.key = c.sha256 || '#' || s.ord \
    where k.id = any($1)) \
    select chunk_id, entity_id, class_iri from segment \
    union all \
    select k.id, e.id, e.class_iri from chunks k \
    join documents d on d.content_id = k.content_id and d.status = 'present' \
    join entities e on e.document_id = d.id and e.key = d.id::text \
    where k.id = any($1) and not exists (select 1 from segment g where g.chunk_id = k.id) \
    order by 1, 2";

pub async fn doc_entities(conn: &mut PgConnection, ids: &[i64]) -> Result<Vec<DocEntity>> {
    let rows = sqlx::query_as(DOC_ENTITIES)
        .bind(ids)
        .fetch_all(&mut *conn)
        .await?;
    Ok(rows)
}

pub async fn doc_classes(pool: &PgPool, ids: &[i64]) -> Result<HashMap<i64, String>> {
    let mut conn = pool.acquire().await?;
    let mut classes = HashMap::new();
    for row in doc_entities(&mut conn, ids).await? {
        classes.entry(row.chunk_id).or_insert(row.class_iri);
    }
    Ok(classes)
}

const COPY_RELATION_TERMS: &str = "copy relation_terms (chunk_id, src_class, label, lang, \
    dst_class, span_start, span_end, method) from stdin";

#[derive(Default)]
struct RelationRows {
    seen: HashSet<(i64, String, i64, i64)>,
    sources: Vec<i64>,
    properties: Vec<String>,
    targets: Vec<i64>,
    chunks: Vec<i64>,
    starts: Vec<i32>,
    ends: Vec<i32>,
    confidences: Vec<f64>,
}

impl RelationRows {
    fn push(&mut self, key: (i64, &str, i64, i64), span: (i32, i32), confidence: f64) {
        let (from, property, to, chunk) = key;
        if !self.seen.insert((from, property.to_string(), to, chunk)) {
            return;
        }
        self.sources.push(from);
        self.properties.push(property.to_string());
        self.targets.push(to);
        self.chunks.push(chunk);
        self.starts.push(span.0);
        self.ends.push(span.1);
        self.confidences.push(confidence);
    }
}

pub async fn write(
    pool: &PgPool,
    facts: &[ChunkFacts],
    method: &str,
    confidence: &Confidence,
    version_id: i64,
) -> Result<()> {
    if facts.is_empty() {
        return Ok(());
    }
    let ids: Vec<i64> = facts.iter().map(|f| f.chunk_id).collect();
    let mut tx = pool.begin().await?;
    sqlx::query("delete from mentions where chunk_id = any($1) and method like $2")
        .bind(&ids)
        .bind(MODEL_METHODS)
        .execute(&mut *tx)
        .await?;
    sqlx::query(
        "delete from relations where chunk_id = any($1) and method like $2 \
         and status not in ('confirmed', 'rejected')",
    )
    .bind(&ids)
    .bind(MODEL_METHODS)
    .execute(&mut *tx)
    .await?;
    sqlx::query("delete from relation_terms where chunk_id = any($1)")
        .bind(&ids)
        .execute(&mut *tx)
        .await?;
    let mut entities = Vec::new();
    for chunk in facts {
        for entity in &chunk.checked.entities {
            entities.push(NewEntity {
                class: entity.class.clone(),
                key: entity.key.clone(),
                name: entity.name.clone(),
                dataset: chunk.dataset.clone(),
                method: method.to_string(),
                confidence: confidence.of(&entity.class) * chunk.scale,
                document_id: None,
            });
        }
    }
    let map = upsert_entities(&mut tx, entities, version_id).await?;
    let mut docs: HashMap<i64, Vec<i64>> = HashMap::new();
    for row in doc_entities(&mut tx, &ids).await? {
        docs.entry(row.chunk_id).or_default().push(row.entity_id);
    }
    let mut mentions = String::new();
    let mut terms = String::new();
    let mut rows = RelationRows::default();
    for chunk in facts {
        let owner = Owner::Chunk(chunk.chunk_id);
        let entity_ids: Vec<Option<i64>> = chunk
            .checked
            .entities
            .iter()
            .map(|e| map.get(&(e.class.clone(), e.key.clone())).copied())
            .collect();
        for (entity, id) in chunk.checked.entities.iter().zip(&entity_ids) {
            if let Some(id) = id {
                mention_row(
                    &mut mentions,
                    *id,
                    &owner,
                    entity.start,
                    entity.end,
                    method,
                    confidence.of(&entity.class) * chunk.scale,
                );
            }
        }
        let resolve = |end: Endpoint| -> Vec<i64> {
            match end {
                Endpoint::Doc => docs.get(&chunk.chunk_id).cloned().unwrap_or_default(),
                Endpoint::Known(i) => chunk.known.get(i).copied().into_iter().collect(),
                Endpoint::Entity(i) => entity_ids.get(i).copied().flatten().into_iter().collect(),
            }
        };
        for relation in &chunk.checked.relations {
            for from in resolve(relation.from) {
                for to in resolve(relation.to) {
                    rows.push(
                        (from, &relation.property, to, chunk.chunk_id),
                        (relation.start as i32, relation.end as i32),
                        confidence.of(&relation.property) * chunk.scale,
                    );
                }
            }
        }
        for other in &chunk.checked.others {
            let id = chunk.chunk_id.to_string();
            let start = other.start.to_string();
            let end = other.end.to_string();
            copy_row(
                &mut terms,
                &[
                    Some(&id),
                    Some(&other.from_class),
                    Some(&other.label),
                    Some(&other.lang),
                    Some(&other.to_class),
                    Some(&start),
                    Some(&end),
                    Some(method),
                ],
            );
        }
    }
    copy(
        &mut tx,
        "copy mentions (entity_id, document_id, chunk_id, span_start, span_end, method, \
         confidence) from stdin",
        mentions,
    )
    .await?;
    copy(&mut tx, COPY_RELATION_TERMS, terms).await?;
    if !rows.sources.is_empty() {
        sqlx::query(
            "insert into relations (src_id, property_iri, dst_id, method, confidence, chunk_id, \
             span_start, span_end) \
             select u.src, u.property, u.dst, $8, u.confidence, u.chunk, u.span_start, u.span_end \
             from unnest($1::bigint[], $2::text[], $3::bigint[], $4::bigint[], $5::int[], \
             $6::int[], $7::float8[]) \
             as u(src, property, dst, chunk, span_start, span_end, confidence) \
             where not exists (select 1 from relations r where r.src_id = u.src \
             and r.property_iri = u.property and r.dst_id = u.dst \
             and r.status in ('confirmed', 'rejected'))",
        )
        .bind(&rows.sources)
        .bind(&rows.properties)
        .bind(&rows.targets)
        .bind(&rows.chunks)
        .bind(&rows.starts)
        .bind(&rows.ends)
        .bind(&rows.confidences)
        .bind(method)
        .execute(&mut *tx)
        .await?;
    }
    let keys: Vec<&str> = facts.iter().map(|f| f.key.as_str()).collect();
    sqlx::query(
        "update chunks k set llm_key = u.key from unnest($1::bigint[], $2::text[]) as u(id, key) \
         where k.id = u.id",
    )
    .bind(&ids)
    .bind(&keys)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(())
}

pub async fn delete_orphans(pool: &PgPool) -> Result<u64> {
    let sql = format!(
        "delete from entities e where e.method like $1 and {}",
        super::entities::ORPHAN
    );
    let count = sqlx::query(sqlx::AssertSqlSafe(sql))
        .bind(MODEL_METHODS)
        .execute(pool)
        .await?
        .rows_affected();
    Ok(count)
}

#[derive(Debug, Clone, FromRow)]
pub struct TermRow {
    pub label: String,
    pub lang: String,
    pub src_class: String,
    pub dst_class: String,
    pub chunks: i64,
    pub documents: i64,
    pub examples: Vec<String>,
}

pub async fn term_rows(pool: &PgPool, examples: i32) -> Result<Vec<TermRow>> {
    let rows = sqlx::query_as(
        "select lower(t.label) as label, t.lang, t.src_class, t.dst_class, \
         count(distinct t.chunk_id) as chunks, count(distinct k.content_id) as documents, \
         (array_agg(distinct d.path || ', pages ' || coalesce(k.page_from::text, '-')))[1:$1] \
         as examples \
         from relation_terms t join chunks k on k.id = t.chunk_id \
         join lateral (select path from documents d where d.content_id = k.content_id \
         and d.status = 'present' order by d.id limit 1) d on true \
         group by 1, 2, 3, 4 order by 5 desc, 1, 3, 4",
    )
    .bind(examples)
    .fetch_all(pool)
    .await?;
    Ok(rows)
}

pub async fn class_counts(pool: &PgPool) -> Result<Vec<(String, i64, i64)>> {
    let rows = sqlx::query_as(
        "select e.class_iri, count(*), count(distinct m.chunk_id) from mentions m \
         join entities e on e.id = m.entity_id where m.method like $1 \
         group by 1 order by 2 desc, 1",
    )
    .bind(MODEL_METHODS)
    .fetch_all(pool)
    .await?;
    Ok(rows)
}

pub async fn top_names(pool: &PgPool, per_class: i64) -> Result<Vec<(String, String, i64)>> {
    let rows = sqlx::query_as(
        "select class_iri, name, n from (select e.class_iri, e.name, count(*) as n, \
         row_number() over (partition by e.class_iri order by count(*) desc, e.name) as r \
         from mentions m join entities e on e.id = m.entity_id where m.method like $1 \
         group by 1, 2) x where r <= $2 order by 1, 3 desc, 2",
    )
    .bind(MODEL_METHODS)
    .bind(per_class)
    .fetch_all(pool)
    .await?;
    Ok(rows)
}

pub async fn property_counts(pool: &PgPool) -> Result<Vec<(String, i64, i64)>> {
    let rows = sqlx::query_as(
        "select property_iri, count(*), count(distinct chunk_id) from relations \
         where method like $1 group by 1 order by 2 desc, 1",
    )
    .bind(MODEL_METHODS)
    .fetch_all(pool)
    .await?;
    Ok(rows)
}

pub async fn extracted_chunks(pool: &PgPool) -> Result<i64> {
    let count = sqlx::query_scalar("select count(*) from chunks where llm_key is not null")
        .fetch_one(pool)
        .await?;
    Ok(count)
}

pub async fn eligible_count(
    pool: &PgPool,
    sources: &[String],
    doc_types: &[String],
    min_chars: i32,
) -> Result<i64> {
    let count = sqlx::query_scalar(
        "select count(*) from chunks k where length(k.text) >= $3 and exists (select 1 \
         from documents d join sources s on s.id = d.source_id \
         where d.content_id = k.content_id and d.status = 'present' \
         and (cardinality($1::text[]) = 0 or s.name = any($1)) \
         and (cardinality($2::text[]) = 0 or d.doc_type = any($2)))",
    )
    .bind(sources)
    .bind(doc_types)
    .bind(min_chars)
    .fetch_one(pool)
    .await?;
    Ok(count)
}

pub async fn method_counts(pool: &PgPool) -> Result<Vec<(String, i64, i64)>> {
    let rows = sqlx::query_as(
        "select method, sum(mentions)::bigint, sum(relations)::bigint from ( \
         select method, count(*) as mentions, 0 as relations from mentions \
         where method like $1 group by 1 \
         union all select method, 0, count(*) from relations where method like $1 group by 1) x \
         group by 1 order by 1",
    )
    .bind(MODEL_METHODS)
    .fetch_all(pool)
    .await?;
    Ok(rows)
}

pub async fn name_candidates(
    pool: &PgPool,
    min_chunks: i64,
) -> Result<Vec<(String, String, String, i64)>> {
    let rows = sqlx::query_as(
        "select e.class_iri, e.key, e.name, count(distinct m.chunk_id) as chunks \
         from entities e join mentions m on m.entity_id = e.id \
         where e.method like $1 and m.method like $1 \
         group by e.id having count(distinct m.chunk_id) >= $2 order by 4 desc, 1, 2",
    )
    .bind(MODEL_METHODS)
    .bind(min_chunks)
    .fetch_all(pool)
    .await?;
    Ok(rows)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_relation_is_written_once_per_chunk() {
        let mut rows = RelationRows::default();
        rows.push((1, "p", 2, 10), (0, 5), 0.9);
        rows.push((1, "p", 2, 10), (7, 9), 0.8);
        rows.push((1, "p", 2, 11), (0, 5), 0.9);
        rows.push((1, "q", 2, 10), (0, 5), 0.9);
        assert_eq!(rows.sources.len(), 3);
        assert_eq!(rows.chunks, [10, 11, 10]);
        assert_eq!(rows.starts, [0, 0, 0]);
    }
}
