use std::collections::{BTreeSet, HashMap};

use anyhow::Result;
use sqlx::{FromRow, PgConnection, PgPool};

use super::copy_row;
use crate::rules::{ClassHit, DocumentFacts, Endpoint, EntityHit, TextFacts};

#[derive(Debug, Clone, FromRow)]
pub struct DocumentWork {
    pub id: i64,
    pub path: String,
    pub status: String,
    pub dataset: String,
}

pub struct DocumentOutput {
    pub id: i64,
    pub path: String,
    pub dataset: String,
    pub document_class: Option<String>,
    pub facts: Option<DocumentFacts>,
}

#[derive(Debug, Clone, FromRow)]
pub struct ChunkWork {
    pub id: i64,
    pub content_id: i64,
    pub text: String,
    pub dataset: String,
}

pub struct ChunkOutput {
    pub id: i64,
    pub dataset: String,
    pub facts: TextFacts,
}

struct NewEntity {
    class: String,
    key: String,
    name: String,
    dataset: String,
    method: String,
    confidence: f64,
    document_id: Option<i64>,
}

const DOCUMENTS_TO_RULE: &str = "from documents d join sources s on s.id = d.source_id \
    where d.source_id = any($2) and ((d.status <> 'deleted' and d.rules_key is distinct from $1) \
    or (d.status = 'deleted' and d.rules_key is not null))";

pub async fn count_documents_to_rule(pool: &PgPool, key: &str, sources: &[i64]) -> Result<i64> {
    let sql = format!("select count(*) {DOCUMENTS_TO_RULE}");
    let count = sqlx::query_scalar(sqlx::AssertSqlSafe(sql))
        .bind(key)
        .bind(sources)
        .fetch_one(pool)
        .await?;
    Ok(count)
}

pub async fn next_documents_to_rule(
    pool: &PgPool,
    key: &str,
    sources: &[i64],
    after: i64,
    limit: i64,
) -> Result<Vec<DocumentWork>> {
    let sql = format!(
        "select d.id, d.path, d.status, s.dataset {DOCUMENTS_TO_RULE} and d.id > $3 \
         order by d.id limit $4"
    );
    let rows = sqlx::query_as(sqlx::AssertSqlSafe(sql))
        .bind(key)
        .bind(sources)
        .bind(after)
        .bind(limit)
        .fetch_all(pool)
        .await?;
    Ok(rows)
}

const CONTENTS_TO_RULE: &str = "from contents c where c.chunk_key = $3 \
    and c.rules_key is distinct from $1 and exists (select 1 from documents d \
    where d.content_id = c.id and d.status = 'present' and d.source_id = any($2))";

pub async fn count_contents_to_rule(
    pool: &PgPool,
    key: &str,
    sources: &[i64],
    chunk_key: &str,
) -> Result<i64> {
    let sql = format!("select count(*) {CONTENTS_TO_RULE}");
    let count = sqlx::query_scalar(sqlx::AssertSqlSafe(sql))
        .bind(key)
        .bind(sources)
        .bind(chunk_key)
        .fetch_one(pool)
        .await?;
    Ok(count)
}

pub async fn next_contents_to_rule(
    pool: &PgPool,
    key: &str,
    sources: &[i64],
    chunk_key: &str,
    after: i64,
    limit: i64,
) -> Result<Vec<i64>> {
    let sql = format!("select c.id {CONTENTS_TO_RULE} and c.id > $4 order by c.id limit $5");
    let ids = sqlx::query_scalar(sqlx::AssertSqlSafe(sql))
        .bind(key)
        .bind(sources)
        .bind(chunk_key)
        .bind(after)
        .bind(limit)
        .fetch_all(pool)
        .await?;
    Ok(ids)
}

pub async fn chunks_of(pool: &PgPool, content_ids: &[i64]) -> Result<Vec<ChunkWork>> {
    let rows = sqlx::query_as(
        "select k.id, k.content_id, k.text, \
         coalesce((select min(s.dataset) from documents d join sources s on s.id = d.source_id \
         where d.content_id = k.content_id and d.status = 'present'), '') as dataset \
         from chunks k where k.content_id = any($1) order by k.content_id, k.ord",
    )
    .bind(content_ids)
    .fetch_all(pool)
    .await?;
    Ok(rows)
}

