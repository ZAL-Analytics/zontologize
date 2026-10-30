use anyhow::Result;
use sqlx::PgPool;

use super::copy_row;
use crate::ontology::Ontology;

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
        "insert into ontology_versions (sha256, files, loaded_run_id) values ($1, $2, $3) \
         returning id",
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
            ],
        );
    }
    if !data.is_empty() {
        let mut copy = tx
            .copy_in_raw(
                "copy properties (ontology_version_id, iri, kind, domain, range, parents, labels, \
                 selected) from stdin",
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
