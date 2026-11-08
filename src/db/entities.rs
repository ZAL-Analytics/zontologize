use std::collections::{BTreeMap, BTreeSet, HashMap};

use anyhow::Result;
use sqlx::{FromRow, PgConnection, PgPool};

use super::{MODEL_METHODS, RESOLVE_METHODS, copy_row};
use crate::rules::pages::Segment;
use crate::rules::{ClassHit, DocumentFacts, Endpoint, EntityHit, TextEnd, TextFacts};

#[derive(Debug, Clone, FromRow)]
pub struct DocumentWork {
    pub id: i64,
    pub path: String,
    pub status: String,
    pub dataset: String,
    pub llm_type: Option<String>,
    pub llm_method: Option<String>,
    pub llm_confidence: Option<f64>,
}

pub struct DocumentOutput {
    pub id: i64,
    pub path: String,
    pub dataset: String,
    pub document_class: Option<String>,
    pub type_method: Option<String>,
    pub type_confidence: f64,
    pub facts: Option<DocumentFacts>,
}

pub struct ModelType {
    pub doc_type: String,
    pub method: String,
    pub confidence: f64,
}

#[derive(Debug, Clone, FromRow)]
pub struct ChunkWork {
    pub id: i64,
    pub content_id: i64,
    pub text: String,
    pub dataset: String,
    pub ocr: bool,
    pub ocr_confidence: Option<f64>,
    pub page_from: Option<i32>,
    pub page_to: Option<i32>,
}

#[derive(Debug, Clone, FromRow)]
pub struct PageWork {
    pub content_id: i64,
    pub sha256: String,
    pub page_no: i32,
    pub text: String,
    pub ocr_confidence: Option<f64>,
}

pub struct SegmentEntity {
    pub class: String,
    pub method: String,
    pub key: String,
    pub name: String,
    pub dataset: String,
    pub chunk_id: i64,
    pub span_end: usize,
    pub confidence: f64,
}

pub struct ContentSegments {
    pub content_id: i64,
    pub segments: Vec<Segment>,
    pub entities: Vec<SegmentEntity>,
}

impl ChunkWork {
    pub fn ocr_scale(&self) -> Option<f64> {
        self.ocr.then(|| self.ocr_confidence.unwrap_or(1.0))
    }
}

pub struct ChunkOutput {
    pub id: i64,
    pub dataset: String,
    pub facts: TextFacts,
}

pub(crate) struct NewEntity {
    pub class: String,
    pub key: String,
    pub name: String,
    pub dataset: String,
    pub method: String,
    pub confidence: f64,
    pub document_id: Option<i64>,
    pub props: BTreeMap<String, String>,
}

const DOCUMENTS_TO_RULE: &str = "from documents d join sources s on s.id = d.source_id \
    left join lateral (select t.doc_type, t.method, t.confidence from llm_types t \
    where t.content_id = d.content_id and t.doc_type is not null \
    and (select count(*) from segments g where g.content_id = d.content_id) <= 1 \
    order by t.page_from limit 1) t on true \
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
        "select d.id, d.path, d.status, s.dataset, t.doc_type as llm_type, \
         t.method as llm_method, t.confidence as llm_confidence {DOCUMENTS_TO_RULE} \
         and d.id > $3 order by d.id limit $4"
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
         where d.content_id = k.content_id and d.status = 'present'), '') as dataset, \
         o.ocr, o.ocr_confidence, k.page_from, k.page_to \
         from chunks k cross join lateral (select count(*) > 0 as ocr, \
         min(p.ocr_confidence) as ocr_confidence from pages p \
         where p.content_id = k.content_id and p.method = 'ocr' \
         and p.page_no between coalesce(k.page_from, 0) and coalesce(k.page_to, 2147483647)) o \
         where k.content_id = any($1) order by k.content_id, k.ord",
    )
    .bind(content_ids)
    .fetch_all(pool)
    .await?;
    Ok(rows)
}

