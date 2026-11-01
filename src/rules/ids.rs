use std::collections::{BTreeMap, HashMap, HashSet};

use regex::{Regex, RegexSet};
use serde::Deserialize;

use super::{EntityHit, KeyCase};
use crate::ontology::Ontology;

const KEY_GROUP: &str = "key";
pub const OCR_METHOD: &str = "regex-ocr";

pub type KnownKeys = HashMap<String, HashSet<String>>;

#[derive(Debug, Default, PartialEq)]
pub struct Tolerant {
    pub hits: Vec<EntityHit>,
    pub rejected: usize,
}

#[derive(Debug, Default, Clone, PartialEq)]
pub struct Confusions {
    map: HashMap<char, char>,
}

impl Confusions {
    pub fn compile(table: &BTreeMap<String, String>) -> Result<Confusions, String> {
        let mut map = HashMap::new();
        for (target, confused) in table {
            let mut chars = target.chars();
            let (Some(to), None) = (chars.next(), chars.next()) else {
                return Err(format!(
                    "ocr_confusions.{target:?}: the key must be exactly one character"
                ));
            };
            for from in confused.chars() {
                if from.len_utf8() != to.len_utf8() {
                    return Err(format!(
                        "ocr_confusions.{target:?}: {from:?} and {to:?} differ in UTF-8 length"
                    ));
                }
                if let Some(other) = map.insert(from, to)
                    && other != to
                {
                    return Err(format!(
                        "ocr_confusions: {from:?} is listed for both {other:?} and {to:?}"
                    ));
                }
            }
        }
        Ok(Confusions { map })
    }

