use std::collections::{BTreeMap, HashMap, HashSet};

use super::{Answer, Built, DOC};
use crate::ontology::Ontology;
use crate::text::{fold, squash};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reason {
    NotInShortlist,
    QuoteNotFound,
    NameNotFound,
    PropertyNotAllowed,
    UnknownRef,
    DomainRange,
    Language,
    EmptyLabel,
    ClassName,
    RefTaken,
}

impl Reason {
    pub fn as_str(self) -> &'static str {
        match self {
            Reason::NotInShortlist => "class not in the shortlist",
            Reason::QuoteNotFound => "quote not in the chunk",
            Reason::NameNotFound => "name not in the chunk",
            Reason::PropertyNotAllowed => "property not allowed",
            Reason::UnknownRef => "unknown or dropped ref",
            Reason::DomainRange => "domain or range not met",
            Reason::Language => "language not configured",
            Reason::EmptyLabel => "empty label",
            Reason::ClassName => "named after its class",
            Reason::RefTaken => "ref already used for a known thing",
        }
    }
}

impl std::fmt::Display for Reason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Endpoint {
    Doc,
    Known(usize),
    Entity(usize),
}

#[derive(Debug, Clone, PartialEq)]
pub struct Entity {
    pub class: String,
    pub key: String,
    pub name: String,
    pub start: usize,
    pub end: usize,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Relation {
    pub from: Endpoint,
    pub property: String,
    pub to: Endpoint,
    pub start: usize,
    pub end: usize,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Other {
    pub from_class: String,
    pub label: String,
    pub lang: String,
    pub to_class: String,
    pub start: usize,
    pub end: usize,
}

#[derive(Debug, Clone, Default)]
pub struct Checked {
    pub entities: Vec<Entity>,
    pub relations: Vec<Relation>,
    pub others: Vec<Other>,
    pub drops: BTreeMap<String, i64>,
}

impl Checked {
    fn drop(&mut self, reason: Reason, about: &str) {
        let about: String = about.chars().take(MAX_DROP_TEXT).collect();
        *self.drops.entry(format!("{reason}|{about}")).or_default() += 1;
    }
}

pub const MAX_DROP_TEXT: usize = 80;

pub const MARKUP: [char; 4] = ['*', '`', '|', '#'];

pub struct Haystack {
    text: String,
    origin: Vec<usize>,
}

impl Haystack {
    pub fn new(source: &str) -> Haystack {
        let mut text = String::new();
        let mut origin = Vec::new();
        let mut space = false;
        for (index, c) in source.chars().enumerate() {
            if MARKUP.contains(&c) {
                continue;
            }
            if c.is_whitespace() {
                space = !text.is_empty();
                continue;
            }
            if space {
                text.push(' ');
                origin.push(index);
                space = false;
            }
            for lower in c.to_lowercase() {
                text.push(lower);
                origin.push(index);
            }
        }
        Haystack { text, origin }
    }

    pub fn find(&self, quote: &str) -> Option<(usize, usize)> {
        let needle = Haystack::new(quote).text;
        if needle.is_empty() {
            return None;
        }
        let byte = self.text.find(&needle)?;
        let first = self.text[..byte].chars().count();
        let last = first + needle.chars().count() - 1;
        Some((self.origin[first], self.origin[last] + 1))
    }
}

fn named_after_class(ontology: &Ontology, class: &str, name: &str) -> bool {
    let Some(class) = ontology.classes.get(class) else {
        return false;
    };
    let name = fold(name);
    class.labels.values().any(|l| fold(l) == name)
        || class.alt_labels.values().flatten().any(|l| fold(l) == name)
}

fn exact<'a>(allowed: &'a [String], value: &str) -> Option<&'a String> {
    allowed.iter().find(|a| *a == value.trim())
}

fn language<'a>(allowed: &'a [String], value: &str) -> Option<&'a String> {
    allowed
        .iter()
        .find(|a| a.eq_ignore_ascii_case(value.trim()))
}

