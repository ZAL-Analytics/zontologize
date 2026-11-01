pub mod report;
pub mod run;

use std::collections::{BTreeSet, HashMap, HashSet};

use serde::Deserialize;

use crate::ontology::{Class, LangMap, Ontology};

pub const TASK: &str = "fit";
pub const NONE: &str = "none";
pub const DEFINITION_CHARS: usize = 200;

#[derive(Debug, Clone, Deserialize)]
pub struct Answer {
    pub classes: Vec<ClassHit>,
    pub terms: Vec<Term>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ClassHit {
    pub iri: String,
    pub evidence: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Term {
    pub term: String,
    pub lang: String,
    pub kind: String,
    pub near: String,
}

#[derive(Debug, Clone, Default)]
pub struct Checked {
    pub classes: Vec<ClassHit>,
    pub terms: Vec<Term>,
    pub dropped: usize,
}

pub fn shortlist(
    ontology: &Ontology,
    counts: &[(String, i64)],
    roots: &[String],
    max: usize,
) -> Vec<String> {
    let mut mentioned: Vec<&(String, i64)> = counts
        .iter()
        .filter(|(iri, _)| ontology.classes.get(iri).is_some_and(|c| c.selected))
        .collect();
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
            if ontology.classes.get(root).is_some_and(|c| c.selected) && seen.insert(root.clone()) {
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
            if ontology.classes.get(&parent).is_some_and(|c| c.selected)
                && seen.insert(parent.clone())
            {
                list.push(parent);
            }
        }
        next += 1;
    }
    list.truncate(max);
    list
}

fn pick(map: &LangMap<String>, languages: &[String]) -> Option<String> {
    for language in languages {
        if let Some(value) = map.get(language) {
            return Some(value.to_string());
        }
    }
    map.values().next().map(|v| v.to_string())
}

fn label(class: &Class, languages: &[String]) -> String {
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

pub fn schema(list: &[String], languages: &[String]) -> serde_json::Value {
    let classes: Vec<&str> = if list.is_empty() {
        vec![NONE]
    } else {
        list.iter().map(String::as_str).collect()
    };
    let mut near: Vec<&str> = list.iter().map(String::as_str).collect();
    near.push(NONE);
    serde_json::json!({
        "type": "object",
        "additionalProperties": false,
        "required": ["classes", "terms"],
        "properties": {
            "classes": {
                "type": "array",
                "items": {
                    "type": "object",
                    "additionalProperties": false,
                    "required": ["iri", "evidence"],
                    "properties": {
                        "iri": {"type": "string", "enum": classes},
                        "evidence": {"type": "string"},
                    },
                },
            },
            "terms": {
                "type": "array",
                "items": {
                    "type": "object",
                    "additionalProperties": false,
                    "required": ["term", "lang", "kind", "near"],
                    "properties": {
                        "term": {"type": "string"},
                        "lang": {"type": "string", "enum": languages},
                        "kind": {"type": "string", "enum": ["alias", "subclass"]},
                        "near": {"type": "string", "enum": near},
                    },
                },
            },
        },
    })
}

pub fn normalise(text: &str) -> String {
    text.split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase()
}

pub fn check(answer: Answer, list: &[String], text: &str, languages: &[String]) -> Checked {
    let haystack = normalise(text);
    let allowed: HashSet<&str> = list.iter().map(String::as_str).collect();
    let found = |quote: &str| {
        let quote = normalise(quote);
        !quote.is_empty() && haystack.contains(&quote)
    };
    let mut checked = Checked::default();
    for hit in answer.classes {
        if allowed.contains(hit.iri.as_str()) && found(&hit.evidence) {
            checked.classes.push(hit);
        } else {
            checked.dropped += 1;
        }
    }
    for term in answer.terms {
        let near_ok = term.near == NONE || allowed.contains(term.near.as_str());
        let kind_ok = term.kind == "alias" || term.kind == "subclass";
        let alias_needs_near = term.kind != "alias" || term.near != NONE;
        if near_ok
            && kind_ok
            && alias_needs_near
            && languages.contains(&term.lang)
            && found(&term.term)
        {
            checked.terms.push(term);
        } else {
            checked.dropped += 1;
        }
    }
    checked
}

pub fn split_sample(
    matched: &[i64],
    unmatched: &[i64],
    size: usize,
    unmatched_share: f64,
) -> Vec<i64> {
    let wanted = (size as f64 * unmatched_share).round() as usize;
    let mut take_unmatched = wanted.min(unmatched.len());
    let take_matched = (size - take_unmatched).min(matched.len());
    take_unmatched = (size - take_matched).min(unmatched.len());
    let mut sample: Vec<i64> = matched[..take_matched].to_vec();
    sample.extend_from_slice(&unmatched[..take_unmatched]);
    sample
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
        let list = shortlist(&onto, &counts, &onto.roots, 10);
        assert_eq!(
            list,
            [iri("Pump"), iri("Valve"), iri("Machine"), iri("Asset")]
        );
        let capped = shortlist(&onto, &counts, &onto.roots, 2);
        assert_eq!(capped, [iri("Pump"), iri("Valve")]);
    }

    #[test]
    fn chunk_without_mentions_gets_the_roots() {
        let onto = ontology();
        assert_eq!(shortlist(&onto, &[], &onto.roots, 10), [iri("Asset")]);
    }

    #[test]
    fn schema_allows_only_the_shortlist() {
        let list = vec![iri("Pump")];
        let built = schema(&list, &languages());
        let classes = &built["properties"]["classes"]["items"]["properties"]["iri"]["enum"];
        assert_eq!(classes, &serde_json::json!([iri("Pump")]));
        let near = &built["properties"]["terms"]["items"]["properties"]["near"]["enum"];
        assert_eq!(near, &serde_json::json!([iri("Pump"), "none"]));
        let empty = schema(&[], &languages());
        assert_eq!(
            empty["properties"]["classes"]["items"]["properties"]["iri"]["enum"],
            serde_json::json!(["none"])
        );
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

    fn answer(json: &str) -> Answer {
        serde_json::from_str(json).unwrap()
    }

    #[test]
    fn check_drops_what_the_chunk_does_not_show() {
        let list = vec![iri("Pump")];
        let text = "The booster   Pump at the site\nhas a worn impeller.";
        let a = answer(&format!(
            r#"{{"classes": [
                {{"iri": "{p}", "evidence": "booster pump"}},
                {{"iri": "{p}", "evidence": "a pump that is not there"}},
                {{"iri": "{v}", "evidence": "booster pump"}}],
              "terms": [
                {{"term": "impeller", "lang": "en", "kind": "subclass", "near": "none"}},
                {{"term": "booster pump", "lang": "en", "kind": "subclass", "near": "{p}"}},
                {{"term": "rotor", "lang": "en", "kind": "subclass", "near": "none"}},
                {{"term": "impeller", "lang": "de", "kind": "subclass", "near": "none"}},
                {{"term": "impeller", "lang": "en", "kind": "alias", "near": "none"}}]}}"#,
            p = iri("Pump"),
            v = iri("Valve")
        ));
        let checked = check(a, &list, text, &languages());
        assert_eq!(checked.classes.len(), 1);
        assert_eq!(checked.terms.len(), 2);
        assert_eq!(checked.dropped, 5);
        let empty = check(
            answer(r#"{"classes": [], "terms": []}"#),
            &list,
            text,
            &languages(),
        );
        assert_eq!(empty.dropped, 0);
        assert!(empty.classes.is_empty());
    }

    #[test]
    fn sample_split_fills_from_the_other_side() {
        let matched: Vec<i64> = (1..=10).collect();
        let unmatched: Vec<i64> = (101..=110).collect();
        assert_eq!(split_sample(&matched, &unmatched, 10, 0.3).len(), 10);
        assert_eq!(
            split_sample(&matched, &unmatched, 10, 0.3)
                .iter()
                .filter(|id| **id > 100)
                .count(),
            3
        );
        let few: Vec<i64> = vec![101];
        let sample = split_sample(&matched, &few, 10, 0.5);
        assert_eq!(sample.len(), 10);
        assert_eq!(sample.iter().filter(|id| **id > 100).count(), 1);
        assert_eq!(split_sample(&[1, 2], &[101], 10, 0.3), [1, 2, 101]);
    }
}
