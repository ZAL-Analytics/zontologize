pub mod check;
pub mod report;
pub mod run;

use std::collections::{BTreeMap, HashMap, HashSet};

use anyhow::Result;
use regex::Regex;
use serde::Deserialize;

use crate::db::curate::{End, NameRow, Proposed, Root};
use crate::llm::prompt::Prompt;
use crate::llm::shortlist::{label, shortlist_lines};
use crate::llm::{ModelName, Request};
use crate::ns::local_name;
use crate::ontology::Ontology;

pub const TASK: &str = "curate";
pub const CACHE_VERSION: u32 = 1;
pub const USER_VARIABLES: [&str; 4] = ["languages", "classes", "members", "reasons"];
const NONE: &str = "none";

#[derive(Debug, Clone, Deserialize)]
pub struct Answer {
    pub groups: Vec<GroupOut>,
    pub not_entities: Vec<NotEntityOut>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct GroupOut {
    pub members: Vec<String>,
    pub name: String,
    pub class: String,
    pub confidence: f64,
}

#[derive(Debug, Clone, Deserialize)]
pub struct NotEntityOut {
    pub member: String,
    pub confidence: f64,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Relation {
    pub property: String,
    pub outgoing: bool,
    pub name: String,
    pub class: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Member {
    pub id: i64,
    pub name: String,
    pub class: String,
    pub aliases: Vec<String>,
    pub mentions: i64,
    pub snippets: Vec<String>,
    pub relations: Vec<Relation>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Cluster {
    pub id: i64,
    pub members: Vec<Member>,
    pub links: Vec<Proposed>,
    pub hits: Vec<Hit>,
    pub rejected: Vec<(i64, i64)>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Source {
    Relation,
    Name,
    Disjoint,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct Hit {
    pub id: i64,
    pub source: Source,
    pub reason: String,
}

#[derive(Debug, Clone)]
pub struct Pattern {
    pub name: String,
    pub class: String,
    pub regex: Regex,
}

#[derive(Debug, Clone)]
pub struct Built {
    pub classes: Vec<String>,
    pub request: Request,
    pub cache_key: String,
}

pub struct Context<'a> {
    pub ontology: &'a Ontology,
    pub languages: &'a [String],
    pub max_shortlist: usize,
    pub definition_chars: usize,
    pub system: &'a Prompt,
    pub user: &'a Prompt,
    pub model: &'a ModelName,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Component {
    pub ids: Vec<i64>,
    pub links: Vec<Proposed>,
    pub hits: Vec<Hit>,
}

pub fn patterns(
    ontology: &Ontology,
    not_in_class: &BTreeMap<String, Vec<String>>,
) -> Result<Vec<Pattern>> {
    let mut out = Vec::new();
    for (name, list) in not_in_class {
        let key = format!("curate.not_in_class.{name}");
        let class = ontology
            .class(name)
            .map_err(|e| anyhow::anyhow!("{key}: {e}"))?;
        for pattern in list {
            let regex = Regex::new(pattern).map_err(|e| anyhow::anyhow!("{key}: {e}"))?;
            out.push(Pattern {
                name: name.clone(),
                class: class.iri.clone(),
                regex,
            });
        }
    }
    Ok(out)
}

pub fn name_hits(ontology: &Ontology, patterns: &[Pattern], roots: &[Root]) -> Vec<Hit> {
    let mut hits = Vec::new();
    for root in roots {
        let found = patterns
            .iter()
            .find(|p| ontology.is_a(&root.class_iri, &p.class) && p.regex.is_match(&root.name));
        if let Some(pattern) = found {
            hits.push(Hit {
                id: root.id,
                source: Source::Name,
                reason: format!("name matches {} for {}", pattern.regex, pattern.name),
            });
        }
    }
    hits
}

pub fn misfits(ontology: &Ontology, ends: &[End], classes: &HashMap<i64, String>) -> Vec<Hit> {
    let mut hits = Vec::new();
    for end in ends {
        let Some(property) = ontology.properties.get(&end.property_iri) else {
            continue;
        };
        for (id, bounds) in [
            (end.src_id, &property.domain),
            (end.dst_id, &property.range),
        ] {
            let Some(class) = classes.get(&id) else {
                continue;
            };
            if bounds.is_empty() || ontology.fits(class, bounds) {
                continue;
            }
            let needs: Vec<&str> = bounds.iter().map(|b| local_name(b)).collect();
            hits.push(Hit {
                id,
                source: Source::Relation,
                reason: format!("{} needs {}", local_name(&property.iri), needs.join(" or ")),
            });
        }
    }
    hits.sort();
    hits.dedup();
    hits
}

pub struct Disjoint {
    pub pairs: Vec<(i64, i64)>,
    pub hits: Vec<Hit>,
    pub skipped: usize,
}

pub fn disjoint(
    ontology: &Ontology,
    normaliser: &crate::resolve::names::Normaliser,
    names: &[NameRow],
    classes: &HashMap<i64, String>,
    max_members: usize,
) -> Disjoint {
    let mut keys: BTreeMap<String, std::collections::BTreeSet<i64>> = BTreeMap::new();
    for row in names.iter().filter(|r| classes.contains_key(&r.root_id)) {
        for name in std::iter::once(&row.name).chain(&row.aliases) {
            let key = normaliser.key(name);
            if !key.is_empty() {
                keys.entry(key).or_default().insert(row.root_id);
            }
        }
    }
    let mut out = Disjoint {
        pairs: Vec::new(),
        hits: Vec::new(),
        skipped: 0,
    };
    for ids in keys.values().filter(|ids| ids.len() > 1) {
        if ids.len() > max_members {
            out.skipped += 1;
            continue;
        }
        let ids: Vec<i64> = ids.iter().copied().collect();
        for (i, a) in ids.iter().enumerate() {
            for b in &ids[i + 1..] {
                let (class_a, class_b) = (&classes[a], &classes[b]);
                if !ontology.disjoint(class_a, class_b) {
                    continue;
                }
                let mut pair = [local_name(class_a), local_name(class_b)];
                pair.sort_unstable();
                let reason = format!("{} and {} are disjoint", pair[0], pair[1]);
                out.pairs.push((*a, *b));
                out.hits.extend([*a, *b].map(|id| Hit {
                    id,
                    source: Source::Disjoint,
                    reason: reason.clone(),
                }));
            }
        }
    }
    out.pairs.sort_unstable();
    out.pairs.dedup();
    out.hits.sort();
    out.hits.dedup();
    out
}

pub fn join(components: Vec<Component>, hits: Vec<Hit>) -> Vec<Component> {
    let mut groups: BTreeMap<i64, Component> =
        components.into_iter().map(|c| (c.ids[0], c)).collect();
    let mut home: HashMap<i64, i64> = HashMap::new();
    for (first, group) in &groups {
        home.extend(group.ids.iter().map(|id| (*id, *first)));
    }
    for hit in hits {
        let first = *home.entry(hit.id).or_insert(hit.id);
        groups
            .entry(first)
            .or_insert_with(|| Component {
                ids: vec![hit.id],
                links: Vec::new(),
                hits: Vec::new(),
            })
            .hits
            .push(hit);
    }
    groups.into_values().collect()
}

pub fn sources(components: &[Component]) -> [(&'static str, usize); 4] {
    let count = |f: &dyn Fn(&Component) -> bool| components.iter().filter(|c| f(c)).count();
    [
        ("proposed link", count(&|c| !c.links.is_empty())),
        (
            "relation check",
            count(&|c| c.hits.iter().any(|h| h.source == Source::Relation)),
        ),
        (
            "name pattern",
            count(&|c| c.hits.iter().any(|h| h.source == Source::Name)),
        ),
        (
            "disjoint classes",
            count(&|c| c.hits.iter().any(|h| h.source == Source::Disjoint)),
        ),
    ]
}

pub fn components(
    links: &[Proposed],
    extra: &[(i64, i64)],
    max_members: usize,
) -> (Vec<Component>, usize) {
    let sources: Vec<&Proposed> = links
        .iter()
        .filter(|l| l.method.starts_with(&crate::resolve::method("")))
        .collect();
    let mut pairs: Vec<(i64, i64)> = sources.iter().map(|l| (l.src_id, l.dst_id)).collect();
    pairs.extend_from_slice(extra);
    let mut ids: Vec<i64> = pairs.iter().flat_map(|(a, b)| [*a, *b]).collect();
    ids.sort_unstable();
    ids.dedup();
    let roots = crate::resolve::clusters(&ids, &pairs);
    let mut groups: BTreeMap<i64, Component> = BTreeMap::new();
    for id in ids {
        groups
            .entry(roots[&id])
            .or_insert_with(|| Component {
                ids: Vec::new(),
                links: Vec::new(),
                hits: Vec::new(),
            })
            .ids
            .push(id);
    }
    for link in sources {
        if let Some(group) = groups.get_mut(&roots[&link.src_id]) {
            group.links.push(link.clone());
        }
    }
    let mut skipped = 0;
    let mut kept = Vec::new();
    for (_, mut group) in groups {
        if group.ids.len() > max_members {
            skipped += 1;
            continue;
        }
        group
            .links
            .sort_by(|a, b| pair(a).cmp(&pair(b)).then_with(|| a.method.cmp(&b.method)));
        kept.push(group);
    }
    (kept, skipped)
}

fn pair(link: &Proposed) -> (i64, i64) {
    (link.src_id.min(link.dst_id), link.src_id.max(link.dst_id))
}

fn children<'a>(ontology: &'a Ontology, iri: &'a str) -> impl Iterator<Item = &'a String> {
    ontology
        .classes
        .values()
        .filter(move |c| c.parents.iter().any(|p| p == iri))
        .map(|c| &c.iri)
}

pub fn allowed_classes(ontology: &Ontology, classes: &[&str], max: usize) -> Vec<String> {
    let keep = |iri: &str| {
        ontology
            .classes
            .get(iri)
            .is_some_and(|c| c.selected && !c.meta)
    };
    let mut related: Vec<&str> = classes.to_vec();
    for &iri in classes {
        let Some(class) = ontology.classes.get(iri) else {
            continue;
        };
        related.extend(class.parents.iter().map(String::as_str));
        if ontology.roots.iter().any(|r| r == iri) {
            related.extend(ontology.roots.iter().map(String::as_str));
        } else {
            for parent in &class.parents {
                related.extend(children(ontology, parent).map(String::as_str));
            }
        }
        related.extend(children(ontology, iri).map(String::as_str));
    }
    let mut seen = HashSet::new();
    related
        .into_iter()
        .filter(|iri| keep(iri) && seen.insert(*iri))
        .take(max)
        .map(str::to_string)
        .collect()
}

pub fn schema(classes: &[String]) -> serde_json::Value {
    let classes: Vec<&str> = if classes.is_empty() {
        vec![NONE]
    } else {
        classes.iter().map(String::as_str).collect()
    };
    let text = serde_json::json!({"type": "string"});
    let confidence = serde_json::json!({"type": "number", "minimum": 0, "maximum": 1});
    serde_json::json!({
        "type": "object",
        "additionalProperties": false,
        "required": ["groups", "not_entities"],
        "properties": {
            "groups": {
                "type": "array",
                "items": {
                    "type": "object",
                    "additionalProperties": false,
                    "required": ["members", "name", "class", "confidence"],
                    "properties": {
                        "members": {"type": "array", "items": text},
                        "name": text,
                        "class": {"type": "string", "enum": classes},
                        "confidence": confidence,
                    },
                },
            },
            "not_entities": {
                "type": "array",
                "items": {
                    "type": "object",
                    "additionalProperties": false,
                    "required": ["member", "confidence"],
                    "properties": {
                        "member": text,
                        "confidence": confidence,
                    },
                },
            },
        },
    })
}

pub fn reference(cluster: &Cluster, id: i64) -> Option<String> {
    cluster
        .members
        .iter()
        .position(|m| m.id == id)
        .map(|i| format!("m{}", i + 1))
}

fn class_name(ontology: &Ontology, iri: &str, languages: &[String]) -> String {
    ontology
        .classes
        .get(iri)
        .map_or_else(|| local_name(iri).to_string(), |c| label(c, languages))
}

fn property_name(ontology: &Ontology, iri: &str, languages: &[String]) -> String {
    ontology
        .properties
        .get(iri)
        .and_then(|p| crate::llm::shortlist::pick(&p.labels, languages))
        .unwrap_or_else(|| local_name(iri).to_string())
}

fn member_lines(ontology: &Ontology, cluster: &Cluster, languages: &[String]) -> String {
    let mut out = String::new();
    for (i, member) in cluster.members.iter().enumerate() {
        out.push_str(&format!(
            "- m{} | {} | {} | {} | {}\n",
            i + 1,
            member.name,
            member.class,
            member.mentions,
            member.aliases.join(", ")
        ));
        for snippet in &member.snippets {
            out.push_str(&format!("  text: {snippet}\n"));
        }
        for relation in &member.relations {
            let arrow = if relation.outgoing { "->" } else { "<-" };
            out.push_str(&format!(
                "  relation: {arrow} {} {} ({})\n",
                property_name(ontology, &relation.property, languages),
                relation.name,
                class_name(ontology, &relation.class, languages)
            ));
        }
    }
    out
}

fn reason_lines(cluster: &Cluster) -> String {
    let mut out = String::new();
    for link in &cluster.links {
        let (Some(a), Some(b)) = (
            reference(cluster, link.src_id.min(link.dst_id)),
            reference(cluster, link.src_id.max(link.dst_id)),
        ) else {
            continue;
        };
        out.push_str(&format!(
            "- {} {:.2} {a} {b}\n",
            link.method, link.confidence
        ));
    }
    for hit in &cluster.hits {
        if let Some(member) = reference(cluster, hit.id) {
            out.push_str(&format!("- {member} {}\n", hit.reason));
        }
    }
    if out.is_empty() {
        out.push_str("(none)\n");
    }
    out
}

pub fn build(context: &Context, cluster: &Cluster) -> Built {
    let ontology = context.ontology;
    let languages = context.languages;
    let member_classes: Vec<&str> = cluster.members.iter().map(|m| m.class.as_str()).collect();
    let classes = allowed_classes(ontology, &member_classes, context.max_shortlist);
    let user = context.user.fill(&[
        ("languages", &languages.join(", ")),
        (
            "classes",
            shortlist_lines(ontology, &classes, languages, context.definition_chars).trim_end(),
        ),
        ("reasons", reason_lines(cluster).trim_end()),
        (
            "members",
            member_lines(ontology, cluster, languages).trim_end(),
        ),
    ]);
    let request = Request {
        task: TASK.to_string(),
        system: context.system.text.clone(),
        user,
        schema: schema(&classes),
    };
    let cache_key = request.stage_key(context.model, CACHE_VERSION);
    Built {
        classes,
        request,
        cache_key,
    }
}

#[cfg(test)]
pub mod tests {
    use super::*;
    use crate::config::OntologyConfig;

    pub const EX: &str = "http://example.org/ex#";

    const TTL: &str = r#"
        @prefix ex: <http://example.org/ex#> .
        @prefix owl: <http://www.w3.org/2002/07/owl#> .
        @prefix rdfs: <http://www.w3.org/2000/01/rdf-schema#> .
        @prefix skos: <http://www.w3.org/2004/02/skos/core#> .
        ex:Organisation a owl:Class ; skos:prefLabel "Organisation"@en .
        ex:Customer a owl:Class ; rdfs:subClassOf ex:Organisation ; skos:prefLabel "Customer"@en .
        ex:Supplier a owl:Class ; rdfs:subClassOf ex:Organisation ; skos:prefLabel "Supplier"@en .
        ex:Shipyard a owl:Class ; rdfs:subClassOf ex:Customer ; skos:prefLabel "Shipyard"@en .
        ex:Site a owl:Class ; skos:prefLabel "Site"@en .
        ex:Person a owl:Class ; skos:prefLabel "Person"@en .
        ex:atSite a owl:ObjectProperty ; rdfs:domain ex:Organisation ; rdfs:range ex:Site ;
            skos:prefLabel "at site"@en .
    "#;

    pub fn iri(name: &str) -> String {
        format!("{EX}{name}")
    }

    pub fn ontology() -> Ontology {
        let config = OntologyConfig {
            files: vec!["a.ttl".to_string()],
            languages: vec!["en".to_string()],
            include: ["ex:Organisation", "ex:Site", "ex:Person"]
                .map(String::from)
                .to_vec(),
            exclude: vec![],
            document_class: String::new(),
        };
        let loaded = crate::ontology::load_sources(
            &[("a.ttl".to_string(), TTL.as_bytes().to_vec())],
            &config,
        );
        assert!(!loaded.has_errors(), "{:?}", loaded.findings);
        loaded.ontology
    }

    fn prompt(text: &str) -> Prompt {
        Prompt {
            source: "test".to_string(),
            text: text.to_string(),
        }
    }

    pub fn member(id: i64, name: &str, class: &str) -> Member {
        Member {
            id,
            name: name.to_string(),
            class: iri(class),
            aliases: vec![],
            mentions: 2,
            snippets: vec![format!("invoice for «{name}»")],
            relations: vec![Relation {
                property: iri("atSite"),
                outgoing: true,
                name: "Havenstad".to_string(),
                class: iri("Site"),
            }],
        }
    }

    pub fn link(src_id: i64, dst_id: i64, method: &str) -> Proposed {
        Proposed {
            src_id,
            dst_id,
            method: method.to_string(),
            confidence: 0.93,
        }
    }

    pub fn cluster(first: i64) -> Cluster {
        Cluster {
            id: first,
            members: vec![
                member(first, "Zuidwal Technick", "Customer"),
                member(first + 5, "Zuidwal Techniek", "Customer"),
            ],
            links: vec![link(first + 5, first, "resolve:fuzzy")],
            hits: vec![],
            rejected: vec![],
        }
    }

    pub fn built(ontology: &Ontology, cluster: &Cluster) -> Built {
        let system = prompt("system");
        let user = prompt("{languages}\n{classes}\n{reasons}\n{members}");
        let model = ModelName::parse("claude-code:haiku").unwrap();
        let languages = vec!["en".to_string()];
        let context = Context {
            ontology,
            languages: &languages,
            max_shortlist: 30,
            definition_chars: 200,
            system: &system,
            user: &user,
            model: &model,
        };
        build(&context, cluster)
    }

    #[test]
    fn allowed_classes_hold_parents_siblings_and_children() {
        let onto = ontology();
        let allowed = allowed_classes(&onto, &[&iri("Customer")], 30);
        assert_eq!(
            allowed,
            [
                iri("Customer"),
                iri("Organisation"),
                iri("Supplier"),
                iri("Shipyard")
            ]
        );
        let roots = allowed_classes(&onto, &[&iri("Site")], 30);
        assert_eq!(roots[0], iri("Site"));
        assert!(roots.contains(&iri("Organisation")) && roots.contains(&iri("Person")));
        assert!(!roots.contains(&iri("Customer")));
    }

    #[test]
    fn allowed_classes_put_member_classes_first_and_respect_the_cut() {
        let onto = ontology();
        let allowed = allowed_classes(&onto, &[&iri("Shipyard"), &iri("Site")], 3);
        assert_eq!(allowed, [iri("Shipyard"), iri("Site"), iri("Customer")]);
        assert!(allowed_classes(&onto, &[&iri("Missing")], 30).is_empty());
    }

    #[test]
    fn refs_follow_id_order() {
        let onto = ontology();
        let request = built(&onto, &cluster(10)).request;
        let first = request.user.find("- m1 | Zuidwal Technick").unwrap();
        let second = request.user.find("- m2 | Zuidwal Techniek").unwrap();
        assert!(first < second, "{}", request.user);
        assert!(
            request.user.contains("- resolve:fuzzy 0.93 m1 m2"),
            "{}",
            request.user
        );
        assert!(!request.user.contains("15"), "{}", request.user);
    }

    #[test]
    fn the_cache_key_follows_snippets_and_classes_but_not_ids() {
        let onto = ontology();
        let before = built(&onto, &cluster(10)).cache_key;
        assert_eq!(built(&onto, &cluster(9000)).cache_key, before);
        let mut snippet = cluster(10);
        snippet.members[0].snippets[0].push_str(" paid");
        assert_ne!(built(&onto, &snippet).cache_key, before);
        let mut class = cluster(10);
        class.members[1].class = iri("Supplier");
        assert_ne!(built(&onto, &class).cache_key, before);
    }

    #[test]
    fn components_come_from_resolve_links_and_large_ones_are_skipped() {
        let links = [
            link(1, 2, "resolve:fuzzy"),
            link(3, 2, "resolve:embed"),
            link(7, 8, "resolve:class-conflict"),
            link(8, 9, "curate:llm:claude-code:haiku"),
            link(20, 21, "resolve:tokens"),
            link(21, 22, "resolve:tokens"),
            link(22, 23, "resolve:tokens"),
        ];
        let (kept, skipped) = components(&links, &[], 3);
        assert_eq!(skipped, 1);
        let ids: Vec<Vec<i64>> = kept.iter().map(|c| c.ids.clone()).collect();
        assert_eq!(ids, [vec![1, 2, 3], vec![7, 8]]);
        assert_eq!(kept[0].links.len(), 2);
        assert_eq!(kept[1].links, [link(7, 8, "resolve:class-conflict")]);
    }

    fn root(id: i64, name: &str, class: &str) -> Root {
        Root {
            id,
            name: name.to_string(),
            class_iri: iri(class),
        }
    }

    fn not_in_class(class: &str) -> BTreeMap<String, Vec<String>> {
        BTreeMap::from([(class.to_string(), vec![r"(?i)\bb\.?v\.?$".to_string()])])
    }

    #[test]
    fn a_pattern_hit_makes_a_cluster_of_one_with_its_reason() {
        let onto = ontology();
        let patterns = patterns(&onto, &not_in_class("ex:Site")).unwrap();
        let roots = [
            root(4, "Havenstad B.V.", "Site"),
            root(5, "Havenstad", "Site"),
            root(6, "Delta B.V.", "Customer"),
        ];
        let hits = name_hits(&onto, &patterns, &roots);
        let joined = join(vec![], hits);
        assert_eq!(joined.len(), 1);
        assert_eq!(joined[0].ids, [4]);
        assert_eq!(
            joined[0].hits[0].reason,
            r"name matches (?i)\bb\.?v\.?$ for ex:Site"
        );
        assert_eq!(sources(&joined)[2], ("name pattern", 1));
    }

    #[test]
    fn a_hit_inside_a_proposed_link_cluster_joins_it() {
        let (components, _) = components(&[link(1, 2, "resolve:fuzzy")], &[], 8);
        let hit = Hit {
            id: 2,
            source: Source::Name,
            reason: "name matches x for ex:Site".to_string(),
        };
        let joined = join(components, vec![hit.clone()]);
        assert_eq!(joined.len(), 1);
        assert_eq!(joined[0].ids, [1, 2]);
        assert_eq!(joined[0].hits, [hit]);
        let mut cluster = cluster(1);
        cluster.members[1].id = 2;
        cluster.links = joined[0].links.clone();
        cluster.hits = joined[0].hits.clone();
        let user = built(&ontology(), &cluster).request.user;
        assert!(user.contains("- m2 name matches x for ex:Site"), "{user}");
    }

    #[test]
    fn an_unknown_class_in_not_in_class_names_the_key() {
        let error = patterns(&ontology(), &not_in_class("ex:Missing")).unwrap_err();
        assert!(
            error
                .to_string()
                .starts_with("curate.not_in_class.ex:Missing: "),
            "{error}"
        );
        let bad = BTreeMap::from([("ex:Site".to_string(), vec!["(".to_string()])]);
        let error = patterns(&ontology(), &bad).unwrap_err();
        assert!(
            error
                .to_string()
                .starts_with("curate.not_in_class.ex:Site: "),
            "{error}"
        );
    }

    #[test]
    fn a_relation_misfit_picks_the_end_that_fails() {
        let onto = ontology();
        let ends = [
            End {
                property_iri: iri("atSite"),
                src_id: 1,
                dst_id: 2,
            },
            End {
                property_iri: iri("atSite"),
                src_id: 3,
                dst_id: 2,
            },
        ];
        let classes = HashMap::from([
            (1, iri("Customer")),
            (2, iri("Person")),
            (3, iri("Shipyard")),
        ]);
        let hits = misfits(&onto, &ends, &classes);
        assert_eq!(
            hits,
            [Hit {
                id: 2,
                source: Source::Relation,
                reason: "atSite needs Site".to_string(),
            }]
        );
        let fits = HashMap::from([(1, iri("Customer")), (2, iri("Site"))]);
        assert!(misfits(&onto, &ends, &fits).is_empty());
    }

    fn disjoint_ontology() -> Ontology {
        let ttl = r#"
            @prefix ex: <http://example.org/ex#> .
            @prefix owl: <http://www.w3.org/2002/07/owl#> .
            @prefix rdfs: <http://www.w3.org/2000/01/rdf-schema#> .
            ex:Site a owl:Class ; owl:disjointWith ex:Company .
            ex:Yard a owl:Class ; rdfs:subClassOf ex:Site .
            ex:Company a owl:Class .
            ex:Person a owl:Class .
        "#;
        let config = OntologyConfig {
            files: vec!["a.ttl".to_string()],
            languages: vec!["en".to_string()],
            include: ["ex:Site", "ex:Company", "ex:Person"]
                .map(String::from)
                .to_vec(),
            exclude: vec![],
            document_class: String::new(),
        };
        let loaded = crate::ontology::load_sources(
            &[("a.ttl".to_string(), ttl.as_bytes().to_vec())],
            &config,
        );
        assert!(!loaded.has_errors(), "{:?}", loaded.findings);
        loaded.ontology
    }

    fn named(root_id: i64, name: &str) -> NameRow {
        NameRow {
            root_id,
            name: name.to_string(),
            aliases: vec![],
        }
    }

    fn disjoint_of(classes: &[(i64, &str)], names: &[NameRow]) -> Disjoint {
        let classes: HashMap<i64, String> = classes.iter().map(|(id, c)| (*id, iri(c))).collect();
        let normaliser = crate::resolve::names::Normaliser::new(&[]);
        disjoint(&disjoint_ontology(), &normaliser, names, &classes, 8)
    }

    #[test]
    fn one_name_in_disjoint_classes_makes_one_cluster_with_the_reason() {
        let found = disjoint_of(
            &[(1, "Site"), (2, "Company")],
            &[named(1, "Delta"), named(2, "delta")],
        );
        assert_eq!(found.pairs, [(1, 2)]);
        let (components, _) = components(&[], &found.pairs, 8);
        let joined = join(components, found.hits);
        assert_eq!(joined.len(), 1);
        assert_eq!(joined[0].ids, [1, 2]);
        assert_eq!(joined[0].hits[0].reason, "Company and Site are disjoint");
        assert_eq!(sources(&joined)[3], ("disjoint classes", 1));
    }

    #[test]
    fn a_subclass_of_a_disjoint_class_counts_and_others_do_not() {
        let mut alias = named(2, "Delta Holding");
        alias.aliases = vec!["Delta".to_string()];
        let yard = disjoint_of(&[(1, "Yard"), (2, "Company")], &[named(1, "Delta"), alias]);
        assert_eq!(yard.pairs, [(1, 2)]);
        let person = disjoint_of(
            &[(1, "Person"), (2, "Company")],
            &[named(1, "Delta"), named(2, "Delta")],
        );
        assert!(person.pairs.is_empty() && person.hits.is_empty());
    }

    #[test]
    fn a_key_held_by_more_than_max_members_is_skipped_and_counted() {
        let classes: HashMap<i64, String> = [(1, "Site"), (2, "Company"), (3, "Company")]
            .iter()
            .map(|(id, c)| (*id, iri(c)))
            .collect();
        let names = [named(1, "Delta"), named(2, "Delta"), named(3, "Delta")];
        let normaliser = crate::resolve::names::Normaliser::new(&[]);
        let found = disjoint(&disjoint_ontology(), &normaliser, &names, &classes, 2);
        assert!(found.pairs.is_empty());
        assert_eq!(found.skipped, 1);
    }

    pub fn recorded<T: serde::de::DeserializeOwned>(name: &str) -> T {
        crate::facts::tests::recorded(name)
    }

    #[test]
    fn recorded_answer_parses() {
        let answer: Answer = recorded("curate-merge.jsonl");
        assert_eq!(answer.groups.len(), 1);
        assert_eq!(answer.groups[0].members, ["m1", "m2"]);
        let low: Answer = recorded("curate-proposed.jsonl");
        assert!(low.groups[0].confidence < 0.9);
    }

    #[test]
    fn built_in_prompts_use_known_variables() {
        let system = crate::llm::prompt::load(None, "curate-system").unwrap();
        system.check(&[]).unwrap();
        let user = crate::llm::prompt::load(None, "curate-user").unwrap();
        user.check(&USER_VARIABLES).unwrap();
        assert!(user.check(&USER_VARIABLES[..3]).is_err());
    }
}