pub fn check(
    answer: Answer,
    built: &Built,
    text: &str,
    ontology: &Ontology,
    languages: &[String],
) -> Checked {
    let haystack = Haystack::new(text);
    let mut checked = Checked::default();
    let mut refs: HashMap<String, Endpoint> = HashMap::new();
    if built.doc_class.is_some() {
        refs.insert(DOC.to_string(), Endpoint::Doc);
    }
    for i in 0..built.known.len() {
        refs.insert(format!("r{}", i + 1), Endpoint::Known(i));
    }
    let reserved: HashSet<String> = refs.keys().cloned().collect();
    let mut by_key: HashMap<(String, String), usize> = HashMap::new();
    for entity in answer.entities {
        let reference = entity.reference.trim().to_lowercase();
        if reserved.contains(&reference) {
            refs.remove(&reference);
            checked.drop(Reason::RefTaken, &reference);
            continue;
        }
        let Some(class) = exact(&built.shortlist, &entity.class) else {
            checked.drop(Reason::NotInShortlist, &entity.class);
            continue;
        };
        if named_after_class(ontology, class, &entity.name) {
            checked.drop(Reason::ClassName, class);
            continue;
        }
        if haystack.find(&entity.name).is_none() {
            checked.drop(Reason::NameNotFound, class);
            continue;
        }
        let Some((start, end)) = haystack.find(&entity.evidence) else {
            checked.drop(Reason::QuoteNotFound, class);
            continue;
        };
        let key = fold(&entity.name);
        let index = *by_key
            .entry((class.clone(), key.clone()))
            .or_insert_with(|| {
                checked.entities.push(Entity {
                    class: class.clone(),
                    key,
                    name: entity.name.trim().to_string(),
                    start,
                    end,
                });
                checked.entities.len() - 1
            });
        refs.insert(reference, Endpoint::Entity(index));
    }
    let class_of = |checked: &Checked, end: Endpoint| -> String {
        match end {
            Endpoint::Doc => built.doc_class.clone().unwrap_or_default(),
            Endpoint::Known(i) => built.known[i].class.clone(),
            Endpoint::Entity(i) => checked.entities[i].class.clone(),
        }
    };
    let endpoint = |name: &str| refs.get(&name.trim().to_lowercase()).copied();
    for relation in answer.relations {
        let Some(property) = exact(&built.properties, &relation.property) else {
            checked.drop(Reason::PropertyNotAllowed, &relation.property);
            continue;
        };
        let (Some(from), Some(to)) = (endpoint(&relation.from), endpoint(&relation.to)) else {
            checked.drop(Reason::UnknownRef, property);
            continue;
        };
        let Some(definition) = ontology.properties.get(property) else {
            checked.drop(Reason::PropertyNotAllowed, property);
            continue;
        };
        if !ontology.fits(&class_of(&checked, from), &definition.domain)
            || !ontology.fits(&class_of(&checked, to), &definition.range)
        {
            checked.drop(Reason::DomainRange, property);
            continue;
        }
        let Some((start, end)) = haystack.find(&relation.evidence) else {
            checked.drop(Reason::QuoteNotFound, property);
            continue;
        };
        let relation = Relation {
            from,
            property: property.clone(),
            to,
            start,
            end,
        };
        if !checked.relations.iter().any(|r| {
            r.from == relation.from && r.property == relation.property && r.to == relation.to
        }) {
            checked.relations.push(relation);
        }
    }
    for other in answer.other_relations {
        let label = squash(&other.label);
        if label.is_empty() {
            checked.drop(Reason::EmptyLabel, "");
            continue;
        }
        let Some(lang) = language(languages, &other.lang) else {
            checked.drop(Reason::Language, &other.lang);
            continue;
        };
        let (Some(from), Some(to)) = (endpoint(&other.from), endpoint(&other.to)) else {
            checked.drop(Reason::UnknownRef, &label);
            continue;
        };
        let Some((start, end)) = haystack.find(&other.evidence) else {
            checked.drop(Reason::QuoteNotFound, &label);
            continue;
        };
        checked.others.push(Other {
            from_class: class_of(&checked, from),
            label,
            lang: lang.clone(),
            to_class: class_of(&checked, to),
            start,
            end,
        });
    }
    checked
}

#[cfg(test)]
mod tests {
    use super::super::Known;
    use super::super::tests::{built, input, iri, languages, ontology};
    use super::*;

    #[test]
    fn drop_reason_texts_are_stored_unchanged() {
        let texts: Vec<&str> = [
            Reason::NotInShortlist,
            Reason::QuoteNotFound,
            Reason::NameNotFound,
            Reason::PropertyNotAllowed,
            Reason::UnknownRef,
            Reason::DomainRange,
            Reason::Language,
            Reason::EmptyLabel,
            Reason::ClassName,
            Reason::RefTaken,
        ]
        .iter()
        .map(|r| r.as_str())
        .collect();
        assert_eq!(
            texts,
            [
                "class not in the shortlist",
                "quote not in the chunk",
                "name not in the chunk",
                "property not allowed",
                "unknown or dropped ref",
                "domain or range not met",
                "language not configured",
                "empty label",
                "named after its class",
                "ref already used for a known thing"
            ]
        );
        let mut checked = Checked::default();
        checked.drop(Reason::Language, "de");
        assert!(checked.drops.contains_key("language not configured|de"));
    }

