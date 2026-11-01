use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;
use std::path::Path;

use anyhow::{Context, Result};
use oxrdf::{Literal, NamedNode, Triple};
use oxttl::TurtleSerializer;

use super::{Checked, NONE, local_name, normalise};
use crate::db::fit::Chunk;
use crate::ontology::Ontology;

const EXAMPLES: usize = 5;
const NEVER_LISTED: usize = 50;
const REPORT_FILE: &str = "fit-report.md";
const PROPOSALS_FILE: &str = "proposals.ttl";
const RDFS_COMMENT: &str = "http://www.w3.org/2000/01/rdf-schema#comment";
const RDFS_SUBCLASS: &str = "http://www.w3.org/2000/01/rdf-schema#subClassOf";
const RDF_TYPE: &str = "http://www.w3.org/1999/02/22-rdf-syntax-ns#type";
const OWL_CLASS: &str = "http://www.w3.org/2002/07/owl#Class";
const SKOS_PREF: &str = "http://www.w3.org/2004/02/skos/core#prefLabel";
const SKOS_ALT: &str = "http://www.w3.org/2004/02/skos/core#altLabel";
const STANDARD_PREFIXES: [(&str, &str); 4] = [
    ("rdf", "http://www.w3.org/1999/02/22-rdf-syntax-ns#"),
    ("rdfs", "http://www.w3.org/2000/01/rdf-schema#"),
    ("owl", "http://www.w3.org/2002/07/owl#"),
    ("skos", "http://www.w3.org/2004/02/skos/core#"),
];

pub struct Entry<'a> {
    pub chunk: &'a Chunk,
    pub shortlist: &'a [String],
    pub checked: Checked,
}

#[derive(Debug, Default)]
pub struct Summary {
    pub job: i64,
    pub model: String,
    pub ontology_version: String,
    pub items: usize,
    pub done: usize,
    pub invalid: usize,
    pub errors: usize,
    pub input_tokens: i64,
    pub output_tokens: i64,
    pub cost_usd: f64,
}

#[derive(Default)]
struct Seen {
    chunks: usize,
    contents: BTreeSet<i64>,
    examples: Vec<String>,
}

impl Seen {
    fn add(&mut self, chunk: &Chunk) {
        self.chunks += 1;
        self.contents.insert(chunk.content_id);
        let example = format!("{} {}", chunk.path, pages(chunk));
        if self.examples.len() < EXAMPLES && !self.examples.contains(&example) {
            self.examples.push(example);
        }
    }
}

fn pages(chunk: &Chunk) -> String {
    match (chunk.page_from, chunk.page_to) {
        (Some(from), Some(to)) if from != to => format!("p. {from}-{to}"),
        (Some(from), _) => format!("p. {from}"),
        _ => String::new(),
    }
}

#[derive(Default)]
struct TermGroup {
    seen: Seen,
    forms: BTreeMap<String, usize>,
    languages: BTreeMap<String, usize>,
}

fn most_common(map: &BTreeMap<String, usize>) -> String {
    map.iter()
        .max_by(|a, b| a.1.cmp(b.1).then_with(|| b.0.cmp(a.0)))
        .map(|(k, _)| k.clone())
        .unwrap_or_default()
}

pub struct Proposal {
    pub kind: String,
    pub term: String,
    pub lang: String,
    pub near: String,
    pub chunks: usize,
    pub documents: usize,
}

