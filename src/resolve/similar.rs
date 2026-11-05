use std::collections::{BTreeMap, BTreeSet, HashMap};

use anyhow::{Result, bail};
use sqlx::PgPool;

use super::{PROPOSED, Stats, method};
use crate::config::SimilarConfig;
use crate::db;
use crate::db::resolve::{NewLink, ValueRow};
use crate::ontology::Ontology;

pub const RULE: &str = "similar";

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Range {
    pub low: f64,
    pub high: f64,
}

#[derive(Debug, Clone)]
pub struct Cluster {
    pub id: i64,
    pub class: String,
    pub values: BTreeMap<String, Range>,
    pub best: BTreeMap<String, (f64, i64)>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Pair {
    pub a: i64,
    pub b: i64,
    pub score: f64,
    pub shared: Vec<String>,
}

pub struct Settings {
    pub tolerance: BTreeMap<String, f64>,
    pub default_tolerance: f64,
    pub weights: BTreeMap<String, f64>,
    pub min_shared: usize,
    pub min_score: f64,
    pub max_links: usize,
}

pub fn property_score(a: Range, b: Range, tolerance: f64) -> f64 {
    if a.low <= b.high && b.low <= a.high {
        return 1.0;
    }
    let (lower, upper) = if a.high < b.low {
        (a.high, b.low)
    } else {
        (b.high, a.low)
    };
    if lower <= 0.0 {
        return 0.0;
    }
    let gap = (upper / lower).ln();
    (1.0 - gap / (1.0 + tolerance).ln()).max(0.0)
}

pub fn score(a: &Cluster, b: &Cluster, settings: &Settings) -> Option<Pair> {
    let mut total = 0.0;
    let mut weights = 0.0;
    let mut shared = Vec::new();
    for (property, range) in &a.values {
        let Some(other) = b.values.get(property) else {
            continue;
        };
        let tolerance = settings
            .tolerance
            .get(property)
            .copied()
            .unwrap_or(settings.default_tolerance);
        let weight = settings.weights.get(property).copied().unwrap_or(1.0);
        total += weight * property_score(*range, *other, tolerance);
        weights += weight;
        shared.push(property.clone());
    }
    if shared.len() < settings.min_shared || weights <= 0.0 {
        return None;
    }
    let score = total / weights;
    (score >= settings.min_score).then(|| Pair {
        a: a.id.min(b.id),
        b: a.id.max(b.id),
        score,
        shared,
    })
}

pub fn clusters(rows: &[ValueRow]) -> Vec<Cluster> {
    let mut map: BTreeMap<i64, Cluster> = BTreeMap::new();
    for row in rows {
        let (Some(low), Some(high)) = (row.si_low.or(row.si_high), row.si_high.or(row.si_low))
        else {
            continue;
        };
        let cluster = map.entry(row.cluster_id).or_insert_with(|| Cluster {
            id: row.cluster_id,
            class: row.cluster_class.clone(),
            values: BTreeMap::new(),
            best: BTreeMap::new(),
        });
        cluster
            .values
            .entry(row.property_iri.clone())
            .and_modify(|r| {
                r.low = r.low.min(low);
                r.high = r.high.max(high);
            })
            .or_insert(Range { low, high });
        let best = cluster
            .best
            .entry(row.property_iri.clone())
            .or_insert((row.confidence, row.chunk_id));
        if row.confidence > best.0 {
            *best = (row.confidence, row.chunk_id);
        }
    }
    map.into_values().collect()
}

pub fn pairs(clusters: &[Cluster], settings: &Settings, max_block: usize) -> (Vec<Pair>, usize) {
    let mut by_class: BTreeMap<&str, Vec<&Cluster>> = BTreeMap::new();
    for cluster in clusters
        .iter()
        .filter(|c| c.values.len() >= settings.min_shared)
    {
        by_class.entry(&cluster.class).or_default().push(cluster);
    }
    let mut skipped = 0;
    let mut per_cluster: HashMap<i64, Vec<Pair>> = HashMap::new();
    for members in by_class.values() {
        if members.len() > max_block {
            skipped += members.len();
            continue;
        }
        for (i, a) in members.iter().enumerate() {
            for b in &members[i + 1..] {
                if let Some(pair) = score(a, b, settings) {
                    per_cluster.entry(pair.a).or_default().push(pair.clone());
                    per_cluster.entry(pair.b).or_default().push(pair);
                }
            }
        }
    }
    let mut kept: BTreeMap<(i64, i64), Pair> = BTreeMap::new();
    for mut list in per_cluster.into_values() {
        list.sort_by(|x, y| {
            y.score
                .total_cmp(&x.score)
                .then_with(|| (x.a, x.b).cmp(&(y.a, y.b)))
        });
        for pair in list.into_iter().take(settings.max_links) {
            kept.insert((pair.a, pair.b), pair);
        }
    }
    (kept.into_values().collect(), skipped)
}

pub fn settings(config: &SimilarConfig, ontology: &Ontology) -> Result<Settings> {
    let mut weights = BTreeMap::new();
    for (name, weight) in &config.weights {
        let Ok(property) = ontology.property(name) else {
            bail!("resolve.similar.weights.{name}: not a selected property of the ontology");
        };
        weights.insert(property.iri.clone(), *weight);
    }
    let tolerance = ontology
        .properties
        .values()
        .filter_map(|p| p.tolerance.map(|t| (p.iri.clone(), t)))
        .collect();
    Ok(Settings {
        tolerance,
        default_tolerance: config.tolerance,
        weights,
        min_shared: config.min_shared,
        min_score: config.min_score,
        max_links: config.max_links,
    })
}

pub async fn write(
    pool: &PgPool,
    config: &crate::config::Config,
    ontology: &Ontology,
    stats: &mut Stats,
) -> Result<()> {
    let settings = settings(&config.resolve.similar, ontology)?;
    let wanted_classes: Vec<String> = config
        .resolve
        .classes
        .iter()
        .filter_map(|name| ontology.resolve(name).ok())
        .collect();
    let rows: Vec<ValueRow> = db::resolve::values(pool)
        .await?
        .into_iter()
        .filter(|r| {
            wanted_classes.is_empty()
                || wanted_classes
                    .iter()
                    .any(|w| ontology.is_a(&r.cluster_class, w))
        })
        .collect();
    let clusters = clusters(&rows);
    let (pairs, skipped) = pairs(&clusters, &settings, config.resolve.max_block);
    if skipped > 0 {
        println!(
            "resolve: similarity skipped {skipped} entities in classes larger than resolve.max_block"
        );
    }
    let by_id: HashMap<i64, &Cluster> = clusters.iter().map(|c| (c.id, c)).collect();
    let property = &config.resolve.similar.property;
    let method = method(RULE);
    let mut wanted = Vec::new();
    for pair in &pairs {
        let Some(source) = by_id.get(&pair.a) else {
            continue;
        };
        let chunk = pair
            .shared
            .iter()
            .filter_map(|p| source.best.get(p))
            .max_by(|x, y| x.0.total_cmp(&y.0).then_with(|| y.1.cmp(&x.1)))
            .map(|(_, chunk)| *chunk);
        wanted.push(NewLink {
            src_id: pair.a,
            dst_id: pair.b,
            property: property.clone(),
            method: method.clone(),
            status: PROPOSED,
            confidence: rounded(pair.score),
            props: serde_json::json!({"score": rounded(pair.score), "shared": pair.shared}),
            document_id: None,
            chunk_id: chunk,
            span_start: None,
            span_end: None,
        });
    }
    let existing = db::resolve::links(pool, property).await?;
    let keep: BTreeSet<(i64, i64, String)> = wanted
        .iter()
        .map(|l| (l.src_id, l.dst_id, format!("{:.4}", l.confidence)))
        .collect();
    let mut have: BTreeSet<(i64, i64)> = BTreeSet::new();
    let mut remove = Vec::new();
    let confidences = db::resolve::confidences(pool, property).await?;
    for link in existing.iter().filter(|l| l.method == method) {
        let confidence = confidences.get(&link.id).copied().unwrap_or(0.0);
        let current = keep.contains(&(link.src_id, link.dst_id, format!("{confidence:.4}")));
        if current || !(link.status == super::AUTO || link.status == PROPOSED) {
            have.insert((link.src_id, link.dst_id));
        } else {
            remove.push(link.id);
        }
    }
    let add: Vec<NewLink> = wanted
        .into_iter()
        .filter(|l| !have.contains(&(l.src_id, l.dst_id)))
        .collect();
    stats.kept += have.len() as u64;
    stats.removed += remove.len() as u64;
    *stats.added.entry(method).or_default() += add.len() as u64;
    db::resolve::delete(pool, &remove).await?;
    db::resolve::insert(pool, &add).await
}

fn rounded(score: f64) -> f64 {
    (score * 10_000.0).round() / 10_000.0
}

#[cfg(test)]
mod tests {
    use super::*;

