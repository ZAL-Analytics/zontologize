pub mod documents;
pub mod names;
pub mod similar;

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::Path;
use std::time::Instant;

use anyhow::Result;
use serde::Serialize;
use sqlx::{PgConnection, PgPool};

use crate::config::Config;
use crate::db;
use crate::db::resolve::{EntityRow, Link, NewLink, REFERENCES, REPLACES, SAME_AS};
use crate::ontology::Ontology;
use crate::rules::Rules;

pub const METHOD: &str = "resolve";
const AUTO: &str = "auto";
const PROPOSED: &str = "proposed";

#[derive(Debug, Clone, Default, Serialize)]
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
    let ontology = crate::ontology::load_or_empty(config_dir, &config.ontology)?;
    let rules = crate::rules::load(config_dir, &ontology, &config.ontology.document_class)?;
    let lock = db::runs::lock(url, &config.database.schema).await?;
    let pool = db::pool::connect(url, &config.database.schema, 2).await?;
    let hash = crate::config::hash(&config)?;
    let work = run_with(&pool, &config, &ontology, &rules);
    let result = db::runs::track(&pool, &hash, METHOD, work).await;
    pool.close().await;
    lock.release().await;
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
    exact_pairs(items, ontology, &mut pairs);
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

fn exact_pairs(
    items: &[Eligible],
    ontology: &Ontology,
    pairs: &mut BTreeMap<(i64, i64), Candidate>,
) {
    let mut groups: BTreeMap<&str, Vec<&Eligible>> = BTreeMap::new();
    for item in items.iter().filter(|e| !e.key.is_empty()) {
        groups.entry(&item.key).or_default().push(item);
    }
    for group in groups.values() {
        let mut firsts: Vec<&Eligible> = Vec::new();
        for item in group {
            let first = firsts
                .iter()
                .find(|e| names::compatible(ontology, &e.row.class_iri, &item.row.class_iri))
                .copied();
            if !firsts.iter().any(|e| e.row.class_iri == item.row.class_iri) {
                firsts.push(item);
            }
            if let Some(first) = first {
                let candidate = Candidate {
                    rule: "name",
                    status: AUTO,
                    score: 1.0,
                };
                offer(pairs, first.row.id, item.row.id, candidate);
            }
        }
    }
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
    let mut found = Vec::new();
    for batch in ids.chunks(config.resolve.neighbour_batch) {
        let k = config.resolve.max_candidates as i64;
        found.extend(db::resolve::neighbours(pool, model_id, dimensions, batch, k).await?);
    }
    let seen: BTreeSet<(i64, i64)> = found
        .iter()
        .map(|&(a, b, _)| (a.min(b), a.max(b)))
        .collect();
    let missed: Vec<(i64, i64)> = db::resolve::links(pool, SAME_AS)
        .await?
        .into_iter()
        .filter(|l| l.status == PROPOSED && l.method == method("embed"))
        .map(|l| (l.src_id.min(l.dst_id), l.src_id.max(l.dst_id)))
        .filter(|key| !seen.contains(key))
        .collect();
    found.extend(db::resolve::pair_scores(pool, model_id, dimensions, &missed).await?);
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
    Ok(None)
}

type Sources = HashMap<i64, (Option<i64>, Option<i64>)>;

fn new_same_as(key: (i64, i64), candidate: &Candidate, sources: &Sources) -> NewLink {
    let of = |id: i64| sources.get(&id).copied().unwrap_or_default();
    let (a, b) = (of(key.0), of(key.1));
    let chunk_id = a.0.or(b.0);
    let document_id = if chunk_id.is_none() {
        a.1.or(b.1)
    } else {
        None
    };
    let chunks: BTreeSet<i64> = [a.0, b.0].into_iter().flatten().collect();
    NewLink {
        src_id: key.0,
        dst_id: key.1,
        property: SAME_AS.to_string(),
        method: method(candidate.rule),
        status: candidate.status,
        confidence: candidate.score,
        props: serde_json::json!({
            "rule": candidate.rule,
            "score": candidate.score,
            "chunks": chunks,
        }),
        document_id,
        chunk_id,
        span_start: None,
        span_end: None,
    }
}

