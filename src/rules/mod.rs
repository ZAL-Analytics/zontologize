pub mod gazetteer;
pub mod ids;
pub mod pages;
pub mod path;

use std::collections::BTreeMap;
use std::path::Path;

use anyhow::{Context, Result, bail};
use serde::Deserialize;
use sha2::{Digest, Sha256};

use crate::ontology::{Ontology, hex};

pub const STAGE_VERSION: u32 = 2;
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
    ocr_confusions: Option<BTreeMap<String, String>>,
    #[serde(default)]
    page_types: Vec<pages::PageTypeSpec>,
    pages: Option<pages::PagesSpec>,
    #[serde(default)]
    revisions: Vec<RevisionSpec>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RevisionOrder {
    #[default]
    Natural,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RevisionSpec {
    pattern: String,
    #[serde(default)]
    order: RevisionOrder,
}

#[derive(Debug, Clone)]
pub struct Revision {
    pub regex: regex::Regex,
    pub order: RevisionOrder,
}

impl Revision {
    pub fn parse<'a>(&self, path: &'a str) -> Option<(&'a str, &'a str)> {
        let captures = self.regex.captures(path)?;
        Some((
            captures.name("series")?.as_str(),
            captures.name("rev")?.as_str(),
        ))
    }
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
    pub confidence: f64,
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
    pub rejected: usize,
}

pub struct Rules {
    pub key: String,
    pub document_class: Option<String>,
    pub doc_types: BTreeMap<String, String>,
    pub path_rules: Vec<path::PathRule>,
    pub ids: ids::IdPatterns,
    pub confusions: ids::Confusions,
    pub page_types: pages::PageTypes,
    pub part_property: Option<String>,
    pub lexicon: gazetteer::Lexicon,
    pub revisions: Vec<Revision>,
}

pub fn load(config_dir: Option<&Path>, ontology: &Ontology, document_class: &str) -> Result<Rules> {
    let mut hasher = Sha256::new();
    hasher.update(STAGE_VERSION.to_le_bytes());
    hasher.update(ontology.version.as_bytes());
    hasher.update(DEFAULT_RULES.as_bytes());
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
    let defaults = defaults()?;
    let confusions = match &file.ocr_confusions {
        Some(table) => ids::Confusions::compile(table),
        None => ids::Confusions::compile(&defaults.ocr_confusions.unwrap_or_default()),
    }
    .map_err(|e| anyhow::anyhow!("{rules_name}: {e}"))?;
    let pages = file.pages.unwrap_or_default();
    let default_pages = defaults.pages.unwrap_or_default();
    let head_chars = pages
        .head_chars
        .or(default_pages.head_chars)
        .unwrap_or_default();
    let page_types = pages::PageTypes::compile(&file.page_types, &doc_types, head_chars)
        .map_err(|e| anyhow::anyhow!("{rules_name}: {e}"))?;
    let part_property = pages
        .part_property
        .or(default_pages.part_property)
        .unwrap_or_default();
    let part_property = part_property_iri(
        &part_property,
        &file.page_types,
        &page_types,
        ontology,
        &doc_types,
        document_class.as_deref(),
    )
    .map_err(|e| anyhow::anyhow!("{rules_name}: pages.part_property: {e}"))?;
    let lexicon = gazetteer::Lexicon::build(gazetteer_sources, ontology)?;
    let mut revisions = Vec::new();
    for (index, spec) in file.revisions.iter().enumerate() {
        let regex = regex::Regex::new(&spec.pattern)
            .map_err(|e| anyhow::anyhow!("{rules_name}: revisions[{index}].pattern: {e}"))?;
        for group in ["series", "rev"] {
            if !regex.capture_names().flatten().any(|n| n == group) {
                bail!("{rules_name}: revisions[{index}].pattern has no named group {group}");
            }
        }
        revisions.push(Revision {
            regex,
            order: spec.order,
        });
    }
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
        confusions,
        page_types,
        part_property,
        lexicon,
        revisions,
    })
}

fn defaults() -> Result<RulesFile> {
    toml::from_str(DEFAULT_RULES).context("built-in defaults/rules.toml is not valid")
}