    fn range(low: f64, high: f64) -> Range {
        Range { low, high }
    }

    fn cluster(id: i64, values: &[(&str, f64, f64)]) -> Cluster {
        Cluster {
            id,
            class: "pump".to_string(),
            values: values
                .iter()
                .map(|(p, l, h)| (p.to_string(), range(*l, *h)))
                .collect(),
            best: BTreeMap::new(),
        }
    }

    fn settings() -> Settings {
        Settings {
            tolerance: BTreeMap::from([("power".to_string(), 0.1)]),
            default_tolerance: 0.2,
            weights: BTreeMap::from([("power".to_string(), 3.0)]),
            min_shared: 2,
            min_score: 0.5,
            max_links: 1,
        }
    }

    #[test]
    fn overlapping_ranges_score_one_and_gaps_fall_off() {
        assert_eq!(property_score(range(8.0, 10.0), range(9.0, 12.0), 0.2), 1.0);
        assert_eq!(
            property_score(range(10.0, 10.0), range(10.0, 10.0), 0.2),
            1.0
        );
        assert_eq!(
            property_score(range(100.0, 100.0), range(120.0, 120.0), 0.2),
            0.0
        );
        assert_eq!(
            property_score(range(100.0, 100.0), range(130.0, 130.0), 0.2),
            0.0
        );
        assert_eq!(
            property_score(range(130.0, 130.0), range(100.0, 100.0), 0.2),
            0.0
        );
        let close = property_score(range(100.0, 100.0), range(105.0, 105.0), 0.2);
        assert!((close - (1.0 - 1.05f64.ln() / 1.2f64.ln())).abs() < 1e-12);
        assert_eq!(property_score(range(-5.0, -1.0), range(1.0, 2.0), 0.2), 0.0);
        assert_eq!(property_score(range(-5.0, 1.0), range(0.0, 2.0), 0.2), 1.0);
    }