fn better_method(new: &NewEntity, old: &NewEntity) -> bool {
    let rank = |e: &NewEntity| {
        (
            e.confidence,
            !e.method.starts_with(crate::llm::METHOD_PREFIX),
        )
    };
    rank(new) > rank(old)
}

fn best_entities(entities: Vec<NewEntity>) -> Vec<NewEntity> {
    let mut best: Vec<NewEntity> = Vec::new();
    let mut index: HashMap<(String, String), usize> = HashMap::new();
    for entity in entities {
        match index.get(&(entity.class.clone(), entity.key.clone())) {
            Some(&i) => {
                best[i].props.extend(entity.props.clone());
                if better_method(&entity, &best[i]) {
                    best[i].method = entity.method;
                    best[i].confidence = entity.confidence;
                }
            }
            None => {
                index.insert((entity.class.clone(), entity.key.clone()), best.len());
                best.push(entity);
            }
        }
    }
    best
}

pub(crate) async fn upsert_entities(
    conn: &mut PgConnection,
    entities: Vec<NewEntity>,
    version_id: i64,
) -> Result<HashMap<(String, String), i64>> {
    let mut classes = Vec::new();
    let mut keys = Vec::new();
    let mut names = Vec::new();
    let mut datasets = Vec::new();
    let mut methods = Vec::new();
    let mut confidences = Vec::new();
    let mut documents = Vec::new();
    let mut props = Vec::new();
    for entity in best_entities(entities) {
        props.push(sqlx::types::Json(entity.props));
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
         ontology_version_id, props) \
         select class_iri, key, name, dataset, method, confidence, document_id, $8, props \
         from unnest($1::text[], $2::text[], $3::text[], $4::text[], $5::text[], \
         $6::float8[], $7::bigint[], $10::jsonb[]) \
         as n(class_iri, key, name, dataset, method, confidence, document_id, props) \
         on conflict (class_iri, key) do update set \
         props = entities.props || excluded.props, \
         name = case when entities.name = entities.key then excluded.name else entities.name end, \
         method = case when (excluded.confidence, excluded.method not like $9) \
         > (entities.confidence, entities.method not like $9) \
         then excluded.method else entities.method end, \
         confidence = greatest(entities.confidence, excluded.confidence), \
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
    .bind(MODEL_METHODS)
    .bind(&props)
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
        props: hit.props.clone(),
    }
}

pub(crate) enum Owner {
    Document(i64),
    Chunk(i64),
}

pub(crate) fn mention_row(
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
    let confidence = hit.confidence.to_string();
    copy_row(
        data,
        &[
            Some(&hit.class),
            document.as_deref(),
            chunk.as_deref(),
            Some(&start),
            Some(&end),
            Some("label"),
            Some(&confidence),
        ],
    );
}

pub(crate) async fn copy(conn: &mut PgConnection, statement: &str, data: String) -> Result<()> {
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
                                   span_start, span_end, method, confidence) from stdin";

