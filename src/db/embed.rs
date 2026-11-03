use std::collections::HashMap;
use std::fmt::Write as _;

use anyhow::{Result, bail};
use sqlx::{FromRow, PgPool};

use super::{copy_row, quote_identifier};

pub const CHUNK: &str = "chunk";
pub const ENTITY: &str = "entity";

#[derive(Debug, Clone)]
pub struct Record<'a> {
    pub name: &'a str,
    pub fingerprint: &'a str,
    pub repo: &'a str,
    pub file: &'a str,
    pub data_files: &'a [String],
    pub tokenizer: &'a str,
    pub revision: &'a str,
    pub sha256: &'a str,
    pub dimensions: i32,
    pub pooling: &'a str,
    pub output: &'a str,
    pub passage_prefix: &'a str,
    pub query_prefix: &'a str,
    pub max_tokens: i32,
    pub normalize: bool,
}

#[derive(Debug, Clone, FromRow)]
pub struct Stored {
    pub id: i64,
    pub name: String,
    pub dimensions: i32,
    pub current: bool,
}

pub struct Vector<'a> {
    pub owner_id: i64,
    pub text_key: &'a str,
    pub truncated: bool,
    pub embedding: &'a str,
}

#[derive(Debug, Clone, FromRow)]
pub struct ChunkCandidate {
    pub id: i64,
    pub text: String,
    pub ocr_confidence: Option<f64>,
    pub text_key: Option<String>,
}

#[derive(Debug, Clone, FromRow)]
pub struct EntityCandidate {
    pub id: i64,
    pub class_iri: String,
    pub name: String,
    pub aliases: Vec<String>,
    pub text_key: Option<String>,
}

pub fn vector_text(vector: &[f32]) -> String {
    let mut out = String::with_capacity(vector.len() * 10 + 2);
    out.push('[');
    for (i, value) in vector.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        let _ = write!(out, "{value}");
    }
    out.push(']');
    out
}

pub fn parse_vector(text: &str) -> Result<Vec<f32>> {
    let inner = text.trim().trim_start_matches('[').trim_end_matches(']');
    let mut out = Vec::new();
    for part in inner.split(',').filter(|p| !p.is_empty()) {
        match part.trim().parse::<f32>() {
            Ok(value) => out.push(value),
            Err(_) => bail!("not a vector: {part:?}"),
        }
    }
    Ok(out)
}

pub async fn find_model(pool: &PgPool, fingerprint: &str) -> Result<Option<Stored>> {
    let row = sqlx::query_as(
        "select id, name, dimensions, current from embedding_models where fingerprint = $1",
    )
    .bind(fingerprint)
    .fetch_optional(pool)
    .await?;
    Ok(row)
}

pub async fn models(pool: &PgPool) -> Result<Vec<Stored>> {
    let rows =
        sqlx::query_as("select id, name, dimensions, current from embedding_models order by id")
            .fetch_all(pool)
            .await?;
    Ok(rows)
}

pub async fn insert_model(pool: &PgPool, record: &Record<'_>) -> Result<i64> {
    let id = sqlx::query_scalar(
        "insert into embedding_models (name, fingerprint, repo, file, data_files, tokenizer, \
         revision, sha256, dimensions, pooling, output, passage_prefix, query_prefix, max_tokens, \
         normalize) values ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15) \
         on conflict (fingerprint) do update set fingerprint = excluded.fingerprint returning id",
    )
    .bind(record.name)
    .bind(record.fingerprint)
    .bind(record.repo)
    .bind(record.file)
    .bind(record.data_files)
    .bind(record.tokenizer)
    .bind(record.revision)
    .bind(record.sha256)
    .bind(record.dimensions)
    .bind(record.pooling)
    .bind(record.output)
    .bind(record.passage_prefix)
    .bind(record.query_prefix)
    .bind(record.max_tokens)
    .bind(record.normalize)
    .fetch_one(pool)
    .await?;
    Ok(id)
}

pub async fn set_current(pool: &PgPool, id: i64) -> Result<()> {
    sqlx::query("update embedding_models set current = (id = $1) where current <> (id = $1)")
        .bind(id)
        .execute(pool)
        .await?;
    Ok(())
}