fn part_property_iri(
    name: &str,
    specs: &[pages::PageTypeSpec],
    page_types: &pages::PageTypes,
    ontology: &Ontology,
    doc_types: &BTreeMap<String, String>,
    document_class: Option<&str>,
) -> Result<Option<String>, String> {
    if page_types.is_empty() {
        return Ok(None);
    }
    if name.is_empty() {
        return Err("must name a property when [[page_types]] are set".to_string());
    }
    let property = ontology.property(name)?;
    let document_classes: Vec<&str> = document_class
        .into_iter()
        .chain(doc_types.values().map(String::as_str))
        .collect();
    let segment_classes: Vec<&str> = document_class
        .into_iter()
        .chain(
            pages::doc_types_of(specs)
                .filter_map(|t| doc_types.get(t))
                .map(String::as_str),
        )
        .collect();
    for (end, classes, allowed) in [
        ("domain", &segment_classes, &property.domain),
        ("range", &document_classes, &property.range),
    ] {
        for class in classes {
            if !allowed.is_empty() && !allowed.iter().any(|a| ontology.is_a(class, a)) {
                return Err(format!(
                    "{} has class {class} outside its {end} {}",
                    property.iri,
                    allowed.join(", ")
                ));
            }
        }
    }
    Ok(Some(property.iri.clone()))
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

    pub fn text_facts(&self, text: &str, ocr: Option<f64>, known: &ids::KnownKeys) -> TextFacts {
        let mut entities = self.ids.find(text, ids::On::Text);
        let mut rejected = 0;
        if ocr.is_some() {
            let tolerant = self
                .ids
                .find_tolerant(text, &self.confusions, known, &entities);
            entities.extend(tolerant.hits);
            rejected = tolerant.rejected;
        }
        let (gazetteer, mut classes) = self.lexicon.find(text);
        entities.extend(gazetteer);
        char_offsets(text, &mut entities, &mut classes);
        if let Some(scale) = ocr {
            entities.iter_mut().for_each(|hit| hit.confidence *= scale);
            classes.iter_mut().for_each(|hit| hit.confidence *= scale);
        }
        TextFacts {
            entities,
            classes,
            rejected,
        }
    }

    pub fn is_document_class(&self, ontology: &Ontology, iri: &str) -> bool {
        self.document_class
            .iter()
            .chain(self.doc_types.values())
            .any(|class| ontology.is_a(iri, class))
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
        ex:forCustomer a owl:ObjectProperty ; rdfs:domain ex:Project ; rdfs:range ex:Customer .\n\
        ex:partOf a owl:ObjectProperty ; rdfs:domain ex:Document ; rdfs:range ex:Document .\n";

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
        let facts = rules.text_facts("é pump", None, &ids::KnownKeys::new());
        assert_eq!((facts.classes[0].start, facts.classes[0].end), (2, 6));
    }

    #[test]
    fn ocr_text_scales_confidence() {
        let rules =
            rules("[[id_patterns]]\nclass = \"ex:Project\"\npattern = 'P-\\d{4}'\n").unwrap();
        let known = ids::KnownKeys::new();
        let facts = rules.text_facts("pump for P-1234", None, &known);
        assert_eq!(facts.entities[0].confidence, 1.0);
        assert_eq!(facts.classes[0].confidence, 1.0);
        let facts = rules.text_facts("pump for P-1234", Some(0.8), &known);
        assert!((facts.entities[0].confidence - 0.8).abs() < 1e-9);
        assert!((facts.classes[0].confidence - 0.8).abs() < 1e-9);
    }

    #[test]
    fn tolerant_ids_only_on_ocr_text_with_default_confusions() {
        let rules =
            rules("[[id_patterns]]\nclass = \"ex:Project\"\npattern = 'P-\\d{4}'\n").unwrap();
        let known = ids::KnownKeys::from([(
            "http://example.org/onto#Project".to_string(),
            std::collections::HashSet::from(["P-1234".to_string()]),
        )]);
        let layer = rules.text_facts("see P-l234", None, &known);
        assert!(layer.entities.is_empty());
        let ocr = rules.text_facts("see P-l234", Some(0.9), &known);
        assert_eq!(ocr.entities.len(), 1);
        assert_eq!(ocr.entities[0].method, ids::OCR_METHOD);
        assert!((ocr.entities[0].confidence - 0.9).abs() < 1e-9);
    }

    #[test]
    fn confusion_table_in_rules_replaces_the_default() {
        let text = "[ocr_confusions]\n\"7\" = \"T\"\n\n[[id_patterns]]\nclass = \"ex:Project\"\npattern = 'P-\\d{4}'\n";
        let rules = rules(text).unwrap();
        let known = ids::KnownKeys::from([(
            "http://example.org/onto#Project".to_string(),
            std::collections::HashSet::from(["P-1234".to_string(), "P-7777".to_string()]),
        )]);
        assert!(
            rules
                .text_facts("P-l234", Some(1.0), &known)
                .entities
                .is_empty()
        );
        assert_eq!(
            rules.text_facts("P-TT77", Some(1.0), &known).entities.len(),
            1
        );
    }

    const PAGE_TYPES: &str = "[doc_types]\nreport = \"ex:Report\"\n\n\
        [[page_types]]\ndoc_type = \"report\"\npattern = 'test report'\n";

    #[test]
    fn page_types_need_a_part_property_inside_domain_and_range() {
        let error = rules(PAGE_TYPES).err().unwrap();
        assert!(
            format!("{error}").contains("pages.part_property"),
            "{error}"
        );
        let outside = format!("[pages]\npart_property = \"ex:forCustomer\"\n{PAGE_TYPES}");
        let error = rules(&outside).err().unwrap();
        assert!(format!("{error}").contains("outside its domain"), "{error}");
        let rules = rules(&format!(
            "[pages]\npart_property = \"ex:partOf\"\n{PAGE_TYPES}"
        ))
        .unwrap();
        assert_eq!(
            rules.part_property.as_deref(),
            Some("http://example.org/onto#partOf")
        );
    }

    #[test]
    fn no_page_types_means_no_part_property() {
        let rules = rules("[pages]\npart_property = \"ex:partOf\"\n").unwrap();
        assert!(rules.page_types.is_empty());
        assert_eq!(rules.part_property, None);
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
    fn revisions_need_series_and_rev() {
        let good = rules(
            "[[revisions]]\npattern = '(?P<series>DWG-\\d+) rev (?P<rev>\\w+)'\norder = \"natural\"\n",
        )
        .unwrap();
        assert_eq!(
            good.revisions[0].parse("a/DWG-12 rev B.pdf"),
            Some(("DWG-12", "B"))
        );
        let error = rules("[[revisions]]\npattern = '(?P<series>DWG-\\d+)'\n")
            .err()
            .unwrap()
            .to_string();
        assert!(error.contains("rev"), "{error}");
        let error = rules("[[revisions]]\npattern = '(?P<series>a)(?P<rev>b)'\norder = \"date\"\n")
            .err()
            .unwrap()
            .to_string();
        assert!(error.contains("natural"), "{error}");
        assert!(rules("[[revisions]]\npattern = '('\n").is_err());
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