fn same_link(link: &Link, wanted: &NewLink) -> bool {
    wanted.method == link.method
        && wanted.status == link.status
        && wanted.chunk_id == link.chunk_id
        && wanted.document_id == link.document_id
}

async fn write_same_as(
    conn: &mut PgConnection,
    pairs: &BTreeMap<(i64, i64), Candidate>,
    stats: &mut Stats,
) -> Result<()> {
    let ids: Vec<i64> = pairs
        .keys()
        .flat_map(|&(a, b)| [a, b])
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();
    let sources = db::resolve::provenance(&mut *conn, &ids).await?;
    let wanted: BTreeMap<(i64, i64), NewLink> = pairs
        .iter()
        .map(|(key, candidate)| (*key, new_same_as(*key, candidate, &sources)))
        .collect();
    let existing = db::resolve::links(&mut *conn, SAME_AS).await?;
    let mut settled = BTreeSet::new();
    let mut remove = Vec::new();
    for link in &existing {
        let key = (link.src_id.min(link.dst_id), link.src_id.max(link.dst_id));
        if link.status == "confirmed" || link.status == "rejected" {
            settled.insert(key);
            stats.kept += 1;
            continue;
        }
        if !link.method.starts_with(&method("")) {
            settled.insert(key);
            continue;
        }
        let same = wanted.get(&key).is_some_and(|w| same_link(link, w));
        if same && settled.insert(key) {
            stats.kept += 1;
        } else {
            remove.push(link.id);
        }
    }
    let mut add = Vec::new();
    for (key, link) in wanted {
        if settled.contains(&key) {
            continue;
        }
        *stats.added.entry(link.method.clone()).or_default() += 1;
        add.push(link);
    }
    stats.removed += remove.len() as u64;
    db::resolve::delete(&mut *conn, &remove).await?;
    db::resolve::insert(&mut *conn, &add).await?;
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
    let mut tx = pool.begin().await?;
    let entities = db::resolve::entities(&mut *tx).await?;
    canonical_with(&mut tx, &entities, stats).await?;
    tx.commit().await?;
    Ok(())
}

fn changed_roots(entities: &[EntityRow], roots: &HashMap<i64, i64>) -> (Vec<i64>, Vec<i64>) {
    entities
        .iter()
        .filter(|e| e.canonical_id != Some(roots[&e.id]))
        .map(|e| (e.id, roots[&e.id]))
        .unzip()
}

async fn canonical_with(
    conn: &mut PgConnection,
    entities: &[EntityRow],
    stats: &mut Stats,
) -> Result<()> {
    let links: Vec<(i64, i64)> = db::resolve::links(&mut *conn, SAME_AS)
        .await?
        .into_iter()
        .filter(|l| l.status == AUTO || l.status == "confirmed")
        .map(|l| (l.src_id, l.dst_id))
        .collect();
    let ids: Vec<i64> = entities.iter().map(|e| e.id).collect();
    let roots = clusters(&ids, &links);
    let (changed, canonical) = changed_roots(entities, &roots);
    stats.canonical_changed += db::resolve::set_canonical(&mut *conn, &changed, &canonical).await?;
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
    let have: BTreeSet<(i64, String)> = db::resolve::resolve_aliases(&mut *conn, METHOD)
        .await?
        .into_iter()
        .collect();
    let add: Vec<(i64, String)> = wanted.difference(&have).cloned().collect();
    let remove: Vec<(i64, String)> = have.difference(&wanted).cloned().collect();
    stats.aliases_added += db::resolve::add_aliases(&mut *conn, &add, METHOD).await?;
    stats.aliases_removed += remove.len() as u64;
    db::resolve::remove_aliases(&mut *conn, &remove, METHOD).await?;
    Ok(())
}

fn link_key(link: &Link) -> (i64, i64, Option<i64>, Option<i64>) {
    (link.src_id, link.dst_id, link.chunk_id, link.document_id)
}

fn new_key(link: &NewLink) -> (i64, i64, Option<i64>, Option<i64>) {
    (link.src_id, link.dst_id, link.chunk_id, link.document_id)
}

async fn write_document_links(
    conn: &mut PgConnection,
    property: &'static str,
    rule: &str,
    wanted: Vec<NewLink>,
    stats: &mut Stats,
) -> Result<()> {
    let method = method(rule);
    let existing: Vec<Link> = db::resolve::links(&mut *conn, property)
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
    db::resolve::delete(&mut *conn, &remove).await?;
    db::resolve::insert(&mut *conn, &add).await?;
    Ok(())
}

