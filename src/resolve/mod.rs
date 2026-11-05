pub mod documents;
pub mod names;

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::Path;
use std::time::Instant;

use anyhow::Result;
use sqlx::PgPool;

use crate::config::Config;
use crate::db;
use crate::db::resolve::{EntityRow, Link, NewLink, REFERENCES, REPLACES, SAME_AS};
use crate::ontology::Ontology;
use crate::rules::Rules;

pub const METHOD: &str = "resolve";
const AUTO: &str = "auto";
const PROPOSED: &str = "proposed";
const NEIGHBOUR_BATCH: usize = 500;

#[derive(Debug, Clone, Default)]
pub struct Stats {
    pub added: BTreeMap<String, u64>,
    pub removed: u64,
    pub kept: u64,
    pub canonical_changed: u64,
    pub clusters: u64,
    pub aliases_added: u64,
    pub aliases_removed: u64,
    pub embed_skipped: Option<String>,
    pub seconds: f64,
}

impl Stats {
    pub fn written(&self) -> u64 {
        self.added.values().sum::<u64>()
            + self.removed
            + self.canonical_changed
            + self.aliases_added
            + self.aliases_removed
    }
}

#[derive(Debug, Clone, PartialEq)]
struct Candidate {
    rule: &'static str,
    status: &'static str,
    score: f64,
}

fn better(new: &Candidate, old: &Candidate) -> bool {
    (new.status == AUTO && old.status != AUTO)
        || (new.status == old.status && new.score > old.score)
}

fn offer(pairs: &mut BTreeMap<(i64, i64), Candidate>, a: i64, b: i64, candidate: Candidate) {
    if a == b {
        return;
    }
    let key = (a.min(b), a.max(b));
    match pairs.get(&key) {
        Some(old) if !better(&candidate, old) => {}
        _ => {
            pairs.insert(key, candidate);
        }
    }
}

pub fn method(rule: &str) -> String {
    format!("{METHOD}:{rule}")
}

pub async fn run(config_dir: Option<&Path>, url: Option<&str>) -> Result<Stats> {
    let config = crate::config::load(config_dir)?;
    let ontology = crate::embed::run::load_ontology(config_dir, &config)?;
    let rules = crate::rules::load(config_dir, &ontology, &config.ontology.document_class)?;
    let pool = db::pool::connect(url, &config.database.schema, 2).await?;
    let result = run_with(&pool, &config, &ontology, &rules).await;
    pool.close().await;
    result
}

struct Eligible<'a> {
    row: &'a EntityRow,
    key: String,
}

fn eligible<'a>(
    entities: &'a [EntityRow],
    config: &Config,
    ontology: &Ontology,
    rules: &Rules,
) -> Vec<Eligible<'a>> {
    let normaliser = names::Normaliser::new(&config.resolve.strip_words);
    let wanted: Vec<String> = config
        .resolve
        .classes
        .iter()
        .filter_map(|name| ontology.resolve(name).ok())
        .collect();
    entities
        .iter()
        .filter(|e| e.document_id.is_none() && !rules.is_document_class(ontology, &e.class_iri))
        .filter(|e| wanted.is_empty() || wanted.iter().any(|w| ontology.is_a(&e.class_iri, w)))
        .map(|row| Eligible {
            row,
            key: normaliser.key(&row.name),
        })
        .collect()
}

