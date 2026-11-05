use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::path::Path;

use anyhow::{Context, Result};
use sqlx::PgPool;

use super::TASK;
use super::check::{AMBIGUOUS_UNIT, UNKNOWN_UNIT};
use crate::config::Config;
use crate::db;
use crate::db::attributes::PropertyRow;
use crate::llm::shortlist::{local_name, pick};
use crate::ontology::Ontology;

pub const REPORT_FILE: &str = "attributes-report.md";
const LISTED_UNITS: usize = 50;

fn property_name(ontology: &Ontology, iri: &str, languages: &[String]) -> String {
    ontology
        .properties
        .get(iri)
        .and_then(|p| pick(&p.labels, languages))
        .unwrap_or_else(|| local_name(iri).to_string())
}

pub fn markdown(
    ontology: &Ontology,
    languages: &[String],
    header: &str,
    rows: &[PropertyRow],
    drops: &BTreeMap<String, i64>,
) -> String {
    let mut out = String::from("# Attributes report\n\n");
    if !header.is_empty() {
        let _ = writeln!(out, "{header}\n");
    }
    out.push_str("## Values per property and unit\n\n");
    out.push_str("| Property | Unit | Values | Chunks | Example |\n|---|---|---|---|---|\n");
    for row in rows {
        let unit = if row.unit.is_empty() {
            "-"
        } else {
            local_name(&row.unit)
        };
        let _ = writeln!(
            out,
            "| {} | {unit} | {} | {} | {} |",
            property_name(ontology, &row.property_iri, languages),
            row.values,
            row.chunks,
            row.example.replace('|', "\\|")
        );
    }
    let mut reasons: BTreeMap<&str, i64> = BTreeMap::new();
    let mut units: Vec<(i64, &str, &str)> = Vec::new();
    for (key, count) in drops {
        let (reason, about) = key.split_once('|').unwrap_or((key, ""));
        *reasons.entry(reason).or_default() += count;
        if reason == UNKNOWN_UNIT || reason == AMBIGUOUS_UNIT {
            units.push((*count, about, reason));
        }
    }
    out.push_str("\n## Dropped values\n\n| Reason | Count |\n|---|---|\n");
    for (reason, count) in &reasons {
        let _ = writeln!(out, "| {reason} | {count} |");
    }
    units.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(b.1)));
    out.push_str(
        "\n## Unit texts that matched no unit\n\nAdd them as `skos:altLabel` of the right unit in \
         the local ontology file.\n\n| Unit text | Reason | Count |\n|---|---|---|\n",
    );
    for (count, text, reason) in units.iter().take(LISTED_UNITS) {
        let _ = writeln!(
            out,
            "| `{}` | {reason} | {count} |",
            text.replace('|', "\\|")
        );
    }
    out
}

async fn header(pool: &PgPool, job: Option<i64>) -> Result<String> {
    let sent = db::attributes::sent_chunks(pool).await?;
    let mut header = format!("Chunks answered by the attributes model: {sent}.");
    let Some(job) = job.filter(|id| *id > 0) else {
        return Ok(header);
    };
    let jobs = db::llm::jobs(pool, i64::MAX).await?;
    if let Some(state) = jobs.into_iter().find(|j| j.id == job)
        && state.status != "finished"
    {
        let total = state.done + state.pending + state.invalid + state.error;
        let _ = write!(
            header,
            "\n\nPartial: job {job} is {}; {} of {total} chunks answered.",
            state.status, state.done
        );
    }
    Ok(header)
}

pub async fn write_report(
    pool: &PgPool,
    ontology: &Ontology,
    config: &Config,
    out: &Path,
    job: Option<i64>,
) -> Result<()> {
    let model = crate::llm::ModelName::parse(&config.models.attributes)
        .map_err(|e| anyhow::anyhow!("models.attributes: {e}"))?;
    let header = header(pool, job).await?;
    let rows = db::attributes::property_counts(pool).await?;
    let drops = db::llm::task_stats(pool, TASK, model.provider.as_str(), &model.model).await?;
    std::fs::create_dir_all(out).with_context(|| format!("cannot create {}", out.display()))?;
    let text = markdown(ontology, &config.ontology.languages, &header, &rows, &drops);
    let path = out.join(REPORT_FILE);
    std::fs::write(&path, text).with_context(|| format!("cannot write {}", path.display()))?;
    println!("attributes: report in {}", path.display());
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::super::tests::{iri, ontology};
    use super::*;

    #[test]
    fn report_lists_values_drops_and_unknown_units() {
        let onto = ontology();
        let rows = vec![PropertyRow {
            property_iri: iri("power"),
            unit: iri("kW"),
            values: 3,
            chunks: 2,
            example: "a/sheet.pdf p. 1".to_string(),
        }];
        let mut drops = BTreeMap::new();
        drops.insert(format!("{UNKNOWN_UNIT}|kWe"), 4);
        drops.insert(format!("{UNKNOWN_UNIT}|bar a"), 2);
        drops.insert("no unit|http://example.org/ex#power".to_string(), 1);
        let languages = vec!["en".to_string()];
        let text = markdown(&onto, &languages, "", &rows, &drops);
        assert!(
            text.contains("| power | kW | 3 | 2 | a/sheet.pdf p. 1 |"),
            "{text}"
        );
        assert!(text.contains("| unknown unit | 6 |"), "{text}");
        assert!(text.contains("| no unit | 1 |"), "{text}");
        let first = text.find("`kWe`").unwrap();
        let second = text.find("`bar a`").unwrap();
        assert!(first < second);
    }
}