async fn upsert_entities(
    conn: &mut PgConnection,
    entities: Vec<NewEntity>,
    version_id: i64,
) -> Result<HashMap<(String, String), i64>> {
    let mut seen = BTreeSet::new();
    let mut classes = Vec::new();
    let mut keys = Vec::new();
    let mut names = Vec::new();
    let mut datasets = Vec::new();
    let mut methods = Vec::new();
    let mut confidences = Vec::new();
    let mut documents = Vec::new();
    for entity in entities {
        if !seen.insert((entity.class.clone(), entity.key.clone())) {
            continue;
        }
        classes.push(entity.class);
        keys.push(entity.key);
        names.push(entity.name);
        datasets.push(entity.dataset);
        methods.push(entity.method);
        confidences.push(entity.confidence);
        documents.push(entity.document_id);
    }
    if classes.is_empty() {
        return Ok(HashMap::new());
    }
    let rows: Vec<(i64, String, String)> = sqlx::query_as(
        "insert into entities (class_iri, key, name, dataset, method, confidence, document_id, \
         ontology_version_id) \
         select class_iri, key, name, dataset, method, confidence, document_id, $8 \
         from unnest($1::text[], $2::text[], $3::text[], $4::text[], $5::text[], \
         $6::float8[], $7::bigint[]) \
         as n(class_iri, key, name, dataset, method, confidence, document_id) \
         on conflict (class_iri, key) do update set \
         name = case when entities.name = entities.key then excluded.name else entities.name end, \
         ontology_version_id = excluded.ontology_version_id \
         returning id, class_iri, key",
    )
    .bind(&classes)
    .bind(&keys)
    .bind(&names)
    .bind(&datasets)
    .bind(&methods)
    .bind(&confidences)
    .bind(&documents)
    .bind(version_id)
    .fetch_all(&mut *conn)
    .await?;
    Ok(rows
        .into_iter()
        .map(|(id, class, key)| ((class, key), id))
        .collect())
}

fn new_entity(hit: &EntityHit, dataset: &str) -> NewEntity {
    NewEntity {
        class: hit.class.clone(),
        key: hit.key.clone(),
        name: hit.name.clone(),
        dataset: dataset.to_string(),
        method: hit.method.to_string(),
        confidence: hit.confidence,
        document_id: None,
    }
}

enum Owner {
    Document(i64),
    Chunk(i64),
}

fn mention_row(
    data: &mut String,
    entity: i64,
    owner: &Owner,
    start: usize,
    end: usize,
    method: &str,
    confidence: f64,
) {
    let entity = entity.to_string();
    let (document, chunk) = match owner {
        Owner::Document(id) => (Some(id.to_string()), None),
        Owner::Chunk(id) => (None, Some(id.to_string())),
    };
    let start = start.to_string();
    let end = end.to_string();
    let confidence = confidence.to_string();
    copy_row(
        data,
        &[
            Some(&entity),
            document.as_deref(),
            chunk.as_deref(),
            Some(&start),
            Some(&end),
            Some(method),
            Some(&confidence),
        ],
    );
}

fn class_row(data: &mut String, hit: &ClassHit, owner: &Owner) {
    let (document, chunk) = match owner {
        Owner::Document(id) => (Some(id.to_string()), None),
        Owner::Chunk(id) => (None, Some(id.to_string())),
    };
    let start = hit.start.to_string();
    let end = hit.end.to_string();
    copy_row(
        data,
        &[
            Some(&hit.class),
            document.as_deref(),
            chunk.as_deref(),
            Some(&start),
            Some(&end),
            Some("label"),
        ],
    );
}

async fn copy(conn: &mut PgConnection, statement: &str, data: String) -> Result<()> {
    if data.is_empty() {
        return Ok(());
    }
    let mut copy = conn.copy_in_raw(statement).await?;
    copy.send(data.into_bytes()).await?;
    copy.finish().await?;
    Ok(())
}

