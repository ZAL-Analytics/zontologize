use std::collections::{BTreeMap, HashMap, HashSet};

use regex::{Regex, RegexSet};
use regex_syntax::hir::{Class, ClassUnicode, ClassUnicodeRange, Hir, HirKind};
use serde::Deserialize;

use super::{EntityHit, KeyCase};
use crate::ontology::Ontology;

const KEY_GROUP: &str = "key";
const MAX_CONFUSED: usize = 8;
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

    fn widen(&self, pattern: &str) -> Result<String, String> {
        let hir = regex_syntax::parse(pattern).map_err(|e| e.to_string())?;
        Ok(self.widen_hir(hir).to_string())
    }

    fn widen_hir(&self, hir: Hir) -> Hir {
        match hir.into_kind() {
            HirKind::Literal(literal) => match std::str::from_utf8(&literal.0) {
                Ok(text) => Hir::concat(
                    text.chars()
                        .map(|c| {
                            self.widen_class(ClassUnicode::new([ClassUnicodeRange::new(c, c)]))
                        })
                        .collect(),
                ),
                Err(_) => Hir::literal(literal.0),
            },
            HirKind::Class(Class::Unicode(class)) => self.widen_class(class),
            HirKind::Class(class) => Hir::class(class),
            HirKind::Repetition(mut repetition) => {
                repetition.sub = Box::new(self.widen_hir(*repetition.sub));
                Hir::repetition(repetition)
            }
            HirKind::Capture(mut capture) => {
                capture.sub = Box::new(self.widen_hir(*capture.sub));
                Hir::capture(capture)
            }
            HirKind::Concat(subs) => {
                Hir::concat(subs.into_iter().map(|h| self.widen_hir(h)).collect())
            }
            HirKind::Alternation(subs) => {
                Hir::alternation(subs.into_iter().map(|h| self.widen_hir(h)).collect())
            }
            HirKind::Look(look) => Hir::look(look),
            HirKind::Empty => Hir::empty(),
        }
    }

    fn widen_class(&self, mut class: ClassUnicode) -> Hir {
        let accepts = |c: char| {
            class
                .ranges()
                .iter()
                .any(|r| r.start() <= c && c <= r.end())
        };
        let extra: Vec<ClassUnicodeRange> = self
            .map
            .iter()
            .filter(|(_, to)| accepts(**to))
            .map(|(from, _)| ClassUnicodeRange::new(*from, *from))
            .collect();
        class.union(&ClassUnicode::new(extra));
        Hir::class(Class::Unicode(class))
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
    tolerant: Regex,
    key_case: KeyCase,
    on: On,
}

pub struct IdPatterns {
    set: RegexSet,
    tolerant: RegexSet,
    patterns: Vec<IdPattern>,
}