pub fn write(
    dir: &Path,
    ontology: &Ontology,
    entries: &[Entry],
    summary: &Summary,
    min_evidence: usize,
    namespace: &str,
) -> Result<Vec<Proposal>> {
    std::fs::create_dir_all(dir).with_context(|| format!("cannot create {}", dir.display()))?;
    let mut classes: BTreeMap<String, Seen> = BTreeMap::new();
    let mut shortlisted: BTreeMap<String, usize> = BTreeMap::new();
    let mut terms: BTreeMap<(String, String, String), TermGroup> = BTreeMap::new();
    let mut dropped = 0;
    for entry in entries {
        dropped += entry.checked.dropped;
        for iri in entry.shortlist {
            *shortlisted.entry(iri.clone()).or_default() += 1;
        }
        let mut confirmed = BTreeSet::new();
        for hit in &entry.checked.classes {
            if confirmed.insert(hit.iri.clone()) {
                classes.entry(hit.iri.clone()).or_default().add(entry.chunk);
            }
        }
        let mut grouped = BTreeSet::new();
        for term in &entry.checked.terms {
            let key = (normalise(&term.term), term.near.clone(), term.kind.clone());
            if !grouped.insert(key.clone()) {
                continue;
            }
            let group = terms.entry(key).or_default();
            group.seen.add(entry.chunk);
            *group.forms.entry(term.term.trim().to_string()).or_default() += 1;
            *group.languages.entry(term.lang.clone()).or_default() += 1;
        }
    }
    let mut proposals = Vec::new();
    for ((_, near, kind), group) in &terms {
        if group.seen.chunks < min_evidence || near == NONE {
            continue;
        }
        proposals.push(Proposal {
            kind: kind.clone(),
            term: most_common(&group.forms),
            lang: most_common(&group.languages),
            near: near.clone(),
            chunks: group.seen.chunks,
            documents: group.seen.contents.len(),
        });
    }
    proposals.sort_by(|a, b| b.chunks.cmp(&a.chunks).then_with(|| a.term.cmp(&b.term)));
    let report = markdown(
        ontology,
        &classes,
        &shortlisted,
        &terms,
        summary,
        dropped,
        min_evidence,
    );
    let path = dir.join(REPORT_FILE);
    std::fs::write(&path, report).with_context(|| format!("cannot write {}", path.display()))?;
    let turtle = turtle(ontology, &proposals, namespace, summary)?;
    let path = dir.join(PROPOSALS_FILE);
    std::fs::write(&path, turtle).with_context(|| format!("cannot write {}", path.display()))?;
    Ok(proposals)
}

fn label(ontology: &Ontology, iri: &str) -> String {
    let class = ontology.classes.get(iri);
    let label = class.and_then(|c| {
        ontology
            .languages
            .iter()
            .find_map(|l| c.labels.get(l))
            .or_else(|| c.labels.values().next())
    });
    match label {
        Some(label) => format!("{label} (`{iri}`)"),
        None => format!("`{iri}`"),
    }
}

fn markdown(
    ontology: &Ontology,
    classes: &BTreeMap<String, Seen>,
    shortlisted: &BTreeMap<String, usize>,
    terms: &BTreeMap<(String, String, String), TermGroup>,
    summary: &Summary,
    dropped: usize,
    min_evidence: usize,
) -> String {
    let mut out = String::new();
    let _ = writeln!(out, "# Ontology fit report\n");
    if summary.done + summary.invalid + summary.errors < summary.items {
        let _ = writeln!(
            out,
            "**Partial: {} of {} chunks answered.** Run `zontologize ontology fit` again to go on.\n",
            summary.done, summary.items
        );
    }
    let _ = writeln!(out, "- Job: {}", summary.job);
    let _ = writeln!(out, "- Model: {}", summary.model);
    let _ = writeln!(out, "- Ontology version: {}", summary.ontology_version);
    let _ = writeln!(
        out,
        "- Chunks: {} sampled, {} answered, {} invalid answers, {} errors",
        summary.items, summary.done, summary.invalid, summary.errors
    );
    let _ = writeln!(
        out,
        "- Entries dropped by validation (IRI outside the shortlist or quote not in the chunk): {dropped}"
    );
    let _ = writeln!(
        out,
        "- Tokens: {} in, {} out; cost ${:.4} (list price for a subscription)\n",
        summary.input_tokens, summary.output_tokens, summary.cost_usd
    );
    let _ = writeln!(out, "## Classes confirmed\n");
    let _ = writeln!(
        out,
        "| Class | Chunks | Documents | Shortlisted | Examples |"
    );
    let _ = writeln!(out, "|---|---|---|---|---|");
    let mut confirmed: Vec<(&String, &Seen)> = classes.iter().collect();
    confirmed.sort_by(|a, b| b.1.chunks.cmp(&a.1.chunks).then_with(|| a.0.cmp(b.0)));
    for (iri, seen) in &confirmed {
        let _ = writeln!(
            out,
            "| {} | {} | {} | {} | {} |",
            label(ontology, iri),
            seen.chunks,
            seen.contents.len(),
            shortlisted.get(*iri).copied().unwrap_or(0),
            seen.examples.join("; ").replace('|', "\\|")
        );
    }
    let _ = writeln!(out, "\n## Shortlisted but never confirmed\n");
    let mut rejected: Vec<(&String, &usize)> = shortlisted
        .iter()
        .filter(|(iri, _)| !classes.contains_key(*iri))
        .collect();
    rejected.sort_by(|a, b| b.1.cmp(a.1).then_with(|| a.0.cmp(b.0)));
    for (iri, count) in &rejected {
        let _ = writeln!(
            out,
            "- {}: shortlisted in {count} chunks",
            label(ontology, iri)
        );
    }
    let never: Vec<&String> = ontology
        .classes
        .values()
        .filter(|c| {
            c.selected && !classes.contains_key(&c.iri) && !shortlisted.contains_key(&c.iri)
        })
        .map(|c| &c.iri)
        .collect();
    let _ = writeln!(
        out,
        "\n## Selected classes the sample never touched\n\n{} classes.",
        never.len()
    );
    for iri in never.iter().take(NEVER_LISTED) {
        let _ = writeln!(out, "- {}", label(ontology, iri));
    }
    if never.len() > NEVER_LISTED {
        let _ = writeln!(out, "- ... {} more", never.len() - NEVER_LISTED);
    }
    let _ = writeln!(
        out,
        "\n## Terms no class covers\n\nTerms seen in at least {min_evidence} chunks become \
         proposals unless they have no near class.\n"
    );
    let _ = writeln!(
        out,
        "| Term | Kind | Near | Chunks | Documents | Examples |"
    );
    let _ = writeln!(out, "|---|---|---|---|---|---|");
    let mut groups: Vec<(&(String, String, String), &TermGroup)> = terms.iter().collect();
    groups.sort_by(|a, b| {
        b.1.seen
            .chunks
            .cmp(&a.1.seen.chunks)
            .then_with(|| a.0.cmp(b.0))
    });
    for ((_, near, kind), group) in groups {
        let near = if near == NONE {
            "-".to_string()
        } else {
            label(ontology, near)
        };
        let _ = writeln!(
            out,
            "| {} | {kind} | {near} | {} | {} | {} |",
            most_common(&group.forms).replace('|', "\\|"),
            group.seen.chunks,
            group.seen.contents.len(),
            group.seen.examples.join("; ").replace('|', "\\|")
        );
    }
    out
}

