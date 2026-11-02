use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;
use std::path::Path;

use anyhow::{Context, Result};
use oxrdf::{Literal, NamedNode, Triple};
use oxttl::TurtleSerializer;
use sqlx::PgPool;

use super::TASK;
use crate::config::Config;
use crate::db;
use crate::db::facts::TermRow;
use crate::fit::report::{RDF_TYPE, RDFS_COMMENT, SKOS_PREF, STANDARD_PREFIXES, label, slug};
use crate::llm::shortlist::local_name;
use crate::ontology::Ontology;

pub const REPORT_FILE: &str = "extract-report.md";
pub const PROPOSALS_FILE: &str = "relation-proposals.ttl";
pub const NAMES_FILE: &str = "gazetteer-candidates.csv";
const EXAMPLES: i32 = 3;
const NAMES_PER_CLASS: i64 = 5;
const LISTED_TERMS: usize = 200;
const OWL_OBJECT_PROPERTY: &str = "http://www.w3.org/2002/07/owl#ObjectProperty";
const RDFS_DOMAIN: &str = "http://www.w3.org/2000/01/rdf-schema#domain";
const RDFS_RANGE: &str = "http://www.w3.org/2000/01/rdf-schema#range";

#[derive(Debug, Clone, PartialEq)]
pub struct Proposal {
    pub label: String,
    pub lang: String,
    pub domain: String,
    pub range: String,
    pub chunks: i64,
    pub documents: i64,
}

#[derive(Debug, Default)]
pub struct Totals {
    pub header: String,
    pub classes: Vec<(String, i64, i64)>,
    pub names: Vec<(String, String, i64)>,
    pub properties: Vec<(String, i64, i64)>,
    pub drops: BTreeMap<String, i64>,
}

pub fn proposals(rows: &[TermRow], min_evidence: usize) -> Vec<Proposal> {
    let mut groups: BTreeMap<(String, String), Vec<&TermRow>> = BTreeMap::new();
    for row in rows {
        groups
            .entry((row.label.clone(), row.lang.clone()))
            .or_default()
            .push(row);
    }
    let mut out = Vec::new();
    for ((label, lang), rows) in groups {
        let chunks: i64 = rows.iter().map(|r| r.chunks).sum();
        if chunks < min_evidence as i64 {
            continue;
        }
        let Some(best) = rows.iter().max_by(|a, b| {
            a.chunks
                .cmp(&b.chunks)
                .then_with(|| b.src_class.cmp(&a.src_class))
                .then_with(|| b.dst_class.cmp(&a.dst_class))
        }) else {
            continue;
        };
        out.push(Proposal {
            label,
            lang,
            domain: best.src_class.clone(),
            range: best.dst_class.clone(),
            chunks,
            documents: rows.iter().map(|r| r.documents).sum(),
        });
    }
    out.sort_by(|a, b| b.chunks.cmp(&a.chunks).then_with(|| a.label.cmp(&b.label)));
    out
}

fn property_name(label: &str) -> String {
    let mut name = slug(label);
    if let Some(first) = name.chars().next() {
        let lower: String = first.to_lowercase().collect();
        name.replace_range(..first.len_utf8(), &lower);
    }
    name
}

