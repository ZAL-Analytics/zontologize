use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::path::Path;

use anyhow::{Context, Result};
use sqlx::PgPool;

use super::TASK;
use crate::config::Config;
use crate::db;
use crate::db::curate::OverrideRow;
use crate::ns::local_name;

pub const REPORT_FILE: &str = "curate-report.md";
const LISTED: usize = 200;

pub struct Totals {
    pub header: String,
    pub links: Vec<(String, i64)>,
    pub rows: Vec<OverrideRow>,
    pub skipped: usize,
    pub sources: Vec<(String, usize)>,
    pub drops: BTreeMap<String, i64>,
}

fn cell(text: &str) -> String {
    text.replace('|', "\\|")
}

fn table(out: &mut String, title: &str, head: &str, lines: &[String]) {
    let _ = writeln!(out, "\n## {title} ({})\n", lines.len());
    let columns = head.matches('|').count() - 1;
    let _ = writeln!(out, "{head}\n|{}", "---|".repeat(columns));
    for line in lines.iter().take(LISTED) {
        let _ = writeln!(out, "{line}");
    }
}

pub fn markdown(totals: &Totals) -> String {
    let mut out = String::from("# Curate report\n\n");
    if !totals.header.is_empty() {
        let _ = writeln!(out, "{}\n", totals.header);
    }
    let _ = writeln!(
        out,
        "Clusters larger than `[curate] max_members`, not sent: {}.",
        totals.skipped
    );
    out.push_str("\n## Clusters per source\n\n| Source | Clusters |\n|---|---|\n");
    for (source, count) in &totals.sources {
        let _ = writeln!(out, "| {source} | {count} |");
    }
    out.push_str("\n## Same-as links written\n\n| Status | Links |\n|---|---|\n");
    for (status, count) in &totals.links {
        let _ = writeln!(out, "| {status} | {count} |");
    }
    let mut classes = Vec::new();
    let mut names = Vec::new();
    let mut not_entities = Vec::new();
    for row in &totals.rows {
        let name = cell(&row.name);
        let old = local_name(&row.class_iri);
        match &row.new_class {
            None => not_entities.push(format!("| {name} | {old} | {} |", row.status)),
            Some(class) if *class != row.class_iri => classes.push(format!(
                "| {name} | {old} | {} | {} |",
                local_name(class),
                row.status
            )),
            Some(_) => {}
        }
        if let Some(new) = row.new_name.as_ref().filter(|n| **n != row.name) {
            names.push(format!("| {name} | {} | {} |", cell(new), row.status));
        }
    }
    table(
        &mut out,
        "Class changes",
        "| Entity | Class | New class | Status |",
        &classes,
    );
    table(&mut out, "Names set", "| Entity | Name | Status |", &names);
    table(
        &mut out,
        "Not an entity",
        "| Entity | Class | Status |",
        &not_entities,
    );
    let mut reasons: BTreeMap<&str, i64> = BTreeMap::new();
    for (key, count) in &totals.drops {
        let (reason, _) = key.split_once('|').unwrap_or((key, ""));
        *reasons.entry(reason).or_default() += count;
    }
    out.push_str("\n## Dropped decisions\n\n| Reason | Count |\n|---|---|\n");
    for (reason, count) in &reasons {
        let _ = writeln!(out, "| {reason} | {count} |");
    }
    out
}

async fn header(pool: &PgPool, job: Option<i64>) -> Result<String> {
    let Some(job) = job.filter(|id| *id > 0) else {
        return Ok(String::new());
    };
    let (calls, cost) = db::curate::calls(pool, job).await?;
    let mut header = format!("Job {job}: {calls} calls, {cost:.4} USD.");
    let jobs = db::llm::jobs(pool, i64::MAX).await?;
    if let Some(state) = jobs.into_iter().find(|j| j.id == job)
        && state.status != "finished"
    {
        let total = state.done + state.pending + state.invalid + state.error;
        let _ = write!(
            header,
            "\n\nPartial: job {job} is {}; {} of {total} clusters answered.",
            state.status, state.done
        );
    }
    Ok(header)
}

pub async fn write_report(
    pool: &PgPool,
    config: &Config,
    out: &Path,
    skipped: usize,
    sources: &[(&str, usize)],
    job: Option<i64>,
) -> Result<()> {
    let model = crate::llm::ModelName::parse(&config.models.curate)
        .map_err(|e| anyhow::anyhow!("models.curate: {e}"))?;
    let totals = Totals {
        header: header(pool, job).await?,
        links: db::curate::link_counts(pool).await?,
        rows: db::curate::overrides(pool).await?,
        skipped,
        sources: sources.iter().map(|(s, n)| (s.to_string(), *n)).collect(),
        drops: db::llm::task_stats(pool, TASK, model.provider.as_str(), &model.model).await?,
    };
    std::fs::create_dir_all(out).with_context(|| format!("cannot create {}", out.display()))?;
    let path = out.join(REPORT_FILE);
    std::fs::write(&path, markdown(&totals))
        .with_context(|| format!("cannot write {}", path.display()))?;
    println!("curate: report in {}", path.display());
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(
        name: &str,
        class: &str,
        new_class: Option<&str>,
        new_name: Option<&str>,
    ) -> OverrideRow {
        OverrideRow {
            entity_id: 1,
            name: name.to_string(),
            class_iri: format!("http://example.org/ex#{class}"),
            new_class: new_class.map(|c| format!("http://example.org/ex#{c}")),
            new_name: new_name.map(str::to_string),
            status: "auto".to_string(),
        }
    }

    #[test]
    fn report_lists_links_changes_names_and_drops() {
        let mut drops = BTreeMap::new();
        drops.insert("unknown ref|m9".to_string(), 2);
        drops.insert("unknown ref|m8".to_string(), 1);
        let totals = Totals {
            header: "Job 3: 2 calls, 0.0000 USD.".to_string(),
            links: vec![("auto".to_string(), 1)],
            rows: vec![
                row(
                    "Zuidwal Technick",
                    "Customer",
                    Some("Customer"),
                    Some("Zuidwal Techniek"),
                ),
                row("Acme", "Site", Some("Customer"), None),
                row("engineer", "Person", None, None),
            ],
            skipped: 4,
            sources: vec![("name pattern".to_string(), 2)],
            drops,
        };
        let text = markdown(&totals);
        assert!(text.contains("Job 3: 2 calls"), "{text}");
        assert!(text.contains("not sent: 4."), "{text}");
        assert!(text.contains("| name pattern | 2 |"), "{text}");
        assert!(text.contains("| auto | 1 |"), "{text}");
        assert!(text.contains("## Class changes (1)"), "{text}");
        assert!(text.contains("| Acme | Site | Customer | auto |"), "{text}");
        assert!(
            text.contains("| Zuidwal Technick | Zuidwal Techniek | auto |"),
            "{text}"
        );
        assert!(text.contains("| engineer | Person | auto |"), "{text}");
        assert!(text.contains("| unknown ref | 3 |"), "{text}");
    }
}