pub async fn write_document_rules(
    pool: &PgPool,
    outputs: &[DocumentOutput],
    key: &str,
    version_id: i64,
) -> Result<()> {
    let ids: Vec<i64> = outputs.iter().map(|o| o.id).collect();
    let mut tx = pool.begin().await?;
    for table in ["mentions", "relations", "class_mentions"] {
        let sql = format!(
            "delete from {table} where document_id = any($1) and method not like $2{}",
            open_only(table)
        );
        sqlx::query(sqlx::AssertSqlSafe(sql))
            .bind(&ids)
            .bind(RESOLVE_METHODS)
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
                method: output
                    .type_method
                    .clone()
                    .unwrap_or_else(|| crate::rules::PATH_METHOD.to_string()),
                confidence: output.type_confidence,
                document_id: Some(output.id),
                props: BTreeMap::new(),
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
    let mut type_methods = Vec::new();
    let mut props = Vec::new();
    for output in outputs {
        let owner = Owner::Document(output.id);
        let Some(facts) = &output.facts else {
            doc_types.push(None);
            type_methods.push(None);
            props.push(serde_json::Value::Object(Default::default()));
            continue;
        };
        doc_types.push(facts.doc_type.clone());
        type_methods.push(output.type_method.clone());
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
                crate::rules::PATH_METHOD,
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
             as r(src, property, dst, document) \
             where not exists (select 1 from relations v where v.src_id = r.src \
             and v.property_iri = r.property and v.dst_id = r.dst \
             and v.status in ('confirmed', 'rejected'))",
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
         type_method = u.type_method, \
         rules_key = case when d.status = 'deleted' then null else $4 end \
         from unnest($1::bigint[], $2::text[], $3::jsonb[], $5::text[]) \
         as u(id, doc_type, props, type_method) \
         where d.id = u.id",
    )
    .bind(&ids)
    .bind(&doc_types)
    .bind(&props)
    .bind(key)
    .bind(&type_methods)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(())
}

pub const SEGMENT_METHOD: &str = "page_rule";

fn open_only(table: &str) -> &'static str {
    if table == "relations" {
        " and status in ('auto', 'proposed')"
    } else {
        ""
    }
}

pub async fn write_text_rules(
    pool: &PgPool,
    content_ids: &[i64],
    outputs: &[ChunkOutput],
    segments: &[ContentSegments],
    part_property: Option<&str>,
    key: &str,
    version_id: i64,
) -> Result<()> {
    let mut tx = pool.begin().await?;
    for table in ["mentions", "class_mentions", "relations"] {
        let sql = format!(
            "delete from {table} where chunk_id in \
             (select id from chunks where content_id = any($1)) \
             and method not like $2 and method not like $3 and method <> 'gliner'{}",
            open_only(table)
        );
        sqlx::query(sqlx::AssertSqlSafe(sql))
            .bind(content_ids)
            .bind(MODEL_METHODS)
            .bind(RESOLVE_METHODS)
            .execute(&mut *tx)
            .await?;
    }
    sqlx::query("delete from segments where content_id = any($1)")
        .bind(content_ids)
        .execute(&mut *tx)
        .await?;
    let mut entities: Vec<NewEntity> = outputs
        .iter()
        .flat_map(|o| {
            o.facts
                .entities
                .iter()
                .map(|hit| new_entity(hit, &o.dataset))
        })
        .collect();
    for content in segments {
        for entity in &content.entities {
            entities.push(NewEntity {
                class: entity.class.clone(),
                key: entity.key.clone(),
                name: entity.name.clone(),
                dataset: entity.dataset.clone(),
                method: entity.method.clone(),
                confidence: entity.confidence,
                document_id: None,
                props: BTreeMap::new(),
            });
        }
    }
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
    let mut sources = Vec::new();
    let mut source_contents = Vec::new();
    let mut source_chunks = Vec::new();
    let mut source_confidences = Vec::new();
    for content in segments {
        for entity in &content.entities {
            let Some(id) = map.get(&(entity.class.clone(), entity.key.clone())) else {
                continue;
            };
            mention_row(
                &mut mentions,
                *id,
                &Owner::Chunk(entity.chunk_id),
                0,
                entity.span_end,
                SEGMENT_METHOD,
                entity.confidence,
            );
            sources.push(*id);
            source_contents.push(content.content_id);
            source_chunks.push(entity.chunk_id);
            source_confidences.push(entity.confidence);
        }
    }
    copy(&mut tx, COPY_MENTIONS, mentions).await?;
    copy(&mut tx, COPY_CLASS_MENTIONS, class_mentions).await?;
    write_text_relations(&mut tx, outputs, &map).await?;
    if let Some(property) = part_property
        && !sources.is_empty()
    {
        sqlx::query(
            "insert into relations (src_id, property_iri, dst_id, method, confidence, chunk_id) \
             select u.src, $5, e.id, $6, u.confidence, u.chunk \
             from unnest($1::bigint[], $2::bigint[], $3::bigint[], $4::float8[]) \
             as u(src, content, chunk, confidence) \
             join documents d on d.content_id = u.content and d.status = 'present' \
             join entities e on e.document_id = d.id and e.key = d.id::text \
             where not exists (select 1 from relations v where v.src_id = u.src \
             and v.property_iri = $5 and v.dst_id = e.id \
             and v.status in ('confirmed', 'rejected'))",
        )
        .bind(&sources)
        .bind(&source_contents)
        .bind(&source_chunks)
        .bind(&source_confidences)
        .bind(property)
        .bind(SEGMENT_METHOD)
        .execute(&mut *tx)
        .await?;
    }
    write_segments(&mut tx, segments, key).await?;
    sqlx::query("update contents set rules_key = $2 where id = any($1)")
        .bind(content_ids)
        .bind(key)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(())
}