pub fn turtle(
    ontology: &Ontology,
    proposals: &[Proposal],
    namespace: &str,
    model: &str,
) -> Result<String> {
    let mut serializer = TurtleSerializer::new();
    let mut bound: BTreeSet<&str> = BTreeSet::new();
    for (prefix, iri) in &ontology.prefixes {
        serializer = serializer
            .with_prefix(prefix.as_str(), iri.as_str())
            .with_context(|| format!("prefix {prefix}: {iri} is not an IRI"))?;
        bound.insert(prefix.as_str());
    }
    for (prefix, iri) in STANDARD_PREFIXES {
        if !bound.contains(prefix) {
            serializer = serializer.with_prefix(prefix, iri)?;
        }
    }
    let mut writer = serializer.for_writer(Vec::new());
    let node = |iri: &str| NamedNode::new(iri).with_context(|| format!("{iri} is not a valid IRI"));
    let mut used = BTreeSet::new();
    for proposal in proposals {
        let base = property_name(&proposal.label);
        let mut name = base.clone();
        let mut n = 2;
        while !used.insert(name.clone())
            || ontology
                .properties
                .contains_key(&format!("{namespace}{name}"))
        {
            name = format!("{base}{n}");
            n += 1;
        }
        let property = node(&format!("{namespace}{name}"))?;
        let label = match Literal::new_language_tagged_literal(&proposal.label, &proposal.lang) {
            Ok(literal) => literal,
            Err(_) => Literal::new_simple_literal(&proposal.label),
        };
        writer.serialize_triple(&Triple::new(
            property.clone(),
            node(RDF_TYPE)?,
            node(OWL_OBJECT_PROPERTY)?,
        ))?;
        writer.serialize_triple(&Triple::new(property.clone(), node(SKOS_PREF)?, label))?;
        writer.serialize_triple(&Triple::new(
            property.clone(),
            node(RDFS_DOMAIN)?,
            node(&proposal.domain)?,
        ))?;
        writer.serialize_triple(&Triple::new(
            property.clone(),
            node(RDFS_RANGE)?,
            node(&proposal.range)?,
        ))?;
        let evidence = format!(
            "Proposed by zontologize extract from {} chunks in {} documents.",
            proposal.chunks, proposal.documents
        );
        writer.serialize_triple(&Triple::new(
            property,
            node(RDFS_COMMENT)?,
            Literal::new_simple_literal(evidence),
        ))?;
    }
    let bytes = writer.finish()?;
    let mut text = format!(
        "# Relation proposals from zontologize extract, model {model}.\n\
         # Copy what you accept into your local ontology file; nothing here is applied.\n"
    );
    text.push_str(&String::from_utf8(bytes).context("Turtle output is not UTF-8")?);
    Ok(text)
}

pub fn compact(ontology: &Ontology, iri: &str) -> String {
    for (prefix, namespace) in &ontology.prefixes {
        if let Some(local) = iri.strip_prefix(namespace.as_str())
            && !local.is_empty()
            && local
                .chars()
                .all(|c| c.is_alphanumeric() || c == '_' || c == '-')
        {
            return format!("{prefix}:{local}");
        }
    }
    format!("<{iri}>")
}

pub fn names_csv(ontology: &Ontology, rows: &[(String, String, String, i64)]) -> Result<String> {
    let mut writer = csv::Writer::from_writer(Vec::new());
    writer.write_record(["class", "key", "name", "aliases", "lang", "chunks"])?;
    for (class, key, name, chunks) in rows {
        writer.write_record([
            compact(ontology, class).as_str(),
            key,
            name,
            "",
            "",
            &chunks.to_string(),
        ])?;
    }
    let bytes = writer.into_inner().map_err(|e| anyhow::anyhow!("{e}"))?;
    String::from_utf8(bytes).context("CSV output is not UTF-8")
}

