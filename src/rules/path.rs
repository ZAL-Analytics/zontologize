use std::collections::BTreeMap;

use regex::{Captures, Regex};
use serde::Deserialize;

use super::{DocumentFacts, Endpoint, EntityHit, KeyCase, RelationHit};
use crate::ontology::Ontology;

const DOCUMENT: &str = "document";

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PathRuleSpec {
    pattern: String,
    #[serde(default)]
    doc_type: String,
    #[serde(default)]
    props: BTreeMap<String, String>,
    #[serde(default)]
    entities: Vec<EntitySpec>,
    #[serde(default)]
    relations: Vec<RelationSpec>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct EntitySpec {
    id: String,
    class: String,
    key: String,
    #[serde(default)]
    name: String,
    #[serde(default)]
    key_case: KeyCase,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RelationSpec {
    from: String,
    property: String,
    to: String,
}

#[derive(Debug)]
struct Template(Vec<Part>);

#[derive(Debug)]
enum Part {
    Text(String),
    Group(String),
}

impl Template {
    fn parse(text: &str, regex: &Regex) -> Result<Template, String> {
        let mut parts = Vec::new();
        let mut rest = text;
        while let Some(open) = rest.find('{') {
            if open > 0 {
                parts.push(Part::Text(rest[..open].to_string()));
            }
            let Some(close) = rest[open..].find('}') else {
                return Err(format!("{text:?} has a {{ without }}"));
            };
            let name = &rest[open + 1..open + close];
            if !regex.capture_names().flatten().any(|n| n == name) {
                return Err(format!(
                    "{text:?} uses {{{name}}}, which is not a named group in the pattern"
                ));
            }
            parts.push(Part::Group(name.to_string()));
            rest = &rest[open + close + 1..];
        }
        if !rest.is_empty() {
            parts.push(Part::Text(rest.to_string()));
        }
        Ok(Template(parts))
    }

    fn fill(&self, captures: &Captures) -> String {
        let mut out = String::new();
        for part in &self.0 {
            match part {
                Part::Text(text) => out.push_str(text),
                Part::Group(name) => out.push_str(captures.name(name).map_or("", |m| m.as_str())),
            }
        }
        out
    }
}

#[derive(Debug)]
struct EntityRule {
    id: String,
    class: String,
    key: Template,
    name: Template,
    key_case: KeyCase,
}

#[derive(Debug)]
struct RelationRule {
    from: String,
    property: String,
    to: String,
}

#[derive(Debug)]
pub struct PathRule {
    regex: Regex,
    doc_type: Option<Template>,
    props: Vec<(String, Template)>,
    entities: Vec<EntityRule>,
    relations: Vec<RelationRule>,
}

impl PathRule {
    pub fn compile(
        spec: &PathRuleSpec,
        ontology: &Ontology,
        doc_types: &BTreeMap<String, String>,
        document_class: Option<&str>,
    ) -> Result<PathRule, String> {
        let regex = Regex::new(&spec.pattern).map_err(|e| format!("pattern: {e}"))?;
        let doc_type = if spec.doc_type.is_empty() {
            None
        } else {
            Some(Template::parse(&spec.doc_type, &regex).map_err(|e| format!("doc_type: {e}"))?)
        };
        let mut props = Vec::new();
        for (name, value) in &spec.props {
            let template =
                Template::parse(value, &regex).map_err(|e| format!("props.{name}: {e}"))?;
            props.push((name.clone(), template));
        }
        let mut entities = Vec::new();
        for entity in &spec.entities {
            if entity.id == DOCUMENT || entities.iter().any(|e: &EntityRule| e.id == entity.id) {
                return Err(format!(
                    "entities: id {:?} is reserved or used twice",
                    entity.id
                ));
            }
            let class = ontology
                .class(&entity.class)
                .map_err(|e| format!("entities.{}: {e}", entity.id))?;
            let name_text = if entity.name.is_empty() {
                &entity.key
            } else {
                &entity.name
            };
            entities.push(EntityRule {
                id: entity.id.clone(),
                class: class.iri.clone(),
                key: Template::parse(&entity.key, &regex)
                    .map_err(|e| format!("entities.{}.key: {e}", entity.id))?,
                name: Template::parse(name_text, &regex)
                    .map_err(|e| format!("entities.{}.name: {e}", entity.id))?,
                key_case: entity.key_case,
            });
        }
        let document_classes: Vec<String> = match (&doc_type, &spec.doc_type) {
            (Some(Template(parts)), text) if parts.iter().all(|p| matches!(p, Part::Text(_))) => {
                match doc_types.get(text) {
                    Some(class) => vec![class.clone()],
                    None => document_class.map(str::to_string).into_iter().collect(),
                }
            }
            _ => document_class
                .map(str::to_string)
                .into_iter()
                .chain(doc_types.values().cloned())
                .collect(),
        };
        let mut relations = Vec::new();
        for relation in &spec.relations {
            let property = ontology
                .property(&relation.property)
                .map_err(|e| format!("relations: {e}"))?;
            let class_of = |id: &str| -> Result<Vec<String>, String> {
                if id == DOCUMENT {
                    return Ok(document_classes.clone());
                }
                entities
                    .iter()
                    .find(|e| e.id == id)
                    .map(|e| vec![e.class.clone()])
                    .ok_or_else(|| format!("relations: {id:?} is not an entity id of this rule"))
            };
            for (end, id, allowed) in [
                ("from", &relation.from, &property.domain),
                ("to", &relation.to, &property.range),
            ] {
                for class in class_of(id)? {
                    if !allowed.is_empty() && !allowed.iter().any(|a| ontology.is_a(&class, a)) {
                        return Err(format!(
                            "relations: {} {end} {id} has class {class}, outside its {} {}",
                            relation.property,
                            if end == "from" { "domain" } else { "range" },
                            allowed.join(", ")
                        ));
                    }
                }
            }
            relations.push(RelationRule {
                from: relation.from.clone(),
                property: property.iri.clone(),
                to: relation.to.clone(),
            });
        }
        Ok(PathRule {
            regex,
            doc_type,
            props,
            entities,
            relations,
        })
    }

    pub fn uses_document(&self) -> bool {
        self.relations
            .iter()
            .any(|r| r.from == DOCUMENT || r.to == DOCUMENT)
    }

    pub fn apply(&self, path: &str, facts: &mut DocumentFacts) {
        let Some(captures) = self.regex.captures(path) else {
            return;
        };
        let whole = captures.get(0).map_or(0..0, |m| m.range());
        if let Some(doc_type) = &self.doc_type {
            let value = doc_type.fill(&captures);
            if !value.trim().is_empty() {
                facts.doc_type = Some(value.trim().to_string());
            }
        }
        for (name, template) in &self.props {
            let value = template.fill(&captures);
            if !value.trim().is_empty() {
                facts.props.insert(name.clone(), value.trim().to_string());
            }
        }
        let mut local: Vec<(&str, usize)> = Vec::new();
        for entity in &self.entities {
            let key = entity.key_case.apply(&entity.key.fill(&captures));
            if key.is_empty() {
                continue;
            }
            let name = entity.name.fill(&captures).trim().to_string();
            local.push((&entity.id, facts.entities.len()));
            facts.entities.push(EntityHit {
                class: entity.class.clone(),
                name: if name.is_empty() { key.clone() } else { name },
                key,
                start: whole.start,
                end: whole.end,
                method: "path",
                confidence: 1.0,
            });
        }
        let endpoint = |id: &str| -> Option<Endpoint> {
            if id == DOCUMENT {
                return Some(Endpoint::Document);
            }
            local
                .iter()
                .find(|(local_id, _)| *local_id == id)
                .map(|(_, index)| Endpoint::Entity(*index))
        };
        for relation in &self.relations {
            if let (Some(from), Some(to)) = (endpoint(&relation.from), endpoint(&relation.to)) {
                facts.relations.push(RelationHit {
                    from,
                    property: relation.property.clone(),
                    to,
                });
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::tests::rules;
    use super::*;

    const RULE: &str = r#"
[doc_types]
report = "ex:Report"

[[path_rules]]
pattern = '^Projects/(?P<year>\d{4})/(?P<project>p-\d{4}-\d{4}) (?P<customer>[^/]+?)/(?P<folder>[^/]+)/'
props = { year = "{year}", folder = "{folder}" }

[[path_rules.entities]]
id = "project"
class = "ex:Project"
key = "{project}"
name = "{project} {customer}"
key_case = "upper"

[[path_rules.entities]]
id = "customer"
class = "ex:Customer"
key = " {customer} "

[[path_rules.relations]]
from = "document"
property = "ex:describes"
to = "project"

[[path_rules.relations]]
from = "project"
property = "ex:forCustomer"
to = "customer"

[[path_rules]]
pattern = '/Reports/'
doc_type = "report"
"#;

    #[test]
    fn fills_placeholders_and_relations() {
        let rules = rules(RULE).unwrap();
        let facts = rules.document_facts("Projects/2021/p-2021-0042 Northwind Ltd/Reports/a.pdf");
        assert_eq!(facts.doc_type.as_deref(), Some("report"));
        assert_eq!(facts.props["year"], "2021");
        assert_eq!(facts.props["folder"], "Reports");
        let path_entities: Vec<(&str, &str)> = facts
            .entities
            .iter()
            .filter(|e| e.method == "path")
            .map(|e| (e.key.as_str(), e.name.as_str()))
            .collect();
        assert_eq!(
            path_entities,
            [
                ("P-2021-0042", "p-2021-0042 Northwind Ltd"),
                ("Northwind Ltd", "Northwind Ltd")
            ]
        );
        assert_eq!(
            facts.relations,
            [
                RelationHit {
                    from: Endpoint::Document,
                    property: "http://example.org/onto#describes".to_string(),
                    to: Endpoint::Entity(0),
                },
                RelationHit {
                    from: Endpoint::Entity(0),
                    property: "http://example.org/onto#forCustomer".to_string(),
                    to: Endpoint::Entity(1),
                },
            ]
        );
    }

    #[test]
    fn non_matching_path_gives_nothing() {
        let rules = rules(RULE).unwrap();
        let facts = rules.document_facts("Other/readme.txt");
        assert_eq!(facts, DocumentFacts::default());
    }

    #[test]
    fn unknown_group_is_an_error() {
        let text = "[[path_rules]]\npattern = '(?P<a>x)'\nprops = { b = \"{nope}\" }\n";
        let error = format!("{}", rules(text).err().unwrap());
        assert!(error.contains("path_rules[0]"), "{error}");
        assert!(error.contains("{nope}"), "{error}");
    }

    #[test]
    fn unknown_class_is_an_error() {
        let text = "[[path_rules]]\npattern = '(?P<a>x)'\n\
            [[path_rules.entities]]\nid = \"e\"\nclass = \"ex:Hidden\"\nkey = \"{a}\"\n";
        let error = format!("{}", rules(text).err().unwrap());
        assert!(error.contains("not selected"), "{error}");
    }

    #[test]
    fn domain_violation_is_an_error() {
        let text = "[[path_rules]]\npattern = '(?P<a>x)'\n\
            [[path_rules.entities]]\nid = \"c\"\nclass = \"ex:Customer\"\nkey = \"{a}\"\n\
            [[path_rules.relations]]\nfrom = \"c\"\nproperty = \"ex:describes\"\nto = \"document\"\n";
        let error = format!("{}", rules(text).err().unwrap());
        assert!(error.contains("outside its domain"), "{error}");
    }

    #[test]
    fn empty_key_skips_entity_and_relation() {
        let text = "[[path_rules]]\npattern = '^(?P<p>P-\\d+)?x'\n\
            [[path_rules.entities]]\nid = \"p\"\nclass = \"ex:Project\"\nkey = \"{p}\"\n\
            [[path_rules.relations]]\nfrom = \"document\"\nproperty = \"ex:describes\"\nto = \"p\"\n";
        let rules = rules(text).unwrap();
        let facts = rules.document_facts("x.txt");
        assert!(facts.entities.is_empty());
        assert!(facts.relations.is_empty());
        assert_eq!(rules.document_facts("P-1x").relations.len(), 1);
    }
}
