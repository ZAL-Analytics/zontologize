use std::collections::{BTreeMap, HashMap};

use super::{Answer, Built, DOC};
use crate::llm::shortlist::normalise;
use crate::ontology::Ontology;

pub const NOT_IN_SHORTLIST: &str = "class not in the shortlist";
pub const QUOTE_NOT_FOUND: &str = "quote not in the chunk";
pub const NAME_NOT_FOUND: &str = "name not in the chunk";
pub const PROPERTY_NOT_ALLOWED: &str = "property not allowed";
pub const UNKNOWN_REF: &str = "unknown or dropped ref";
pub const DOMAIN_RANGE: &str = "domain or range not met";
pub const LANGUAGE: &str = "language not configured";
pub const EMPTY_LABEL: &str = "empty label";
pub const CLASS_NAME: &str = "named after its class";

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
    fn drop(&mut self, reason: &str, about: &str) {
        *self.drops.entry(format!("{reason}|{about}")).or_default() += 1;
    }
}

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
    let name = normalise(name);
    class.labels.values().any(|l| normalise(l) == name)
        || class
            .alt_labels
            .values()
            .flatten()
            .any(|l| normalise(l) == name)
}

fn exact<'a>(allowed: &'a [String], value: &str) -> Option<&'a String> {
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
    let mut by_key: HashMap<(String, String), usize> = HashMap::new();
    for entity in answer.entities {
        let Some(class) = exact(&built.shortlist, &entity.class) else {
            checked.drop(NOT_IN_SHORTLIST, &entity.class);
            continue;
        };
        if named_after_class(ontology, class, &entity.name) {
            checked.drop(CLASS_NAME, class);
            continue;
        }
        if haystack.find(&entity.name).is_none() {
            checked.drop(NAME_NOT_FOUND, class);
            continue;
        }
        let Some((start, end)) = haystack.find(&entity.evidence) else {
            checked.drop(QUOTE_NOT_FOUND, class);
            continue;
        };
        let key = normalise(&entity.name);
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
        refs.insert(
            entity.reference.trim().to_lowercase(),
            Endpoint::Entity(index),
        );
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
            checked.drop(PROPERTY_NOT_ALLOWED, &relation.property);
            continue;
        };
        let (Some(from), Some(to)) = (endpoint(&relation.from), endpoint(&relation.to)) else {
            checked.drop(UNKNOWN_REF, property);
            continue;
        };
        let Some(definition) = ontology.properties.get(property) else {
            checked.drop(PROPERTY_NOT_ALLOWED, property);
            continue;
        };
        let fits = |class: &str, bounds: &[String]| {
            bounds.is_empty() || bounds.iter().any(|b| ontology.is_a(class, b))
        };
        if !fits(&class_of(&checked, from), &definition.domain)
            || !fits(&class_of(&checked, to), &definition.range)
        {
            checked.drop(DOMAIN_RANGE, property);
            continue;
        }
        let Some((start, end)) = haystack.find(&relation.evidence) else {
            checked.drop(QUOTE_NOT_FOUND, property);
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
        let label = other.label.split_whitespace().collect::<Vec<_>>().join(" ");
        if label.is_empty() {
            checked.drop(EMPTY_LABEL, "");
            continue;
        }
        let Some(lang) = exact(languages, &other.lang) else {
            checked.drop(LANGUAGE, &other.lang);
            continue;
        };
        let (Some(from), Some(to)) = (endpoint(&other.from), endpoint(&other.to)) else {
            checked.drop(UNKNOWN_REF, &label);
            continue;
        };
        let Some((start, end)) = haystack.find(&other.evidence) else {
            checked.drop(QUOTE_NOT_FOUND, &label);
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
                    {"ref": "e1", "class": iri("pump"), "name": "Booster Pump", "evidence": "the booster pump"},
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
        assert_eq!(checked.drops.values().sum::<i64>(), 4);
        assert_eq!(checked.drops[&format!("{CLASS_NAME}|{}", iri("Pump"))], 1);
        assert_eq!(
            checked.drops[&format!("{NOT_IN_SHORTLIST}|{}", iri("Site"))],
            1
        );
        assert_eq!(
            checked.drops[&format!("{NAME_NOT_FOUND}|{}", iri("Pump"))],
            1
        );
        assert_eq!(
            checked.drops[&format!("{QUOTE_NOT_FOUND}|{}", iri("Pump"))],
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
                    {"from": "E1", "property": iri("installedat"), "to": "r1", "evidence": "installed at"},
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
            checked.drops[&format!("{DOMAIN_RANGE}|{}", iri("installedAt"))],
            1
        );
        assert_eq!(
            checked.drops[&format!("{UNKNOWN_REF}|{}", iri("installedAt"))],
            1
        );
        assert_eq!(
            checked.drops[&format!("{PROPERTY_NOT_ALLOWED}|{}", iri("owns"))],
            1
        );
        assert_eq!(
            checked.drops[&format!("{QUOTE_NOT_FOUND}|{}", iri("installedAt"))],
            1
        );
        assert_eq!(checked.others.len(), 1);
        assert_eq!(checked.others[0].label, "placed near");
        assert_eq!(checked.others[0].lang, "en");
        assert_eq!(checked.others[0].to_class, iri("Site"));
        assert_eq!(checked.drops[&format!("{LANGUAGE}|de")], 1);
        assert_eq!(checked.drops[&format!("{UNKNOWN_REF}|near")], 1);
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
