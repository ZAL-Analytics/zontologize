use std::collections::BTreeSet;
use std::path::Path;
use std::time::Instant;

use anyhow::Result;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::db;
use crate::db::resolve::{REFERENCES, REPLACES, SAME_AS};
use crate::ns::local_name;

const MAX_LABEL_BYTES: usize = 63;
const GRAPH_FORMAT: u32 = 2;
pub const BUILT_IN: [(&str, &str); 3] = [
    (SAME_AS, "same as"),
    (REFERENCES, "references"),
    (REPLACES, "replaces"),
];

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Kind {
    Vertex,
    Edge,
}

impl Kind {
    pub fn as_str(self) -> &'static str {
        match self {
            Kind::Vertex => "vertex",
            Kind::Edge => "edge",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Label {
    pub kind: Kind,
    pub iri: String,
    pub name: String,
}

fn camel(text: &str, upper_first: bool) -> String {
    let plain = crate::text::strip_accents(text);
    let mut out = String::new();
    for word in plain.split(|c: char| !c.is_ascii_alphanumeric()) {
        let mut chars = word.chars();
        let Some(first) = chars.next() else {
            continue;
        };
        if out.is_empty() && !upper_first {
            out.push(first.to_ascii_lowercase());
        } else {
            out.push(first.to_ascii_uppercase());
        }
        out.extend(chars);
    }
    out
}

fn base_name(kind: Kind, iri: &str, label: Option<&str>) -> String {
    let upper = kind == Kind::Vertex;
    let mut name = label.map(|l| camel(l, upper)).unwrap_or_default();
    if name.is_empty() {
        name = camel(local_name(iri), upper);
    }
    let prefix = if upper { "C" } else { "p" };
    if name.is_empty() || name.starts_with(|c: char| c.is_ascii_digit()) {
        name = format!("{prefix}{name}");
    }
    name
}

fn cut(name: &str, room: usize) -> &str {
    &name[..name.len().min(MAX_LABEL_BYTES - room)]
}

pub fn names(mut items: Vec<(Kind, String, Option<String>)>, stored: &[Label]) -> Vec<Label> {
    items.sort_by(|a, b| (a.0, &a.1).cmp(&(b.0, &b.1)));
    items.dedup_by(|a, b| a.0 == b.0 && a.1 == b.1);
    let mut taken = BTreeSet::new();
    let mut kept = std::collections::BTreeMap::new();
    for (kind, iri, _) in &items {
        let found = stored.iter().find(|l| l.kind == *kind && &l.iri == iri);
        if let Some(label) = found
            && taken.insert(label.name.clone())
        {
            kept.insert((*kind, iri.clone()), label.name.clone());
        }
    }
    let mut labels = Vec::new();
    for (kind, iri, label) in items {
        if let Some(name) = kept.remove(&(kind, iri.clone())) {
            labels.push(Label { kind, iri, name });
            continue;
        }
        let base = base_name(kind, &iri, label.as_deref());
        let mut name = cut(&base, 0).to_string();
        let mut n = 2;
        while !taken.insert(name.clone()) {
            let suffix = format!("_{n}");
            name = format!("{}{suffix}", cut(&base, suffix.len()));
            n += 1;
        }
        labels.push(Label { kind, iri, name });
    }
    labels
}

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Stats {
    pub vertices: u64,
    pub edges: u64,
    pub labels: usize,
    pub seconds: f64,
    pub fingerprint: String,
    pub unchanged: bool,
}

fn fingerprint(graph: &str, tables: &str, labels: &[Label]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(format!("{GRAPH_FORMAT}\0{graph}\0{tables}\0").as_bytes());
    for label in labels {
        hasher
            .update(format!("{}\0{}\0{}\0", label.kind.as_str(), label.iri, label.name).as_bytes());
    }
    crate::hash::hex(&hasher.finalize())
}

pub async fn sync(config_dir: Option<&Path>, url: Option<&str>) -> Result<Stats> {
    let config = crate::config::load(config_dir)?;
    let schema = &config.database.schema;
    let lock = db::runs::lock(url, schema).await?;
    let pool = db::pool::connect(url, schema, 2).await?;
    let hash = crate::config::hash(&config)?;
    let work = run_with(&pool, url, &config);
    let result = db::runs::track(&pool, &hash, "graph", work).await;
    pool.close().await;
    lock.release().await;
    result
}

async fn run_with(
    pool: &sqlx::PgPool,
    url: Option<&str>,
    config: &crate::config::Config,
) -> Result<Stats> {
    let started = Instant::now();
    let graph = &config.database.graph;
    let language = config
        .ontology
        .languages
        .first()
        .cloned()
        .unwrap_or_default();
    let mut items = Vec::new();
    for (iri, label) in db::graph::classes(pool, &language).await? {
        items.push((Kind::Vertex, iri, label));
    }
    for (iri, label) in db::graph::properties(pool, &language).await? {
        let built_in = BUILT_IN
            .iter()
            .find(|(b, _)| *b == iri)
            .map(|(_, l)| l.to_string());
        items.push((Kind::Edge, iri, built_in.or(label)));
    }
    let labels = names(items, &db::graph::stored_labels(pool).await?);
    let tables = db::graph::fingerprint(pool).await?;
    let fingerprint = fingerprint(graph, &tables, &labels);
    let previous = db::graph::last_sync(pool).await?;
    if let Some(previous) = previous
        && previous.fingerprint == fingerprint
        && db::graph::exists(pool, graph).await?
    {
        return Ok(Stats {
            unchanged: true,
            seconds: started.elapsed().as_secs_f64(),
            ..previous
        });
    }
    let mut conn = db::connect(url, &config.database.schema).await?;
    let result = db::graph::rebuild(
        &mut conn,
        &config.database.schema,
        &config.database.api_schema,
        graph,
        &labels,
    )
    .await;
    db::close(conn).await;
    let (vertices, edges) = result?;
    Ok(Stats {
        vertices,
        edges,
        labels: labels.len(),
        seconds: started.elapsed().as_secs_f64(),
        fingerprint,
        unchanged: false,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item(kind: Kind, iri: &str, label: Option<&str>) -> (Kind, String, Option<String>) {
        (kind, iri.to_string(), label.map(str::to_string))
    }

    #[test]
    fn labels_are_camel_case() {
        assert_eq!(
            camel("centrifugal compressor", true),
            "CentrifugalCompressor"
        );
        assert_eq!(camel("Société générale", true), "SocieteGenerale");
        assert_eq!(camel("part of", false), "partOf");
        assert_eq!(camel("OEM supplier", false), "oEMSupplier");
    }

    #[test]
    fn names_fall_back_and_are_unique() {
        let labels = names(
            vec![
                item(Kind::Vertex, "http://x/b#Pump", Some("pump")),
                item(Kind::Vertex, "http://x/a#Pump", Some("pump")),
                item(Kind::Vertex, "http://x/REF-30000001", None),
                item(Kind::Vertex, "http://x/123", None),
                item(Kind::Edge, "http://x#partOf", Some("part of")),
                item(Kind::Vertex, "http://x#Thing", Some("…")),
            ],
            &[],
        );
        let found: Vec<(&str, &str)> = labels
            .iter()
            .map(|l| (l.iri.as_str(), l.name.as_str()))
            .collect();
        assert_eq!(
            found,
            vec![
                ("http://x#Thing", "Thing"),
                ("http://x/123", "C123"),
                ("http://x/REF-30000001", "REF30000001"),
                ("http://x/a#Pump", "Pump"),
                ("http://x/b#Pump", "Pump_2"),
                ("http://x#partOf", "partOf"),
            ]
        );
    }

    #[test]
    fn stored_names_stay_when_a_class_is_added() {
        let stored = names(
            vec![item(Kind::Vertex, "http://x/b#Pump", Some("pump"))],
            &[],
        );
        assert_eq!(stored[0].name, "Pump");
        let labels = names(
            vec![
                item(Kind::Vertex, "http://x/b#Pump", Some("pump")),
                item(Kind::Vertex, "http://x/a#Pump", Some("pump")),
            ],
            &stored,
        );
        let found: Vec<(&str, &str)> = labels
            .iter()
            .map(|l| (l.iri.as_str(), l.name.as_str()))
            .collect();
        assert_eq!(
            found,
            [("http://x/a#Pump", "Pump_2"), ("http://x/b#Pump", "Pump")]
        );
    }

    #[test]
    fn graph_fingerprints_change_with_tables_labels_and_graph() {
        let labels = names(vec![item(Kind::Vertex, "http://x#A", Some("a"))], &[]);
        let base = fingerprint("g", "1.5:2.7", &labels);
        assert_eq!(base, fingerprint("g", "1.5:2.7", &labels));
        assert_ne!(base, fingerprint("h", "1.5:2.7", &labels));
        assert_ne!(base, fingerprint("g", "1.5:2.8", &labels));
        assert_ne!(base, fingerprint("g", "1.5:2.7", &[]));
    }

    #[test]
    fn long_names_are_cut_to_fit_a_suffix() {
        let long = "word ".repeat(30);
        let labels = names(
            vec![
                item(Kind::Vertex, "http://x#A", Some(&long)),
                item(Kind::Vertex, "http://x#B", Some(&long)),
            ],
            &[],
        );
        assert_eq!(labels[0].name.len(), MAX_LABEL_BYTES);
        assert_eq!(labels[1].name.len(), MAX_LABEL_BYTES);
        assert!(labels[1].name.ends_with("_2"));
    }
}