pub fn markdown(
    ontology: &Ontology,
    totals: &Totals,
    rows: &[TermRow],
    proposals: &[Proposal],
) -> String {
    let mut out = String::from("# Extract report\n\n");
    out.push_str(&totals.header);
    out.push_str("\n## Entities per class\n\n");
    if totals.classes.is_empty() {
        out.push_str("None yet.\n");
    }
    let mut names: BTreeMap<&str, Vec<String>> = BTreeMap::new();
    for (class, name, count) in &totals.names {
        names
            .entry(class.as_str())
            .or_default()
            .push(format!("{name} ({count})"));
    }
    for (class, mentions, chunks) in &totals.classes {
        let _ = writeln!(
            out,
            "- {}: {mentions} mentions in {chunks} chunks; most named: {}",
            label(ontology, class),
            names
                .get(class.as_str())
                .map_or(String::new(), |n| n.join(", "))
        );
    }
    out.push_str("\n## Relations per property\n\n");
    if totals.properties.is_empty() {
        out.push_str("None yet.\n");
    }
    for (property, count, chunks) in &totals.properties {
        let _ = writeln!(
            out,
            "- {} (`{property}`): {count} relations in {chunks} chunks",
            local_name(property)
        );
    }
    out.push_str("\n## Dropped by validation (jobs of this model)\n\n");
    if totals.drops.is_empty() {
        out.push_str("Nothing dropped.\n");
    }
    for (key, count) in &totals.drops {
        let (reason, about) = key.split_once('|').unwrap_or((key, ""));
        let about = if about.is_empty() {
            String::new()
        } else {
            format!(" ({})", local_name(about))
        };
        let _ = writeln!(out, "- {reason}{about}: {count}");
    }
    out.push_str("\n## Relations no property covers\n\n");
    if rows.is_empty() {
        out.push_str("None yet.\n");
    }
    for row in rows.iter().take(LISTED_TERMS) {
        let _ = writeln!(
            out,
            "- \"{}\"@{}: {} -> {}, {} chunks in {} documents; e.g. {}",
            row.label,
            row.lang,
            local_name(&row.src_class),
            local_name(&row.dst_class),
            row.chunks,
            row.documents,
            row.examples.join("; ")
        );
    }
    if rows.len() > LISTED_TERMS {
        let _ = writeln!(out, "- ... {} more groups", rows.len() - LISTED_TERMS);
    }
    let _ = writeln!(
        out,
        "\n## Proposals\n\n{} object properties in {PROPOSALS_FILE}.",
        proposals.len()
    );
    out
}