fn name_pairs(
    items: &[Eligible],
    entities: &[EntityRow],
    config: &Config,
    ontology: &Ontology,
    rules: &Rules,
) -> BTreeMap<(i64, i64), Candidate> {
    let mut pairs = BTreeMap::new();
    let by_key: HashMap<(&str, &str), i64> = entities
        .iter()
        .map(|e| ((e.class_iri.as_str(), e.key.as_str()), e.id))
        .collect();
    let ids: HashMap<i64, &Eligible> = items.iter().map(|e| (e.row.id, e)).collect();
    for item in items {
        for (class, key) in names::id_keys(&rules.ids, &item.row.name) {
            let Some(&target) = by_key.get(&(class.as_str(), key.as_str())) else {
                continue;
            };
            let Some(other) = ids.get(&target) else {
                continue;
            };
            if names::compatible(ontology, &item.row.class_iri, &class)
                && names::digits_agree(&item.key, &other.key)
            {
                let candidate = Candidate {
                    rule: "id",
                    status: AUTO,
                    score: 1.0,
                };
                offer(&mut pairs, item.row.id, target, candidate);
            }
        }
    }
    let mut groups: BTreeMap<&str, Vec<&Eligible>> = BTreeMap::new();
    for item in items.iter().filter(|e| !e.key.is_empty()) {
        groups.entry(&item.key).or_default().push(item);
    }
    for group in groups.values() {
        for (i, item) in group.iter().enumerate() {
            let first = group[..i]
                .iter()
                .find(|e| names::compatible(ontology, &e.row.class_iri, &item.row.class_iri));
            if let Some(first) = first {
                let candidate = Candidate {
                    rule: "name",
                    status: AUTO,
                    score: 1.0,
                };
                offer(&mut pairs, first.row.id, item.row.id, candidate);
            }
        }
    }
    for block in names::blocks(items, |e| &e.key, config.resolve.block_chars).values() {
        if block.len() > config.resolve.max_block {
            continue;
        }
        for (n, &i) in block.iter().enumerate() {
            for &j in &block[n + 1..] {
                let (a, b) = (&items[i], &items[j]);
                if a.key == b.key
                    || !names::compatible(ontology, &a.row.class_iri, &b.row.class_iri)
                    || !names::digits_agree(&a.key, &b.key)
                {
                    continue;
                }
                let score = names::similarity(&a.key, &b.key);
                if score >= config.resolve.fuzzy_score {
                    let candidate = Candidate {
                        rule: "fuzzy",
                        status: PROPOSED,
                        score,
                    };
                    offer(&mut pairs, a.row.id, b.row.id, candidate);
                }
            }
        }
    }
    pairs
}