async fn document_links(
    conn: &mut PgConnection,
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
    let references = db::resolve::references(&mut *conn, &classes)
        .await?
        .into_iter()
        .map(|r| NewLink {
            src_id: r.src_id,
            dst_id: r.dst_id,
            property: REFERENCES.to_string(),
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
    write_document_links(conn, REFERENCES, "references", references, stats).await?;
    let documents = db::resolve::document_entities(&mut *conn).await?;
    let revisions = documents::chains(&documents, &rules.revisions)
        .into_iter()
        .map(|(newer, older)| NewLink {
            src_id: documents[newer].entity_id,
            dst_id: documents[older].entity_id,
            property: REPLACES.to_string(),
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
    write_document_links(conn, REPLACES, "revisions", revisions, stats).await
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
    let mut tx = pool.begin().await?;
    write_same_as(&mut tx, &pairs, &mut stats).await?;
    tx.commit().await?;
    let mut tx = pool.begin().await?;
    canonical_with(&mut tx, &entities, &mut stats).await?;
    tx.commit().await?;
    let mut tx = pool.begin().await?;
    document_links(&mut tx, ontology, rules, &mut stats).await?;
    tx.commit().await?;
    let mut tx = pool.begin().await?;
    similar::write(&mut tx, config, ontology, &mut stats).await?;
    tx.commit().await?;
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
    fn same_as_links_take_a_mention_chunk_of_either_side() {
        let candidate = Candidate {
            rule: "name",
            status: AUTO,
            score: 1.0,
        };
        let sources: Sources = HashMap::from([
            (1, (None, Some(50))),
            (2, (Some(7), Some(51))),
            (3, (Some(9), None)),
        ]);
        let link = new_same_as((1, 2), &candidate, &sources);
        assert_eq!((link.chunk_id, link.document_id), (Some(7), None));
        assert_eq!(link.props["chunks"], serde_json::json!([7]));
        let link = new_same_as((2, 3), &candidate, &sources);
        assert_eq!((link.chunk_id, link.document_id), (Some(7), None));
        assert_eq!(link.props["chunks"], serde_json::json!([7, 9]));
        let link = new_same_as((1, 4), &candidate, &sources);
        assert_eq!((link.chunk_id, link.document_id), (None, Some(50)));
        let link = new_same_as((4, 5), &candidate, &sources);
        assert_eq!((link.chunk_id, link.document_id), (None, None));
    }

    #[test]
    fn only_changed_canonical_ids_are_written() {
        let row = |id: i64, canonical_id: Option<i64>| EntityRow {
            id,
            class_iri: "c".to_string(),
            key: id.to_string(),
            name: id.to_string(),
            document_id: None,
            canonical_id,
        };
        let entities = [
            row(1, Some(1)),
            row(2, None),
            row(3, Some(3)),
            row(4, Some(1)),
        ];
        let roots = clusters(&[1, 2, 3, 4], &[(1, 3)]);
        assert_eq!(
            changed_roots(&entities, &roots),
            (vec![2, 3, 4], vec![2, 1, 4])
        );
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

    #[test]
    fn equal_names_pair_with_the_first_compatible_entity() {
        let ontology = crate::rules::tests::ontology();
        let class = |name: &str| format!("http://example.org/onto#{name}");
        let rows: Vec<EntityRow> = ["Project", "Report", "Document", "Report", "Project"]
            .iter()
            .enumerate()
            .map(|(i, name)| EntityRow {
                id: i as i64 + 1,
                class_iri: class(name),
                key: format!("k{i}"),
                name: "same".to_string(),
                document_id: None,
                canonical_id: None,
            })
            .collect();
        let items: Vec<Eligible> = rows
            .iter()
            .map(|row| Eligible {
                row,
                key: "same".to_string(),
            })
            .collect();
        let mut pairs = BTreeMap::new();
        exact_pairs(&items, &ontology, &mut pairs);
        let found: Vec<(i64, i64)> = pairs.keys().copied().collect();
        assert_eq!(found, [(1, 5), (2, 3), (2, 4)]);
    }
}
