use anyhow::{Context, Result, bail};
use sqlx::PgConnection;
use sqlx::PgPool;

use super::{identifier, quote_identifier};
use crate::graph::{Kind, Label, Stats};

pub const AGE_VERSION: &str = "1.8";

pub fn check_age(found: Option<&str>) -> Result<()> {
    let Some(found) = found else {
        bail!("the AGE extension is not installed in this database");
    };
    let major_minor: Vec<&str> = found.split('.').take(2).collect();
    if major_minor.join(".") != AGE_VERSION {
        bail!(
            "AGE {found} is installed, but graph sync writes AGE's own tables and is tested \
             with AGE {AGE_VERSION}.x only; install AGE {AGE_VERSION} or a zontologize \
             version made for AGE {found}"
        );
    }
    Ok(())
}

pub async fn stored_labels(pool: &PgPool) -> Result<Vec<Label>> {
    let rows: Vec<(String, String, String)> =
        sqlx::query_as("select kind, iri, label from graph_labels")
            .fetch_all(pool)
            .await?;
    Ok(rows
        .into_iter()
        .map(|(kind, iri, name)| Label {
            kind: if kind == Kind::Vertex.as_str() {
                Kind::Vertex
            } else {
                Kind::Edge
            },
            iri,
            name,
        })
        .collect())
}

pub async fn fingerprint(pool: &PgPool) -> Result<String> {
    let text = sqlx::query_scalar(
        "select (select count(*) || '.' || coalesce(sum(hashtextextended(row(id, class_iri, key, \
         name, canonical_id, dataset, document_id)::text, 0)), 0) from entities) || ':' || \
         (select count(*) || '.' || coalesce(sum(hashtextextended(row(id, src_id, property_iri, \
         dst_id, status, confidence, method, document_id, chunk_id, span_start, span_end)::text, \
         0)), 0) from relations where status <> 'rejected')",
    )
    .fetch_one(pool)
    .await?;
    Ok(text)
}

pub async fn last_sync(pool: &PgPool) -> Result<Option<Stats>> {
    let row: Option<sqlx::types::Json<Stats>> = sqlx::query_scalar(
        "select stats -> 'graph' from runs where status = 'finished' and stats ? 'graph' \
         order by id desc limit 1",
    )
    .fetch_optional(pool)
    .await?;
    Ok(row.map(|json| json.0))
}

pub async fn exists(pool: &PgPool, graph: &str) -> Result<bool> {
    let found =
        sqlx::query_scalar("select exists(select 1 from ag_catalog.ag_graph where name = $1)")
            .bind(graph)
            .fetch_one(pool)
            .await?;
    Ok(found)
}

pub async fn classes(pool: &PgPool, language: &str) -> Result<Vec<(String, Option<String>)>> {
    let rows = sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "select e.class_iri, coalesce(c.labels ->> $1, \
         (select value from jsonb_each_text(c.labels) order by key limit 1)) \
         from (select distinct class_iri from entities) e \
         left join classes c on c.iri = e.class_iri and c.ontology_version_id = ({}) \
         order by 1",
        super::ontology::CURRENT
    )))
    .bind(language)
    .fetch_all(pool)
    .await?;
    Ok(rows)
}

pub async fn properties(pool: &PgPool, language: &str) -> Result<Vec<(String, Option<String>)>> {
    let rows = sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "select r.property_iri, coalesce(p.labels ->> $1, \
         (select value from jsonb_each_text(p.labels) order by key limit 1)) \
         from (select distinct property_iri from relations where status <> 'rejected') r \
         left join properties p on p.iri = r.property_iri and p.ontology_version_id = ({}) \
         order by 1",
        super::ontology::CURRENT
    )))
    .bind(language)
    .fetch_all(pool)
    .await?;
    Ok(rows)
}

async fn load_age(conn: &mut PgConnection) -> Result<()> {
    if let Err(error) = sqlx::query("load 'age'").execute(&mut *conn).await {
        bail!(
            "cannot load AGE ({error}); a superuser has to add 'age' to \
             session_preload_libraries for this role or database, for example:\n  \
             alter role <role> set session_preload_libraries = 'age';"
        );
    }
    Ok(())
}