    #[test]
    fn drop_keys_cut_long_model_text() {
        let mut checked = Checked::default();
        checked.drop(Reason::UnknownRef, &"x".repeat(500));
        let key = checked.drops.keys().next().unwrap();
        assert_eq!(
            key,
            &format!("{}|{}", Reason::UnknownRef, "x".repeat(MAX_DROP_TEXT))
        );
    }

    fn answer(json: serde_json::Value) -> Answer {
        serde_json::from_value(json).unwrap()
    }

    #[test]
    fn haystack_maps_quotes_back_to_char_offsets() {
        let text = "Één  booster\nPump at the site; booster pump again.";
        let haystack = Haystack::new(text);
        let (start, end) = haystack.find("BOOSTER pump").unwrap();
        let found: String = text.chars().skip(start).take(end - start).collect();
        assert_eq!(found, "booster\nPump");
        assert_eq!(start, 5);
        assert!(haystack.find("rotor").is_none());
        let markdown = "**Subject**: Pump  | **Site** |";
        let (start, end) = Haystack::new(markdown).find("Subject: Pump Site").unwrap();
        let found: String = markdown.chars().skip(start).take(end - start).collect();
        assert_eq!(found, "Subject**: Pump  | **Site");
        assert!(Haystack::new(markdown).find("**").is_none());
        assert!(haystack.find("  ").is_none());
    }

    #[test]
    fn entities_are_checked_against_shortlist_and_text() {
        let onto = ontology();
        let built = built(&onto, &input(&[("Pump", 1)]));
        let text = "The booster pump at the north site, booster pump B.";
        let checked = check(
            answer(serde_json::json!({
                "entities": [
                    {"ref": "e1", "class": iri("Pump"), "name": "Booster Pump", "evidence": "the booster pump"},
                    {"ref": "e7", "class": iri("pump"), "name": "booster pump", "evidence": "the booster pump"},
                    {"ref": "e2", "class": iri("Site"), "name": "north site", "evidence": "north site"},
                    {"ref": "e3", "class": iri("Pump"), "name": "rotor", "evidence": "booster"},
                    {"ref": "e4", "class": iri("Pump"), "name": "booster pump", "evidence": "a pump not here"},
                    {"ref": "e5", "class": iri("Pump"), "name": "booster  pump", "evidence": "booster pump B"},
                    {"ref": "e6", "class": iri("Pump"), "name": " PUMP ", "evidence": "booster pump B"}
                ],
                "other_relations": []
            })),
            &built,
            text,
            &onto,
            &languages(),
        );
        assert_eq!(checked.entities.len(), 1);
        let entity = &checked.entities[0];
        assert_eq!(entity.class, iri("Pump"));
        assert_eq!(entity.key, "booster pump");
        assert_eq!((entity.start, entity.end), (0, 16));
        assert_eq!(checked.drops.values().sum::<i64>(), 5);
        assert_eq!(
            checked.drops[&format!("{}|{}", Reason::ClassName, iri("Pump"))],
            1
        );
        assert_eq!(
            checked.drops[&format!("{}|{}", Reason::NotInShortlist, iri("Site"))],
            1
        );
        assert_eq!(
            checked.drops[&format!("{}|{}", Reason::NotInShortlist, iri("pump"))],
            1
        );
        assert_eq!(
            checked.drops[&format!("{}|{}", Reason::NameNotFound, iri("Pump"))],
            1
        );
        assert_eq!(
            checked.drops[&format!("{}|{}", Reason::QuoteNotFound, iri("Pump"))],
            1
        );
    }