pub async fn chunk_candidates(
    pool: &PgPool,
    model_id: i64,
    min_chars: i32,
    after: i64,
    limit: i64,
) -> Result<Vec<ChunkCandidate>> {
    let rows = sqlx::query_as(
        "select k.id, k.text, \
         (select min(p.ocr_confidence) from pages p where p.content_id = k.content_id \
         and p.method = 'ocr' and p.page_no between coalesce(k.page_from, 0) \
         and coalesce(k.page_to, 2147483647)) as ocr_confidence, \
         e.text_key from chunks k \
         left join embeddings e on e.model_id = $1 and e.owner_kind = 'chunk' \
         and e.owner_id = k.id \
         where k.id > $3 and length(k.text) >= $2 \
         and exists (select 1 from documents d where d.content_id = k.content_id \
         and d.status = 'present') \
         order by k.id limit $4",
    )
    .bind(model_id)
    .bind(min_chars)
    .bind(after)
    .bind(limit)
    .fetch_all(pool)
    .await?;
    Ok(rows)
}

pub async fn entity_candidates(
    pool: &PgPool,
    model_id: i64,
    max_aliases: i64,
    after: i64,
    limit: i64,
) -> Result<Vec<EntityCandidate>> {
    let rows = sqlx::query_as(
        "select n.id, n.class_iri, n.name, \
         array(select a.alias from aliases a where a.entity_id = n.id and a.alias <> n.name \
         and a.method <> $5 order by a.alias limit $2) as aliases, e.text_key from entities n \
         left join embeddings e on e.model_id = $1 and e.owner_kind = 'entity' \
         and e.owner_id = n.id \
         where n.id > $3 order by n.id limit $4",
    )
    .bind(model_id)
    .bind(max_aliases)
    .bind(after)
    .bind(limit)
    .bind(crate::resolve::METHOD)
    .fetch_all(pool)
    .await?;
    Ok(rows)
}

pub async fn class_keys(pool: &PgPool, model_id: i64) -> Result<HashMap<String, String>> {
    let rows: Vec<(String, String)> =
        sqlx::query_as("select iri, text_key from class_embeddings where model_id = $1")
            .bind(model_id)
            .fetch_all(pool)
            .await?;
    Ok(rows.into_iter().collect())
}

pub async fn known_vectors(
    pool: &PgPool,
    model_id: i64,
    keys: &[String],
) -> Result<HashMap<String, (bool, String)>> {
    let rows: Vec<(String, bool, String)> = sqlx::query_as(
        "select distinct on (text_key) text_key, truncated, embedding::text from embeddings \
         where model_id = $1 and text_key = any($2)",
    )
    .bind(model_id)
    .bind(keys)
    .fetch_all(pool)
    .await?;
    Ok(rows
        .into_iter()
        .map(|(key, truncated, vector)| (key, (truncated, vector)))
        .collect())
}

