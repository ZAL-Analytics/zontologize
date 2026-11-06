pub mod check;
pub mod report;
pub mod run;

use std::collections::{BTreeMap, BTreeSet};

use aho_corasick::{AhoCorasick, MatchKind};
use anyhow::{Context as _, Result};
use serde::Deserialize;
use sha2::{Digest, Sha256};

use crate::hash::hex;
use crate::llm::prompt::Prompt;
use crate::llm::shortlist::{label, pick};
use crate::llm::{ModelName, Request};
use crate::ns::local_name;
use crate::ontology::{Ontology, Property, ValueKind};
use crate::units::{UnitFinder, UnitIndex};

pub const TASK: &str = "attributes";
pub const CACHE_VERSION: u32 = 1;
pub const CHECK_VERSION: u32 = 2;
pub const DOC: &str = "doc";
pub const USER_VARIABLES: [&str; 6] = [
    "path",
    "pages",
    "subjects",
    "properties",
    "context",
    "chunk",
];
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
    pub max_subjects: usize,
    pub unit_symbols: usize,
    pub check_key: &'a str,
    pub system: &'a Prompt,
    pub user: &'a Prompt,
    pub model: &'a ModelName,
}

#[derive(Debug, Default, Clone, PartialEq)]
pub struct Hits {
    pub kinds: BTreeSet<String>,
    pub properties: BTreeSet<String>,
}

impl Hits {
    pub fn is_empty(&self) -> bool {
        self.kinds.is_empty() && self.properties.is_empty()
    }
}

pub struct Matcher {
    units: UnitFinder,
    labels: Option<AhoCorasick>,
    owners: Vec<String>,
}

