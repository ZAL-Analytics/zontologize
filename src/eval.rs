use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::Path;

use anyhow::{Result, anyhow, bail};

use crate::db;
use crate::ontology::Ontology;

#[derive(Debug, Clone, PartialEq)]
pub struct WorldName {
    pub name: String,
    pub class_name: String,
    pub class: String,
    pub variants: BTreeSet<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Entity {
    pub id: i64,
    pub cluster: i64,
    pub class: String,
    pub names: Vec<String>,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Score {
    pub class: String,
    pub world: usize,
    pub found: usize,
    pub duplicates: usize,
    pub extra_clusters: usize,
    pub wrong_merges: usize,
    pub matched: usize,
    pub right_class: usize,
}

impl Score {
    pub fn duplicate_rate(&self) -> f64 {
        ratio(self.duplicates, self.found)
    }

    pub fn class_accuracy(&self) -> f64 {
        ratio(self.right_class, self.matched)
    }
}

fn ratio(part: usize, whole: usize) -> f64 {
    if whole == 0 {
        0.0
    } else {
        part as f64 / whole as f64
    }
}

fn normalise(text: &str) -> String {
    text.split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase()
}

pub fn parse_world(name: &str, text: &str, ontology: &Ontology) -> Result<Vec<WorldName>> {
    let mut reader = csv::ReaderBuilder::new()
        .trim(csv::Trim::All)
        .from_reader(text.as_bytes());
    let mut names: Vec<WorldName> = Vec::new();
    let mut index: HashMap<String, usize> = HashMap::new();
    let mut owner: HashMap<String, String> = HashMap::new();
    for record in reader.records() {
        let record = record.map_err(|e| anyhow!("{name}: {e}"))?;
        let line = record.position().map_or(0, |p| p.line());
        let [world_name, variant, class_name] = [0, 1, 2].map(|i| record.get(i).unwrap_or(""));
        if world_name.is_empty() || variant.is_empty() || class_name.is_empty() {
            bail!("{name} line {line}: name, variant and class must not be empty");
        }
        let class = ontology
            .class(class_name)
            .map_err(|e| anyhow!("{name} line {line}: {e}"))?
            .iri
            .clone();
        let key = normalise(variant);
        let first = owner
            .entry(key.clone())
            .or_insert_with(|| world_name.to_string());
        if first != world_name {
            bail!(
                "{name} line {line}: variant {variant:?} is listed under {first:?} and {world_name:?}"
            );
        }
        let at = *index.entry(world_name.to_string()).or_insert_with(|| {
            names.push(WorldName {
                name: world_name.to_string(),
                class_name: class_name.to_string(),
                class,
                variants: BTreeSet::new(),
            });
            names.len() - 1
        });
        names[at].variants.insert(key);
    }
    Ok(names)
}

pub fn score(world: &[WorldName], entities: &[Entity], ontology: &Ontology) -> Vec<Score> {
    let mut by_variant: HashMap<&str, usize> = HashMap::new();
    for (i, name) in world.iter().enumerate() {
        for variant in &name.variants {
            by_variant.insert(variant, i);
        }
    }
    let mut clusters_of: Vec<BTreeSet<i64>> = vec![BTreeSet::new(); world.len()];
    let mut names_in: BTreeMap<i64, BTreeSet<usize>> = BTreeMap::new();
    let mut matches: Vec<(usize, bool)> = Vec::new();
    for entity in entities {
        let hits: BTreeSet<usize> = entity
            .names
            .iter()
            .filter_map(|n| by_variant.get(normalise(n).as_str()).copied())
            .collect();
        for &i in &hits {
            clusters_of[i].insert(entity.cluster);
            names_in.entry(entity.cluster).or_default().insert(i);
            matches.push((i, ontology.is_a(&entity.class, &world[i].class)));
        }
    }
    let mut scores: Vec<Score> = Vec::new();
    let mut slot: HashMap<&str, usize> = HashMap::new();
    for name in world {
        slot.entry(&name.class_name).or_insert_with(|| {
            scores.push(Score {
                class: name.class_name.clone(),
                ..Score::default()
            });
            scores.len() - 1
        });
    }
    for (i, name) in world.iter().enumerate() {
        let score = &mut scores[slot[name.class_name.as_str()]];
        score.world += 1;
        let clusters = clusters_of[i].len();
        if clusters > 0 {
            score.found += 1;
            score.extra_clusters += clusters - 1;
            if clusters > 1 {
                score.duplicates += 1;
            }
        }
    }
    for (i, right) in matches {
        let score = &mut scores[slot[world[i].class_name.as_str()]];
        score.matched += 1;
        score.right_class += usize::from(right);
    }
    let mut total = Score {
        class: "total".to_string(),
        ..Score::default()
    };
    for members in names_in.values().filter(|m| m.len() > 1) {
        let classes: BTreeSet<usize> = members
            .iter()
            .map(|&i| slot[world[i].class_name.as_str()])
            .collect();
        for class in classes {
            scores[class].wrong_merges += 1;
        }
        total.wrong_merges += 1;
    }
    for score in &scores {
        total.world += score.world;
        total.found += score.found;
        total.duplicates += score.duplicates;
        total.extra_clusters += score.extra_clusters;
        total.matched += score.matched;
        total.right_class += score.right_class;
    }
    scores.push(total);
    scores
}

pub fn table(scores: &[Score]) -> String {
    let width = scores
        .iter()
        .map(|s| s.class.len())
        .max()
        .unwrap_or(5)
        .max(5);
    let mut out = format!(
        "{:<width$}  {:>6}  {:>6}  {:>9}  {:>6}  {:>12}  {:>9}\n",
        "class", "world", "found", "dup rate", "extra", "wrong merges", "class acc"
    );
    for s in scores {
        out.push_str(&format!(
            "{:<width$}  {:>6}  {:>6}  {:>8.1}%  {:>6}  {:>12}  {:>8.1}%\n",
            s.class,
            s.world,
            s.found,
            s.duplicate_rate() * 100.0,
            s.extra_clusters,
            s.wrong_merges,
            s.class_accuracy() * 100.0
        ));
    }
    out
}

pub async fn run(config_dir: Option<&Path>, url: Option<&str>) -> Result<Vec<Score>> {
    let config = crate::config::load(config_dir)?;
    if config.eval.world.is_empty() {
        bail!("[eval] world is empty; set it to the world list (name,variant,class)");
    }
    let path = crate::scan::resolve_root(config_dir, &config.eval.world);
    let text = std::fs::read_to_string(&path)
        .map_err(|e| anyhow!("reading {} ([eval] world): {e}", path.display()))?;
    let ontology = crate::ontology::load_or_empty(config_dir, &config.ontology)?;
    let world = parse_world(&path.display().to_string(), &text, &ontology)?;
    let pool = db::pool::connect(url, &config.database.schema, 2).await?;
    let rows = db::entities::with_clusters(&pool).await;
    pool.close().await;
    let entities: Vec<Entity> = rows?
        .into_iter()
        .map(|(id, cluster, class, names)| Entity {
            id,
            cluster,
            class,
            names,
        })
        .collect();
    Ok(score(&world, &entities, &ontology))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::OntologyConfig;

    const TTL: &str = "@prefix ex: <https://example.org/x#> .
@prefix owl: <http://www.w3.org/2002/07/owl#> .
@prefix rdfs: <http://www.w3.org/2000/01/rdf-schema#> .
ex:Org a owl:Class .
ex:Customer a owl:Class ; rdfs:subClassOf ex:Org .
ex:Site a owl:Class .
";

    fn ontology() -> Ontology {
        let config = OntologyConfig {
            files: Vec::new(),
            languages: vec!["en".to_string()],
            include: Vec::new(),
            exclude: Vec::new(),
            document_class: String::new(),
        };
        crate::ontology::load_sources(&[("x.ttl".to_string(), TTL.as_bytes().to_vec())], &config)
            .ontology
    }

    fn iri(local: &str) -> String {
        format!("https://example.org/x#{local}")
    }

    fn entity(id: i64, cluster: i64, class: &str, names: &[&str]) -> Entity {
        Entity {
            id,
            cluster,
            class: iri(class),
            names: names.iter().map(|n| n.to_string()).collect(),
        }
    }

    const WORLD: &str = "name,variant,class
Acme BV,Acme BV,ex:Org
Acme BV,Acme B.V.,ex:Org
Brix Ltd,Brix Ltd,ex:Org
Eastport,Eastport,ex:Site
";

    fn world() -> Vec<WorldName> {
        parse_world("world.csv", WORLD, &ontology()).unwrap()
    }

    #[test]
    fn two_variants_in_two_clusters_are_a_duplicate() {
        let entities = [
            entity(1, 1, "Org", &["Acme BV"]),
            entity(2, 2, "Org", &["Acme B.V."]),
        ];
        let scores = score(&world(), &entities, &ontology());
        assert_eq!(scores[0].class, "ex:Org");
        assert_eq!(scores[0].world, 2);
        assert_eq!(scores[0].found, 1);
        assert_eq!(scores[0].duplicates, 1);
        assert_eq!(scores[0].extra_clusters, 1);
        assert_eq!(scores[0].duplicate_rate(), 1.0);
    }

    #[test]
    fn two_world_names_in_one_cluster_are_a_wrong_merge() {
        let entities = [
            entity(1, 1, "Org", &["Acme BV"]),
            entity(2, 1, "Org", &["Brix Ltd"]),
        ];
        let scores = score(&world(), &entities, &ontology());
        assert_eq!(scores[0].wrong_merges, 1);
        assert_eq!(scores[0].duplicates, 0);
        assert_eq!(scores.last().unwrap().wrong_merges, 1);
    }

    #[test]
    fn a_subclass_counts_as_the_right_class() {
        let entities = [
            entity(1, 1, "Customer", &["Acme BV"]),
            entity(2, 2, "Site", &["Brix Ltd"]),
        ];
        let scores = score(&world(), &entities, &ontology());
        assert_eq!(scores[0].matched, 2);
        assert_eq!(scores[0].right_class, 1);
        assert_eq!(scores[0].class_accuracy(), 0.5);
    }

    #[test]
    fn matching_ignores_case_and_extra_spaces() {
        let entities = [entity(1, 1, "Site", &["x", "  EASTPORT "])];
        let scores = score(&world(), &entities, &ontology());
        assert_eq!(scores[1].class, "ex:Site");
        assert_eq!(scores[1].found, 1);
    }

    #[test]
    fn a_variant_under_two_names_is_an_error() {
        let text = "name,variant,class\nAcme,Acme,ex:Org\nBrix,ACME,ex:Org\n";
        let error = parse_world("w.csv", text, &ontology())
            .unwrap_err()
            .to_string();
        assert!(error.contains("w.csv line 3"), "{error}");
        assert!(error.contains("listed under"), "{error}");
    }

    #[test]
    fn an_unknown_class_is_an_error() {
        let text = "name,variant,class\nAcme,Acme,ex:Nope\n";
        let error = parse_world("w.csv", text, &ontology())
            .unwrap_err()
            .to_string();
        assert!(error.contains("w.csv line 2"), "{error}");
    }
}