    pub fn normalise(&self, text: &str) -> Option<String> {
        if !text.chars().any(|c| self.map.contains_key(&c)) {
            return None;
        }
        Some(
            text.chars()
                .map(|c| self.map.get(&c).copied().unwrap_or(c))
                .collect(),
        )
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum On {
    Path,
    Text,
    #[default]
    Both,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct IdPatternSpec {
    class: String,
    pattern: String,
    #[serde(default)]
    key_case: KeyCase,
    #[serde(default)]
    on: On,
}

struct IdPattern {
    class: String,
    regex: Regex,
    key_case: KeyCase,
    on: On,
}

pub struct IdPatterns {
    set: RegexSet,
    patterns: Vec<IdPattern>,
}

impl IdPatterns {
    pub fn compile(specs: &[IdPatternSpec], ontology: &Ontology) -> Result<IdPatterns, String> {
        let mut patterns = Vec::new();
        for (index, spec) in specs.iter().enumerate() {
            let class = ontology
                .class(&spec.class)
                .map_err(|e| format!("id_patterns[{index}]: {e}"))?;
            let regex = Regex::new(&spec.pattern)
                .map_err(|e| format!("id_patterns[{index}].pattern: {e}"))?;
            patterns.push(IdPattern {
                class: class.iri.clone(),
                regex,
                key_case: spec.key_case,
                on: spec.on,
            });
        }
        let set = RegexSet::new(specs.iter().map(|s| &s.pattern))
            .map_err(|e| format!("id_patterns: {e}"))?;
        Ok(IdPatterns { set, patterns })
    }

    pub fn find(&self, text: &str, on: On) -> Vec<EntityHit> {
        let mut hits = Vec::new();
        for index in self.set.matches(text).iter() {
            let pattern = &self.patterns[index];
            if pattern.on != On::Both && pattern.on != on {
                continue;
            }
            for captures in pattern.regex.captures_iter(text) {
                let Some(found) = captures.name(KEY_GROUP).or_else(|| captures.get(0)) else {
                    continue;
                };
                let key = pattern.key_case.apply(found.as_str());
                if key.is_empty() {
                    continue;
                }
                hits.push(EntityHit {
                    class: pattern.class.clone(),
                    name: key.clone(),
                    key,
                    start: found.start(),
                    end: found.end(),
                    method: "regex",
                    confidence: 1.0,
                });
            }
        }
        hits
    }

    pub fn find_tolerant(
        &self,
        text: &str,
        confusions: &Confusions,
        known: &KnownKeys,
        exact: &[EntityHit],
    ) -> Tolerant {
        let mut result = Tolerant::default();
        let Some(normalised) = confusions.normalise(text) else {
            return result;
        };
        for mut hit in self.find(&normalised, On::Text) {
            let covered = exact
                .iter()
                .any(|e| e.class == hit.class && e.start < hit.end && hit.start < e.end);
            if covered {
                continue;
            }
            if !known
                .get(&hit.class)
                .is_some_and(|keys| keys.contains(&hit.key))
            {
                result.rejected += 1;
                continue;
            }
            hit.method = OCR_METHOD;
            result.hits.push(hit);
        }
        result
    }
}

#[cfg(test)]
mod tests {
    use super::super::tests::rules;
    use super::*;

    const PATTERNS: &str = r#"
[[id_patterns]]
class = "ex:Project"
pattern = '(?i)\b(?P<key>p-\d{4}-\d{4})\b'
key_case = "upper"

[[id_patterns]]
class = "ex:Pump"
pattern = '\bPU-\d+\b'
on = "text"

[[id_patterns]]
class = "ex:Valve"
pattern = '\b[A-Z]{2}-\d+\b'
on = "path"
"#;

    #[test]
    fn key_group_and_case() {
        let rules = rules(PATTERNS).unwrap();
        let hits = rules.ids.find("see p-2020-0001 and P-2020-0002.", On::Text);
        let keys: Vec<(&str, usize, usize)> = hits
            .iter()
            .map(|h| (h.key.as_str(), h.start, h.end))
            .collect();
        assert_eq!(keys, [("P-2020-0001", 4, 15), ("P-2020-0002", 20, 31)]);
        assert!(
            hits.iter()
                .all(|h| h.method == "regex" && h.confidence == 1.0)
        );
    }

    #[test]
    fn overlapping_patterns_both_hit_and_on_filters() {
        let rules = rules(PATTERNS).unwrap();
        let text: Vec<String> = rules
            .ids
            .find("PU-12", On::Text)
            .iter()
            .map(|h| h.class.clone())
            .collect();
        assert_eq!(text, ["http://example.org/onto#Pump"]);
        let path: Vec<String> = rules
            .ids
            .find("PU-12", On::Path)
            .iter()
            .map(|h| h.class.clone())
            .collect();
        assert_eq!(path, ["http://example.org/onto#Valve"]);
    }

    fn confusions() -> Confusions {
        let table = BTreeMap::from([
            ("0".to_string(), "Oo".to_string()),
            ("1".to_string(), "Il|".to_string()),
            ("5".to_string(), "S".to_string()),
            ("8".to_string(), "B".to_string()),
        ]);
        Confusions::compile(&table).unwrap()
    }

    fn known(class: &str, key: &str) -> KnownKeys {
        KnownKeys::from([(class.to_string(), HashSet::from([key.to_string()]))])
    }

    const PROJECT: &str = "http://example.org/onto#Project";

    #[test]
    fn normalising_keeps_offsets() {
        let text = "é P-2O24-0l17 dóne";
        let normalised = confusions().normalise(text).unwrap();
        assert_eq!(normalised.len(), text.len());
        assert_eq!(&normalised[3..14], "P-2024-0117");
        assert_eq!(confusions().normalise("P-2024-0117"), None);
    }

    #[test]
    fn confused_id_matches_a_known_key() {
        let rules = rules(PATTERNS).unwrap();
        let text = "werkbon P-2O24-0l17 klaar";
        let exact = rules.ids.find(text, On::Text);
        assert!(exact.is_empty());
        let found =
            rules
                .ids
                .find_tolerant(text, &confusions(), &known(PROJECT, "P-2024-0117"), &exact);
        assert_eq!(found.rejected, 0);
        assert_eq!(found.hits.len(), 1);
        let hit = &found.hits[0];
        assert_eq!((hit.key.as_str(), hit.method), ("P-2024-0117", OCR_METHOD));
        assert_eq!(&text[hit.start..hit.end], "P-2O24-0l17");
    }

    #[test]
    fn unknown_key_is_rejected() {
        let rules = rules(PATTERNS).unwrap();
        let found = rules.ids.find_tolerant(
            "werkbon P-2O24-0l18",
            &confusions(),
            &known(PROJECT, "P-2024-0117"),
            &[],
        );
        assert!(found.hits.is_empty());
        assert_eq!(found.rejected, 1);
    }

    #[test]
    fn exact_hit_is_not_reported_twice() {
        let rules = rules(PATTERNS).unwrap();
        let text = "Order P-2024-0117 voor Oost";
        let exact = rules.ids.find(text, On::Text);
        let found =
            rules
                .ids
                .find_tolerant(text, &confusions(), &known(PROJECT, "P-2024-0117"), &exact);
        assert!(found.hits.is_empty());
        assert_eq!(found.rejected, 0);
    }

    #[test]
    fn bad_confusion_tables_are_errors() {
        let two = BTreeMap::from([("10".to_string(), "I".to_string())]);
        assert!(Confusions::compile(&two).is_err());
        let both = BTreeMap::from([
            ("0".to_string(), "O".to_string()),
            ("Q".to_string(), "O".to_string()),
        ]);
        assert!(Confusions::compile(&both).is_err());
        let width = BTreeMap::from([("e".to_string(), "é".to_string())]);
        assert!(Confusions::compile(&width).is_err());
    }

    #[test]
    fn set_is_compiled_once_with_every_pattern() {
        let rules = rules(PATTERNS).unwrap();
        assert_eq!(rules.ids.set.len(), 3);
        assert_eq!(rules.ids.patterns.len(), 3);
    }
}