const COPY_MENTIONS: &str = "copy mentions (entity_id, document_id, chunk_id, span_start, \
                             span_end, method, confidence) from stdin";
const COPY_CLASS_MENTIONS: &str = "copy class_mentions (class_iri, document_id, chunk_id, \
                                   span_start, span_end, method) from stdin";

pub async fn write_document_rules(
    pool: &PgPool,
    outputs: &[DocumentOutput],
    key: &str,
    version_id: i64,
) -> Result<()> {
    let ids: Vec<i64> = outputs.iter().map(|o| o.id).collect();
    let mut tx = pool.begin().await?;
    for table in ["mentions", "relations", "class_mentions"] {
        let sql = format!("delete from {table} where document_id = any($1)");
        sqlx::query(sqlx::AssertSqlSafe(sql))
            .bind(&ids)
            .execute(&mut *tx)
            .await?;
    }
    let mut entities = Vec::new();
    for output in outputs {
        let Some(facts) = &output.facts else {
            continue;
        };
        if let Some(class) = &output.document_class {
            entities.push(NewEntity {
                class: class.clone(),
                key: output.id.to_string(),
                name: file_name(&output.path).to_string(),
                dataset: output.dataset.clone(),
                method: "path".to_string(),
                confidence: 1.0,
                document_id: Some(output.id),
            });
        }
        for hit in &facts.entities {
            entities.push(new_entity(hit, &output.dataset));
        }
    }
    let map = upsert_entities(&mut tx, entities, version_id).await?;
    let mut mentions = String::new();
    let mut class_mentions = String::new();
    let mut sources = Vec::new();
    let mut properties = Vec::new();
    let mut targets = Vec::new();
    let mut relation_documents = Vec::new();
    let mut doc_types = Vec::new();
    let mut props = Vec::new();
    for output in outputs {
        let owner = Owner::Document(output.id);
        let Some(facts) = &output.facts else {
            doc_types.push(None);
            props.push(serde_json::Value::Object(Default::default()));
            continue;
        };
        doc_types.push(facts.doc_type.clone());
        props.push(serde_json::to_value(&facts.props)?);
        let document_entity = output
            .document_class
            .as_ref()
            .and_then(|class| map.get(&(class.clone(), output.id.to_string())))
            .copied();
        if let Some(entity) = document_entity {
            mention_row(
                &mut mentions,
                entity,
                &owner,
                0,
                output.path.chars().count(),
                "path",
                1.0,
            );
        }
        let ids: Vec<Option<i64>> = facts
            .entities
            .iter()
            .map(|hit| map.get(&(hit.class.clone(), hit.key.clone())).copied())
            .collect();
        for (hit, id) in facts.entities.iter().zip(&ids) {
            if let Some(id) = id {
                mention_row(
                    &mut mentions,
                    *id,
                    &owner,
                    hit.start,
                    hit.end,
                    hit.method,
                    hit.confidence,
                );
            }
        }
        let endpoint = |end: &Endpoint| match end {
            Endpoint::Document => document_entity,
            Endpoint::Entity(i) => ids.get(*i).copied().flatten(),
        };
        let mut seen = BTreeSet::new();
        for relation in &facts.relations {
            let (Some(from), Some(to)) = (endpoint(&relation.from), endpoint(&relation.to)) else {
                continue;
            };
            if !seen.insert((from, relation.property.clone(), to)) {
                continue;
            }
            sources.push(from);
            properties.push(relation.property.clone());
            targets.push(to);
            relation_documents.push(output.id);
        }
        for hit in &facts.classes {
            class_row(&mut class_mentions, hit, &owner);
        }
    }
    copy(&mut tx, COPY_MENTIONS, mentions).await?;
    copy(&mut tx, COPY_CLASS_MENTIONS, class_mentions).await?;
    if !sources.is_empty() {
        sqlx::query(
            "insert into relations (src_id, property_iri, dst_id, method, confidence, document_id) \
             select src, property, dst, 'path', 1.0, document \
             from unnest($1::bigint[], $2::text[], $3::bigint[], $4::bigint[]) \
             as r(src, property, dst, document)",
        )
        .bind(&sources)
        .bind(&properties)
        .bind(&targets)
        .bind(&relation_documents)
        .execute(&mut *tx)
        .await?;
    }
    let props: Vec<sqlx::types::Json<serde_json::Value>> =
        props.into_iter().map(sqlx::types::Json).collect();
    sqlx::query(
        "update documents d set doc_type = u.doc_type, props = u.props, \
         rules_key = case when d.status = 'deleted' then null else $4 end \
         from unnest($1::bigint[], $2::text[], $3::jsonb[]) as u(id, doc_type, props) \
         where d.id = u.id",
    )
    .bind(&ids)
    .bind(&doc_types)
    .bind(&props)
    .bind(key)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(())
}

