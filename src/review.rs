use std::collections::HashMap;
use std::path::Path;

use anyhow::Result;
use tokio::io::{AsyncBufRead, AsyncBufReadExt};

use crate::config;
use crate::db;
use crate::db::review::{Fact, Precision};
use crate::llm::shortlist::local_name;

pub const CONTEXT_CHARS: usize = 100;
const WILSON_Z: f64 = 1.96;

pub struct Options {
    pub n: Option<usize>,
    pub method: String,
    pub class: String,
    pub seed: String,
}

#[derive(Debug, Default, PartialEq)]
pub struct Tally {
    pub right: usize,
    pub wrong: usize,
    pub skipped: usize,
}

pub fn context(text: &str, start: Option<i32>, end: Option<i32>) -> String {
    let chars: Vec<char> = text.chars().collect();
    let (Some(start), Some(end)) = (start, end) else {
        return chars.iter().take(2 * CONTEXT_CHARS).collect::<String>();
    };
    let start = (start.max(0) as usize).min(chars.len());
    let end = (end.max(0) as usize).clamp(start, chars.len());
    let before: String = chars[start.saturating_sub(CONTEXT_CHARS)..start]
        .iter()
        .collect();
    let found: String = chars[start..end].iter().collect();
    let after: String = chars[end..(end + CONTEXT_CHARS).min(chars.len())]
        .iter()
        .collect();
    let flat = |s: String| s.split_whitespace().collect::<Vec<_>>().join(" ");
    format!("{} «{}» {}", flat(before), flat(found), flat(after))
        .trim()
        .to_string()
}

pub fn describe(fact: &Fact) -> String {
    let page = fact
        .page_from
        .map_or(String::new(), |p| format!(", page {p}"));
    let what = match (&fact.property_iri, &fact.object) {
        (Some(property), Some(object)) => format!(
            "{} ({}) {} {}",
            fact.subject,
            local_name(&fact.class_iri),
            local_name(property),
            object
        ),
        _ => format!("{} is a {}", fact.subject, local_name(&fact.class_iri)),
    };
    format!(
        "{} {}: {what}\n  method {}, confidence {:.2}\n  {}{page}\n  {}",
        fact.kind,
        fact.id,
        fact.method,
        fact.confidence,
        fact.path,
        context(&fact.text, fact.span_start, fact.span_end)
    )
}

pub fn wilson_lower(right: i64, total: i64) -> f64 {
    if total <= 0 {
        return 0.0;
    }
    let n = total as f64;
    let p = right as f64 / n;
    let z2 = WILSON_Z * WILSON_Z;
    let centre = p + z2 / (2.0 * n);
    let margin = WILSON_Z * (p * (1.0 - p) / n + z2 / (4.0 * n * n)).sqrt();
    ((centre - margin) / (1.0 + z2 / n)).clamp(0.0, 1.0)
}

pub fn measured(rows: &[Precision], method: &str, min_verdicts: usize) -> HashMap<String, f64> {
    if min_verdicts == 0 {
        return HashMap::new();
    }
    rows.iter()
        .filter(|r| r.method == method && r.total >= min_verdicts as i64)
        .map(|r| (r.iri.clone(), wilson_lower(r.right, r.total)))
        .collect()
}

pub async fn ask<R: AsyncBufRead + Unpin>(
    pool: &sqlx::PgPool,
    facts: &[Fact],
    input: &mut R,
) -> Result<Tally> {
    let mut tally = Tally::default();
    let mut lines = input.lines();
    for (i, fact) in facts.iter().enumerate() {
        let snapshot = describe(fact);
        println!("[{}/{}] {snapshot}", i + 1, facts.len());
        println!("right? y = right, n = wrong, s = skip, q = quit");
        let Some(line) = lines.next_line().await? else {
            break;
        };
        match line.trim().to_lowercase().as_str() {
            "y" | "yes" => {
                db::review::record(pool, fact, "right", &snapshot).await?;
                tally.right += 1;
            }
            "n" | "no" => {
                db::review::record(pool, fact, "wrong", &snapshot).await?;
                tally.wrong += 1;
            }
            "q" | "quit" => break,
            _ => tally.skipped += 1,
        }
    }
    Ok(tally)
}

pub async fn sample<R: AsyncBufRead + Unpin>(
    config_dir: Option<&Path>,
    url: Option<&str>,
    options: &Options,
    input: &mut R,
) -> Result<Tally> {
    let config = config::load(config_dir)?;
    let iri = if options.class.is_empty() {
        String::new()
    } else {
        let loaded = crate::ontology::load(config_dir, &config.ontology)?;
        let ontology = loaded.ontology;
        ontology
            .resolve(&options.class)
            .map_err(|e| anyhow::anyhow!("--class: {e}"))?
    };
    let pool = db::pool::connect(url, &config.database.schema, 2).await?;
    let n = options.n.unwrap_or(config.review.sample);
    let result = async {
        let facts =
            db::review::sample(&pool, &options.method, &iri, &options.seed, n as i64).await?;
        if facts.is_empty() {
            println!("review: no facts left to review for this filter");
            return Ok(Tally::default());
        }
        let tally = ask(&pool, &facts, input).await?;
        println!(
            "review: {} right, {} wrong, {} skipped",
            tally.right, tally.wrong, tally.skipped
        );
        Ok(tally)
    }
    .await;
    pool.close().await;
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn context_marks_the_span_and_cuts_around_it() {
        let text = format!("{}The booster pump\nruns.", "x".repeat(150));
        let found = context(&text, Some(154), Some(166));
        assert!(found.ends_with("«booster pump» runs."), "{found}");
        assert_eq!(found.chars().filter(|c| *c == 'x').count(), 96);
        assert_eq!(context("a  b", None, None), "a  b");
        assert_eq!(context("short", Some(2), Some(99)), "sh «ort»");
    }

    #[test]
    fn wilson_lower_bound_grows_with_evidence() {
        assert_eq!(wilson_lower(0, 0), 0.0);
        let few = wilson_lower(9, 10);
        let many = wilson_lower(90, 100);
        assert!(few < many && many < 0.9, "{few} {many}");
        assert!((wilson_lower(90, 100) - 0.8256).abs() < 0.001);
    }

    #[test]
    fn measured_needs_enough_verdicts_for_the_method() {
        let rows = vec![
            Precision {
                iri: "a".to_string(),
                method: "llm:m".to_string(),
                right: 30,
                total: 30,
            },
            Precision {
                iri: "b".to_string(),
                method: "llm:m".to_string(),
                right: 5,
                total: 5,
            },
            Precision {
                iri: "c".to_string(),
                method: "regex".to_string(),
                right: 40,
                total: 40,
            },
        ];
        let found = measured(&rows, "llm:m", 30);
        assert_eq!(found.len(), 1);
        assert!(found["a"] > 0.88 && found["a"] < 1.0);
        assert!(measured(&rows, "llm:m", 0).is_empty());
    }
}
