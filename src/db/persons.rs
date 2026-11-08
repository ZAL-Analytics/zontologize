use anyhow::Result;
use sqlx::PgPool;

use super::entities::{NewEntity, Owner, copy, mention_row, upsert_entities};

pub struct Hit {
    pub chunk_id: i64,
    pub key: String,
    pub name: String,
    pub dataset: String,
    pub start: usize,
    pub end: usize,
    pub confidence: f64,
}

const COPY_MENTIONS: &str = "copy mentions (entity_id, document_id, chunk_id, span_start, \
    span_end, method, confidence) from stdin";

pub async fn organisation_names(pool: &PgPool, classes: &[String]) -> Result<Vec<String>> {
    let names = sqlx::query_scalar(
        "select lower(e.name) from entities e where e.class_iri = any($1) \
         union select lower(a.alias) from aliases a join entities e on e.id = a.entity_id \
         where e.class_iri = any($1)",
    )
    .bind(classes)
    .fetch_all(pool)
    .await?;
    Ok(names)
}

fn patterns(paths: &[String]) -> Vec<String> {
    if paths.is_empty() {
        return vec!["%".to_string()];
    }
    paths
        .iter()
        .map(|p| {
            let escaped = p
                .replace('\\', "\\\\")
                .replace('%', "\\%")
                .replace('_', "\\_");
            format!("{escaped}%")
        })
        .collect()
}

pub async fn count_contents(pool: &PgPool, key: &str, paths: &[String]) -> Result<i64> {
    let count = sqlx::query_scalar(
        "select count(*) from contents c where c.persons_key is distinct from $1 \
         and exists (select 1 from documents d where d.content_id = c.id and d.status = 'present' \
         and d.path like any($2))",
    )
    .bind(key)
    .bind(patterns(paths))
    .fetch_one(pool)
    .await?;
    Ok(count)
}

pub async fn next_contents(
    pool: &PgPool,
    key: &str,
    paths: &[String],
    after: i64,
    limit: i64,
) -> Result<Vec<i64>> {
    let ids = sqlx::query_scalar(
        "select c.id from contents c where c.id > $2 and c.persons_key is distinct from $1 \
         and exists (select 1 from documents d where d.content_id = c.id and d.status = 'present' \
         and d.path like any($4)) \
         order by c.id limit $3",
    )
    .bind(key)
    .bind(after)
    .bind(limit)
    .bind(patterns(paths))
    .fetch_all(pool)
    .await?;
    Ok(ids)
}

pub async fn write(
    pool: &PgPool,
    content_ids: &[i64],
    hits: Vec<Hit>,
    class: &str,
    method: &str,
    key: &str,
    version_id: i64,
) -> Result<()> {
    let mut tx = pool.begin().await?;
    sqlx::query(
        "delete from mentions where method = $2 and chunk_id in \
         (select id from chunks where content_id = any($1))",
    )
    .bind(content_ids)
    .bind(method)
    .execute(&mut *tx)
    .await?;
    let entities = hits
        .iter()
        .map(|hit| NewEntity {
            class: class.to_string(),
            key: hit.key.clone(),
            name: hit.name.clone(),
            dataset: hit.dataset.clone(),
            method: method.to_string(),
            confidence: hit.confidence,
            document_id: None,
            props: Default::default(),
        })
        .collect();
    let map = upsert_entities(&mut tx, entities, version_id).await?;
    let mut mentions = String::new();
    for hit in &hits {
        if let Some(id) = map.get(&(class.to_string(), hit.key.clone())) {
            mention_row(
                &mut mentions,
                *id,
                &Owner::Chunk(hit.chunk_id),
                hit.start,
                hit.end,
                method,
                hit.confidence,
            );
        }
    }
    copy(&mut tx, COPY_MENTIONS, mentions).await?;
    sqlx::query("update contents set persons_key = $2 where id = any($1)")
        .bind(content_ids)
        .bind(key)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn path_prefixes_become_escaped_like_patterns() {
        assert_eq!(patterns(&[]), ["%"]);
        let paths = ["N-1 a_b 50%".to_string(), r"x\y".to_string()];
        assert_eq!(patterns(&paths), [r"N-1 a\_b 50\%%", r"x\\y%"]);
    }
}