    #[test]
    fn relations_need_allowed_properties_refs_and_domain_and_range() {
        let onto = ontology();
        let mut chunk = input(&[("Pump", 1), ("Site", 1)]);
        chunk.doc_class = Some(iri("Report"));
        chunk.known = vec![Known {
            entity_id: 9,
            class: iri("Site"),
            name: "north".to_string(),
        }];
        let built = built(&onto, &chunk);
        let text = "The booster pump is installed at the north site.";
        let checked = check(
            answer(serde_json::json!({
                "entities": [
                    {"ref": "e1", "class": iri("Pump"), "name": "booster pump", "evidence": "booster pump"},
                    {"ref": "e2", "class": iri("Pump"), "name": "rotor", "evidence": "rotor"}
                ],
                "relations": [
                    {"from": "E1", "property": iri("installedAt"), "to": "r1", "evidence": "installed at"},
                    {"from": "e1", "property": iri("installedat"), "to": "r1", "evidence": "installed at"},
                    {"from": "e1", "property": iri("installedAt"), "to": "r1", "evidence": "installed at the north"},
                    {"from": "r1", "property": iri("installedAt"), "to": "e1", "evidence": "installed at"},
                    {"from": "e1", "property": iri("installedAt"), "to": "e2", "evidence": "installed at"},
                    {"from": "doc", "property": iri("mentions"), "to": "e1", "evidence": "booster pump"},
                    {"from": "e1", "property": iri("owns"), "to": "r1", "evidence": "installed at"},
                    {"from": "e1", "property": iri("installedAt"), "to": "r1", "evidence": "nowhere"}
                ],
                "other_relations": [
                    {"from": "e1", "label": " placed   near ", "lang": "EN", "to": "r1", "evidence": "at the north"},
                    {"from": "e1", "label": "near", "lang": "de", "to": "r1", "evidence": "at the north"},
                    {"from": "e2", "label": "near", "lang": "en", "to": "r1", "evidence": "at the north"}
                ]
            })),
            &built,
            text,
            &onto,
            &languages(),
        );
        assert_eq!(
            checked.relations,
            [
                Relation {
                    from: Endpoint::Entity(0),
                    property: iri("installedAt"),
                    to: Endpoint::Known(0),
                    start: 20,
                    end: 32,
                },
                Relation {
                    from: Endpoint::Doc,
                    property: iri("mentions"),
                    to: Endpoint::Entity(0),
                    start: 4,
                    end: 16,
                }
            ]
        );
        assert_eq!(
            checked.drops[&format!("{}|{}", Reason::DomainRange, iri("installedAt"))],
            1
        );
        assert_eq!(
            checked.drops[&format!("{}|{}", Reason::UnknownRef, iri("installedAt"))],
            1
        );
        assert_eq!(
            checked.drops[&format!("{}|{}", Reason::PropertyNotAllowed, iri("owns"))],
            1
        );
        assert_eq!(
            checked.drops[&format!("{}|{}", Reason::PropertyNotAllowed, iri("installedat"))],
            1
        );
        assert_eq!(
            checked.drops[&format!("{}|{}", Reason::QuoteNotFound, iri("installedAt"))],
            1
        );
        assert_eq!(checked.others.len(), 1);
        assert_eq!(checked.others[0].label, "placed near");
        assert_eq!(checked.others[0].lang, "en");
        assert_eq!(checked.others[0].to_class, iri("Site"));
        assert_eq!(checked.drops[&format!("{}|de", Reason::Language)], 1);
        assert_eq!(checked.drops[&format!("{}|near", Reason::UnknownRef)], 1);
    }

    #[test]
    fn an_entity_cannot_take_a_known_ref() {
        let onto = ontology();
        let mut chunk = input(&[("Pump", 1), ("Site", 1)]);
        chunk.doc_class = Some(iri("Report"));
        chunk.known = vec![Known {
            entity_id: 9,
            class: iri("Site"),
            name: "north".to_string(),
        }];
        let built = built(&onto, &chunk);
        let text = "The booster pump is installed at the north site.";
        let checked = check(
            answer(serde_json::json!({
                "entities": [
                    {"ref": "e1", "class": iri("Pump"), "name": "booster pump", "evidence": "booster pump"},
                    {"ref": "R1", "class": iri("Site"), "name": "north site", "evidence": "north site"},
                    {"ref": "doc", "class": iri("Pump"), "name": "booster pump", "evidence": "booster pump"}
                ],
                "relations": [
                    {"from": "e1", "property": iri("installedAt"), "to": "r1", "evidence": "installed at"},
                    {"from": "doc", "property": iri("mentions"), "to": "e1", "evidence": "booster pump"}
                ],
                "other_relations": []
            })),
            &built,
            text,
            &onto,
            &languages(),
        );
        assert_eq!(checked.entities.len(), 1);
        assert!(checked.relations.is_empty(), "{:?}", checked.relations);
        assert_eq!(checked.drops[&format!("{}|r1", Reason::RefTaken)], 1);
        assert_eq!(checked.drops[&format!("{}|doc", Reason::RefTaken)], 1);
    }

    #[test]
    fn empty_answer_is_valid() {
        let onto = ontology();
        let built = built(&onto, &input(&[]));
        let checked = check(
            answer(serde_json::json!({"entities": [], "other_relations": []})),
            &built,
            "text",
            &onto,
            &languages(),
        );
        assert!(checked.entities.is_empty() && checked.drops.is_empty());
    }
}
