use anyhow::Result;
use sqlx::PgConnection;

#[derive(Debug, Clone, PartialEq)]
pub struct Override {
    pub entity_id: i64,
    pub class_iri: Option<String>,
    pub name: Option<String>,
    pub method: String,
    pub status: String,
    pub confidence: f64,
    pub ontology_version_id: i64,
}

pub async fn upsert(conn: &mut PgConnection, rows: &[Override]) -> Result<u64> {
    let mut written = 0;
    for row in rows {
        written += sqlx::query(
            "insert into entity_classes (entity_id, class_iri, name, method, status, confidence, \
             ontology_version_id) values ($1, $2, $3, $4, $5, $6, $7) \
             on conflict (entity_id) do update set class_iri = excluded.class_iri, \
             name = excluded.name, method = excluded.method, status = excluded.status, \
             confidence = excluded.confidence, ontology_version_id = excluded.ontology_version_id, \
             created_at = now() \
             where entity_classes.status not in ('confirmed', 'rejected')",
        )
        .bind(row.entity_id)
        .bind(&row.class_iri)
        .bind(&row.name)
        .bind(&row.method)
        .bind(&row.status)
        .bind(row.confidence)
        .bind(row.ontology_version_id)
        .execute(&mut *conn)
        .await?
        .rows_affected();
    }
    Ok(written)
}

pub async fn delete(
    conn: &mut PgConnection,
    entity_ids: &[i64],
    method_prefix: &str,
) -> Result<u64> {
    let deleted = sqlx::query(
        "delete from entity_classes where entity_id = any($1) and starts_with(method, $2) \
         and status in ('auto', 'proposed')",
    )
    .bind(entity_ids)
    .bind(method_prefix)
    .execute(&mut *conn)
    .await?
    .rows_affected();
    Ok(deleted)
}

pub async fn set_status(conn: &mut PgConnection, entity_id: i64, status: &str) -> Result<u64> {
    let updated = sqlx::query(
        "update entity_classes set status = $2 where entity_id = $1 and status = 'proposed'",
    )
    .bind(entity_id)
    .bind(status)
    .execute(&mut *conn)
    .await?
    .rows_affected();
    Ok(updated)
}