    #[test]
    fn weights_tolerance_and_min_shared() {
        let a = cluster(1, &[("power", 100.0, 100.0), ("pressure", 10.0, 10.0)]);
        let b = cluster(2, &[("power", 100.0, 100.0), ("pressure", 12.4, 12.4)]);
        let pair = score(&a, &b, &settings()).unwrap();
        assert_eq!(pair.shared, ["power", "pressure"]);
        assert!((pair.score - 0.75).abs() < 1e-12, "{}", pair.score);
        let c = cluster(3, &[("power", 100.0, 100.0)]);
        assert!(score(&a, &c, &settings()).is_none());
        let d = cluster(4, &[("power", 111.0, 111.0), ("pressure", 10.0, 10.0)]);
        assert!(score(&a, &d, &settings()).is_none());
        let loose = Settings {
            min_score: 0.2,
            ..settings()
        };
        assert!((score(&a, &d, &loose).unwrap().score - 0.25).abs() < 1e-12);
        let strict = Settings {
            min_score: 0.9,
            ..settings()
        };
        assert!(score(&a, &b, &strict).is_none());
    }

    #[test]
    fn top_links_per_cluster_and_large_classes_are_skipped() {
        let a = cluster(1, &[("power", 100.0, 100.0), ("pressure", 10.0, 10.0)]);
        let b = cluster(2, &[("power", 100.0, 100.0), ("pressure", 10.0, 10.0)]);
        let c = cluster(3, &[("power", 100.0, 100.0), ("pressure", 10.5, 10.5)]);
        let (pairs, skipped) = pairs(&[a.clone(), b.clone(), c.clone()], &settings(), 10);
        assert_eq!(skipped, 0);
        let keys: Vec<(i64, i64)> = pairs.iter().map(|p| (p.a, p.b)).collect();
        assert_eq!(keys, [(1, 2), (1, 3)]);
        let (pairs, skipped) = pairs_for_block(&[a, b, c]);
        assert!(pairs.is_empty());
        assert_eq!(skipped, 3);
    }

    fn pairs_for_block(clusters: &[Cluster]) -> (Vec<Pair>, usize) {
        pairs(clusters, &settings(), 2)
    }

    #[test]
    fn clusters_take_the_union_of_ranges() {
        let row = |cluster: i64,
                   property: &str,
                   low: Option<f64>,
                   high: Option<f64>,
                   chunk: i64,
                   confidence: f64| ValueRow {
            cluster_id: cluster,
            cluster_class: "pump".to_string(),
            property_iri: property.to_string(),
            si_low: low,
            si_high: high,
            chunk_id: chunk,
            confidence,
        };
        let rows = vec![
            row(1, "power", Some(100.0), Some(100.0), 10, 0.5),
            row(1, "power", Some(90.0), Some(95.0), 11, 0.9),
            row(1, "pressure", None, Some(50.0), 12, 0.7),
            row(2, "power", None, None, 13, 0.7),
        ];
        let clusters = clusters(&rows);
        assert_eq!(clusters.len(), 1);
        assert_eq!(clusters[0].values["power"], range(90.0, 100.0));
        assert_eq!(clusters[0].values["pressure"], range(50.0, 50.0));
        assert_eq!(clusters[0].best["power"], (0.9, 11));
    }
}