fn property_texts(property: &Property) -> Vec<String> {
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
        let lowered = crate::text::nfkc_squash(text).to_lowercase();
        let hits = Hits {
            kinds: self.units.kinds(&lowered),
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

fn property_hit(property: &Property, hits: &Hits) -> bool {
    hits.properties.contains(&property.iri)
        || property
            .quantity_kind
            .as_ref()
            .is_some_and(|k| hits.kinds.contains(k))
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
        let (kind, units) = match (&property.value, &property.quantity_kind) {
            (Some(ValueKind::Number), Some(quantity)) => (
                "number",
                context
                    .units
                    .symbols(quantity, context.unit_symbols)
                    .join(", "),
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

pub fn check_key(units: &UnitIndex, max_text_chars: usize) -> String {
    let mut hasher = Sha256::new();
    hasher.update(CHECK_VERSION.to_le_bytes());
    hasher.update(units.fingerprint().as_bytes());
    hasher.update((max_text_chars as u64).to_le_bytes());
    hex(&hasher.finalize())
}

pub fn state_key(cache_key: &str, check_key: &str, subjects: &[Subject]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(cache_key.as_bytes());
    hasher.update(check_key.as_bytes());
    for subject in subjects {
        hasher.update(subject.entity_id.to_le_bytes());
    }
    hex(&hasher.finalize())
}

pub fn build(context: &Context, input: &ChunkInput, hits: &Hits) -> Option<Built> {
    let ontology = context.ontology;
    let candidates: Vec<&Subject> = input
        .known
        .iter()
        .take(context.max_subjects)
        .chain(&input.doc)
        .collect();
    let mut found: Vec<(&String, Vec<String>)> = Vec::new();
    for property in ontology.properties.values().filter(|p| p.is_attribute()) {
        if !property_hit(property, hits) {
            continue;
        }
        let allowed: Vec<String> = candidates
            .iter()
            .filter(|s| ontology.fits(&s.class, &property.domain))
            .map(|s| s.reference.clone())
            .collect();
        if !allowed.is_empty() {
            found.push((&property.iri, allowed));
        }
    }
    if found.is_empty() {
        return None;
    }
    if found.len() > context.max_properties {
        found.sort_by_key(|(iri, _)| !hits.properties.contains(*iri));
        found.truncate(context.max_properties);
        found.sort_by(|a, b| a.0.cmp(b.0));
    }
    let mut properties = Vec::new();
    let mut refs: BTreeMap<String, Vec<String>> = BTreeMap::new();
    let mut used: BTreeSet<String> = BTreeSet::new();
    for (iri, allowed) in found {
        used.extend(allowed.iter().cloned());
        properties.push(iri.clone());
        refs.insert(iri.clone(), allowed);
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
    let cache_key = request.stage_key(context.model, CACHE_VERSION);
    let key = state_key(&cache_key, context.check_key, &subjects);
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
        ex:stages a owl:DatatypeProperty ; rdfs:domain ex:Asset ;
            rdfs:range xsd:nonNegativeInteger ;
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
        built_with(onto, input, 30, 30)
    }

    fn built_with(
        onto: &Ontology,
        input: &ChunkInput,
        max_properties: usize,
        max_subjects: usize,
    ) -> Option<Built> {
        let units = UnitIndex::new(onto);
        let matcher = Matcher::new(onto, &units).unwrap();
        let system = Prompt {
            source: "system".to_string(),
            text: "system".to_string(),
        };
        let user = crate::llm::prompt::load(None, "attributes-user").unwrap();
        let model = ModelName::parse("claude-code:sonnet").unwrap();
        let languages = vec!["en".to_string(), "nl".to_string()];
        let check = check_key(&units, 120);
        let context = Context {
            ontology: onto,
            units: &units,
            languages: &languages,
            max_properties,
            max_subjects,
            unit_symbols: 12,
            check_key: &check,
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
        assert_eq!(hits.kinds.len(), 2);
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
    fn caps_keep_named_properties_and_the_first_subjects() {
        let first = subject("r1", 10, "Pump", "P-101");
        let second = subject("r2", 11, "Pump", "P-102");
        let chunk = input(
            "P-101 and P-102: pressure 10 bar, 7,5 kW",
            vec![first.clone(), second],
            None,
        );
        let all = built_for(&ontology(), &chunk).unwrap();
        assert_eq!(all.properties, [iri("power"), iri("pressure")]);
        assert_eq!(all.subjects.len(), 2);
        let capped = built_with(&ontology(), &chunk, 1, 1).unwrap();
        assert_eq!(capped.properties, [iri("pressure")]);
        assert_eq!(capped.subjects, [first]);
    }

    #[test]
    fn an_attribute_without_a_domain_is_never_asked() {
        let config = OntologyConfig {
            files: Vec::new(),
            languages: vec!["en".to_string(), "nl".to_string()],
            include: vec![iri("Asset"), iri("Company"), iri("Document"), iri("colour")],
            exclude: Vec::new(),
            document_class: String::new(),
        };
        let text = format!(
            "{TTL}\nex:colour a owl:DatatypeProperty ; rdfs:range xsd:string ; skos:prefLabel \"colour\"@en ."
        );
        let onto =
            crate::ontology::load_sources(&[("t.ttl".to_string(), text.into())], &config).ontology;
        assert!(onto.properties[&iri("colour")].is_attribute());
        let pump = subject("r1", 10, "Pump", "P-101");
        let chunk = input("P-101 colour red", vec![pump], None);
        assert!(built_for(&onto, &chunk).is_none());
    }

    #[test]
    fn a_new_unit_alias_changes_the_key_but_not_the_request() {
        let pump = subject("r1", 10, "Pump", "P-101");
        let chunk = input("P-101 power 7,5 kWe", vec![pump], None);
        let before = built_for(&ontology(), &chunk).unwrap();
        let config = OntologyConfig {
            files: Vec::new(),
            languages: vec!["en".to_string(), "nl".to_string()],
            include: vec![iri("Asset"), iri("Company"), iri("Document")],
            exclude: Vec::new(),
            document_class: String::new(),
        };
        let text = format!("{TTL}\nex:kW skos:altLabel \"kWe\" .");
        let changed =
            crate::ontology::load_sources(&[("t.ttl".to_string(), text.into())], &config).ontology;
        let after = built_for(&changed, &chunk).unwrap();
        assert_eq!(before.cache_key, after.cache_key);
        assert_ne!(before.key, after.key);
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
