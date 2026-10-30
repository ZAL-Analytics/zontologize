use regex::{Regex, RegexSet};
use serde::Deserialize;

use super::{EntityHit, KeyCase};
use crate::ontology::Ontology;

const KEY_GROUP: &str = "key";

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

    #[test]
    fn set_is_compiled_once_with_every_pattern() {
        let rules = rules(PATTERNS).unwrap();
        assert_eq!(rules.ids.set.len(), 3);
        assert_eq!(rules.ids.patterns.len(), 3);
    }
}