async fn create_graph(conn: &mut PgConnection, graph: &str) -> Result<()> {
    let exists: bool =
        sqlx::query_scalar("select exists(select 1 from ag_catalog.ag_graph where name = $1)")
            .bind(graph)
            .fetch_one(&mut *conn)
            .await?;
    if exists {
        return Ok(());
    }
    let created = sqlx::query("select ag_catalog.create_graph($1)")
        .bind(graph)
        .execute(&mut *conn)
        .await;
    if let Err(error) = created {
        let denied = error
            .as_database_error()
            .is_some_and(|e| e.code().as_deref() == Some("42501"));
        if !denied {
            return Err(error).with_context(|| format!("cannot create graph {graph}"));
        }
        bail!(
            "cannot create graph {graph} ({error}); a database owner or superuser has to run:\n  \
             select ag_catalog.create_graph('{graph}');\n  \
             alter schema {graph} owner to <role>;\n  \
             grant usage on schema ag_catalog to <role>;\n  \
             grant select, insert, update, delete on ag_catalog.ag_label to <role>;"
        );
    }
    Ok(())
}

async fn existing(conn: &mut PgConnection, graph: &str) -> Result<Vec<(String, String)>> {
    let rows = sqlx::query_as(
        "select l.name::text, l.kind::text from ag_catalog.ag_label l \
         join ag_catalog.ag_graph g on g.graphid = l.graph \
         where g.name = $1 and l.name::text not like '\\_ag\\_label\\_%'",
    )
    .bind(graph)
    .fetch_all(&mut *conn)
    .await?;
    Ok(rows)
}

async fn label_id(conn: &mut PgConnection, graph: &str, name: &str) -> Result<i32> {
    let id = sqlx::query_scalar("select ag_catalog._label_id($1, $2)::integer")
        .bind(graph)
        .bind(name)
        .fetch_one(&mut *conn)
        .await
        .with_context(|| format!("label {name} in graph {graph}"))?;
    Ok(id)
}

fn age_kind(kind: Kind) -> &'static str {
    match kind {
        Kind::Vertex => "v",
        Kind::Edge => "e",
    }
}

async fn ensure_labels(conn: &mut PgConnection, graph: &str, labels: &[Label]) -> Result<()> {
    let have = existing(conn, graph).await?;
    for (name, kind) in &have {
        let wanted = labels
            .iter()
            .any(|l| &l.name == name && age_kind(l.kind) == kind);
        if !wanted {
            sqlx::query("select ag_catalog.drop_label($1, $2, true)")
                .bind(graph)
                .bind(name)
                .execute(&mut *conn)
                .await?;
        }
    }
    for label in labels {
        let present = have
            .iter()
            .any(|(name, kind)| name == &label.name && kind == age_kind(label.kind));
        if present {
            continue;
        }
        let create = match label.kind {
            Kind::Vertex => "select ag_catalog.create_vlabel($1::text::cstring, $2::text::cstring)",
            Kind::Edge => "select ag_catalog.create_elabel($1::text::cstring, $2::text::cstring)",
        };
        sqlx::query(create)
            .bind(graph)
            .bind(&label.name)
            .execute(&mut *conn)
            .await
            .with_context(|| format!("cannot create label {} in graph {graph}", label.name))?;
    }
    Ok(())
}