pub async fn write_text_rules(
    pool: &PgPool,
    content_ids: &[i64],
    outputs: &[ChunkOutput],
    key: &str,
    version_id: i64,
) -> Result<()> {
    let mut tx = pool.begin().await?;
    for table in ["mentions", "class_mentions"] {
        let sql = format!(
            "delete from {table} where chunk_id in \
             (select id from chunks where content_id = any($1))"
        );
        sqlx::query(sqlx::AssertSqlSafe(sql))
            .bind(content_ids)
            .execute(&mut *tx)
            .await?;
    }
    let entities = outputs
        .iter()
        .flat_map(|o| {
            o.facts
                .entities
                .iter()
                .map(|hit| new_entity(hit, &o.dataset))
        })
        .collect();
    let map = upsert_entities(&mut tx, entities, version_id).await?;
    let mut mentions = String::new();
    let mut class_mentions = String::new();
    for output in outputs {
        let owner = Owner::Chunk(output.id);
        for hit in &output.facts.entities {
            if let Some(id) = map.get(&(hit.class.clone(), hit.key.clone())) {
                mention_row(
                    &mut mentions,
                    *id,
                    &owner,
                    hit.start,
                    hit.end,
                    hit.method,
                    hit.confidence,
                );
            }
        }
        for hit in &output.facts.classes {
            class_row(&mut class_mentions, hit, &owner);
        }
    }
    copy(&mut tx, COPY_MENTIONS, mentions).await?;
    copy(&mut tx, COPY_CLASS_MENTIONS, class_mentions).await?;
    sqlx::query("update contents set rules_key = $2 where id = any($1)")
        .bind(content_ids)
        .bind(key)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(())
}

pub async fn delete_orphans(pool: &PgPool) -> Result<u64> {
    let count = sqlx::query(
        "delete from entities e where not exists (select 1 from mentions m where m.entity_id = e.id)",
    )
    .execute(pool)
    .await?
    .rows_affected();
    Ok(count)
}

pub async fn sync_aliases(pool: &PgPool, entries: &[crate::rules::gazetteer::Entry]) -> Result<()> {
    let mut classes = Vec::new();
    let mut keys = Vec::new();
    let mut aliases = Vec::new();
    let mut langs = Vec::new();
    for entry in entries {
        for alias in &entry.aliases {
            classes.push(entry.class.clone());
            keys.push(entry.key.clone());
            aliases.push(alias.clone());
            langs.push(entry.lang.clone());
        }
    }
    let mut tx = pool.begin().await?;
    sqlx::query("delete from aliases where method = 'gazetteer'")
        .execute(&mut *tx)
        .await?;
    if !classes.is_empty() {
        sqlx::query(
            "insert into aliases (entity_id, alias, lang, method) \
             select e.id, a.alias, a.lang, 'gazetteer' \
             from unnest($1::text[], $2::text[], $3::text[], $4::text[]) \
             as a(class_iri, key, alias, lang) \
             join entities e on e.class_iri = a.class_iri and e.key = a.key \
             on conflict do nothing",
        )
        .bind(&classes)
        .bind(&keys)
        .bind(&aliases)
        .bind(&langs)
        .execute(&mut *tx)
        .await?;
    }
    tx.commit().await?;
    Ok(())
}

fn file_name(path: &str) -> &str {
    path.rsplit('/').next().unwrap_or(path)
}