const DOCUMENT_RELATIONS: &str = "insert into relations \
    (src_id, property_iri, dst_id, method, confidence, chunk_id) \
    select distinct on (e.id, u.property, u.dst) e.id, u.property, u.dst, u.method, \
    u.confidence, u.chunk \
    from unnest($1::bigint[], $2::text[], $3::bigint[], $4::text[], $5::float8[]) \
    as u(chunk, property, dst, method, confidence) \
    join chunks c on c.id = u.chunk \
    join documents d on d.content_id = c.content_id and d.status = 'present' \
    join entities e on e.document_id = d.id and e.key = d.id::text \
    where not exists (select 1 from relations v where v.src_id = e.id \
    and v.property_iri = u.property and v.dst_id = u.dst \
    and (v.status in ('confirmed', 'rejected') or v.chunk_id = u.chunk))";

const KNOWN_RELATIONS: &str = "insert into relations \
    (src_id, property_iri, dst_id, method, confidence, chunk_id) \
    select distinct on (u.src, u.property, e.id) u.src, u.property, e.id, u.method, \
    u.confidence, u.chunk \
    from unnest($1::bigint[], $2::text[], $3::text[], $4::text[], $5::text[], \
    $6::float8[], $7::bigint[]) as u(src, property, class, key, method, confidence, chunk) \
    join entities e on e.class_iri = u.class and e.key = u.key \
    where not exists (select 1 from relations v where v.src_id = u.src \
    and v.property_iri = u.property and v.dst_id = e.id)";

#[derive(Default)]
struct DocumentRows {
    chunks: Vec<i64>,
    properties: Vec<String>,
    targets: Vec<i64>,
    methods: Vec<String>,
    confidences: Vec<f64>,
}

#[derive(Default)]
struct KnownRows {
    sources: Vec<i64>,
    properties: Vec<String>,
    classes: Vec<String>,
    keys: Vec<String>,
    methods: Vec<String>,
    confidences: Vec<f64>,
    chunks: Vec<i64>,
}

