use anyhow::Result;
use sqlx::PgPool;

use super::copy_row;
use crate::ontology::Ontology;

pub const CURRENT: &str = "select v.id from ontology_versions v join runs r on r.id = v.last_run_id \
     where r.status = 'finished' order by r.id desc limit 1";

pub async fn current(pool: &PgPool) -> Result<Option<i64>> {
    let id = sqlx::query_scalar(CURRENT).fetch_optional(pool).await?;
    Ok(id)
}

pub async fn mark_used(pool: &PgPool, ontology: &Ontology, run_id: i64) -> Result<()> {
    sqlx::query("update ontology_versions set last_run_id = $2 where sha256 = $1")
        .bind(&ontology.version)
        .bind(run_id)
        .execute(pool)
        .await?;
    Ok(())
}

pub async fn store(pool: &PgPool, ontology: &Ontology, run_id: i64) -> Result<(i64, bool)> {
    let existing: Option<i64> =
        sqlx::query_scalar("select id from ontology_versions where sha256 = $1")
            .bind(&ontology.version)
            .fetch_optional(pool)
            .await?;
    if let Some(id) = existing {
        return Ok((id, false));
    }
    let mut tx = pool.begin().await?;
    let id: i64 = sqlx::query_scalar(
        "insert into ontology_versions (sha256, files, loaded_run_id) \
         values ($1, $2, $3) returning id",
    )
    .bind(&ontology.version)
    .bind(sqlx::types::Json(&ontology.files))
    .bind(run_id)
    .fetch_one(&mut *tx)
    .await?;
    let version = id.to_string();
    let mut data = String::new();
    for class in ontology.classes.values() {
        let parents = array(&class.parents);
        let types = array(&class.types);
        let labels = serde_json::to_string(&class.labels)?;
        let alt_labels = serde_json::to_string(&class.alt_labels)?;
        let definition = serde_json::to_string(&class.definition)?;
        copy_row(
            &mut data,
            &[
                Some(&version),
                Some(&class.iri),
                Some(&parents),
                Some(&types),
                Some(&labels),
                Some(&alt_labels),
                Some(&definition),
                Some(if class.selected { "t" } else { "f" }),
            ],
        );
    }
    if !data.is_empty() {
        let mut copy = tx
            .copy_in_raw(
                "copy classes (ontology_version_id, iri, parents, types, labels, alt_labels, \
                 definition, selected) from stdin",
            )
            .await?;
        copy.send(data.into_bytes()).await?;
        copy.finish().await?;
    }
    let mut data = String::new();
    for property in ontology.properties.values() {
        let domain = array(&property.domain);
        let range = array(&property.range);
        let parents = array(&property.parents);
        let labels = serde_json::to_string(&property.labels)?;
        let alt_labels = serde_json::to_string(&property.alt_labels)?;
        let tolerance = property.tolerance.map(|t| t.to_string());
        copy_row(
            &mut data,
            &[
                Some(&version),
                Some(&property.iri),
                Some(property.kind.as_str()),
                Some(&domain),
                Some(&range),
                Some(&parents),
                Some(&labels),
                Some(if property.selected { "t" } else { "f" }),
                Some(&alt_labels),
                property.value.map(|v| v.as_str()),
                property.quantity_kind.as_deref(),
                property.dimension.as_deref(),
                tolerance.as_deref(),
            ],
        );
    }
    if !data.is_empty() {
        let mut copy = tx
            .copy_in_raw(
                "copy properties (ontology_version_id, iri, kind, domain, range, parents, labels, \
                 selected, alt_labels, value_kind, quantity_kind, dimension, tolerance) from stdin",
            )
            .await?;
        copy.send(data.into_bytes()).await?;
        copy.finish().await?;
    }
    let mut data = String::new();
    for unit in ontology.units.values() {
        let symbols = array(&unit.symbols);
        let labels = serde_json::to_string(&unit.labels)?;
        let alt_labels = serde_json::to_string(&unit.alt_labels)?;
        let multiplier = unit.multiplier.to_string();
        let offset = unit.offset.to_string();
        copy_row(
            &mut data,
            &[
                Some(&version),
                Some(&unit.iri),
                Some(&symbols),
                Some(&labels),
                Some(&alt_labels),
                Some(&unit.dimension),
                Some(&multiplier),
                Some(&offset),
            ],
        );
    }
    if !data.is_empty() {
        let mut copy = tx
            .copy_in_raw(
                "copy units (ontology_version_id, iri, symbols, labels, alt_labels, dimension, \
                 conversion_multiplier, conversion_offset) from stdin",
            )
            .await?;
        copy.send(data.into_bytes()).await?;
        copy.finish().await?;
    }
    tx.commit().await?;
    Ok((id, true))
}

