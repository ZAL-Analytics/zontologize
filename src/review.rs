use std::collections::HashMap;
use std::path::Path;

use anyhow::Result;
use tokio::io::{AsyncBufRead, AsyncBufReadExt};

use crate::config;
use crate::db;
use crate::db::review::{Fact, Precision};
use crate::ns::local_name;

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

pub fn context(text: &str, start: Option<i32>, end: Option<i32>, around: usize) -> String {
    let chars: Vec<char> = text.chars().collect();
    let (Some(start), Some(end)) = (start, end) else {
        return chars.iter().take(2 * around).collect::<String>();
    };
    let start = (start.max(0) as usize).min(chars.len());
    let end = (end.max(0) as usize).clamp(start, chars.len());
    let before: String = chars[start.saturating_sub(around)..start].iter().collect();
    let found: String = chars[start..end].iter().collect();
    let after: String = chars[end..(end + around).min(chars.len())].iter().collect();
    let flat = |s: String| crate::text::squash(&s);
    format!("{} «{}» {}", flat(before), flat(found), flat(after))
        .trim()
        .to_string()
}

fn printable(text: &str) -> String {
    text.chars()
        .filter(|c| *c == '\n' || !c.is_control())
        .collect()
}

fn describe(fact: &Fact) -> String {
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
        context(&fact.text, fact.span_start, fact.span_end, CONTEXT_CHARS)
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

async fn ask<R: AsyncBufRead + Unpin>(
    pool: &sqlx::PgPool,
    facts: &[Fact],
    input: &mut R,
) -> Result<Tally> {
    let mut tally = Tally::default();
    let mut lines = input.lines();
    for (i, fact) in facts.iter().enumerate() {
        let snapshot = describe(fact);
        println!("[{}/{}] {}", i + 1, facts.len(), printable(&snapshot));
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
    let lock = db::runs::lock(url, &config.database.schema).await?;
    let pool = db::pool::connect(
        url,
        &config.database.schema,
        config.database.max_connections,
    )
    .await?;
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
    lock.release().await;
    result
}

pub const MERGE_EXAMPLES: i64 = 2;

pub struct MergeOptions {
    pub n: Option<usize>,
    pub class: String,
    pub seed: String,
}

#[derive(Debug, Default, PartialEq)]
pub struct MergeTally {
    pub confirmed: usize,
    pub rejected: usize,
    pub skipped: usize,
}

async fn card(pool: &sqlx::PgPool, id: i64, name: &str, class: &str) -> Result<String> {
    let (aliases, mentions, examples) = db::review::entity_card(pool, id, MERGE_EXAMPLES).await?;
    let mut out = format!("  {name} ({}), {mentions} mentions", local_name(class));
    if !aliases.is_empty() {
        out.push_str(&format!(", aliases: {}", aliases.join("; ")));
    }
    for example in examples {
        let page = example
            .page_from
            .map_or(String::new(), |p| format!(", page {p}"));
        out.push_str(&format!(
            "\n    {}{page}: {}",
            example.path,
            context(
                &example.text,
                Some(example.span_start),
                Some(example.span_end),
                CONTEXT_CHARS
            )
        ));
    }
    Ok(out)
}

async fn ask_merges<R: AsyncBufRead + Unpin>(
    pool: &sqlx::PgPool,
    merges: &[db::review::Merge],
    input: &mut R,
) -> Result<MergeTally> {
    let mut tally = MergeTally::default();
    let mut lines = input.lines();
    for (i, merge) in merges.iter().enumerate() {
        println!(
            "[{}/{}] link {}: {}, score {:.2}",
            i + 1,
            merges.len(),
            merge.id,
            merge.method,
            merge.confidence
        );
        let a = card(pool, merge.a_id, &merge.a_name, &merge.a_class).await?;
        println!("{}", printable(&a));
        let b = card(pool, merge.b_id, &merge.b_name, &merge.b_class).await?;
        println!("{}", printable(&b));
        println!("same thing? y = yes, n = no, s = skip, q = quit");
        let Some(line) = lines.next_line().await? else {
            break;
        };
        match line.trim().to_lowercase().as_str() {
            "y" | "yes" => {
                db::review::set_merge_status(pool, merge.id, "confirmed").await?;
                tally.confirmed += 1;
            }
            "n" | "no" => {
                db::review::set_merge_status(pool, merge.id, "rejected").await?;
                tally.rejected += 1;
            }
            "q" | "quit" => break,
            _ => tally.skipped += 1,
        }
    }
    Ok(tally)
}

pub async fn merges<R: AsyncBufRead + Unpin>(
    config_dir: Option<&Path>,
    url: Option<&str>,
    options: &MergeOptions,
    input: &mut R,
) -> Result<MergeTally> {
    let config = config::load(config_dir)?;
    let iri = if options.class.is_empty() {
        String::new()
    } else {
        let loaded = crate::ontology::load(config_dir, &config.ontology)?;
        loaded
            .ontology
            .resolve(&options.class)
            .map_err(|e| anyhow::anyhow!("--class: {e}"))?
    };
    let lock = db::runs::lock(url, &config.database.schema).await?;
    let pool = db::pool::connect(
        url,
        &config.database.schema,
        config.database.max_connections,
    )
    .await?;
    let n = options.n.unwrap_or(config.review.sample);
    let result = async {
        let merges = db::review::proposed_merges(&pool, &iri, &options.seed, n as i64).await?;
        if merges.is_empty() {
            println!("review merges: no proposed links left for this filter");
            return Ok(MergeTally::default());
        }
        let tally = ask_merges(&pool, &merges, input).await?;
        let mut stats = crate::resolve::Stats::default();
        crate::resolve::canonical(&pool, &mut stats).await?;
        println!(
            "review merges: {} confirmed, {} rejected, {} skipped; {} canonical ids changed",
            tally.confirmed, tally.rejected, tally.skipped, stats.canonical_changed
        );
        Ok(tally)
    }
    .await;
    pool.close().await;
    lock.release().await;
    result
}

pub struct ClassOptions {
    pub n: Option<usize>,
    pub class: String,
    pub seed: String,
}

#[derive(Debug, Default, PartialEq)]
pub struct ClassTally {
    pub confirmed: usize,
    pub rejected: usize,
    pub skipped: usize,
}

fn class_change(change: &db::review::ClassChange) -> String {
    let to = change
        .proposed_class
        .as_deref()
        .map_or("not an entity".to_string(), |c| local_name(c).to_string());
    let name = match &change.proposed_name {
        Some(name) if *name != change.name => format!(", name {name}"),
        _ => String::new(),
    };
    format!(
        "{} ({}) -> {to}{name}",
        change.name,
        local_name(&change.class_iri)
    )
}

async fn ask_classes<R: AsyncBufRead + Unpin>(
    pool: &sqlx::PgPool,
    changes: &[db::review::ClassChange],
    input: &mut R,
) -> Result<ClassTally> {
    let mut tally = ClassTally::default();
    let mut lines = input.lines();
    for (i, change) in changes.iter().enumerate() {
        println!(
            "[{}/{}] {}: {}, confidence {:.2}",
            i + 1,
            changes.len(),
            printable(&class_change(change)),
            change.method,
            change.confidence
        );
        let entity = card(pool, change.entity_id, &change.name, &change.class_iri).await?;
        println!("{}", printable(&entity));
        println!("right? y = yes, n = no, s = skip, q = quit");
        let Some(line) = lines.next_line().await? else {
            break;
        };
        let status = match line.trim().to_lowercase().as_str() {
            "y" | "yes" => "confirmed",
            "n" | "no" => "rejected",
            "q" | "quit" => break,
            _ => {
                tally.skipped += 1;
                continue;
            }
        };
        let mut conn = pool.acquire().await?;
        db::entity_classes::set_status(&mut conn, change.entity_id, status).await?;
        if status == "confirmed" {
            tally.confirmed += 1;
        } else {
            tally.rejected += 1;
        }
    }
    Ok(tally)
}

pub async fn classes<R: AsyncBufRead + Unpin>(
    config_dir: Option<&Path>,
    url: Option<&str>,
    options: &ClassOptions,
    input: &mut R,
) -> Result<ClassTally> {
    let config = config::load(config_dir)?;
    let iri = if options.class.is_empty() {
        String::new()
    } else {
        let loaded = crate::ontology::load(config_dir, &config.ontology)?;
        loaded
            .ontology
            .resolve(&options.class)
            .map_err(|e| anyhow::anyhow!("--class: {e}"))?
    };
    let lock = db::runs::lock(url, &config.database.schema).await?;
    let pool = db::pool::connect(
        url,
        &config.database.schema,
        config.database.max_connections,
    )
    .await?;
    let n = options.n.unwrap_or(config.review.sample);
    let result = async {
        let changes = db::review::proposed_classes(&pool, &iri, &options.seed, n as i64).await?;
        if changes.is_empty() {
            println!("review classes: no proposed class changes left for this filter");
            return Ok(ClassTally::default());
        }
        let tally = ask_classes(&pool, &changes, input).await?;
        println!(
            "review classes: {} confirmed, {} rejected, {} skipped",
            tally.confirmed, tally.rejected, tally.skipped
        );
        Ok(tally)
    }
    .await;
    pool.close().await;
    lock.release().await;
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    fn change(
        proposed_class: Option<&str>,
        proposed_name: Option<&str>,
    ) -> db::review::ClassChange {
        db::review::ClassChange {
            entity_id: 7,
            name: "Delta".to_string(),
            class_iri: "https://example.org/acme#Pump".to_string(),
            proposed_class: proposed_class.map(str::to_string),
            proposed_name: proposed_name.map(str::to_string),
            method: "curate:llm:claude-code:haiku".to_string(),
            confidence: 0.6,
        }
    }

    #[test]
    fn class_card_shows_the_proposed_class() {
        let card = class_change(&change(
            Some("https://example.org/acme#Site"),
            Some("Delta"),
        ));
        assert_eq!(card, "Delta (Pump) -> Site");
        let renamed = class_change(&change(
            Some("https://example.org/acme#Site"),
            Some("Delta Site"),
        ));
        assert_eq!(renamed, "Delta (Pump) -> Site, name Delta Site");
    }

    #[test]
    fn class_card_shows_not_an_entity() {
        assert_eq!(
            class_change(&change(None, None)),
            "Delta (Pump) -> not an entity"
        );
    }

    #[test]
    fn printable_drops_control_characters() {
        assert_eq!(printable("a\u{1b}[2Jb\r\nc\u{7}"), "a[2Jb\nc");
    }

    #[test]
    fn context_marks_the_span_and_cuts_around_it() {
        let text = format!("{}The booster pump\nruns.", "x".repeat(150));
        let found = context(&text, Some(154), Some(166), CONTEXT_CHARS);
        assert!(found.ends_with("«booster pump» runs."), "{found}");
        assert_eq!(found.chars().filter(|c| *c == 'x').count(), 96);
        assert_eq!(context("a  b", None, None, CONTEXT_CHARS), "a  b");
        assert_eq!(
            context("short", Some(2), Some(99), CONTEXT_CHARS),
            "sh «ort»"
        );
        assert_eq!(context("abcdef", Some(3), Some(4), 2), "bc «d» ef");
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