pub async fn write(pool: &PgPool, model_id: i64, kind: &str, vectors: &[Vector<'_>]) -> Result<()> {
    if vectors.is_empty() {
        return Ok(());
    }
    let ids: Vec<i64> = vectors.iter().map(|v| v.owner_id).collect();
    let model = model_id.to_string();
    let mut tx = pool.begin().await?;
    sqlx::query(
        "delete from embeddings where model_id = $1 and owner_kind = $2 and owner_id = any($3)",
    )
    .bind(model_id)
    .bind(kind)
    .bind(&ids)
    .execute(&mut *tx)
    .await?;
    let mut data = String::new();
    for vector in vectors {
        let owner = vector.owner_id.to_string();
        let truncated = if vector.truncated { "t" } else { "f" };
        copy_row(
            &mut data,
            &[
                Some(&model),
                Some(kind),
                Some(&owner),
                Some(vector.text_key),
                Some(truncated),
                Some(vector.embedding),
            ],
        );
    }
    let mut copy = tx
        .copy_in_raw(
            "copy embeddings (model_id, owner_kind, owner_id, text_key, truncated, embedding) \
             from stdin",
        )
        .await?;
    copy.send(data.as_bytes()).await?;
    copy.finish().await?;
    tx.commit().await?;
    Ok(())
}

pub async fn write_classes(
    pool: &PgPool,
    model_id: i64,
    rows: &[(String, String, bool, String)],
) -> Result<()> {
    let mut tx = pool.begin().await?;
    for (iri, key, truncated, embedding) in rows {
        sqlx::query(
            "insert into class_embeddings (model_id, iri, text_key, truncated, embedding) \
             values ($1, $2, $3, $4, $5::halfvec) on conflict (model_id, iri) do update \
             set text_key = excluded.text_key, truncated = excluded.truncated, \
             embedding = excluded.embedding",
        )
        .bind(model_id)
        .bind(iri)
        .bind(key)
        .bind(truncated)
        .bind(embedding)
        .execute(&mut *tx)
        .await?;
    }
    tx.commit().await?;
    Ok(())
}

pub async fn delete_classes_except(pool: &PgPool, model_id: i64, keep: &[String]) -> Result<u64> {
    let done = sqlx::query("delete from class_embeddings where model_id = $1 and iri <> all($2)")
        .bind(model_id)
        .bind(keep)
        .execute(pool)
        .await?;
    Ok(done.rows_affected())
}

pub async fn delete_stale(
    pool: &PgPool,
    model_id: i64,
    kind: &str,
    after: i64,
    up_to: Option<i64>,
    keep: &[i64],
) -> Result<u64> {
    let done = sqlx::query(
        "delete from embeddings where model_id = $1 and owner_kind = $2 and owner_id > $3 \
         and ($4::bigint is null or owner_id <= $4) and owner_id <> all($5)",
    )
    .bind(model_id)
    .bind(kind)
    .bind(after)
    .bind(up_to)
    .bind(keep)
    .execute(pool)
    .await?;
    Ok(done.rows_affected())
}

pub fn index_name(model_id: i64, kind: &str) -> Result<String> {
    let name = format!("embeddings_m{model_id}_{kind}");
    if !crate::config::is_identifier(&name) {
        bail!("index name {name:?} is not a plain identifier");
    }
    Ok(name)
}

pub async fn index_exists(pool: &PgPool, model_id: i64, kind: &str) -> Result<bool> {
    let name = index_name(model_id, kind)?;
    let exists = sqlx::query_scalar(
        "select exists (select 1 from pg_indexes where schemaname = current_schema() \
         and indexname = $1)",
    )
    .bind(&name)
    .fetch_one(pool)
    .await?;
    Ok(exists)
}

pub async fn create_index(pool: &PgPool, model_id: i64, kind: &str, dimensions: i32) -> Result<()> {
    if ![CHUNK, ENTITY].contains(&kind) || dimensions <= 0 {
        bail!("cannot index {kind} vectors of {dimensions} dimensions");
    }
    let name = quote_identifier(&index_name(model_id, kind)?);
    let sql = format!(
        "create index if not exists {name} on embeddings using hnsw \
         ((embedding::halfvec({dimensions})) halfvec_cosine_ops) \
         where model_id = {model_id} and owner_kind = '{kind}'"
    );
    sqlx::raw_sql(sqlx::AssertSqlSafe(sql))
        .execute(pool)
        .await?;
    Ok(())
}

pub async fn prune(pool: &PgPool, keep: i64) -> Result<Vec<String>> {
    let gone: Vec<Stored> = sqlx::query_as(
        "select id, name, dimensions, current from embedding_models where id <> $1 order by id",
    )
    .bind(keep)
    .fetch_all(pool)
    .await?;
    for model in &gone {
        for kind in [CHUNK, ENTITY] {
            let name = quote_identifier(&index_name(model.id, kind)?);
            sqlx::raw_sql(sqlx::AssertSqlSafe(format!("drop index if exists {name}")))
                .execute(pool)
                .await?;
        }
        sqlx::query("delete from embedding_models where id = $1")
            .bind(model.id)
            .execute(pool)
            .await?;
    }
    Ok(gone.into_iter().map(|m| m.name).collect())
}

#[derive(Debug, Clone, FromRow)]
pub struct Counts {
    pub name: String,
    pub id: i64,
    pub current: bool,
    pub chunks: i64,
    pub entities: i64,
    pub classes: i64,
    pub truncated: i64,
}

pub async fn counts(pool: &PgPool) -> Result<Vec<Counts>> {
    let rows = sqlx::query_as(
        "select m.name, m.id, m.current, \
         (select count(*) from embeddings e where e.model_id = m.id and e.owner_kind = 'chunk') \
         as chunks, \
         (select count(*) from embeddings e where e.model_id = m.id and e.owner_kind = 'entity') \
         as entities, \
         (select count(*) from class_embeddings c where c.model_id = m.id) as classes, \
         (select count(*) from embeddings e where e.model_id = m.id and e.truncated) as truncated \
         from embedding_models m order by m.id",
    )
    .fetch_all(pool)
    .await?;
    Ok(rows)
}

#[derive(Debug, Clone, FromRow)]
pub struct Linked {
    pub entity_id: i64,
    pub chunk_id: i64,
    pub content_id: i64,
}

pub async fn linked_mentions(
    pool: &PgPool,
    min_chars: i32,
    skip_method: &str,
) -> Result<Vec<Linked>> {
    let rows = sqlx::query_as(
        "with m as (select distinct m.entity_id, m.chunk_id, k.content_id from mentions m \
         join chunks k on k.id = m.chunk_id join entities e on e.id = m.entity_id \
         where e.document_id is null and m.method not like 'llm:%' and m.method <> $2 \
         and length(k.text) >= $1 \
         and exists (select 1 from documents d where d.content_id = k.content_id \
         and d.status = 'present')), \
         linked as (select entity_id from m group by entity_id \
         having count(distinct content_id) >= 2) \
         select m.entity_id, m.chunk_id, m.content_id from m join linked using (entity_id) \
         order by m.entity_id, m.chunk_id",
    )
    .bind(min_chars)
    .bind(skip_method)
    .fetch_all(pool)
    .await?;
    Ok(rows)
}

pub async fn distractors(
    pool: &PgPool,
    min_chars: i32,
    seed: &str,
    limit: i64,
) -> Result<Vec<i64>> {
    let rows = sqlx::query_scalar(
        "select k.id from chunks k where length(k.text) >= $1 \
         and exists (select 1 from documents d where d.content_id = k.content_id \
         and d.status = 'present') order by md5($2 || k.id::text), k.id limit $3",
    )
    .bind(min_chars)
    .bind(seed)
    .bind(limit)
    .fetch_all(pool)
    .await?;
    Ok(rows)
}

#[derive(Debug, Clone, FromRow)]
pub struct EvalChunk {
    pub id: i64,
    pub content_id: i64,
    pub text: String,
    pub ocr_confidence: Option<f64>,
}

pub async fn eval_chunks(pool: &PgPool, ids: &[i64]) -> Result<Vec<EvalChunk>> {
    let rows = sqlx::query_as(
        "select k.id, k.content_id, k.text, \
         (select min(p.ocr_confidence) from pages p where p.content_id = k.content_id \
         and p.method = 'ocr' and p.page_no between coalesce(k.page_from, 0) \
         and coalesce(k.page_to, 2147483647)) as ocr_confidence \
         from chunks k where k.id = any($1) order by k.id",
    )
    .bind(ids)
    .fetch_all(pool)
    .await?;
    Ok(rows)
}

pub async fn content_chunks(pool: &PgPool, contents: &[i64], min_chars: i32) -> Result<Vec<i64>> {
    let rows = sqlx::query_scalar(
        "select id from chunks where content_id = any($1) and length(text) >= $2 order by id",
    )
    .bind(contents)
    .bind(min_chars)
    .fetch_all(pool)
    .await?;
    Ok(rows)
}

pub async fn present_paths(pool: &PgPool) -> Result<Vec<(String, i64)>> {
    let rows = sqlx::query_as(
        "select path, content_id from documents where status = 'present' \
         and content_id is not null order by path",
    )
    .fetch_all(pool)
    .await?;
    Ok(rows)
}

pub async fn class_mentions(pool: &PgPool, chunks: &[i64]) -> Result<Vec<(i64, String)>> {
    let rows = sqlx::query_as(
        "select distinct chunk_id, class_iri from class_mentions where chunk_id = any($1) \
         order by 1, 2",
    )
    .bind(chunks)
    .fetch_all(pool)
    .await?;
    Ok(rows)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn vectors_round_trip_as_text() {
        let text = vector_text(&[0.5, -0.25, 1.0]);
        assert_eq!(text, "[0.5,-0.25,1]");
        assert_eq!(parse_vector(&text).unwrap(), [0.5, -0.25, 1.0]);
        assert!(parse_vector("[a]").is_err());
        assert!(parse_vector("[]").unwrap().is_empty());
    }

    #[test]
    fn index_names_are_plain() {
        assert_eq!(index_name(3, CHUNK).unwrap(), "embeddings_m3_chunk");
    }
}