pub async fn rebuild(
    conn: &mut PgConnection,
    schema: &str,
    graph: &str,
    labels: &[Label],
) -> Result<(u64, u64)> {
    load_age(conn).await?;
    check_age(super::extension_version(conn, "age").await?.as_deref())?;
    let g = identifier(graph)?;
    let s = identifier(schema)?;
    let mut tx = sqlx::Connection::begin(&mut *conn).await?;
    sqlx::query(
        "select set_config('search_path', 'ag_catalog, ' || current_setting('search_path'), true)",
    )
    .execute(&mut *tx)
    .await?;
    create_graph(&mut tx, graph).await?;
    ensure_labels(&mut tx, graph, labels).await?;
    sqlx::raw_sql(sqlx::AssertSqlSafe(format!(
        "delete from {g}._ag_label_edge; delete from {g}._ag_label_vertex;"
    )))
    .execute(&mut *tx)
    .await?;
    let mut iris = Vec::new();
    let mut ids = Vec::new();
    for label in labels.iter().filter(|l| l.kind == Kind::Vertex) {
        iris.push(label.iri.clone());
        ids.push(label_id(&mut tx, graph, &label.name).await?);
    }
    let mut vertices = 0;
    for (label, &id) in labels.iter().filter(|l| l.kind == Kind::Vertex).zip(&ids) {
        let table = format!("{g}.{}", quote_identifier(&label.name));
        let index = quote_identifier(&format!("v{id}_properties"));
        sqlx::query(sqlx::AssertSqlSafe(format!(
            "drop index if exists {g}.{index}"
        )))
        .execute(&mut *tx)
        .await?;
        let done = sqlx::query(sqlx::AssertSqlSafe(format!(
            "insert into {table} (id, properties) \
             select ag_catalog._graphid($1, e.id), jsonb_build_object('id', e.id, \
             'class', e.class_iri, 'key', e.key, 'name', e.name, \
             'canonical_id', e.canonical_id, 'dataset', e.dataset, \
             'document_id', e.document_id)::text::ag_catalog.agtype \
             from {s}.entities e where e.class_iri = $2"
        )))
        .bind(id)
        .bind(&label.iri)
        .execute(&mut *tx)
        .await?;
        vertices += done.rows_affected();
        set_sequence(&mut tx, graph, &label.name, &format!("{s}.entities")).await?;
        sqlx::query(sqlx::AssertSqlSafe(format!(
            "create index {index} on {table} using gin (properties)"
        )))
        .execute(&mut *tx)
        .await?;
    }
    let mut edges = 0;
    for label in labels.iter().filter(|l| l.kind == Kind::Edge) {
        let id = label_id(&mut tx, graph, &label.name).await?;
        let table = format!("{g}.{}", quote_identifier(&label.name));
        let done = sqlx::query(sqlx::AssertSqlSafe(format!(
            "with m as (select * from unnest($3::text[], $4::integer[]) as m(iri, label_id)) \
             insert into {table} (id, start_id, end_id, properties) \
             select ag_catalog._graphid($1, r.id), \
             ag_catalog._graphid(ms.label_id, r.src_id), ag_catalog._graphid(md.label_id, r.dst_id), \
             jsonb_build_object('id', r.id, 'iri', r.property_iri, 'status', r.status, \
             'confidence', r.confidence, 'method', r.method, 'document_id', r.document_id, \
             'chunk_id', r.chunk_id, 'span_start', r.span_start, \
             'span_end', r.span_end)::text::ag_catalog.agtype \
             from {s}.relations r \
             join {s}.entities a on a.id = r.src_id join m ms on ms.iri = a.class_iri \
             join {s}.entities b on b.id = r.dst_id join m md on md.iri = b.class_iri \
             where r.property_iri = $2 and r.status <> 'rejected'"
        )))
        .bind(id)
        .bind(&label.iri)
        .bind(&iris)
        .bind(&ids)
        .execute(&mut *tx)
        .await?;
        edges += done.rows_affected();
        set_sequence(&mut tx, graph, &label.name, &format!("{s}.relations")).await?;
    }
    sqlx::query(sqlx::AssertSqlSafe(format!("delete from {s}.graph_labels")))
        .execute(&mut *tx)
        .await?;
    let kinds: Vec<&str> = labels.iter().map(|l| l.kind.as_str()).collect();
    let label_iris: Vec<&str> = labels.iter().map(|l| l.iri.as_str()).collect();
    let names: Vec<&str> = labels.iter().map(|l| l.name.as_str()).collect();
    sqlx::query(sqlx::AssertSqlSafe(format!(
        "insert into {s}.graph_labels (kind, iri, label) \
         select * from unnest($1::text[], $2::text[], $3::text[])"
    )))
    .bind(&kinds)
    .bind(&label_iris)
    .bind(&names)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok((vertices, edges))
}

async fn set_sequence(
    conn: &mut PgConnection,
    graph: &str,
    label: &str,
    table: &str,
) -> Result<()> {
    sqlx::query(sqlx::AssertSqlSafe(format!(
        "select setval(format('%I.%I', $1::text, l.seq_name), \
         greatest((select max(id) from {table}), 1)) \
         from ag_catalog.ag_label l join ag_catalog.ag_graph g on g.graphid = l.graph \
         where g.name = $1 and l.name = $2"
    )))
    .bind(graph)
    .bind(label)
    .execute(&mut *conn)
    .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_the_pinned_age_minor_version_is_accepted() {
        assert!(check_age(Some("1.8.0")).is_ok());
        assert!(check_age(Some("1.8.1")).is_ok());
        let error = check_age(Some("1.9.0")).unwrap_err().to_string();
        assert!(error.contains("AGE 1.9.0 is installed"), "{error}");
        assert!(check_age(Some("1.80.0")).is_err());
        assert!(check_age(Some("2.8.0")).is_err());
        assert!(check_age(None).is_err());
    }
}
