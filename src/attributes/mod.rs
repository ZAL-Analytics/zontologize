pub mod check;
pub mod report;
pub mod run;

use std::collections::{BTreeMap, BTreeSet};

use aho_corasick::{AhoCorasick, MatchKind};
use anyhow::{Context as _, Result};
use serde::Deserialize;
use sha2::{Digest, Sha256};

use crate::llm::prompt::Prompt;
use crate::llm::shortlist::{label, local_name, pick};
use crate::llm::{ModelName, Request};
use crate::ontology::{Ontology, Property, ValueKind, hex};
use crate::units::{UnitFinder, UnitIndex};

pub const TASK: &str = "attributes";
pub const STAGE_VERSION: u32 = 1;
pub const DOC: &str = "doc";
pub const USER_VARIABLES: [&str; 6] = [
    "path",
    "pages",
    "subjects",
    "properties",
    "context",
    "chunk",
];
const UNIT_SYMBOLS: usize = 12;

#[derive(Debug, Clone, Deserialize)]
pub struct Answer {
    pub values: Vec<ValueOut>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ValueOut {
    #[serde(rename = "ref")]
    pub reference: String,
    pub property: String,
    pub min: Option<f64>,
    pub max: Option<f64>,
    pub unit: String,
    pub text: String,
    pub evidence: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Subject {
    pub reference: String,
    pub entity_id: i64,
    pub class: String,
    pub name: String,
}

#[derive(Debug, Clone)]
pub struct ChunkInput {
    pub id: i64,
    pub text: String,
    pub context: String,
    pub path: String,
    pub pages: String,
    pub known: Vec<Subject>,
    pub doc: Option<Subject>,
}

#[derive(Debug, Clone)]
pub struct Built {
    pub subjects: Vec<Subject>,
    pub properties: Vec<String>,
    pub refs: BTreeMap<String, Vec<String>>,
    pub request: Request,
    pub cache_key: String,
    pub key: String,
}

pub struct Context<'a> {
    pub ontology: &'a Ontology,
    pub units: &'a UnitIndex<'a>,
    pub languages: &'a [String],
    pub max_properties: usize,
    pub system: &'a Prompt,
    pub user: &'a Prompt,
    pub model: &'a ModelName,
}

#[derive(Debug, Default, Clone, PartialEq)]
pub struct Hits {
    pub dimensions: BTreeSet<String>,
    pub properties: BTreeSet<String>,
}

impl Hits {
    pub fn is_empty(&self) -> bool {
        self.dimensions.is_empty() && self.properties.is_empty()
    }
}

pub struct Matcher {
    units: UnitFinder,
    labels: Option<AhoCorasick>,
    owners: Vec<String>,
}

pub fn property_texts(property: &Property) -> Vec<String> {
    let mut texts: BTreeSet<String> = property.labels.values().map(|l| l.to_lowercase()).collect();
    texts.extend(
        property
            .alt_labels
            .values()
            .flatten()
            .map(|l| l.to_lowercase()),
    );
    texts.into_iter().filter(|t| !t.trim().is_empty()).collect()
}

impl Matcher {
    pub fn new(ontology: &Ontology, units: &UnitIndex) -> Result<Matcher> {
        let finder = units.finder()?;
        let mut patterns = Vec::new();
        let mut owners = Vec::new();
        for property in ontology.properties.values().filter(|p| p.is_attribute()) {
            for text in property_texts(property) {
                patterns.push(text);
                owners.push(property.iri.clone());
            }
        }
        let labels = if patterns.is_empty() {
            None
        } else {
            Some(
                AhoCorasick::builder()
                    .ascii_case_insensitive(true)
                    .match_kind(MatchKind::Standard)
                    .build(&patterns)
                    .context("cannot build the property label matcher")?,
            )
        };
        Ok(Matcher {
            units: finder,
            labels,
            owners,
        })
    }

    pub fn hits(&self, text: &str) -> Hits {
        let lowered = crate::units::normalise(text).to_lowercase();
        let hits = Hits {
            dimensions: self.units.dimensions(&lowered),
            properties: BTreeSet::new(),
        };
        let Some(labels) = &self.labels else {
            return hits;
        };
        self.label_hits(labels, &lowered, hits)
    }