fn array(items: &[String]) -> String {
    let quoted: Vec<String> = items
        .iter()
        .map(|item| format!("\"{}\"", item.replace('\\', "\\\\").replace('"', "\\\"")))
        .collect();
    format!("{{{}}}", quoted.join(","))
}

#[derive(sqlx::FromRow)]
struct ClassRow {
    iri: String,
    parents: Vec<String>,
    types: Vec<String>,
    labels: sqlx::types::Json<crate::ontology::LangMap<String>>,
    alt_labels: sqlx::types::Json<crate::ontology::LangMap<Vec<String>>>,
    definition: sqlx::types::Json<crate::ontology::LangMap<String>>,
}

#[derive(sqlx::FromRow)]
struct PropertyRow {
    iri: String,
    kind: String,
    domain: Vec<String>,
    range: Vec<String>,
    parents: Vec<String>,
    labels: sqlx::types::Json<crate::ontology::LangMap<String>>,
    alt_labels: sqlx::types::Json<crate::ontology::LangMap<Vec<String>>>,
    value_kind: Option<String>,
    quantity_kind: Option<String>,
    dimension: Option<String>,
    tolerance: Option<f64>,
}

#[derive(sqlx::FromRow)]
struct UnitRow {
    iri: String,
    symbols: Vec<String>,
    labels: sqlx::types::Json<crate::ontology::LangMap<String>>,
    alt_labels: sqlx::types::Json<crate::ontology::LangMap<Vec<String>>>,
    dimension: String,
    conversion_multiplier: f64,
    conversion_offset: f64,
}

pub struct Stored {
    pub sha256: String,
    pub classes: Vec<crate::ontology::Class>,
    pub properties: Vec<crate::ontology::Property>,
    pub units: Vec<crate::ontology::Unit>,
}

pub async fn selected(pool: &PgPool, version_id: i64) -> Result<Stored> {
    let sha256 = sqlx::query_scalar("select sha256 from ontology_versions where id = $1")
        .bind(version_id)
        .fetch_one(pool)
        .await?;
    let classes: Vec<ClassRow> = sqlx::query_as(
        "select iri, parents, types, labels, alt_labels, definition from classes \
         where ontology_version_id = $1 and selected order by iri",
    )
    .bind(version_id)
    .fetch_all(pool)
    .await?;
    let properties: Vec<PropertyRow> = sqlx::query_as(
        "select iri, kind, domain, range, parents, labels, alt_labels, value_kind, quantity_kind, \
         dimension, tolerance from properties \
         where ontology_version_id = $1 and selected order by iri",
    )
    .bind(version_id)
    .fetch_all(pool)
    .await?;
    let classes = classes
        .into_iter()
        .map(|row| crate::ontology::Class {
            iri: row.iri,
            parents: row.parents,
            types: row.types,
            labels: row.labels.0,
            alt_labels: row.alt_labels.0,
            definition: row.definition.0,
            selected: true,
            meta: false,
        })
        .collect();
    let mut out = Vec::new();
    for row in properties {
        let kind = crate::ontology::PropertyKind::parse(&row.kind)
            .ok_or_else(|| anyhow::anyhow!("property {} has unknown kind {}", row.iri, row.kind))?;
        out.push(crate::ontology::Property {
            domain: row.domain,
            range: row.range,
            parents: row.parents,
            labels: row.labels.0,
            alt_labels: row.alt_labels.0,
            value: row
                .value_kind
                .as_deref()
                .and_then(crate::ontology::ValueKind::parse),
            quantity_kind: row.quantity_kind,
            dimension: row.dimension,
            tolerance: row.tolerance,
            selected: true,
            ..crate::ontology::Property::new(&row.iri, kind)
        });
    }
    let units: Vec<UnitRow> = sqlx::query_as(
        "select iri, symbols, labels, alt_labels, dimension, conversion_multiplier, \
         conversion_offset from units u where ontology_version_id = $1 and exists \
         (select 1 from properties p where p.ontology_version_id = $1 and p.selected \
         and p.dimension = u.dimension) order by iri",
    )
    .bind(version_id)
    .fetch_all(pool)
    .await?;
    let units = units
        .into_iter()
        .map(|row| crate::ontology::Unit {
            iri: row.iri,
            symbols: row.symbols,
            labels: row.labels.0,
            alt_labels: row.alt_labels.0,
            dimension: row.dimension,
            quantity_kinds: Vec::new(),
            multiplier: row.conversion_multiplier,
            offset: row.conversion_offset,
        })
        .collect();
    Ok(Stored {
        sha256,
        classes,
        properties: out,
        units,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn array_literal_quotes_items() {
        assert_eq!(array(&[]), "{}");
        assert_eq!(
            array(&["a".to_string(), "b\"c".to_string()]),
            "{\"a\",\"b\\\"c\"}"
        );
    }
}