fn slug(term: &str) -> String {
    let mut out = String::new();
    for word in term.split(|c: char| !c.is_alphanumeric()) {
        let mut chars = word.chars();
        if let Some(first) = chars.next() {
            out.extend(first.to_uppercase());
            out.push_str(&chars.as_str().to_lowercase());
        }
    }
    if out.is_empty() {
        out.push_str("Term");
    }
    out
}

fn turtle(
    ontology: &Ontology,
    proposals: &[Proposal],
    namespace: &str,
    summary: &Summary,
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
        let near = node(&proposal.near)?;
        let label = match Literal::new_language_tagged_literal(&proposal.term, &proposal.lang) {
            Ok(literal) => literal,
            Err(_) => Literal::new_simple_literal(&proposal.term),
        };
        let evidence = format!(
            "Proposed by zontologize fit job {} from {} chunks in {} documents.",
            summary.job, proposal.chunks, proposal.documents
        );
        if proposal.kind == "alias" {
            writer.serialize_triple(&Triple::new(near.clone(), node(SKOS_ALT)?, label))?;
            let note = format!(
                "Proposed skos:altLabel \"{}\"@{} for {}: {evidence}",
                proposal.term,
                proposal.lang,
                local_name(&proposal.near)
            );
            writer.serialize_triple(&Triple::new(
                near,
                node(RDFS_COMMENT)?,
                Literal::new_simple_literal(note),
            ))?;
            continue;
        }
        let base = slug(&proposal.term);
        let mut name = base.clone();
        let mut n = 2;
        while !used.insert(name.clone())
            || ontology.classes.contains_key(&format!("{namespace}{name}"))
        {
            name = format!("{base}{n}");
            n += 1;
        }
        let class = node(&format!("{namespace}{name}"))?;
        writer.serialize_triple(&Triple::new(
            class.clone(),
            node(RDF_TYPE)?,
            node(OWL_CLASS)?,
        ))?;
        writer.serialize_triple(&Triple::new(class.clone(), node(RDFS_SUBCLASS)?, near))?;
        writer.serialize_triple(&Triple::new(class.clone(), node(SKOS_PREF)?, label))?;
        writer.serialize_triple(&Triple::new(
            class,
            node(RDFS_COMMENT)?,
            Literal::new_simple_literal(evidence),
        ))?;
    }
    let bytes = writer.finish()?;
    let mut text = format!(
        "# Proposals from zontologize ontology fit, job {}, model {}.\n\
         # Copy what you accept into your local ontology file; nothing here is applied.\n",
        summary.job, summary.model
    );
    text.push_str(&String::from_utf8(bytes).context("Turtle output is not UTF-8")?);
    Ok(text)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::OntologyConfig;
    use crate::fit::{ClassHit, Term};

    const TTL: &str = r#"
        @prefix ex: <http://example.org/ex#> .
        @prefix owl: <http://www.w3.org/2002/07/owl#> .
        @prefix skos: <http://www.w3.org/2004/02/skos/core#> .
        @prefix rdfs: <http://www.w3.org/2000/01/rdf-schema#> .
        ex:Machine a owl:Class ; skos:prefLabel "Machine"@en .
        ex:Pump a owl:Class ; rdfs:subClassOf ex:Machine ; skos:prefLabel "Pump"@en .
    "#;

    fn config() -> OntologyConfig {
        OntologyConfig {
            files: vec!["a.ttl".to_string()],
            languages: vec!["en".to_string(), "nl".to_string()],
            include: vec![],
            exclude: vec![],
            document_class: String::new(),
        }
    }

    fn ontology() -> Ontology {
        crate::ontology::load_sources(&[("a.ttl".to_string(), TTL.as_bytes().to_vec())], &config())
            .ontology
    }

    fn chunk(id: i64, content: i64) -> Chunk {
        Chunk {
            id,
            content_id: content,
            text: String::new(),
            page_from: Some(1),
            page_to: Some(2),
            path: format!("projects/file {content}.txt"),
        }
    }

    fn term(text: &str, lang: &str, kind: &str, near: &str) -> Term {
        Term {
            term: text.to_string(),
            lang: lang.to_string(),
            kind: kind.to_string(),
            near: near.to_string(),
        }
    }

    const PUMP: &str = "http://example.org/ex#Pump";
    const MACHINE: &str = "http://example.org/ex#Machine";

    #[test]
    fn groups_terms_and_writes_turtle_that_loads_back() {
        let onto = ontology();
        let chunks: Vec<Chunk> = (1..=4).map(|i| chunk(i, i / 2)).collect();
        let shortlist = vec![PUMP.to_string(), MACHINE.to_string()];
        let entries: Vec<Entry> = chunks
            .iter()
            .enumerate()
            .map(|(i, c)| Entry {
                chunk: c,
                shortlist: &shortlist,
                checked: Checked {
                    classes: vec![ClassHit {
                        iri: PUMP.to_string(),
                        evidence: "pump".to_string(),
                    }],
                    terms: if i < 3 {
                        vec![
                            term("Pomp", "nl", "alias", PUMP),
                            term(" Booster  pump", "en", "subclass", PUMP),
                            term("impeller", "en", "subclass", NONE),
                        ]
                    } else {
                        vec![term("rotor", "en", "subclass", MACHINE)]
                    },
                    dropped: 1,
                },
            })
            .collect();
        let dir = std::env::temp_dir().join(format!("zontologize-report-{}", std::process::id()));
        let summary = Summary {
            job: 7,
            model: "claude-code:haiku".to_string(),
            items: 5,
            done: 4,
            ..Summary::default()
        };
        let proposals = write(&dir, &onto, &entries, &summary, 3, "urn:acme:proposal#").unwrap();
        assert_eq!(proposals.len(), 2);
        let report = std::fs::read_to_string(dir.join(REPORT_FILE)).unwrap();
        assert!(report.contains("Partial: 4 of 5"), "{report}");
        assert!(
            report.contains("Machine (`http://example.org/ex#Machine`): shortlisted in 4 chunks")
        );
        assert!(
            report.contains("| impeller | subclass | - | 3 |"),
            "{report}"
        );
        let turtle = std::fs::read_to_string(dir.join(PROPOSALS_FILE)).unwrap();
        assert!(turtle.contains("\"Pomp\"@nl"), "{turtle}");
        assert!(turtle.contains("BoosterPump"), "{turtle}");
        assert!(!turtle.contains("impeller"), "{turtle}");
        assert!(!turtle.contains("rotor"), "{turtle}");
        let loaded = crate::ontology::load_sources(
            &[
                ("a.ttl".to_string(), TTL.as_bytes().to_vec()),
                ("p.ttl".to_string(), turtle.into_bytes()),
            ],
            &config(),
        );
        assert!(!loaded.has_errors(), "{:?}", loaded.findings);
        let class = &loaded.ontology.classes["urn:acme:proposal#BoosterPump"];
        assert_eq!(class.parents, [PUMP]);
        assert!(loaded.ontology.classes[PUMP].alt_labels["nl"].contains(&"Pomp".to_string()));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn slug_makes_a_local_name() {
        assert_eq!(slug("booster pump"), "BoosterPump");
        assert_eq!(slug("Wärme-tauscher"), "WärmeTauscher");
        assert_eq!(slug("--"), "Term");
    }
}
