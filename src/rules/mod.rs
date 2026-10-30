pub mod gazetteer;
pub mod ids;
pub mod path;

use std::collections::BTreeMap;
use std::path::Path;

use anyhow::{Context, Result, bail};
use serde::Deserialize;
use sha2::{Digest, Sha256};

use crate::ontology::{Ontology, hex};

pub const STAGE_VERSION: u32 = 1;
const DEFAULT_RULES: &str = include_str!("../../defaults/rules.toml");
const RULES_FILE: &str = "rules.toml";
const GAZETTEER_DIR: &str = "gazetteer";

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum KeyCase {
    #[default]
    Keep,
    Upper,
    Lower,
}

impl KeyCase {
    pub fn apply(self, key: &str) -> String {
        let key = key.trim();
        match self {
            KeyCase::Keep => key.to_string(),
            KeyCase::Upper => key.to_uppercase(),
            KeyCase::Lower => key.to_lowercase(),
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RulesFile {
    #[serde(default)]
    doc_types: BTreeMap<String, String>,
    #[serde(default)]
    path_rules: Vec<path::PathRuleSpec>,
    #[serde(default)]
    id_patterns: Vec<ids::IdPatternSpec>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct EntityHit {
    pub class: String,
    pub key: String,
    pub name: String,
    pub start: usize,
    pub end: usize,
    pub method: &'static str,
    pub confidence: f64,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ClassHit {
    pub class: String,
    pub start: usize,
    pub end: usize,
}

#[derive(Debug, Clone, PartialEq)]
pub struct RelationHit {
    pub from: Endpoint,
    pub property: String,
    pub to: Endpoint,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Endpoint {
    Document,
    Entity(usize),
}

#[derive(Debug, Default, PartialEq)]
pub struct DocumentFacts {
    pub doc_type: Option<String>,
    pub props: BTreeMap<String, String>,
    pub entities: Vec<EntityHit>,
    pub relations: Vec<RelationHit>,
    pub classes: Vec<ClassHit>,
}

#[derive(Debug, Default, PartialEq)]
pub struct TextFacts {
    pub entities: Vec<EntityHit>,
    pub classes: Vec<ClassHit>,
}

pub struct Rules {
    pub key: String,
    pub document_class: Option<String>,
    pub doc_types: BTreeMap<String, String>,
    pub path_rules: Vec<path::PathRule>,
    pub ids: ids::IdPatterns,
    pub lexicon: gazetteer::Lexicon,
}

pub fn load(config_dir: Option<&Path>, ontology: &Ontology, document_class: &str) -> Result<Rules> {
    let mut hasher = Sha256::new();
    hasher.update(STAGE_VERSION.to_le_bytes());
    hasher.update(ontology.version.as_bytes());
    let (rules_name, rules_text) = match config_dir.map(|d| d.join(RULES_FILE)) {
        Some(path) if path.exists() => {
            let text = std::fs::read_to_string(&path)
                .with_context(|| format!("cannot read {}", path.display()))?;
            (path.display().to_string(), text)
        }
        _ => (
            "built-in defaults/rules.toml".to_string(),
            DEFAULT_RULES.to_string(),
        ),
    };
    hasher.update(rules_text.as_bytes());
    let mut gazetteer_files = Vec::new();
    if let Some(dir) = config_dir.map(|d| d.join(GAZETTEER_DIR))
        && dir.is_dir()
    {
        let entries =
            std::fs::read_dir(&dir).with_context(|| format!("cannot read {}", dir.display()))?;
        for entry in entries {
            let path = entry?.path();
            if path.extension().is_some_and(|e| e == "csv") {
                gazetteer_files.push(path);
            }
        }
    }
    gazetteer_files.sort();
    let mut gazetteer_sources = Vec::new();
    for path in gazetteer_files {
        let bytes =
            std::fs::read(&path).with_context(|| format!("cannot read {}", path.display()))?;
        hasher.update(
            path.file_name()
                .map(|n| n.as_encoded_bytes())
                .unwrap_or(b""),
        );
        hasher.update((bytes.len() as u64).to_le_bytes());
        hasher.update(&bytes);
        gazetteer_sources.push((path.display().to_string(), bytes));
    }
    let key = format!("rules-v{STAGE_VERSION}-{}", &hex(&hasher.finalize())[..16]);
    compile(
        &rules_name,
        &rules_text,
        &gazetteer_sources,
        ontology,
        document_class,
        key,
    )
}

pub fn compile(
    rules_name: &str,
    rules_text: &str,
    gazetteer_sources: &[(String, Vec<u8>)],
    ontology: &Ontology,
    document_class: &str,
    key: String,
) -> Result<Rules> {
    let file: RulesFile = toml::from_str(rules_text)
        .map_err(|e| anyhow::anyhow!("{rules_name}: {}", e.to_string().trim_end()))?;
    let document_class = if document_class.is_empty() {
        None
    } else {
        Some(
            ontology
                .class(document_class)
                .map_err(|e| anyhow::anyhow!("[ontology] document_class: {e}"))?
                .iri
                .clone(),
        )
    };
    let mut doc_types = BTreeMap::new();
    for (doc_type, class) in &file.doc_types {
        let iri = ontology
            .class(class)
            .map_err(|e| anyhow::anyhow!("{rules_name}: doc_types.{doc_type}: {e}"))?
            .iri
            .clone();
        doc_types.insert(doc_type.clone(), iri);
    }
    let mut path_rules = Vec::new();
    for (index, spec) in file.path_rules.iter().enumerate() {
        let rule = path::PathRule::compile(spec, ontology, &doc_types, document_class.as_deref())
            .map_err(|e| anyhow::anyhow!("{rules_name}: path_rules[{index}]: {e}"))?;
        path_rules.push(rule);
    }
    let ids = ids::IdPatterns::compile(&file.id_patterns, ontology)
        .map_err(|e| anyhow::anyhow!("{rules_name}: {e}"))?;
    let lexicon = gazetteer::Lexicon::build(gazetteer_sources, ontology)?;
    if path_rules.iter().any(|r| r.uses_document())
        && document_class.is_none()
        && doc_types.is_empty()
    {
        bail!(
            "{rules_name}: a relation uses document, but neither [ontology] document_class nor doc_types is set"
        );
    }
    Ok(Rules {
        key,
        document_class,
        doc_types,
        path_rules,
        ids,
        lexicon,
    })
}

impl Rules {
    pub fn document_facts(&self, path: &str) -> DocumentFacts {
        let mut facts = DocumentFacts::default();
        for rule in &self.path_rules {
            rule.apply(path, &mut facts);
        }
        for hit in self.ids.find(path, ids::On::Path) {
            facts.entities.push(hit);
        }
        let (entities, classes) = self.lexicon.find(path);
        facts.entities.extend(entities);
        facts.classes = classes;
        char_offsets(path, &mut facts.entities, &mut facts.classes);
        facts
    }

    pub fn text_facts(&self, text: &str) -> TextFacts {
        let mut entities = self.ids.find(text, ids::On::Text);
        let (gazetteer, mut classes) = self.lexicon.find(text);
        entities.extend(gazetteer);
        char_offsets(text, &mut entities, &mut classes);
        TextFacts { entities, classes }
    }

    pub fn document_class_for(&self, doc_type: Option<&str>) -> Option<&str> {
        doc_type
            .and_then(|t| self.doc_types.get(t))
            .or(self.document_class.as_ref())
            .map(String::as_str)
    }
}

fn char_offsets(text: &str, entities: &mut [EntityHit], classes: &mut [ClassHit]) {
    if text.is_ascii() {
        return;
    }
    let chars = |byte: usize| text[..byte.min(text.len())].chars().count();
    for hit in entities.iter_mut() {
        (hit.start, hit.end) = (chars(hit.start), chars(hit.end));
    }
    for hit in classes.iter_mut() {
        (hit.start, hit.end) = (chars(hit.start), chars(hit.end));
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::config::OntologyConfig;
    use crate::ontology::load_sources;

    pub const ONTOLOGY: &str = "@prefix ex: <http://example.org/onto#> .\n\
        @prefix owl: <http://www.w3.org/2002/07/owl#> .\n\
        @prefix rdfs: <http://www.w3.org/2000/01/rdf-schema#> .\n\
        @prefix skos: <http://www.w3.org/2004/02/skos/core#> .\n\
        ex:Document a owl:Class ; skos:prefLabel \"document\"@en .\n\
        ex:Report a owl:Class ; rdfs:subClassOf ex:Document ; skos:prefLabel \"report\"@en .\n\
        ex:Project a owl:Class ; skos:prefLabel \"project\"@en .\n\
        ex:Customer a owl:Class ; skos:prefLabel \"customer\"@en .\n\
        ex:Pump a owl:Class ; skos:prefLabel \"pump\"@en ; skos:altLabel \"gear pump\"@en .\n\
        ex:Valve a owl:Class ; skos:prefLabel \"valve\"@en, \"kogelkraan\"@nl .\n\
        ex:Hidden a owl:Class ; skos:prefLabel \"hidden\"@en .\n\
        ex:describes a owl:ObjectProperty ; rdfs:domain ex:Document ; rdfs:range ex:Project .\n\
        ex:forCustomer a owl:ObjectProperty ; rdfs:domain ex:Project ; rdfs:range ex:Customer .\n";

    pub fn ontology() -> Ontology {
        let config = OntologyConfig {
            files: Vec::new(),
            languages: vec!["en".to_string(), "nl".to_string()],
            include: [
                "ex:Document",
                "ex:Project",
                "ex:Customer",
                "ex:Pump",
                "ex:Valve",
            ]
            .iter()
            .map(|s| s.to_string())
            .collect(),
            exclude: Vec::new(),
            document_class: String::new(),
        };
        let loaded = load_sources(
            &[("o.ttl".to_string(), ONTOLOGY.as_bytes().to_vec())],
            &config,
        );
        assert!(!loaded.has_errors(), "{:?}", loaded.findings);
        loaded.ontology
    }

    pub fn rules(text: &str) -> Result<Rules> {
        compile(
            "rules.toml",
            text,
            &[],
            &ontology(),
            "ex:Document",
            "k".to_string(),
        )
    }

    #[test]
    fn spans_are_character_offsets() {
        let rules = rules("").unwrap();
        let facts = rules.text_facts("é pump");
        assert_eq!((facts.classes[0].start, facts.classes[0].end), (2, 6));
    }

    #[test]
    fn key_normalisation() {
        assert_eq!(KeyCase::Upper.apply("  p-2021-001 "), "P-2021-001");
        assert_eq!(KeyCase::Lower.apply("AB"), "ab");
        assert_eq!(KeyCase::Keep.apply(" Ab "), "Ab");
    }

    #[test]
    fn unknown_doc_type_class_is_an_error() {
        let error = rules("[doc_types]\nreport = \"ex:Nope\"\n").err().unwrap();
        assert!(format!("{error}").contains("doc_types.report"), "{error}");
    }

    #[test]
    fn document_class_follows_doc_type() {
        let rules = rules("[doc_types]\nreport = \"ex:Report\"\n").unwrap();
        assert_eq!(
            rules.document_class_for(Some("report")),
            Some("http://example.org/onto#Report")
        );
        assert_eq!(
            rules.document_class_for(None),
            Some("http://example.org/onto#Document")
        );
    }

    #[test]
    fn unknown_field_names_the_file() {
        let error = rules("[[path_rules]]\npatern = \"x\"\n").err().unwrap();
        assert!(format!("{error}").contains("rules.toml"), "{error}");
    }
}
