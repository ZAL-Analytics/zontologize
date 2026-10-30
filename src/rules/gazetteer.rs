use std::collections::HashMap;

use aho_corasick::{AhoCorasick, MatchKind};
use anyhow::{Result, anyhow};
use serde::Deserialize;

use super::{ClassHit, EntityHit};
use crate::ontology::Ontology;

#[derive(Debug, Deserialize)]
struct Row {
    class: String,
    key: String,
    name: String,
    #[serde(default)]
    aliases: String,
    #[serde(default)]
    lang: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Entry {
    pub class: String,
    pub key: String,
    pub name: String,
    pub aliases: Vec<String>,
    pub lang: String,
}

#[derive(Debug, Clone, Copy)]
enum Target {
    Entity(usize),
    Class(usize),
}

pub struct Lexicon {
    pub entries: Vec<Entry>,
    classes: Vec<String>,
    automaton: Option<AhoCorasick>,
    targets: Vec<Vec<Target>>,
}

impl Lexicon {
    pub fn build(sources: &[(String, Vec<u8>)], ontology: &Ontology) -> Result<Lexicon> {
        let mut entries = Vec::new();
        for (name, bytes) in sources {
            let mut reader = csv::ReaderBuilder::new()
                .trim(csv::Trim::All)
                .from_reader(bytes.as_slice());
            for (index, row) in reader.deserialize::<Row>().enumerate() {
                let line = index + 2;
                let row = row.map_err(|e| anyhow!("{name}:{line}: {e}"))?;
                let class = ontology
                    .class(&row.class)
                    .map_err(|e| anyhow!("{name}:{line}: {e}"))?;
                if row.key.is_empty() || row.name.is_empty() {
                    return Err(anyhow!("{name}:{line}: key and name must not be empty"));
                }
                entries.push(Entry {
                    class: class.iri.clone(),
                    key: row.key,
                    name: row.name,
                    aliases: row
                        .aliases
                        .split('|')
                        .map(str::trim)
                        .filter(|a| !a.is_empty())
                        .map(str::to_string)
                        .collect(),
                    lang: row.lang,
                });
            }
        }
        let mut patterns: Vec<String> = Vec::new();
        let mut targets: Vec<Vec<Target>> = Vec::new();
        let mut index: HashMap<String, usize> = HashMap::new();
        let mut add = |text: &str, target: Target| {
            let pattern = lowercase(text).0;
            if pattern.trim().is_empty() {
                return;
            }
            let slot = *index.entry(pattern.clone()).or_insert_with(|| {
                patterns.push(pattern);
                targets.push(Vec::new());
                targets.len() - 1
            });
            targets[slot].push(target);
        };
        for (i, entry) in entries.iter().enumerate() {
            add(&entry.name, Target::Entity(i));
            for alias in &entry.aliases {
                add(alias, Target::Entity(i));
            }
        }
        let mut classes = Vec::new();
        for class in ontology.classes.values().filter(|c| c.selected) {
            let slot = classes.len();
            classes.push(class.iri.clone());
            for lang in &ontology.languages {
                if let Some(label) = class.labels.get(lang) {
                    add(label, Target::Class(slot));
                }
                for alias in class.alt_labels.get(lang).into_iter().flatten() {
                    add(alias, Target::Class(slot));
                }
            }
        }
        for list in &mut targets {
            dedup_targets(list);
        }
        let automaton = if patterns.is_empty() {
            None
        } else {
            Some(
                AhoCorasick::builder()
                    .match_kind(MatchKind::Standard)
                    .build(&patterns)
                    .map_err(|e| anyhow!("gazetteer and class labels: {e}"))?,
            )
        };
        Ok(Lexicon {
            entries,
            classes,
            automaton,
            targets,
        })
    }

