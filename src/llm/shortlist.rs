use std::collections::{BTreeSet, HashMap, HashSet};

use crate::ontology::{Class, LangMap, Ontology};

pub const DEFINITION_CHARS: usize = 200;

pub fn shortlist(
    ontology: &Ontology,
    counts: &[(String, i64)],
    roots: &[String],
    exclude: &[String],
    max: usize,
) -> Vec<String> {
    let keep = |iri: &str| {
        ontology
            .classes
            .get(iri)
            .is_some_and(|c| c.selected && !c.meta)
            && !exclude.iter().any(|e| ontology.is_a(iri, e))
    };
    let mut mentioned: Vec<&(String, i64)> = counts.iter().filter(|(iri, _)| keep(iri)).collect();
    mentioned.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    let mut list: Vec<String> = Vec::new();
    let mut seen = HashSet::new();
    for (iri, _) in &mentioned {
        if seen.insert(iri.clone()) {
            list.push(iri.clone());
        }
    }
    if list.is_empty() {
        for root in roots {
            if keep(root) && seen.insert(root.clone()) {
                list.push(root.clone());
            }
        }
    }
    let mut next = 0;
    while next < list.len() {
        let parents = ontology
            .classes
            .get(&list[next])
            .map(|c| c.parents.clone())
            .unwrap_or_default();
        for parent in parents {
            if keep(&parent) && seen.insert(parent.clone()) {
                list.push(parent);
            }
        }
        next += 1;
    }
    list.truncate(max);
    list
}

pub fn pick(map: &LangMap<String>, languages: &[String]) -> Option<String> {
    for language in languages {
        if let Some(value) = map.get(language) {
            return Some(value.to_string());
        }
    }
    map.values().next().map(|v| v.to_string())
}

pub fn label(class: &Class, languages: &[String]) -> String {
    pick(&class.labels, languages).unwrap_or_else(|| local_name(&class.iri).to_string())
}

pub fn local_name(iri: &str) -> &str {
    iri.rsplit(['#', '/', ':']).next().unwrap_or(iri)
}

pub fn shortlist_lines(ontology: &Ontology, list: &[String], languages: &[String]) -> String {
    let mut out = String::new();
    for iri in list {
        let Some(class) = ontology.classes.get(iri) else {
            continue;
        };
        let mut aliases = BTreeSet::new();
        for language in languages {
            for alias in class.alt_labels.get(language).into_iter().flatten() {
                aliases.insert(alias.as_str());
            }
        }
        let definition: String = pick(&class.definition, languages)
            .unwrap_or_default()
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ")
            .chars()
            .take(DEFINITION_CHARS)
            .collect();
        out.push_str(&format!(
            "- {iri} | {} | {} | {definition}\n",
            label(class, languages),
            aliases.into_iter().collect::<Vec<_>>().join(", ")
        ));
    }
    if out.is_empty() {
        out.push_str("(none)\n");
    }
    out
}

pub fn normalise(text: &str) -> String {
    text.split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase()
}