async fn embed_pairs(
    pool: &PgPool,
    items: &[Eligible<'_>],
    config: &Config,
    pairs: &mut BTreeMap<(i64, i64), Candidate>,
) -> Result<Option<String>> {
    let Some((model_id, dimensions)) = db::resolve::current_model(pool).await? else {
        return Ok(Some(
            "no embeddings of [models] embed yet, run embed first".to_string(),
        ));
    };
    if db::resolve::entity_vectors(pool, model_id).await? == 0 {
        return Ok(Some(
            "entities have no vectors of [models] embed, run embed first".to_string(),
        ));
    }
    let keys: HashMap<i64, &str> = items.iter().map(|e| (e.row.id, e.key.as_str())).collect();
    let ids: Vec<i64> = items.iter().map(|e| e.row.id).collect();
    for batch in ids.chunks(NEIGHBOUR_BATCH) {
        let found = db::resolve::neighbours(
            pool,
            model_id,
            dimensions,
            batch,
            config.resolve.max_candidates as i64,
        )
        .await?;
        for (a, b, score) in found {
            let (Some(ka), Some(kb)) = (keys.get(&a), keys.get(&b)) else {
                continue;
            };
            if score < config.resolve.embed_score || !names::digits_agree(ka, kb) {
                continue;
            }
            let candidate = Candidate {
                rule: "embed",
                status: PROPOSED,
                score,
            };
            offer(pairs, a, b, candidate);
        }
    }
    Ok(None)
}

fn new_same_as(key: (i64, i64), candidate: &Candidate) -> NewLink {
    NewLink {
        src_id: key.0,
        dst_id: key.1,
        property: SAME_AS,
        method: method(candidate.rule),
        status: candidate.status,
        confidence: candidate.score,
        props: serde_json::json!({"rule": candidate.rule, "score": candidate.score}),
        document_id: None,
        chunk_id: None,
        span_start: None,
        span_end: None,
    }
}

async fn write_same_as(
    pool: &PgPool,
    pairs: &BTreeMap<(i64, i64), Candidate>,
    stats: &mut Stats,
) -> Result<()> {
    let existing = db::resolve::links(pool, SAME_AS).await?;
    let mut settled = BTreeSet::new();
    let mut remove = Vec::new();
    for link in &existing {
        let key = (link.src_id.min(link.dst_id), link.src_id.max(link.dst_id));
        if link.status == "confirmed" || link.status == "rejected" {
            settled.insert(key);
            stats.kept += 1;
            continue;
        }
        let same = pairs
            .get(&key)
            .is_some_and(|c| method(c.rule) == link.method && c.status == link.status);
        if same && settled.insert(key) {
            stats.kept += 1;
        } else {
            remove.push(link.id);
        }
    }
    let mut add = Vec::new();
    for (key, candidate) in pairs {
        if settled.contains(key) {
            continue;
        }
        add.push(new_same_as(*key, candidate));
        *stats.added.entry(method(candidate.rule)).or_default() += 1;
    }
    stats.removed += remove.len() as u64;
    db::resolve::delete(pool, &remove).await?;
    db::resolve::insert(pool, &add).await?;
    Ok(())
}

pub fn clusters(ids: &[i64], links: &[(i64, i64)]) -> HashMap<i64, i64> {
    let mut parent: HashMap<i64, i64> = ids.iter().map(|&id| (id, id)).collect();
    fn find(parent: &mut HashMap<i64, i64>, id: i64) -> i64 {
        let mut root = id;
        while let Some(&up) = parent.get(&root) {
            if up == root {
                break;
            }
            root = up;
        }
        let mut node = id;
        while let Some(&up) = parent.get(&node) {
            if up == root {
                break;
            }
            parent.insert(node, root);
            node = up;
        }
        root
    }
    for &(a, b) in links {
        if !parent.contains_key(&a) || !parent.contains_key(&b) {
            continue;
        }
        let (ra, rb) = (find(&mut parent, a), find(&mut parent, b));
        if ra != rb {
            parent.insert(ra.max(rb), ra.min(rb));
        }
    }
    ids.iter().map(|&id| (id, find(&mut parent, id))).collect()
}

pub async fn canonical(pool: &PgPool, stats: &mut Stats) -> Result<()> {
    let entities = db::resolve::entities(pool).await?;
    let links: Vec<(i64, i64)> = db::resolve::links(pool, SAME_AS)
        .await?
        .into_iter()
        .filter(|l| l.status == AUTO || l.status == "confirmed")
        .map(|l| (l.src_id, l.dst_id))
        .collect();
    let ids: Vec<i64> = entities.iter().map(|e| e.id).collect();
    let roots = clusters(&ids, &links);
    let canonical: Vec<i64> = ids.iter().map(|id| roots[id]).collect();
    stats.canonical_changed += db::resolve::set_canonical(pool, &ids, &canonical).await?;
    let names: HashMap<i64, &str> = entities.iter().map(|e| (e.id, e.name.as_str())).collect();
    let mut wanted: BTreeSet<(i64, String)> = BTreeSet::new();
    let mut sizes: HashMap<i64, u64> = HashMap::new();
    for (&id, &root) in &roots {
        *sizes.entry(root).or_default() += 1;
        if id != root && names[&id] != names[&root] {
            wanted.insert((root, names[&id].to_string()));
        }
    }
    stats.clusters = sizes.values().filter(|&&n| n > 1).count() as u64;
    let have: BTreeSet<(i64, String)> = db::resolve::resolve_aliases(pool, METHOD)
        .await?
        .into_iter()
        .collect();
    let add: Vec<(i64, String)> = wanted.difference(&have).cloned().collect();
    let remove: Vec<(i64, String)> = have.difference(&wanted).cloned().collect();
    stats.aliases_added += db::resolve::add_aliases(pool, &add, METHOD).await?;
    stats.aliases_removed += remove.len() as u64;
    db::resolve::remove_aliases(pool, &remove, METHOD).await?;
    Ok(())
}

fn link_key(link: &Link) -> (i64, i64, Option<i64>, Option<i64>) {
    (link.src_id, link.dst_id, link.chunk_id, link.document_id)
}

fn new_key(link: &NewLink) -> (i64, i64, Option<i64>, Option<i64>) {
    (link.src_id, link.dst_id, link.chunk_id, link.document_id)
}

async fn write_document_links(
    pool: &PgPool,
    property: &'static str,
    rule: &str,
    wanted: Vec<NewLink>,
    stats: &mut Stats,
) -> Result<()> {
    let method = method(rule);
    let existing: Vec<Link> = db::resolve::links(pool, property)
        .await?
        .into_iter()
        .filter(|l| l.method == method)
        .collect();
    let have: BTreeSet<_> = existing.iter().map(link_key).collect();
    let keys: BTreeSet<_> = wanted.iter().map(new_key).collect();
    let remove: Vec<i64> = existing
        .iter()
        .filter(|l| !keys.contains(&link_key(l)) && (l.status == AUTO || l.status == PROPOSED))
        .map(|l| l.id)
        .collect();
    stats.kept += existing.len() as u64 - remove.len() as u64;
    let add: Vec<NewLink> = wanted
        .into_iter()
        .filter(|l| !have.contains(&new_key(l)))
        .collect();
    *stats.added.entry(method).or_default() += add.len() as u64;
    stats.removed += remove.len() as u64;
    db::resolve::delete(pool, &remove).await?;
    db::resolve::insert(pool, &add).await?;
    Ok(())
}

async fn document_links(
    pool: &PgPool,
    ontology: &Ontology,
    rules: &Rules,
    stats: &mut Stats,
) -> Result<()> {
    let classes: Vec<String> = ontology
        .classes
        .keys()
        .filter(|iri| rules.is_document_class(ontology, iri))
        .cloned()
        .collect();
    let references = db::resolve::references(pool, &classes)
        .await?
        .into_iter()
        .map(|r| NewLink {
            src_id: r.src_id,
            dst_id: r.dst_id,
            property: REFERENCES,
            method: method("references"),
            status: AUTO,
            confidence: r.confidence,
            props: serde_json::json!({}),
            document_id: None,
            chunk_id: Some(r.chunk_id),
            span_start: Some(r.span_start),
            span_end: Some(r.span_end),
        })
        .collect();
    write_document_links(pool, REFERENCES, "references", references, stats).await?;
    let documents = db::resolve::document_entities(pool).await?;
    let revisions = documents::chains(&documents, &rules.revisions)
        .into_iter()
        .map(|(newer, older)| NewLink {
            src_id: documents[newer].entity_id,
            dst_id: documents[older].entity_id,
            property: REPLACES,
            method: method("revisions"),
            status: AUTO,
            confidence: 1.0,
            props: serde_json::json!({}),
            document_id: Some(documents[newer].document_id),
            chunk_id: None,
            span_start: None,
            span_end: None,
        })
        .collect();
    write_document_links(pool, REPLACES, "revisions", revisions, stats).await
}

pub async fn run_with(
    pool: &PgPool,
    config: &Config,
    ontology: &Ontology,
    rules: &Rules,
) -> Result<Stats> {
    let started = Instant::now();
    let mut stats = Stats::default();
    let entities = db::resolve::entities(pool).await?;
    let items = eligible(&entities, config, ontology, rules);
    let mut pairs = name_pairs(&items, &entities, config, ontology, rules);
    stats.embed_skipped = embed_pairs(pool, &items, config, &mut pairs).await?;
    write_same_as(pool, &pairs, &mut stats).await?;
    canonical(pool, &mut stats).await?;
    document_links(pool, ontology, rules, &mut stats).await?;
    stats.seconds = started.elapsed().as_secs_f64();
    Ok(stats)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clusters_take_the_lowest_id() {
        let roots = clusters(&[1, 2, 3, 4, 5], &[(4, 2), (2, 5), (9, 1)]);
        assert_eq!(roots[&5], 2);
        assert_eq!(roots[&4], 2);
        assert_eq!(roots[&1], 1);
        assert_eq!(roots[&3], 3);
    }

    #[test]
    fn auto_beats_proposed_and_higher_scores_win() {
        let mut pairs = BTreeMap::new();
        let fuzzy = Candidate {
            rule: "fuzzy",
            status: PROPOSED,
            score: 0.95,
        };
        offer(&mut pairs, 2, 1, fuzzy.clone());
        offer(
            &mut pairs,
            1,
            2,
            Candidate {
                score: 0.93,
                ..fuzzy.clone()
            },
        );
        assert_eq!(pairs[&(1, 2)].score, 0.95);
        let name = Candidate {
            rule: "name",
            status: AUTO,
            score: 1.0,
        };
        offer(&mut pairs, 1, 2, name.clone());
        offer(&mut pairs, 1, 2, fuzzy);
        assert_eq!(pairs[&(1, 2)], name);
        offer(&mut pairs, 3, 3, name);
        assert_eq!(pairs.len(), 1);
    }
}