async fn write_text_relations(
    conn: &mut PgConnection,
    outputs: &[ChunkOutput],
    map: &HashMap<(String, String), i64>,
) -> Result<()> {
    let mut documents = DocumentRows::default();
    let mut known = KnownRows::default();
    for output in outputs {
        let entities = &output.facts.entities;
        let id_of = |end: &TextEnd| match end {
            TextEnd::Entity(index) => entities
                .get(*index)
                .and_then(|hit| map.get(&(hit.class.clone(), hit.key.clone())))
                .map(|id| (*id, &entities[*index])),
            _ => None,
        };
        for relation in &output.facts.relations {
            match (&relation.from, &relation.to) {
                (TextEnd::Document, to) => {
                    let Some((target, hit)) = id_of(to) else {
                        continue;
                    };
                    documents.chunks.push(output.id);
                    documents.properties.push(relation.property.clone());
                    documents.targets.push(target);
                    documents.methods.push(hit.method.to_string());
                    documents.confidences.push(hit.confidence);
                }
                (from, TextEnd::Known(class, key)) => {
                    let Some((source, hit)) = id_of(from) else {
                        continue;
                    };
                    known.sources.push(source);
                    known.properties.push(relation.property.clone());
                    known.classes.push(class.clone());
                    known.keys.push(key.clone());
                    known.methods.push(hit.method.to_string());
                    known.confidences.push(hit.confidence);
                    known.chunks.push(output.id);
                }
                _ => {}
            }
        }
    }
    if !documents.chunks.is_empty() {
        sqlx::query(DOCUMENT_RELATIONS)
            .bind(&documents.chunks)
            .bind(&documents.properties)
            .bind(&documents.targets)
            .bind(&documents.methods)
            .bind(&documents.confidences)
            .execute(&mut *conn)
            .await?;
    }
    if !known.sources.is_empty() {
        sqlx::query(KNOWN_RELATIONS)
            .bind(&known.sources)
            .bind(&known.properties)
            .bind(&known.classes)
            .bind(&known.keys)
            .bind(&known.methods)
            .bind(&known.confidences)
            .bind(&known.chunks)
            .execute(&mut *conn)
            .await?;
    }
    Ok(())
}

async fn write_segments(
    conn: &mut PgConnection,
    segments: &[ContentSegments],
    key: &str,
) -> Result<()> {
    let mut contents = Vec::new();
    let mut ords = Vec::new();
    let mut froms = Vec::new();
    let mut tos = Vec::new();
    let mut doc_types = Vec::new();
    for content in segments {
        for segment in &content.segments {
            contents.push(content.content_id);
            ords.push(segment.ord);
            froms.push(segment.page_from);
            tos.push(segment.page_to);
            doc_types.push(segment.doc_type.clone());
        }
    }
    if contents.is_empty() {
        return Ok(());
    }
    sqlx::query(
        "insert into segments (content_id, ord, page_from, page_to, doc_type, rules_key) \
         select u.content_id, u.ord, u.page_from, u.page_to, u.doc_type, $6 \
         from unnest($1::bigint[], $2::int[], $3::int[], $4::int[], $5::text[]) \
         as u(content_id, ord, page_from, page_to, doc_type)",
    )
    .bind(&contents)
    .bind(&ords)
    .bind(&froms)
    .bind(&tos)
    .bind(&doc_types)
    .bind(key)
    .execute(&mut *conn)
    .await?;
    Ok(())
}

pub async fn model_types(pool: &PgPool, content_ids: &[i64]) -> Result<Vec<(i64, i32, ModelType)>> {
    let rows: Vec<(i64, i32, String, String, f64)> = sqlx::query_as(
        "select content_id, page_from, doc_type, method, confidence from llm_types \
         where content_id = any($1) and doc_type is not null",
    )
    .bind(content_ids)
    .fetch_all(pool)
    .await?;
    Ok(rows
        .into_iter()
        .map(|(content, page, doc_type, method, confidence)| {
            (
                content,
                page,
                ModelType {
                    doc_type,
                    method,
                    confidence,
                },
            )
        })
        .collect())
}

pub async fn pages_of(pool: &PgPool, content_ids: &[i64]) -> Result<Vec<PageWork>> {
    let rows = sqlx::query_as(
        "select p.content_id, c.sha256, p.page_no, p.text, \
         case when p.method = 'ocr' then p.ocr_confidence end as ocr_confidence \
         from pages p join contents c on c.id = p.content_id \
         where p.content_id = any($1) order by p.content_id, p.page_no",
    )
    .bind(content_ids)
    .fetch_all(pool)
    .await?;
    Ok(rows)
}

pub async fn known_keys(pool: &PgPool, method_to_skip: &str) -> Result<Vec<(String, String)>> {
    let rows = sqlx::query_as("select class_iri, key from entities where method <> $1")
        .bind(method_to_skip)
        .fetch_all(pool)
        .await?;
    Ok(rows)
}