pub fn counts_by_chunk(rows: Vec<(i64, String, i64)>) -> HashMap<i64, Vec<(String, i64)>> {
    let mut map: HashMap<i64, Vec<(String, i64)>> = HashMap::new();
    for (chunk, iri, count) in rows {
        map.entry(chunk).or_default().push((iri, count));
    }
    map
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::OntologyConfig;

    const TTL: &str = r#"
        @prefix ex: <http://example.org/ex#> .
        @prefix owl: <http://www.w3.org/2002/07/owl#> .
        @prefix rdfs: <http://www.w3.org/2000/01/rdf-schema#> .
        @prefix skos: <http://www.w3.org/2004/02/skos/core#> .
        ex:Asset a owl:Class ; skos:prefLabel "Asset"@en .
        ex:Machine a owl:Class ; rdfs:subClassOf ex:Asset ; skos:prefLabel "Machine"@en ;
            skos:altLabel "Apparaat"@nl ; skos:definition "Something   that works."@en .
        ex:Pump a owl:Class ; rdfs:subClassOf ex:Machine ; skos:prefLabel "Pump"@en, "Pomp"@nl .
        ex:Valve a owl:Class ; rdfs:subClassOf ex:Machine ; skos:prefLabel "Valve"@en .
        ex:Other a owl:Class ; skos:prefLabel "Other"@en .
    "#;

    fn ontology() -> Ontology {
        let config = OntologyConfig {
            files: vec!["a.ttl".to_string()],
            languages: vec!["en".to_string(), "nl".to_string()],
            include: vec!["ex:Asset".to_string()],
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

    const EX: &str = "http://example.org/ex#";

    fn iri(name: &str) -> String {
        format!("{EX}{name}")
    }

    fn languages() -> Vec<String> {
        vec!["en".to_string(), "nl".to_string()]
    }

    #[test]
    fn shortlist_adds_ancestors_and_caps() {
        let onto = ontology();
        let counts = vec![(iri("Valve"), 1), (iri("Pump"), 5), (iri("Other"), 9)];
        let list = shortlist(&onto, &counts, &onto.roots, &[], 10);
        assert_eq!(
            list,
            [iri("Pump"), iri("Valve"), iri("Machine"), iri("Asset")]
        );
        let capped = shortlist(&onto, &counts, &onto.roots, &[], 2);
        assert_eq!(capped, [iri("Pump"), iri("Valve")]);
    }

    #[test]
    fn chunk_without_mentions_gets_the_roots() {
        let onto = ontology();
        assert_eq!(shortlist(&onto, &[], &onto.roots, &[], 10), [iri("Asset")]);
    }

    #[test]
    fn meta_classes_never_enter_the_shortlist() {
        let ttl = "@prefix ex: <http://example.org/ex#> .\n\
            @prefix owl: <http://www.w3.org/2002/07/owl#> .\n\
            ex:EquipmentClass a owl:Class .\n\
            ex:Pump a owl:Class, ex:EquipmentClass .\n";
        let config = OntologyConfig {
            files: vec!["a.ttl".to_string()],
            languages: vec!["en".to_string()],
            include: vec!["ex:EquipmentClass".to_string()],
            exclude: vec![],
            document_class: String::new(),
        };
        let onto = crate::ontology::load_sources(
            &[("a.ttl".to_string(), ttl.as_bytes().to_vec())],
            &config,
        )
        .ontology;
        let counts = vec![(iri("EquipmentClass"), 3), (iri("Pump"), 1)];
        assert_eq!(
            shortlist(&onto, &counts, &onto.roots, &[], 10),
            [iri("Pump")]
        );
        assert!(shortlist(&onto, &[], &onto.roots, &[], 10).is_empty());
    }

    #[test]
    fn excluded_roots_leave_the_shortlist_with_their_descendants() {
        let onto = ontology();
        let counts = vec![(iri("Valve"), 1), (iri("Pump"), 5)];
        let list = shortlist(&onto, &counts, &onto.roots, &[iri("Pump")], 10);
        assert_eq!(list, [iri("Valve"), iri("Machine"), iri("Asset")]);
        let list = shortlist(&onto, &counts, &onto.roots, &[iri("Machine")], 10);
        assert_eq!(list, [iri("Asset")]);
        assert!(shortlist(&onto, &[], &onto.roots, &[iri("Asset")], 10).is_empty());
    }

    #[test]
    fn shortlist_lines_carry_label_aliases_and_short_definition() {
        let onto = ontology();
        let lines = shortlist_lines(&onto, &[iri("Machine")], &languages());
        assert_eq!(
            lines,
            format!(
                "- {} | Machine | Apparaat | Something that works.\n",
                iri("Machine")
            )
        );
        assert_eq!(shortlist_lines(&onto, &[], &languages()), "(none)\n");
    }
}