async fn header(pool: &PgPool, job: Option<i64>) -> Result<String> {
    let extracted = db::facts::extracted_chunks(pool).await?;
    let mut header = format!("Chunks extracted by a model: {extracted}.\n");
    let Some(job) = job.filter(|id| *id > 0) else {
        return Ok(header);
    };
    let Some(state) = db::llm::jobs(pool, i64::MAX)
        .await?
        .into_iter()
        .find(|j| j.id == job)
    else {
        return Ok(header);
    };
    let total = state.done + state.pending + state.invalid + state.error;
    if state.status != "finished" {
        let _ = writeln!(
            header,
            "\nPartial: job {job} is {}{}; {} of {total} chunks answered.",
            state.status,
            state
                .pause_reason
                .as_deref()
                .map_or(String::new(), |r| format!(" ({r})")),
            state.done
        );
    } else {
        let _ = writeln!(
            header,
            "Job {job} finished: {} of {total} chunks answered.",
            state.done
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
    let model = crate::llm::ModelName::parse(&config.models.extract)
        .map_err(|e| anyhow::anyhow!("models.extract: {e}"))?;
    let totals = Totals {
        header: header(pool, job).await?,
        classes: db::facts::class_counts(pool).await?,
        names: db::facts::top_names(pool, NAMES_PER_CLASS).await?,
        properties: db::facts::property_counts(pool).await?,
        drops: db::llm::task_stats(pool, TASK, model.provider.as_str(), &model.model).await?,
    };
    let rows = db::facts::term_rows(pool, EXAMPLES).await?;
    let proposals = proposals(&rows, config.llm_extract.min_evidence);
    std::fs::create_dir_all(out).with_context(|| format!("cannot create {}", out.display()))?;
    let report = markdown(ontology, &totals, &rows, &proposals);
    let path = out.join(REPORT_FILE);
    std::fs::write(&path, report).with_context(|| format!("cannot write {}", path.display()))?;
    let text = turtle(
        ontology,
        &proposals,
        &config.fit.proposal_namespace,
        &config.models.extract,
    )?;
    let path = out.join(PROPOSALS_FILE);
    std::fs::write(&path, text).with_context(|| format!("cannot write {}", path.display()))?;
    let names = db::facts::name_candidates(pool, config.llm_extract.min_evidence as i64).await?;
    let path = out.join(NAMES_FILE);
    std::fs::write(&path, names_csv(ontology, &names)?)
        .with_context(|| format!("cannot write {}", path.display()))?;
    println!(
        "extract: report, {} relation proposals and {} name candidates in {}",
        proposals.len(),
        names.len(),
        out.display()
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::super::tests::{iri, ontology};
    use super::*;

    fn row(label: &str, src: &str, dst: &str, chunks: i64) -> TermRow {
        TermRow {
            label: label.to_string(),
            lang: "en".to_string(),
            src_class: iri(src),
            dst_class: iri(dst),
            chunks,
            documents: chunks,
            examples: vec![format!("a/{label}.txt, pages 1")],
        }
    }

    #[test]
    fn groups_need_min_evidence_and_take_the_most_frequent_pair() {
        let rows = vec![
            row("supplied by", "Pump", "Company", 3),
            row("supplied by", "Asset", "Company", 1),
            row("near", "Pump", "Site", 2),
        ];
        let found = proposals(&rows, 3);
        assert_eq!(
            found,
            [Proposal {
                label: "supplied by".to_string(),
                lang: "en".to_string(),
                domain: iri("Pump"),
                range: iri("Company"),
                chunks: 4,
                documents: 4,
            }]
        );
        assert_eq!(proposals(&rows, 2).len(), 2);
    }

    #[test]
    fn turtle_loads_back_as_object_properties() {
        let onto = ontology();
        let found = proposals(&[row("supplied by", "Pump", "Company", 3)], 1);
        let text = turtle(&onto, &found, "urn:test:proposal#", "claude-code:haiku").unwrap();
        assert!(text.contains("suppliedBy"), "{text}");
        let config = crate::config::OntologyConfig {
            files: vec!["p.ttl".to_string()],
            languages: vec!["en".to_string()],
            include: vec![],
            exclude: vec![],
            document_class: String::new(),
        };
        let loaded = crate::ontology::load_sources(
            &[
                (
                    "a.ttl".to_string(),
                    super::super::tests::TTL.as_bytes().to_vec(),
                ),
                ("p.ttl".to_string(), text.into_bytes()),
            ],
            &config,
        );
        assert!(!loaded.has_errors(), "{:?}", loaded.findings);
        let property = &loaded.ontology.properties["urn:test:proposal#suppliedBy"];
        assert_eq!(property.domain, [iri("Pump")]);
        assert_eq!(property.range, [iri("Company")]);
    }

    #[test]
    fn name_candidates_load_as_a_gazetteer() {
        let onto = ontology();
        let rows = vec![(
            iri("Company"),
            "acme pumps".to_string(),
            "Acme Pumps".to_string(),
            4,
        )];
        let text = names_csv(&onto, &rows).unwrap();
        assert!(
            text.starts_with("class,key,name,aliases,lang,chunks\nex:Company,"),
            "{text}"
        );
        let lexicon = crate::rules::gazetteer::Lexicon::build(
            &[("names.csv".to_string(), text.into_bytes())],
            &onto,
        )
        .unwrap();
        assert_eq!(lexicon.entries.len(), 1);
    }

    #[test]
    fn markdown_says_when_a_job_is_partial() {
        let onto = ontology();
        let totals = Totals {
            header: "Partial: job 3 is paused; 1 of 4 chunks answered.\n".to_string(),
            drops: BTreeMap::from([(format!("quote not in the chunk|{}", iri("Pump")), 2)]),
            ..Totals::default()
        };
        let rows = vec![row("near", "Pump", "Site", 2)];
        let text = markdown(&onto, &totals, &rows, &[]);
        assert!(text.contains("Partial: job 3"), "{text}");
        assert!(text.contains("quote not in the chunk (Pump): 2"), "{text}");
        assert!(
            text.contains("\"near\"@en: Pump -> Site, 2 chunks"),
            "{text}"
        );
    }
}