impl IdPatterns {
    pub fn compile(
        specs: &[IdPatternSpec],
        ontology: &Ontology,
        confusions: &Confusions,
    ) -> Result<IdPatterns, String> {
        let mut patterns = Vec::new();
        for (index, spec) in specs.iter().enumerate() {
            let class = ontology
                .class(&spec.class)
                .map_err(|e| format!("id_patterns[{index}]: {e}"))?;
            let regex = Regex::new(&spec.pattern)
                .map_err(|e| format!("id_patterns[{index}].pattern: {e}"))?;
            let tolerant = confusions
                .widen(&spec.pattern)
                .and_then(|p| Regex::new(&p).map_err(|e| e.to_string()))
                .map_err(|e| format!("id_patterns[{index}].pattern: {e}"))?;
            patterns.push(IdPattern {
                class: class.iri.clone(),
                regex,
                tolerant,
                key_case: spec.key_case,
                on: spec.on,
            });
        }
        let set = RegexSet::new(specs.iter().map(|s| &s.pattern))
            .map_err(|e| format!("id_patterns: {e}"))?;
        let tolerant = RegexSet::new(patterns.iter().map(|p| p.tolerant.as_str()))
            .map_err(|e| format!("id_patterns: {e}"))?;
        Ok(IdPatterns {
            set,
            tolerant,
            patterns,
        })
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
                    method: super::REGEX_METHOD,
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
        if !text.chars().any(|c| confusions.map.contains_key(&c)) {
            return result;
        }
        for index in self.tolerant.matches(text).iter() {
            let pattern = &self.patterns[index];
            if pattern.on == On::Path {
                continue;
            }
            for found in pattern.tolerant.find_iter(text) {
                let Some(mut hit) = pattern.swapped(text, found.range(), confusions) else {
                    continue;
                };
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
        }
        result
    }
}

impl IdPattern {
    fn swapped(
        &self,
        text: &str,
        span: std::ops::Range<usize>,
        confusions: &Confusions,
    ) -> Option<EntityHit> {
        let confused: Vec<(usize, char)> = text[span.clone()]
            .char_indices()
            .filter_map(|(at, c)| Some((span.start + at, *confusions.map.get(&c)?)))
            .collect();
        if confused.is_empty() || confused.len() > MAX_CONFUSED {
            return None;
        }
        let mut masks: Vec<u32> = (1..1u32 << confused.len()).collect();
        masks.sort_by_key(|m| std::cmp::Reverse(m.count_ones()));
        for mask in masks {
            let mut candidate = text.to_string();
            for (bit, (at, to)) in confused.iter().enumerate() {
                if mask & (1 << bit) != 0 {
                    let width = to.len_utf8();
                    candidate.replace_range(*at..*at + width, to.encode_utf8(&mut [0; 4]));
                }
            }
            let Some(captures) = self.regex.captures_at(&candidate, span.start) else {
                continue;
            };
            let Some(whole) = captures.get(0) else {
                continue;
            };
            if whole.range() != span {
                continue;
            }
            let found = captures.name(KEY_GROUP).unwrap_or(whole);
            let key = self.key_case.apply(found.as_str());
            if key.is_empty() {
                return None;
            }
            return Some(EntityHit {
                class: self.class.clone(),
                name: key.clone(),
                key,
                start: found.start(),
                end: found.end(),
                method: super::REGEX_METHOD,
                confidence: 1.0,
            });
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::super::tests::{CONFUSIONS, rules};
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
    const DRAWING_CLASS: &str = "http://example.org/onto#Valve";
    const DRAWING: &str = r#"
[[id_patterns]]
class = "ex:Valve"
pattern = '\bDWG-\d{3}-[A-Z]\d\b'
"#;

    #[test]
    fn only_positions_that_take_a_digit_are_swapped() {
        let rules = rules(&format!("{CONFUSIONS}{DRAWING}")).unwrap();
        let text = "tekening DWG-1O2-B1 rev";
        let exact = rules.ids.find(text, On::Text);
        assert!(exact.is_empty());
        let found = rules.ids.find_tolerant(
            text,
            &confusions(),
            &known(DRAWING_CLASS, "DWG-102-B1"),
            &exact,
        );
        assert_eq!(found.rejected, 0);
        assert_eq!(found.hits.len(), 1);
        let hit = &found.hits[0];
        assert_eq!(hit.key, "DWG-102-B1");
        assert_eq!(&text[hit.start..hit.end], "DWG-1O2-B1");
    }

    #[test]
    fn widened_pattern_accepts_confusions_only_for_digits() {
        let widened = confusions().widen(r"[A-Z]\d0").unwrap();
        let regex = Regex::new(&widened).unwrap();
        assert!(regex.is_match("BOO"));
        assert!(regex.is_match("BIo"));
        assert!(!regex.is_match("0OO"));
    }

    #[test]
    fn confused_id_matches_a_known_key() {
        let rules = rules(&format!("{CONFUSIONS}{PATTERNS}")).unwrap();
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
        let rules = rules(&format!("{CONFUSIONS}{PATTERNS}")).unwrap();
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
