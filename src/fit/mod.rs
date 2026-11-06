pub mod report;
pub mod run;

use std::collections::HashSet;

use serde::Deserialize;

use crate::facts::check::Haystack;

pub const TASK: &str = "fit";
pub const NONE: &str = "none";

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

pub fn check(answer: Answer, list: &[String], text: &str, languages: &[String]) -> Checked {
    let haystack = Haystack::new(text);
    let allowed: HashSet<&str> = list.iter().map(String::as_str).collect();
    let found = |quote: &str| haystack.find(quote).is_some();
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

#[cfg(test)]
mod tests {
    use super::*;

    const EX: &str = "http://example.org/ex#";

    fn iri(name: &str) -> String {
        format!("{EX}{name}")
    }

    fn languages() -> Vec<String> {
        vec!["en".to_string(), "nl".to_string()]
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
