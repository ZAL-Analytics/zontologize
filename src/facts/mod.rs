pub mod check;
pub mod report;
pub mod run;
pub mod sample;

use serde::Deserialize;
use sha2::{Digest, Sha256};

use crate::llm::prompt::Prompt;
use crate::llm::shortlist::{label, local_name, shortlist, shortlist_lines};
use crate::llm::{ModelName, Request};
use crate::ontology::{Ontology, PropertyKind, hex};

pub const TASK: &str = "extract";
pub const STAGE_VERSION: u32 = 1;
pub const DOC: &str = "doc";
pub const USER_VARIABLES: [&str; 7] = [
    "path",
    "pages",
    "languages",
    "shortlist",
    "properties",
    "known",
    "chunk",
];
const NONE: &str = "none";

#[derive(Debug, Clone, Deserialize)]
pub struct Answer {
    pub entities: Vec<EntityOut>,
    #[serde(default)]
    pub relations: Vec<RelationOut>,
    pub other_relations: Vec<OtherOut>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct EntityOut {
    #[serde(rename = "ref")]
    pub reference: String,
    pub class: String,
    pub name: String,
    pub evidence: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct RelationOut {
    pub from: String,
    pub property: String,
    pub to: String,
    pub evidence: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct OtherOut {
    pub from: String,
    pub label: String,
    pub lang: String,
    pub to: String,
    pub evidence: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Known {
    pub entity_id: i64,
    pub class: String,
    pub name: String,
}

#[derive(Debug, Clone)]
pub struct ChunkInput {
    pub id: i64,
    pub text: String,
    pub path: String,
    pub pages: String,
    pub counts: Vec<(String, i64)>,
    pub known: Vec<Known>,
    pub doc_class: Option<String>,
}

#[derive(Debug, Clone)]
pub struct Built {
    pub shortlist: Vec<String>,
    pub properties: Vec<String>,
    pub known: Vec<Known>,
    pub doc_class: Option<String>,
    pub request: Request,
    pub key: String,
}

pub struct Context<'a> {
    pub ontology: &'a Ontology,
    pub languages: &'a [String],
    pub max_shortlist: usize,
    pub exclude: &'a [String],
    pub system: &'a Prompt,
    pub user: &'a Prompt,
    pub model: &'a ModelName,
}

pub fn allowed_properties(ontology: &Ontology, available: &[&str]) -> Vec<String> {
    let fits = |bounds: &[String]| {
        bounds.is_empty()
            || available
                .iter()
                .any(|class| bounds.iter().any(|bound| ontology.is_a(class, bound)))
    };
    ontology
        .properties
        .values()
        .filter(|p| p.selected && p.kind == PropertyKind::Object)
        .filter(|p| fits(&p.domain) && fits(&p.range))
        .map(|p| p.iri.clone())
        .collect()
}

fn class_name(ontology: &Ontology, iri: &str, languages: &[String]) -> String {
    ontology
        .classes
        .get(iri)
        .map_or_else(|| local_name(iri).to_string(), |c| label(c, languages))
}

fn bound_names(ontology: &Ontology, bounds: &[String], languages: &[String]) -> String {
    if bounds.is_empty() {
        return "anything".to_string();
    }
    bounds
        .iter()
        .map(|b| class_name(ontology, b, languages))
        .collect::<Vec<_>>()
        .join(" or ")
}

pub fn property_lines(ontology: &Ontology, properties: &[String], languages: &[String]) -> String {
    let mut out = String::new();
    for iri in properties {
        let Some(property) = ontology.properties.get(iri) else {
            continue;
        };
        let name = crate::llm::shortlist::pick(&property.labels, languages)
            .unwrap_or_else(|| local_name(iri).to_string());
        out.push_str(&format!(
            "- {iri} | {name} | {} -> {}\n",
            bound_names(ontology, &property.domain, languages),
            bound_names(ontology, &property.range, languages)
        ));
    }
    if out.is_empty() {
        out.push_str("(none)\n");
    }
    out
}

pub fn known_lines(known: &[Known], doc_class: Option<&str>) -> String {
    let mut out = String::new();
    if let Some(class) = doc_class {
        out.push_str(&format!("- {DOC} | {class} | this document\n"));
    }
    for (i, entity) in known.iter().enumerate() {
        out.push_str(&format!(
            "- r{} | {} | {}\n",
            i + 1,
            entity.class,
            entity.name
        ));
    }
    if out.is_empty() {
        out.push_str("(none)\n");
    }
    out
}

pub fn schema(
    shortlist: &[String],
    properties: &[String],
    languages: &[String],
) -> serde_json::Value {
    let classes: Vec<&str> = if shortlist.is_empty() {
        vec![NONE]
    } else {
        shortlist.iter().map(String::as_str).collect()
    };
    let text = serde_json::json!({"type": "string"});
    let mut fields = serde_json::Map::new();
    fields.insert(
        "entities".to_string(),
        serde_json::json!({
            "type": "array",
            "items": {
                "type": "object",
                "additionalProperties": false,
                "required": ["ref", "class", "name", "evidence"],
                "properties": {
                    "ref": text,
                    "class": {"type": "string", "enum": classes},
                    "name": text,
                    "evidence": text,
                },
            },
        }),
    );
    let mut required = vec!["entities"];
    if !properties.is_empty() {
        fields.insert(
            "relations".to_string(),
            serde_json::json!({
                "type": "array",
                "items": {
                    "type": "object",
                    "additionalProperties": false,
                    "required": ["from", "property", "to", "evidence"],
                    "properties": {
                        "from": text,
                        "property": {"type": "string", "enum": properties},
                        "to": text,
                        "evidence": text,
                    },
                },
            }),
        );
        required.push("relations");
    }
    fields.insert(
        "other_relations".to_string(),
        serde_json::json!({
            "type": "array",
            "items": {
                "type": "object",
                "additionalProperties": false,
                "required": ["from", "label", "lang", "to", "evidence"],
                "properties": {
                    "from": text,
                    "label": text,
                    "lang": {"type": "string", "enum": languages},
                    "to": text,
                    "evidence": text,
                },
            },
        }),
    );
    required.push("other_relations");
    serde_json::json!({
        "type": "object",
        "additionalProperties": false,
        "required": required,
        "properties": fields,
    })
}

pub fn key(request: &Request, model: &ModelName) -> String {
    let mut hasher = Sha256::new();
    hasher.update(TASK.as_bytes());
    hasher.update(STAGE_VERSION.to_le_bytes());
    hasher.update(request.key(model).as_bytes());
    hex(&hasher.finalize())
}

pub fn build(context: &Context, input: &ChunkInput) -> Built {
    let ontology = context.ontology;
    let languages = context.languages;
    let list = shortlist(
        ontology,
        &input.counts,
        &ontology.roots,
        context.exclude,
        context.max_shortlist,
    );
    let known: Vec<Known> = input
        .known
        .iter()
        .take(context.max_shortlist)
        .cloned()
        .collect();
    let mut available: Vec<&str> = list.iter().map(String::as_str).collect();
    available.extend(known.iter().map(|k| k.class.as_str()));
    if let Some(class) = &input.doc_class {
        available.push(class);
    }
    let properties = allowed_properties(ontology, &available);
    let user = context.user.fill(&[
        ("path", &input.path),
        ("pages", &input.pages),
        ("languages", &languages.join(", ")),
        (
            "shortlist",
            shortlist_lines(ontology, &list, languages).trim_end(),
        ),
        (
            "properties",
            property_lines(ontology, &properties, languages).trim_end(),
        ),
        (
            "known",
            known_lines(&known, input.doc_class.as_deref()).trim_end(),
        ),
        ("chunk", &input.text),
    ]);
    let request = Request {
        task: TASK.to_string(),
        system: context.system.text.clone(),
        user,
        schema: schema(&list, &properties, languages),
    };
    let key = key(&request, context.model);
    Built {
        shortlist: list,
        properties,
        known,
        doc_class: input.doc_class.clone(),
        request,
        key,
    }
}

#[cfg(test)]
pub mod tests {
    use super::*;
    use crate::config::OntologyConfig;

    pub const EX: &str = "http://example.org/ex#";

    pub const TTL: &str = r#"
        @prefix ex: <http://example.org/ex#> .
        @prefix owl: <http://www.w3.org/2002/07/owl#> .
        @prefix rdfs: <http://www.w3.org/2000/01/rdf-schema#> .
        @prefix skos: <http://www.w3.org/2004/02/skos/core#> .
        ex:Asset a owl:Class ; skos:prefLabel "Asset"@en .
        ex:Pump a owl:Class ; rdfs:subClassOf ex:Asset ; skos:prefLabel "Pump"@en .
        ex:Site a owl:Class ; skos:prefLabel "Site"@en .
        ex:Company a owl:Class ; skos:prefLabel "Company"@en .
        ex:Document a owl:Class ; skos:prefLabel "Document"@en .
        ex:Report a owl:Class ; rdfs:subClassOf ex:Document ; skos:prefLabel "Report"@en .
        ex:installedAt a owl:ObjectProperty ; rdfs:domain ex:Asset ; rdfs:range ex:Site ;
            skos:prefLabel "installed at"@en .
        ex:mentions a owl:ObjectProperty ; rdfs:domain ex:Document ; rdfs:range ex:Asset .
        ex:owns a owl:ObjectProperty ; rdfs:domain ex:Company ; rdfs:range ex:Asset .
    "#;

    pub fn iri(name: &str) -> String {
        format!("{EX}{name}")
    }

    pub fn ontology_from(ttl: &str) -> Ontology {
        let config = OntologyConfig {
            files: vec!["a.ttl".to_string()],
            languages: vec!["en".to_string()],
            include: ["ex:Asset", "ex:Site", "ex:Company", "ex:Document"]
                .map(String::from)
                .to_vec(),
            exclude: vec![],
            document_class: String::new(),
        };
        let loaded = crate::ontology::load_sources(
            &[("a.ttl".to_string(), ttl.as_bytes().to_vec())],
            &config,
        );
        assert!(!loaded.has_errors(), "{:?}", loaded.findings);
        loaded.ontology
    }

    pub fn ontology() -> Ontology {
        ontology_from(TTL)
    }

    pub fn languages() -> Vec<String> {
        vec!["en".to_string()]
    }

    fn prompt(text: &str) -> Prompt {
        Prompt {
            source: "test".to_string(),
            text: text.to_string(),
        }
    }

    pub fn input(counts: &[(&str, i64)]) -> ChunkInput {
        ChunkInput {
            id: 1,
            text: "The booster pump at the north site.".to_string(),
            path: "a/b.txt".to_string(),
            pages: "1".to_string(),
            counts: counts.iter().map(|(c, n)| (iri(c), *n)).collect(),
            known: vec![],
            doc_class: None,
        }
    }

    pub fn built(ontology: &Ontology, input: &ChunkInput) -> Built {
        let system = prompt("system");
        let user =
            prompt("{path} {pages} {languages}\n{shortlist}\n{properties}\n{known}\n{chunk}");
        let model = ModelName::parse("claude-code:haiku").unwrap();
        let languages = languages();
        let context = Context {
            ontology,
            languages: &languages,
            max_shortlist: 10,
            exclude: &[],
            system: &system,
            user: &user,
            model: &model,
        };
        build(&context, input)
    }

    #[test]
    fn properties_need_domain_and_range_among_the_available_classes() {
        let onto = ontology();
        let mut allowed = allowed_properties(&onto, &[&iri("Pump")]);
        assert!(allowed.is_empty(), "{allowed:?}");
        allowed = allowed_properties(&onto, &[&iri("Pump"), &iri("Site")]);
        assert_eq!(allowed, [iri("installedAt")]);
        allowed = allowed_properties(&onto, &[&iri("Pump"), &iri("Report")]);
        assert_eq!(allowed, [iri("mentions")]);
    }

    #[test]
    fn document_class_and_known_entities_open_properties() {
        let onto = ontology();
        let mut chunk = input(&[("Pump", 2)]);
        assert!(built(&onto, &chunk).properties.is_empty());
        chunk.doc_class = Some(iri("Report"));
        assert_eq!(built(&onto, &chunk).properties, [iri("mentions")]);
        chunk.doc_class = None;
        chunk.known = vec![Known {
            entity_id: 7,
            class: iri("Company"),
            name: "Acme".to_string(),
        }];
        let result = built(&onto, &chunk);
        assert_eq!(result.properties, [iri("owns")]);
        assert!(
            result.request.user.contains("- r1 | "),
            "{}",
            result.request.user
        );
    }

    #[test]
    fn no_allowed_property_leaves_relations_out_of_the_schema() {
        let onto = ontology();
        let result = built(&onto, &input(&[("Pump", 1)]));
        let fields = &result.request.schema["properties"];
        assert!(fields.get("relations").is_none());
        assert_eq!(
            result.request.schema["required"],
            serde_json::json!(["entities", "other_relations"])
        );
        let with = built(&onto, &input(&[("Pump", 1), ("Site", 1)]));
        assert_eq!(
            with.request.schema["properties"]["relations"]["items"]["properties"]["property"]["enum"],
            serde_json::json!([iri("installedAt")])
        );
    }

    #[test]
    fn key_follows_what_the_request_shows() {
        let onto = ontology();
        let chunk = input(&[("Pump", 1)]);
        let before = built(&onto, &chunk).key;
        let alias = TTL.replace(
            r#"ex:Site a owl:Class ; skos:prefLabel "Site"@en ."#,
            r#"ex:Site a owl:Class ; skos:prefLabel "Site"@en ; skos:altLabel "Plant"@en ."#,
        );
        assert_eq!(built(&ontology_from(&alias), &chunk).key, before);
        let property = format!(
            "{TTL} ex:checks a owl:ObjectProperty ; rdfs:domain ex:Pump ; rdfs:range ex:Asset ."
        );
        assert_ne!(built(&ontology_from(&property), &chunk).key, before);
    }

    pub fn recorded<T: serde::de::DeserializeOwned>(name: &str) -> T {
        let path = format!(
            "{}/tests/fixtures/llm/claude-code/{name}",
            env!("CARGO_MANIFEST_DIR")
        );
        let text = std::fs::read_to_string(&path).unwrap();
        let line = text
            .lines()
            .find(|l| l.contains("structured_output"))
            .unwrap();
        let value: serde_json::Value = serde_json::from_str(line).unwrap();
        serde_json::from_value(value["structured_output"].clone()).unwrap()
    }

    #[test]
    fn recorded_answer_parses() {
        let answer: Answer = recorded("extract-recorded.jsonl");
        assert_eq!(answer.entities.len(), 1);
        assert_eq!(answer.relations.len(), 1);
        assert!(answer.relations[0].property.ends_with("#atSite"));
    }

    #[test]
    fn built_in_prompts_use_known_variables() {
        let system = crate::llm::prompt::load(None, "extract-system").unwrap();
        system.check(&[]).unwrap();
        let user = crate::llm::prompt::load(None, "extract-user").unwrap();
        user.check(&USER_VARIABLES).unwrap();
        assert!(user.check(&USER_VARIABLES[..6]).is_err());
    }
}