pub async fn clear_unlisted_contents(pool: &PgPool) -> Result<u64> {
    let mut tx = pool.begin().await?;
    let ids: Vec<i64> = sqlx::query_scalar(
        "select c.id from contents c where c.rules_key is not null and not exists \
         (select 1 from documents d where d.content_id = c.id and d.status = 'present')",
    )
    .fetch_all(&mut *tx)
    .await?;
    if ids.is_empty() {
        return Ok(0);
    }
    for table in [
        "mentions",
        "class_mentions",
        "relations",
        "relation_terms",
        "attributes",
    ] {
        let sql = format!(
            "delete from {table} where chunk_id in \
             (select id from chunks where content_id = any($1)){}",
            open_only(table)
        );
        sqlx::query(sqlx::AssertSqlSafe(sql))
            .bind(&ids)
            .execute(&mut *tx)
            .await?;
    }
    sqlx::query("delete from segments where content_id = any($1)")
        .bind(&ids)
        .execute(&mut *tx)
        .await?;
    sqlx::query("update chunks set llm_key = null, attr_key = null where content_id = any($1)")
        .bind(&ids)
        .execute(&mut *tx)
        .await?;
    sqlx::query("update contents set rules_key = null where id = any($1)")
        .bind(&ids)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(ids.len() as u64)
}

pub async fn delete_unselected(pool: &PgPool, version_id: i64, keep: &str) -> Result<u64> {
    let mut tx = pool.begin().await?;
    let mentions = sqlx::query(
        "delete from mentions m using entities e where m.entity_id = e.id \
         and not exists (select 1 from classes c where c.ontology_version_id = $1 \
         and c.iri = e.class_iri and c.selected)",
    )
    .bind(version_id)
    .execute(&mut *tx)
    .await?
    .rows_affected();
    let relations = sqlx::query(
        "delete from relations r where r.status in ('auto', 'proposed') \
         and r.property_iri <> all($2) and not exists (select 1 from properties p \
         where p.ontology_version_id = $1 and p.iri = r.property_iri and p.selected)",
    )
    .bind(version_id)
    .bind(
        super::resolve::BUILT_IN
            .iter()
            .copied()
            .chain([keep])
            .collect::<Vec<_>>(),
    )
    .execute(&mut *tx)
    .await?
    .rows_affected();
    let attributes = sqlx::query(
        "delete from attributes a where not exists (select 1 from properties p \
         where p.ontology_version_id = $1 and p.iri = a.property_iri and p.selected)",
    )
    .bind(version_id)
    .execute(&mut *tx)
    .await?
    .rows_affected();
    tx.commit().await?;
    Ok(mentions + relations + attributes)
}

pub const ORPHAN: &str = "not exists (select 1 from mentions m where m.entity_id = e.id) \
                          and not exists (select 1 from relations r \
                          where (r.src_id = e.id or r.dst_id = e.id) \
                          and r.status in ('confirmed', 'rejected'))";

pub async fn delete_orphans(pool: &PgPool) -> Result<u64> {
    let sql = format!("delete from entities e where {ORPHAN}");
    let count = sqlx::query(sqlx::AssertSqlSafe(sql))
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
    sqlx::query(
        "create temporary table wanted_aliases on commit drop as \
         select distinct on (e.id, a.alias) e.id as entity_id, a.alias, a.lang \
         from unnest($1::text[], $2::text[], $3::text[], $4::text[]) \
         with ordinality as a(class_iri, key, alias, lang, n) \
         join entities e on e.class_iri = a.class_iri and e.key = a.key \
         order by e.id, a.alias, a.n",
    )
    .bind(&classes)
    .bind(&keys)
    .bind(&aliases)
    .bind(&langs)
    .execute(&mut *tx)
    .await?;
    sqlx::query(
        "delete from aliases x where x.method = 'gazetteer' and not exists \
         (select 1 from wanted_aliases w where w.entity_id = x.entity_id \
         and w.alias = x.alias and w.lang = x.lang)",
    )
    .execute(&mut *tx)
    .await?;
    sqlx::query(
        "insert into aliases (entity_id, alias, lang, method) \
         select entity_id, alias, lang, 'gazetteer' from wanted_aliases \
         on conflict do nothing",
    )
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(())
}