    fn label_hits(&self, labels: &AhoCorasick, text: &str, mut hits: Hits) -> Hits {
        let word = |c: Option<char>| c.is_some_and(char::is_alphanumeric);
        for found in labels.find_overlapping_iter(text) {
            let before = text[..found.start()].chars().next_back();
            let after = text[found.end()..].chars().next();
            if !word(before) && !word(after) {
                hits.properties
                    .insert(self.owners[found.pattern().as_usize()].clone());
            }
        }
        hits
    }
}

fn allows(ontology: &Ontology, property: &Property, class: &str) -> bool {
    property.domain.iter().any(|d| ontology.is_a(class, d))
}

fn property_hit(property: &Property, hits: &Hits) -> bool {
    hits.properties.contains(&property.iri)
        || property
            .dimension
            .as_ref()
            .is_some_and(|d| hits.dimensions.contains(d))
}

fn class_name(ontology: &Ontology, iri: &str, languages: &[String]) -> String {
    ontology
        .classes
        .get(iri)
        .map_or_else(|| local_name(iri).to_string(), |c| label(c, languages))
}

fn subject_lines(ontology: &Ontology, subjects: &[Subject], languages: &[String]) -> String {
    let mut out = String::new();
    for subject in subjects {
        let class = class_name(ontology, &subject.class, languages);
        let name = if subject.reference == DOC {
            "this document"
        } else {
            &subject.name
        };
        out.push_str(&format!("- {} | {class} | {name}\n", subject.reference));
    }
    out
}

fn property_lines(
    context: &Context,
    properties: &[String],
    refs: &BTreeMap<String, Vec<String>>,
) -> String {
    let mut out = String::new();
    for iri in properties {
        let Some(property) = context.ontology.properties.get(iri) else {
            continue;
        };
        let name = pick(&property.labels, context.languages)
            .unwrap_or_else(|| local_name(iri).to_string());
        let (kind, units) = match (&property.value, &property.dimension) {
            (Some(ValueKind::Number), Some(dimension)) => (
                "number",
                context.units.symbols(dimension, UNIT_SYMBOLS).join(", "),
            ),
            (Some(ValueKind::Number), None) => ("number", "no unit".to_string()),
            _ => ("text", "no unit".to_string()),
        };
        let allowed = refs.get(iri).map(|r| r.join(", ")).unwrap_or_default();
        out.push_str(&format!(
            "- {iri} | {name} | {kind} | {units} | {allowed}\n"
        ));
    }
    out
}

pub fn schema(properties: &[String], refs: &[String]) -> serde_json::Value {
    let text = serde_json::json!({"type": "string"});
    let number = serde_json::json!({"type": ["number", "null"]});
    serde_json::json!({
        "type": "object",
        "additionalProperties": false,
        "required": ["values"],
        "properties": {
            "values": {
                "type": "array",
                "items": {
                    "type": "object",
                    "additionalProperties": false,
                    "required": ["ref", "property", "min", "max", "unit", "text", "evidence"],
                    "properties": {
                        "ref": {"type": "string", "enum": refs},
                        "property": {"type": "string", "enum": properties},
                        "min": number,
                        "max": number,
                        "unit": text,
                        "text": text,
                        "evidence": text,
                    },
                },
            },
        },
    })
}

pub fn cache_key(request: &Request, model: &ModelName) -> String {
    let mut hasher = Sha256::new();
    hasher.update(TASK.as_bytes());
    hasher.update(STAGE_VERSION.to_le_bytes());
    hasher.update(request.key(model).as_bytes());
    hex(&hasher.finalize())
}

pub fn state_key(cache_key: &str, subjects: &[Subject]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(cache_key.as_bytes());
    for subject in subjects {
        hasher.update(subject.entity_id.to_le_bytes());
    }
    hex(&hasher.finalize())
}

pub fn build(context: &Context, input: &ChunkInput, hits: &Hits) -> Option<Built> {
    let ontology = context.ontology;
    let candidates: Vec<&Subject> = input.known.iter().chain(&input.doc).collect();
    let mut properties = Vec::new();
    let mut refs: BTreeMap<String, Vec<String>> = BTreeMap::new();
    let mut used: BTreeSet<String> = BTreeSet::new();
    for property in ontology.properties.values().filter(|p| p.is_attribute()) {
        if properties.len() >= context.max_properties || !property_hit(property, hits) {
            continue;
        }
        let allowed: Vec<String> = candidates
            .iter()
            .filter(|s| allows(ontology, property, &s.class))
            .map(|s| s.reference.clone())
            .collect();
        if allowed.is_empty() {
            continue;
        }
        used.extend(allowed.iter().cloned());
        properties.push(property.iri.clone());
        refs.insert(property.iri.clone(), allowed);
    }
    if properties.is_empty() {
        return None;
    }
    let subjects: Vec<Subject> = candidates
        .into_iter()
        .filter(|s| used.contains(&s.reference))
        .cloned()
        .collect();
    let references: Vec<String> = subjects.iter().map(|s| s.reference.clone()).collect();
    let user = context.user.fill(&[
        ("path", &input.path),
        ("pages", &input.pages),
        (
            "subjects",
            subject_lines(ontology, &subjects, context.languages).trim_end(),
        ),
        (
            "properties",
            property_lines(context, &properties, &refs).trim_end(),
        ),
        ("context", &input.context),
        ("chunk", &input.text),
    ]);
    let request = Request {
        task: TASK.to_string(),
        system: context.system.text.clone(),
        user,
        schema: schema(&properties, &references),
    };
    let cache_key = cache_key(&request, context.model);
    let key = state_key(&cache_key, &subjects);
    Some(Built {
        subjects,
        properties,
        refs,
        request,
        cache_key,
        key,
    })
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
        @prefix xsd: <http://www.w3.org/2001/XMLSchema#> .
        @prefix qudt: <http://qudt.org/schema/qudt/> .
        @prefix qkdv: <http://qudt.org/vocab/dimensionvector/> .
        ex:Asset a owl:Class ; skos:prefLabel "Asset"@en .
        ex:Pump a owl:Class ; rdfs:subClassOf ex:Asset ; skos:prefLabel "Pump"@en .
        ex:Company a owl:Class ; skos:prefLabel "Company"@en .
        ex:Document a owl:Class ; skos:prefLabel "Document"@en .
        ex:Sheet a owl:Class ; rdfs:subClassOf ex:Document ; skos:prefLabel "Data sheet"@en .
        ex:Letter a owl:Class ; rdfs:subClassOf ex:Document ; skos:prefLabel "Letter"@en .
        ex:Pressure a qudt:QuantityKind ; qudt:hasDimensionVector qkdv:P .
        ex:Power a qudt:QuantityKind ; qudt:hasDimensionVector qkdv:W .
        ex:bar a qudt:Unit ; qudt:symbol "bar" ; qudt:conversionMultiplier 100000.0 ;
            qudt:hasDimensionVector qkdv:P .
        ex:barg a qudt:Unit ; qudt:symbol "barg" ; qudt:conversionMultiplier 100000.0 ;
            qudt:conversionOffset 1.01325 ; qudt:hasDimensionVector qkdv:P .
        ex:kW a qudt:Unit ; qudt:symbol "kW" ; qudt:conversionMultiplier 1000.0 ;
            qudt:hasDimensionVector qkdv:W .
        ex:MW a qudt:Unit ; qudt:symbol "MW" ; qudt:conversionMultiplier 1000000.0 ;
            qudt:hasDimensionVector qkdv:W .
        ex:mW a qudt:Unit ; qudt:symbol "mW" ; qudt:conversionMultiplier 0.001 ;
            qudt:hasDimensionVector qkdv:W .
        ex:pressure a owl:DatatypeProperty ; rdfs:domain ex:Asset ;
            qudt:hasQuantityKind ex:Pressure ; skos:prefLabel "pressure"@en ;
            skos:altLabel "druk"@nl .
        ex:power a owl:DatatypeProperty ; rdfs:domain ex:Asset, ex:Sheet ;
            qudt:hasQuantityKind ex:Power ; skos:prefLabel "power"@en .
        ex:stages a owl:DatatypeProperty ; rdfs:domain ex:Asset ; rdfs:range xsd:integer ;
            skos:prefLabel "stages"@en .
        ex:material a owl:DatatypeProperty ; rdfs:domain ex:Asset ; rdfs:range xsd:string ;
            skos:prefLabel "material"@en ; skos:altLabel "materiaal"@nl .
    "#;

    pub fn ontology() -> Ontology {
        let config = OntologyConfig {
            files: Vec::new(),
            languages: vec!["en".to_string(), "nl".to_string()],
            include: vec![
                format!("{EX}Asset"),
                format!("{EX}Company"),
                format!("{EX}Document"),
            ],
            exclude: Vec::new(),
            document_class: String::new(),
        };
        let loaded = crate::ontology::load_sources(&[("t.ttl".to_string(), TTL.into())], &config);
        assert!(!loaded.has_errors(), "{:?}", loaded.findings);
        loaded.ontology
    }

    pub fn iri(name: &str) -> String {
        format!("{EX}{name}")
    }

    pub fn subject(reference: &str, id: i64, class: &str, name: &str) -> Subject {
        Subject {
            reference: reference.to_string(),
            entity_id: id,
            class: iri(class),
            name: name.to_string(),
        }
    }

    pub fn input(text: &str, known: Vec<Subject>, doc: Option<Subject>) -> ChunkInput {
        ChunkInput {
            id: 1,
            text: text.to_string(),
            context: String::new(),
            path: "a/sheet.pdf".to_string(),
            pages: "1".to_string(),
            known,
            doc,
        }
    }

    pub fn built_for(onto: &Ontology, input: &ChunkInput) -> Option<Built> {
        let units = UnitIndex::new(onto);
        let matcher = Matcher::new(onto, &units).unwrap();
        let system = Prompt {
            source: "system".to_string(),
            text: "system".to_string(),
        };
        let user = crate::llm::prompt::load(None, "attributes-user").unwrap();
        let model = ModelName::parse("claude-code:sonnet").unwrap();
        let languages = vec!["en".to_string(), "nl".to_string()];
        let context = Context {
            ontology: onto,
            units: &units,
            languages: &languages,
            max_properties: 30,
            system: &system,
            user: &user,
            model: &model,
        };
        let mut hits = matcher.hits(&input.text);
        let context_hits = matcher.hits(&input.context);
        hits.properties.extend(context_hits.properties);
        build(&context, input, &hits)
    }

    #[test]
    fn prompt_variables_are_known() {
        let user = crate::llm::prompt::load(None, "attributes-user").unwrap();
        user.check(&USER_VARIABLES).unwrap();
        let system = crate::llm::prompt::load(None, "attributes-system").unwrap();
        system.check(&[]).unwrap();
    }

    #[test]
    fn hits_find_units_and_labels_on_word_boundaries() {
        let onto = ontology();
        let units = UnitIndex::new(&onto);
        let matcher = Matcher::new(&onto, &units).unwrap();
        let hits = matcher.hits("Rated 7,5 kW. Materiaal: RVS. Druk max 10 barg");
        assert_eq!(hits.dimensions.len(), 2);
        assert_eq!(
            hits.properties.iter().cloned().collect::<Vec<_>>(),
            [iri("material"), iri("pressure")]
        );
        assert!(matcher.hits("the powerhouse at 10 o'clock").is_empty());
        assert!(!matcher.hits("POWER [kW]").properties.is_empty());
    }

    #[test]
    fn a_chunk_needs_a_hit_and_a_subject_in_the_domain() {
        let onto = ontology();
        let pump = subject("r1", 10, "Pump", "P-101");
        let company = subject("r2", 11, "Company", "Acme");
        let letter = subject(DOC, 12, "Letter", "");
        let sheet = subject(DOC, 13, "Sheet", "");
        assert!(built_for(&onto, &input("nothing here", vec![pump.clone()], None)).is_none());
        assert!(
            built_for(
                &onto,
                &input("rated 7,5 kW", vec![company.clone()], Some(letter.clone()))
            )
            .is_none()
        );
        let built = built_for(
            &onto,
            &input("rated 7,5 kW", vec![company.clone()], Some(sheet.clone())),
        )
        .unwrap();
        assert_eq!(built.properties, [iri("power")]);
        assert_eq!(built.subjects, std::slice::from_ref(&sheet));
        let built = built_for(
            &onto,
            &input(
                "P-101: 7,5 kW, 10 bar, 3 stages",
                vec![pump.clone(), company],
                Some(sheet),
            ),
        )
        .unwrap();
        assert_eq!(
            built.properties,
            [iri("power"), iri("pressure"), iri("stages")]
        );
        assert_eq!(built.refs[&iri("power")], ["r1", "doc"]);
        assert_eq!(built.refs[&iri("pressure")], ["r1"]);
        let schema = built.request.schema.to_string();
        assert!(schema.contains("\"enum\":[\"r1\",\"doc\"]"), "{schema}");
        assert!(!schema.contains("material"));
        assert!(built.request.user.contains("| number | MW, kW, mW |"));
    }

    #[test]
    fn keys_follow_request_and_subject_ids() {
        let onto = ontology();
        let pump = subject("r1", 10, "Pump", "P-101");
        let a = built_for(&onto, &input("P-101: 7,5 kW", vec![pump.clone()], None)).unwrap();
        let b = built_for(&onto, &input("P-101: 7,5 kW", vec![pump.clone()], None)).unwrap();
        assert_eq!(a.key, b.key);
        let moved = subject("r1", 99, "Pump", "P-101");
        let c = built_for(&onto, &input("P-101: 7,5 kW", vec![moved], None)).unwrap();
        assert_eq!(a.cache_key, c.cache_key);
        assert_ne!(a.key, c.key);
        let other = format!(
            "{TTL}\nex:other a owl:DatatypeProperty ; rdfs:domain ex:Company ; skos:prefLabel \"other\"@en ; skos:altLabel \"kW\"@en ."
        );
        let config = OntologyConfig {
            files: Vec::new(),
            languages: vec!["en".to_string(), "nl".to_string()],
            include: vec![iri("Asset"), iri("Company"), iri("Document")],
            exclude: Vec::new(),
            document_class: String::new(),
        };
        let changed =
            crate::ontology::load_sources(&[("t.ttl".to_string(), other.into())], &config).ontology;
        let d = built_for(&changed, &input("P-101: 7,5 kW", vec![pump], None)).unwrap();
        assert_eq!(a.cache_key, d.cache_key);
    }
}