    pub fn find(&self, text: &str) -> (Vec<EntityHit>, Vec<ClassHit>) {
        let mut entities = Vec::new();
        let mut classes = Vec::new();
        let Some(automaton) = &self.automaton else {
            return (entities, classes);
        };
        let (lower, offsets) = lowercase(text);
        let mut found: Vec<(usize, usize, usize)> = automaton
            .find_overlapping_iter(&lower)
            .filter_map(|m| {
                let start = offsets[m.start()];
                let end = offsets[m.end()];
                is_word(text, start, end).then_some((start, end, m.pattern().as_usize()))
            })
            .collect();
        found.sort_by(|a, b| a.0.cmp(&b.0).then(b.1.cmp(&a.1)));
        let mut last_end = 0;
        for (start, end, pattern) in found {
            if start < last_end {
                continue;
            }
            last_end = end;
            for target in &self.targets[pattern] {
                match *target {
                    Target::Entity(i) => {
                        let entry = &self.entries[i];
                        entities.push(EntityHit {
                            class: entry.class.clone(),
                            key: entry.key.clone(),
                            name: entry.name.clone(),
                            start,
                            end,
                            method: "gazetteer",
                            confidence: 1.0,
                        });
                    }
                    Target::Class(i) => classes.push(ClassHit {
                        class: self.classes[i].clone(),
                        start,
                        end,
                    }),
                }
            }
        }
        (entities, classes)
    }
}

fn dedup_targets(list: &mut Vec<Target>) {
    let mut seen: Vec<(bool, usize)> = Vec::new();
    list.retain(|target| {
        let id = match *target {
            Target::Entity(i) => (true, i),
            Target::Class(i) => (false, i),
        };
        if seen.contains(&id) {
            false
        } else {
            seen.push(id);
            true
        }
    });
}

pub fn lowercase(text: &str) -> (String, Vec<usize>) {
    let mut lower = String::with_capacity(text.len());
    let mut offsets = Vec::with_capacity(text.len() + 1);
    for (index, c) in text.char_indices() {
        for l in c.to_lowercase() {
            for _ in 0..l.len_utf8() {
                offsets.push(index);
            }
            lower.push(l);
        }
    }
    offsets.push(text.len());
    (lower, offsets)
}

fn is_word(text: &str, start: usize, end: usize) -> bool {
    if start >= end || !text.is_char_boundary(start) || !text.is_char_boundary(end) {
        return false;
    }
    let before = text[..start].chars().next_back();
    let after = text[end..].chars().next();
    !before.is_some_and(char::is_alphanumeric) && !after.is_some_and(char::is_alphanumeric)
}

#[cfg(test)]
mod tests {
    use super::super::tests::ontology;
    use super::*;

    const CSV: &str = "class,key,name,aliases,lang\n\
        ex:Customer,C-1,Northwind Ltd,Northwind|NW Ltd,en\n\
        ex:Customer,C-2,Ölwerke Süd,,de\n";

    fn lexicon() -> Lexicon {
        Lexicon::build(
            &[("g.csv".to_string(), CSV.as_bytes().to_vec())],
            &ontology(),
        )
        .unwrap()
    }

    #[test]
    fn lowercase_offsets_map_back_on_non_ascii() {
        let text = "İx ÖLWERKE SÜD";
        let (lower, offsets) = lowercase(text);
        assert_eq!(offsets.len(), lower.len() + 1);
        let lexicon = lexicon();
        let (entities, _) = lexicon.find(text);
        assert_eq!(entities.len(), 1);
        assert_eq!(&text[entities[0].start..entities[0].end], "ÖLWERKE SÜD");
        assert_eq!(entities[0].key, "C-2");
    }

    #[test]
    fn word_boundaries_are_required() {
        let lexicon = lexicon();
        let (entities, classes) = lexicon.find("pumpkin NorthwindX valves");
        assert!(entities.is_empty(), "{entities:?}");
        assert!(classes.is_empty(), "{classes:?}");
        let (_, classes) = lexicon.find("the pump.");
        assert_eq!(classes.len(), 1);
        assert_eq!((classes[0].start, classes[0].end), (4, 8));
    }

    #[test]
    fn longest_match_wins() {
        let lexicon = lexicon();
        let (entities, _) = lexicon.find("Northwind Ltd and northwind");
        let spans: Vec<(usize, usize)> = entities.iter().map(|e| (e.start, e.end)).collect();
        assert_eq!(spans, [(0, 13), (18, 27)]);
        let (_, classes) = lexicon.find("a gear pump here");
        assert_eq!(classes.len(), 1);
        assert_eq!((classes[0].start, classes[0].end), (2, 11));
    }

    #[test]
    fn class_labels_in_configured_languages_only_for_selected() {
        let lexicon = lexicon();
        let (_, classes) = lexicon.find("kogelkraan hidden project");
        let iris: Vec<&str> = classes.iter().map(|c| c.class.as_str()).collect();
        assert_eq!(
            iris,
            [
                "http://example.org/onto#Valve",
                "http://example.org/onto#Project"
            ]
        );
    }

    #[test]
    fn unknown_class_names_file_and_line() {
        let csv = "class,key,name,aliases,lang\nex:Hidden,H,h,,\n";
        let error = Lexicon::build(
            &[("g.csv".to_string(), csv.as_bytes().to_vec())],
            &ontology(),
        )
        .err()
        .unwrap();
        assert!(format!("{error}").starts_with("g.csv:2:"), "{error}");
    }
}