fn file_name(path: &str) -> &str {
    path.rsplit('/').next().unwrap_or(path)
}

#[derive(Debug, Default, PartialEq)]
pub struct Folded {
    pub merged: u64,
    pub moved: u64,
}

const FOLD_ALIASES: &str = "insert into aliases (entity_id, alias, lang, method) \
    select o.id, a.alias, a.lang, a.method from entities c \
    join entities o on o.class_iri = $2 and o.key = c.key \
    join aliases a on a.entity_id = c.id where c.class_iri = $1 \
    union all select o.id, c.name, '', 'fold' from entities c \
    join entities o on o.class_iri = $2 and o.key = c.key \
    where c.class_iri = $1 and c.name <> o.name \
    on conflict do nothing";

const FOLD_MENTIONS: &str = "update mentions m set entity_id = o.id from entities c \
    join entities o on o.class_iri = $2 and o.key = c.key \
    where c.class_iri = $1 and m.entity_id = c.id";

const FOLD_SOURCES: &str = "update relations r set src_id = o.id from entities c \
    join entities o on o.class_iri = $2 and o.key = c.key \
    where c.class_iri = $1 and r.src_id = c.id";

const FOLD_TARGETS: &str = "update relations r set dst_id = o.id from entities c \
    join entities o on o.class_iri = $2 and o.key = c.key \
    where c.class_iri = $1 and r.dst_id = c.id";

const FOLD_DELETE: &str = "delete from entities c using entities o \
    where c.class_iri = $1 and o.class_iri = $2 and o.key = c.key";

pub async fn fold_class(conn: &mut PgConnection, from: &str, into: &str) -> Result<Folded> {
    for statement in [FOLD_ALIASES, FOLD_MENTIONS, FOLD_SOURCES, FOLD_TARGETS] {
        sqlx::query(statement)
            .bind(from)
            .bind(into)
            .execute(&mut *conn)
            .await?;
    }
    sqlx::query("delete from relations where src_id = dst_id")
        .execute(&mut *conn)
        .await?;
    let merged = sqlx::query(FOLD_DELETE)
        .bind(from)
        .bind(into)
        .execute(&mut *conn)
        .await?
        .rows_affected();
    let moved =
        sqlx::query("update entities set class_iri = $2, canonical_id = null where class_iri = $1")
            .bind(from)
            .bind(into)
            .execute(&mut *conn)
            .await?
            .rows_affected();
    Ok(Folded { merged, moved })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entity(key: &str, method: &str, confidence: f64) -> NewEntity {
        NewEntity {
            class: "c".to_string(),
            key: key.to_string(),
            name: key.to_string(),
            dataset: String::new(),
            method: method.to_string(),
            confidence,
            document_id: None,
            props: BTreeMap::new(),
        }
    }

    #[test]
    fn the_best_method_wins_and_rules_win_ties() {
        let best = best_entities(vec![
            entity("a", "ocr", 0.6),
            entity("a", "regex", 1.0),
            entity("a", "llm:m", 1.0),
            entity("b", "llm:m", 0.8),
            entity("b", "gazetteer", 0.8),
            entity("c", "regex", 1.0),
            entity("c", "ocr", 0.5),
        ]);
        let found: Vec<(&str, &str, f64)> = best
            .iter()
            .map(|e| (e.key.as_str(), e.method.as_str(), e.confidence))
            .collect();
        assert_eq!(
            found,
            [
                ("a", "regex", 1.0),
                ("b", "gazetteer", 0.8),
                ("c", "regex", 1.0)
            ]
        );
    }
}
