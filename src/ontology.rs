use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::Path;

use anyhow::{Context, Result};
use oxrdf::{NamedOrBlankNode, Term};
use oxttl::TurtleParser;
use sha2::{Digest, Sha256};

use crate::config::OntologyConfig;

const RDF: &str = "http://www.w3.org/1999/02/22-rdf-syntax-ns#";
const RDFS: &str = "http://www.w3.org/2000/01/rdf-schema#";
const OWL: &str = "http://www.w3.org/2002/07/owl#";
const SKOS: &str = "http://www.w3.org/2004/02/skos/core#";
const XSD: &str = "http://www.w3.org/2001/XMLSchema#";
const QUDT: &str = "http://qudt.org/schema/qudt/";
const ZONTOLOGIZE: &str = "urn:zontologize#";
pub const TOLERANCE: &str = "urn:zontologize#similarityTolerance";
const NUMBER_TYPES: [&str; 16] = [
    "decimal",
    "double",
    "float",
    "integer",
    "int",
    "long",
    "short",
    "byte",
    "nonNegativeInteger",
    "positiveInteger",
    "nonPositiveInteger",
    "negativeInteger",
    "unsignedLong",
    "unsignedInt",
    "unsignedShort",
    "unsignedByte",
];
const TEXT_TYPES: [&str; 3] = ["string", "normalizedString", "token"];

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Severity {
    Error,
    Warning,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Finding {
    pub severity: Severity,
    pub message: String,
}

impl Finding {
    fn error(message: String) -> Self {
        Finding {
            severity: Severity::Error,
            message,
        }
    }

    fn warning(message: String) -> Self {
        Finding {
            severity: Severity::Warning,
            message,
        }
    }
}

pub type LangMap<T> = BTreeMap<String, T>;

#[derive(Debug, Clone, Default)]
pub struct Class {
    pub iri: String,
    pub parents: Vec<String>,
    pub types: Vec<String>,
    pub labels: LangMap<String>,
    pub alt_labels: LangMap<Vec<String>>,
    pub definition: LangMap<String>,
    pub selected: bool,
    pub meta: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PropertyKind {
    Object,
    Datatype,
    Rdf,
}

impl PropertyKind {
    pub fn as_str(self) -> &'static str {
        match self {
            PropertyKind::Object => "object",
            PropertyKind::Datatype => "datatype",
            PropertyKind::Rdf => "rdf",
        }
    }

    pub fn parse(text: &str) -> Option<PropertyKind> {
        [
            PropertyKind::Object,
            PropertyKind::Datatype,
            PropertyKind::Rdf,
        ]
        .into_iter()
        .find(|kind| kind.as_str() == text)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ValueKind {
    Number,
    Text,
}

impl ValueKind {
    pub fn as_str(self) -> &'static str {
        match self {
            ValueKind::Number => "number",
            ValueKind::Text => "text",
        }
    }

    pub fn parse(text: &str) -> Option<ValueKind> {
        [ValueKind::Number, ValueKind::Text]
            .into_iter()
            .find(|kind| kind.as_str() == text)
    }
}

#[derive(Debug, Clone)]
pub struct Property {
    pub iri: String,
    pub kind: PropertyKind,
    pub domain: Vec<String>,
    pub range: Vec<String>,
    pub parents: Vec<String>,
    pub labels: LangMap<String>,
    pub alt_labels: LangMap<Vec<String>>,
    pub value: Option<ValueKind>,
    pub quantity_kind: Option<String>,
    pub dimension: Option<String>,
    pub tolerance: Option<f64>,
    pub selected: bool,
}

impl Property {
    pub fn new(iri: &str, kind: PropertyKind) -> Property {
        Property {
            iri: iri.to_string(),
            kind,
            domain: Vec::new(),
            range: Vec::new(),
            parents: Vec::new(),
            labels: LangMap::new(),
            alt_labels: LangMap::new(),
            value: None,
            quantity_kind: None,
            dimension: None,
            tolerance: None,
            selected: false,
        }
    }

    pub fn is_attribute(&self) -> bool {
        self.selected && self.value.is_some()
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Unit {
    pub iri: String,
    pub symbols: Vec<String>,
    pub labels: LangMap<String>,
    pub alt_labels: LangMap<Vec<String>>,
    pub dimension: String,
    pub multiplier: f64,
    pub offset: f64,
}

#[derive(Debug, Clone, Default)]
pub struct Ontology {
    pub version: String,
    pub files: Vec<String>,
    pub languages: Vec<String>,
    pub classes: BTreeMap<String, Class>,
    pub properties: BTreeMap<String, Property>,
    pub units: BTreeMap<String, Unit>,
    pub quantity_kinds: BTreeMap<String, String>,
    pub prefixes: BTreeMap<String, String>,
    pub ontology_iris: BTreeSet<String>,
    pub imports: Vec<(String, String)>,
    pub roots: Vec<String>,
}

#[derive(Default)]
struct Node {
    types: Vec<String>,
    parents: Vec<String>,
    domain: Vec<String>,
    range: Vec<String>,
    super_properties: Vec<String>,
    pref_labels: Vec<(String, String)>,
    labels: Vec<(String, String)>,
    alt_labels: Vec<(String, String)>,
    definitions: Vec<(String, String)>,
    comments: Vec<(String, String)>,
    symbols: Vec<String>,
    multiplier: Option<String>,
    offset: Option<String>,
    dimension: Option<String>,
    quantity_kinds: Vec<String>,
    deprecated: bool,
    tolerance: Option<String>,
}

pub struct Loaded {
    pub ontology: Ontology,
    pub findings: Vec<Finding>,
}

impl Loaded {
    pub fn has_errors(&self) -> bool {
        self.findings.iter().any(|f| f.severity == Severity::Error)
    }

    pub fn first_error(&self) -> Option<&str> {
        self.findings
            .iter()
            .find(|f| f.severity == Severity::Error)
            .map(|f| f.message.as_str())
    }
}

pub fn load(config_dir: Option<&Path>, config: &OntologyConfig) -> Result<Loaded> {
    let base = config_dir.unwrap_or(Path::new("."));
    let mut sources = Vec::new();
    for file in &config.files {
        let path = base.join(file);
        let bytes =
            std::fs::read(&path).with_context(|| format!("cannot read {}", path.display()))?;
        sources.push((path.display().to_string(), bytes));
    }
    Ok(load_sources(&sources, config))
}

pub fn load_sources(sources: &[(String, Vec<u8>)], config: &OntologyConfig) -> Loaded {
    let mut findings = Vec::new();
    let mut nodes: HashMap<String, Node> = HashMap::new();
    let mut ontology = Ontology {
        version: version(sources, config),
        files: sources.iter().map(|(name, _)| name.clone()).collect(),
        languages: config.languages.clone(),
        ..Ontology::default()
    };
    for (name, bytes) in sources {
        parse_file(name, bytes, &mut nodes, &mut ontology, &mut findings);
    }
    if !findings.is_empty() {
        return Loaded { ontology, findings };
    }
    build(&mut ontology, nodes, &mut findings);
    check_references(&ontology, &mut findings);
    check_cycles(&ontology, &mut findings);
    check_imports(&ontology, &mut findings);
    if let Err(message) = select(&mut ontology, config) {
        findings.push(Finding::error(message));
        return Loaded { ontology, findings };
    }
    check_labels(&ontology, &mut findings);
    check_attributes(&ontology, &mut findings);
    Loaded { ontology, findings }
}

fn version(sources: &[(String, Vec<u8>)], config: &OntologyConfig) -> String {
    let mut hasher = Sha256::new();
    for (_, bytes) in sources {
        hasher.update((bytes.len() as u64).to_le_bytes());
        hasher.update(bytes);
    }
    for list in [
        &config.languages,
        &config.include,
        &config.exclude,
        &vec![config.document_class.clone()],
    ] {
        hasher.update(list.join("\n").as_bytes());
        hasher.update([0]);
    }
    hex(&hasher.finalize())
}

pub fn hex(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push_str(&format!("{byte:02x}"));
    }
    out
}

fn parse_file(
    name: &str,
    bytes: &[u8],
    nodes: &mut HashMap<String, Node>,
    ontology: &mut Ontology,
    findings: &mut Vec<Finding>,
) {
    let mut parser = TurtleParser::new().for_slice(bytes);
    for triple in parser.by_ref() {
        let triple = match triple {
            Ok(triple) => triple,
            Err(error) => {
                let start = error.location().start;
                findings.push(Finding::error(format!(
                    "{name}:{}:{}: {}",
                    start.line + 1,
                    start.column + 1,
                    error.message()
                )));
                return;
            }
        };
        let NamedOrBlankNode::NamedNode(subject) = triple.subject else {
            continue;
        };
        let subject = subject.into_string();
        let predicate = triple.predicate.as_str();
        add_triple(name, subject, predicate, triple.object, nodes, ontology);
    }
    for (prefix, iri) in parser.prefixes() {
        match ontology.prefixes.get(prefix) {
            Some(existing) if existing != iri => findings.push(Finding::error(format!(
                "{name}: prefix {prefix}: is bound to {iri} here and to {existing} in an earlier file"
            ))),
            Some(_) => {}
            None => {
                ontology
                    .prefixes
                    .insert(prefix.to_string(), iri.to_string());
            }
        }
    }
}

fn add_triple(
    file: &str,
    subject: String,
    predicate: &str,
    object: Term,
    nodes: &mut HashMap<String, Node>,
    ontology: &mut Ontology,
) {
    let iri = match &object {
        Term::NamedNode(node) => Some(node.as_str().to_string()),
        _ => None,
    };
    let literal = match &object {
        Term::Literal(literal) if !literal.value().trim().is_empty() => Some((
            literal.language().unwrap_or("").to_string(),
            literal.value().trim().to_string(),
        )),
        _ => None,
    };
    let Some(local) = predicate.strip_prefix(RDF).map(|l| (RDF, l)).or_else(|| {
        [RDFS, OWL, SKOS, QUDT, ZONTOLOGIZE]
            .into_iter()
            .find_map(|ns| predicate.strip_prefix(ns).map(|l| (ns, l)))
    }) else {
        return;
    };
    if local == (RDF, "type") && iri.as_deref() == Some(&format!("{OWL}Ontology")) {
        ontology.ontology_iris.insert(subject);
        return;
    }
    if local == (OWL, "versionIRI") {
        if let Some(iri) = iri {
            ontology.ontology_iris.insert(iri);
        }
        return;
    }
    if local == (OWL, "imports") {
        if let Some(iri) = iri {
            ontology.imports.push((file.to_string(), iri));
        }
        return;
    }
    let node = nodes.entry(subject).or_default();
    match (local, iri, literal) {
        ((RDF, "type"), Some(iri), _) => node.types.push(iri),
        ((RDFS, "subClassOf"), Some(iri), _) => node.parents.push(iri),
        ((RDFS, "domain"), Some(iri), _) => node.domain.push(iri),
        ((RDFS, "range"), Some(iri), _) => node.range.push(iri),
        ((RDFS, "subPropertyOf"), Some(iri), _) => node.super_properties.push(iri),
        ((SKOS, "prefLabel"), _, Some(value)) => node.pref_labels.push(value),
        ((RDFS, "label"), _, Some(value)) => node.labels.push(value),
        ((SKOS, "altLabel"), _, Some(value)) => node.alt_labels.push(value),
        ((SKOS, "definition"), _, Some(value)) => node.definitions.push(value),
        ((RDFS, "comment"), _, Some(value)) => node.comments.push(value),
        ((QUDT, "symbol" | "ucumCode"), _, Some((_, value))) => node.symbols.push(value),
        ((QUDT, "conversionMultiplier"), _, Some((_, value))) => node.multiplier = Some(value),
        ((QUDT, "conversionOffset"), _, Some((_, value))) => node.offset = Some(value),
        ((QUDT, "hasDimensionVector"), Some(iri), _) => node.dimension = Some(iri),
        ((QUDT, "hasQuantityKind"), Some(iri), _) => node.quantity_kinds.push(iri),
        ((QUDT, "deprecated"), _, Some((_, value))) => node.deprecated = value == "true",
        ((ZONTOLOGIZE, "similarityTolerance"), _, Some((_, value))) => node.tolerance = Some(value),
        _ => {}
    }
}

fn build(ontology: &mut Ontology, nodes: HashMap<String, Node>, findings: &mut Vec<Finding>) {
    let languages = ontology.languages.clone();
    let class_types = [format!("{OWL}Class"), format!("{RDFS}Class")];
    let unit_type = format!("{QUDT}Unit");
    let kind_type = format!("{QUDT}QuantityKind");
    let mut incomplete_units = 0;
    for (iri, node) in nodes {
        if node.types.contains(&kind_type) {
            if let Some(dimension) = node.dimension {
                ontology.quantity_kinds.insert(iri, dimension);
            }
            continue;
        }
        if node.types.contains(&unit_type) {
            if node.deprecated {
                continue;
            }
            match unit(&iri, &node, &languages) {
                Some(unit) => {
                    ontology.units.insert(iri, unit);
                }
                None => incomplete_units += 1,
            }
            continue;
        }
        let kind = property_kind(&node.types);
        let is_class = node.types.iter().any(|t| class_types.contains(t));
        let preferred = if node.pref_labels.is_empty() {
            &node.labels
        } else {
            &node.pref_labels
        };
        let labels = first_per_language(preferred, &languages);
        if is_class {
            let definitions = if node.definitions.is_empty() {
                &node.comments
            } else {
                &node.definitions
            };
            let class = Class {
                iri: iri.clone(),
                parents: dedup(node.parents),
                types: dedup(
                    node.types
                        .into_iter()
                        .filter(|t| !class_types.contains(t))
                        .collect(),
                ),
                labels,
                alt_labels: all_per_language(&node.alt_labels, &languages),
                definition: first_per_language(definitions, &languages),
                selected: false,
                meta: false,
            };
            ontology.classes.insert(iri, class);
        } else if let Some(kind) = kind {
            let tolerance = match node.tolerance.as_deref().map(str::parse::<f64>) {
                None => None,
                Some(Ok(value)) if value > 0.0 && value.is_finite() => Some(value),
                Some(_) => {
                    findings.push(Finding::error(format!(
                        "property {iri}: similarityTolerance {:?} is not a positive number",
                        node.tolerance.unwrap_or_default()
                    )));
                    None
                }
            };
            if node.quantity_kinds.len() > 1 {
                findings.push(Finding::warning(format!(
                    "property {iri}: several quantity kinds, only {} is used",
                    node.quantity_kinds[0]
                )));
            }
            let range = dedup(node.range);
            let quantity_kind = node.quantity_kinds.first().cloned();
            let value = match kind {
                PropertyKind::Datatype => value_kind(&range, quantity_kind.is_some()),
                _ => None,
            };
            let property = Property {
                domain: dedup(node.domain),
                range,
                parents: dedup(node.super_properties),
                labels,
                alt_labels: all_per_language(&node.alt_labels, &languages),
                value,
                quantity_kind,
                tolerance,
                ..Property::new(&iri, kind)
            };
            ontology.properties.insert(iri, property);
        }
    }
    if incomplete_units > 0 {
        findings.push(Finding::warning(format!(
            "{incomplete_units} units without a conversion multiplier or dimension vector are not used"
        )));
    }
    let dimensions = ontology.quantity_kinds.clone();
    for property in ontology.properties.values_mut() {
        if let Some(kind) = &property.quantity_kind {
            property.dimension = dimensions.get(kind).cloned();
        }
    }
    let metas: BTreeSet<String> = ontology
        .classes
        .values()
        .flat_map(|c| c.types.iter().cloned())
        .collect();
    for iri in metas {
        if let Some(class) = ontology.classes.get_mut(&iri) {
            class.meta = true;
        }
    }
}

fn unit(iri: &str, node: &Node, languages: &[String]) -> Option<Unit> {
    let multiplier: f64 = node.multiplier.as_deref()?.parse().ok()?;
    let offset: f64 = match node.offset.as_deref() {
        Some(text) => text.parse().ok()?,
        None => 0.0,
    };
    if multiplier == 0.0 || !multiplier.is_finite() || !offset.is_finite() {
        return None;
    }
    Some(Unit {
        iri: iri.to_string(),
        symbols: dedup(node.symbols.clone()),
        labels: first_per_language(&node.labels, languages),
        alt_labels: all_per_language(&node.alt_labels, languages),
        dimension: node.dimension.clone()?,
        multiplier,
        offset,
    })
}

fn value_kind(range: &[String], has_quantity_kind: bool) -> Option<ValueKind> {
    if has_quantity_kind {
        return Some(ValueKind::Number);
    }
    let Some(first) = range.first() else {
        return Some(ValueKind::Text);
    };
    if range.len() > 1 {
        return None;
    }
    if let Some(local) = first.strip_prefix(XSD) {
        if NUMBER_TYPES.contains(&local) {
            return Some(ValueKind::Number);
        }
        if TEXT_TYPES.contains(&local) {
            return Some(ValueKind::Text);
        }
    }
    if *first == format!("{RDF}langString") {
        return Some(ValueKind::Text);
    }
    None
}

fn property_kind(types: &[String]) -> Option<PropertyKind> {
    types.iter().find_map(|t| {
        if *t == format!("{OWL}ObjectProperty") {
            Some(PropertyKind::Object)
        } else if *t == format!("{OWL}DatatypeProperty") {
            Some(PropertyKind::Datatype)
        } else if *t == format!("{RDF}Property") {
            Some(PropertyKind::Rdf)
        } else {
            None
        }
    })
}

fn dedup(mut list: Vec<String>) -> Vec<String> {
    let mut seen = BTreeSet::new();
    list.retain(|item| seen.insert(item.clone()));
    list
}

fn first_per_language(values: &[(String, String)], languages: &[String]) -> LangMap<String> {
    let mut map = LangMap::new();
    for (lang, value) in values {
        if !lang.is_empty() {
            map.entry(lang.clone()).or_insert_with(|| value.clone());
        }
    }
    if let Some((_, untagged)) = values.iter().find(|(lang, _)| lang.is_empty()) {
        for lang in languages {
            map.entry(lang.clone()).or_insert_with(|| untagged.clone());
        }
    }
    map
}

fn all_per_language(values: &[(String, String)], languages: &[String]) -> LangMap<Vec<String>> {
    let mut map: LangMap<Vec<String>> = LangMap::new();
    for (lang, value) in values {
        let targets: Vec<String> = if lang.is_empty() {
            languages.to_vec()
        } else {
            vec![lang.clone()]
        };
        for target in targets {
            let list = map.entry(target).or_default();
            if !list.contains(value) {
                list.push(value.clone());
            }
        }
    }
    map
}

fn is_builtin(iri: &str) -> bool {
    [RDF, RDFS, OWL, XSD, SKOS]
        .iter()
        .any(|ns| iri.starts_with(ns))
}

fn check_references(ontology: &Ontology, findings: &mut Vec<Finding>) {
    for class in ontology.classes.values() {
        for parent in &class.parents {
            if !ontology.classes.contains_key(parent) && !is_builtin(parent) {
                findings.push(Finding::warning(format!(
                    "class {}: unknown superclass {parent}",
                    class.iri
                )));
            }
        }
    }
    for property in ontology.properties.values() {
        for (what, list) in [("domain", &property.domain), ("range", &property.range)] {
            for iri in list {
                if !ontology.classes.contains_key(iri) && !is_builtin(iri) {
                    findings.push(Finding::warning(format!(
                        "property {}: unknown {what} {iri}",
                        property.iri
                    )));
                }
            }
        }
    }
}

fn check_cycles(ontology: &Ontology, findings: &mut Vec<Finding>) {
    let mut state: HashMap<&str, u8> = HashMap::new();
    for start in ontology.classes.keys() {
        if state.contains_key(start.as_str()) {
            continue;
        }
        let mut stack: Vec<(&str, usize)> = vec![(start, 0)];
        let mut path: Vec<&str> = vec![start];
        state.insert(start, 1);
        while let Some((iri, next)) = stack.pop() {
            let parents = ontology
                .classes
                .get(iri)
                .map(|c| c.parents.as_slice())
                .unwrap_or(&[]);
            if next >= parents.len() {
                state.insert(iri, 2);
                path.pop();
                continue;
            }
            stack.push((iri, next + 1));
            let parent = parents[next].as_str();
            if !ontology.classes.contains_key(parent) {
                continue;
            }
            match state.get(parent) {
                Some(1) => {
                    let from = path.iter().position(|p| *p == parent).unwrap_or(0);
                    let mut cycle: Vec<&str> = path[from..].to_vec();
                    cycle.push(parent);
                    findings.push(Finding::error(format!(
                        "subclass cycle: {}",
                        cycle.join(" -> ")
                    )));
                }
                Some(_) => {}
                None => {
                    state.insert(parent, 1);
                    path.push(parent);
                    stack.push((parent, 0));
                }
            }
        }
    }
}

fn check_imports(ontology: &Ontology, findings: &mut Vec<Finding>) {
    for (file, iri) in &ontology.imports {
        if !ontology.ontology_iris.contains(iri) {
            findings.push(Finding::warning(format!(
                "{file}: imports {iri}, which no loaded file declares"
            )));
        }
    }
}

fn check_labels(ontology: &Ontology, findings: &mut Vec<Finding>) {
    for lang in &ontology.languages {
        for class in ontology.classes.values().filter(|c| c.selected) {
            if !class.labels.contains_key(lang) {
                findings.push(Finding::warning(format!(
                    "class {}: no label in {lang}",
                    class.iri
                )));
            }
        }
        for property in ontology.properties.values().filter(|p| p.selected) {
            if !property.labels.contains_key(lang) {
                findings.push(Finding::warning(format!(
                    "property {}: no label in {lang}",
                    property.iri
                )));
            }
        }
    }
}

fn check_attributes(ontology: &Ontology, findings: &mut Vec<Finding>) {
    for property in ontology.properties.values().filter(|p| p.is_attribute()) {
        let Some(kind) = &property.quantity_kind else {
            continue;
        };
        if property.dimension.is_none() {
            findings.push(Finding::error(format!(
                "property {}: quantity kind {kind} is not loaded (add QUDT's quantitykind file or declare it)",
                property.iri
            )));
            continue;
        }
        let units = ontology.fitting_units(property);
        if units.is_empty() {
            findings.push(Finding::error(format!(
                "property {}: no loaded unit fits quantity kind {kind}",
                property.iri
            )));
            continue;
        }
        let mut owners: BTreeMap<&str, Vec<&str>> = BTreeMap::new();
        for unit in &units {
            for text in &unit.symbols {
                owners.entry(text).or_default().push(&unit.iri);
            }
        }
        for (text, iris) in owners.iter().filter(|(_, iris)| iris.len() > 1) {
            findings.push(Finding::warning(format!(
                "property {}: symbol {text:?} belongs to {} units ({}); it is not used",
                property.iri,
                iris.len(),
                iris.join(", ")
            )));
        }
    }
}

impl Ontology {
    pub fn fitting_units(&self, property: &Property) -> Vec<&Unit> {
        let Some(dimension) = &property.dimension else {
            return Vec::new();
        };
        self.units
            .values()
            .filter(|u| &u.dimension == dimension)
            .collect()
    }

    pub fn resolve(&self, name: &str) -> Result<String, String> {
        let name = name.trim();
        if let Some(inner) = name.strip_prefix('<').and_then(|n| n.strip_suffix('>')) {
            return Ok(inner.to_string());
        }
        let Some((prefix, local)) = name.split_once(':') else {
            return Err(format!("{name:?} is neither a prefixed name nor an IRI"));
        };
        if let Some(namespace) = self.prefixes.get(prefix) {
            return Ok(format!("{namespace}{local}"));
        }
        if local.starts_with("//") || prefix == "urn" {
            return Ok(name.to_string());
        }
        Err(format!("{name:?} uses the unknown prefix {prefix}:"))
    }

    pub fn class(&self, name: &str) -> Result<&Class, String> {
        let iri = self.resolve(name)?;
        match self.classes.get(&iri) {
            Some(class) if class.selected => Ok(class),
            Some(_) => Err(format!(
                "class {name} is not selected by [ontology] include"
            )),
            None => Err(format!("class {name} is not in the ontology")),
        }
    }

    pub fn property(&self, name: &str) -> Result<&Property, String> {
        let iri = self.resolve(name)?;
        match self.properties.get(&iri) {
            Some(property) if property.selected => Ok(property),
            Some(_) => Err(format!(
                "property {name} is not selected by [ontology] include"
            )),
            None => Err(format!("property {name} is not in the ontology")),
        }
    }

    pub fn is_a(&self, class: &str, ancestor: &str) -> bool {
        let mut stack = vec![class];
        let mut seen = BTreeSet::new();
        while let Some(iri) = stack.pop() {
            if iri == ancestor {
                return true;
            }
            if !seen.insert(iri) {
                continue;
            }
            if let Some(class) = self.classes.get(iri) {
                stack.extend(class.parents.iter().chain(&class.types).map(String::as_str));
            }
        }
        false
    }

    pub fn selected_under(&self, root: &str) -> (usize, usize) {
        let closure = class_closure(self, &[root.to_string()]);
        let sub_properties = property_closure(self, &[root.to_string()]);
        let classes = closure
            .iter()
            .filter(|iri| self.classes.get(*iri).is_some_and(|c| c.selected))
            .count();
        let properties = self
            .properties
            .values()
            .filter(|p| p.selected)
            .filter(|p| {
                sub_properties.contains(&p.iri)
                    || (!p.domain.is_empty() && p.domain.iter().all(|d| closure.contains(d)))
            })
            .count();
        (classes, properties)
    }
}

fn class_closure(ontology: &Ontology, roots: &[String]) -> BTreeSet<String> {
    let mut children: HashMap<&str, Vec<&str>> = HashMap::new();
    for class in ontology.classes.values() {
        for parent in class.parents.iter().chain(&class.types) {
            children.entry(parent).or_default().push(&class.iri);
        }
    }
    let mut selected = BTreeSet::new();
    let mut stack: Vec<&str> = roots.iter().map(String::as_str).collect();
    while let Some(iri) = stack.pop() {
        if !selected.insert(iri.to_string()) {
            continue;
        }
        if let Some(list) = children.get(iri) {
            stack.extend(list);
        }
    }
    selected
}

fn property_closure(ontology: &Ontology, roots: &[String]) -> BTreeSet<String> {
    let mut selected: BTreeSet<String> = roots.iter().cloned().collect();
    loop {
        let before = selected.len();
        for property in ontology.properties.values() {
            if property.parents.iter().any(|p| selected.contains(p)) {
                selected.insert(property.iri.clone());
            }
        }
        if selected.len() == before {
            return selected;
        }
    }
}

fn select(ontology: &mut Ontology, config: &OntologyConfig) -> Result<(), String> {
    let mut include = Vec::new();
    for name in &config.include {
        let iri = ontology
            .resolve(name)
            .map_err(|e| format!("[ontology] include: {e}"))?;
        if !ontology.classes.contains_key(&iri) && !ontology.properties.contains_key(&iri) {
            return Err(format!(
                "[ontology] include: {name} is neither a class nor a property in the ontology"
            ));
        }
        include.push(iri);
    }
    let mut exclude = Vec::new();
    for name in &config.exclude {
        let iri = ontology
            .resolve(name)
            .map_err(|e| format!("[ontology] exclude: {e}"))?;
        exclude.push(iri);
    }
    let excluded_classes = class_closure(ontology, &exclude);
    let excluded_properties = property_closure(ontology, &exclude);
    let (classes, properties) = if include.is_empty() {
        (
            ontology.classes.keys().cloned().collect(),
            ontology.properties.keys().cloned().collect(),
        )
    } else {
        (
            class_closure(ontology, &include),
            property_closure(ontology, &include),
        )
    };
    for class in ontology.classes.values_mut() {
        class.selected = classes.contains(&class.iri) && !excluded_classes.contains(&class.iri);
    }
    let selected_classes: BTreeSet<String> = ontology
        .classes
        .values()
        .filter(|c| c.selected)
        .map(|c| c.iri.clone())
        .collect();
    for property in ontology.properties.values_mut() {
        let domain_selected = !property.domain.is_empty()
            && property.domain.iter().all(|d| selected_classes.contains(d));
        let by_classes = if property.value.is_some() {
            domain_selected
        } else {
            domain_selected
                && !property.range.is_empty()
                && property.range.iter().all(|r| selected_classes.contains(r))
        };
        property.selected = (properties.contains(&property.iri) || by_classes)
            && !excluded_properties.contains(&property.iri);
    }
    ontology.roots = if include.is_empty() {
        Vec::new()
    } else {
        include
    };
    if !config.document_class.is_empty() {
        ontology
            .class(&config.document_class)
            .map_err(|e| format!("[ontology] document_class: {e}"))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const PREFIXES: &str = "@prefix ex: <http://example.org/onto#> .\n\
        @prefix owl: <http://www.w3.org/2002/07/owl#> .\n\
        @prefix rdfs: <http://www.w3.org/2000/01/rdf-schema#> .\n\
        @prefix skos: <http://www.w3.org/2004/02/skos/core#> .\n";

    fn config(include: &[&str], exclude: &[&str]) -> OntologyConfig {
        OntologyConfig {
            files: Vec::new(),
            languages: vec!["en".to_string(), "nl".to_string()],
            include: include.iter().map(|s| s.to_string()).collect(),
            exclude: exclude.iter().map(|s| s.to_string()).collect(),
            document_class: String::new(),
        }
    }

    fn load_text(files: &[&str], config: &OntologyConfig) -> Loaded {
        let sources: Vec<(String, Vec<u8>)> = files
            .iter()
            .enumerate()
            .map(|(i, text)| {
                (
                    format!("file{i}.ttl"),
                    format!("{PREFIXES}{text}").into_bytes(),
                )
            })
            .collect();
        load_sources(&sources, config)
    }

    fn selected(loaded: &Loaded) -> Vec<String> {
        loaded
            .ontology
            .classes
            .values()
            .filter(|c| c.selected)
            .map(|c| {
                c.iri
                    .trim_start_matches("http://example.org/onto#")
                    .to_string()
            })
            .collect()
    }

    fn messages(loaded: &Loaded, severity: Severity) -> Vec<String> {
        loaded
            .findings
            .iter()
            .filter(|f| f.severity == severity)
            .map(|f| f.message.clone())
            .collect()
    }

    const TREE: &str = "ex:Thing a owl:Class .\n\
        ex:Equipment a owl:Class ; rdfs:subClassOf ex:Thing .\n\
        ex:Pump a owl:Class ; rdfs:subClassOf ex:Equipment .\n\
        ex:Valve a owl:Class ; rdfs:subClassOf ex:Equipment .\n\
        ex:Project a owl:Class ; rdfs:subClassOf ex:Thing .\n";

    #[test]
    fn subclass_selection() {
        let loaded = load_text(&[TREE], &config(&["ex:Equipment"], &[]));
        assert_eq!(selected(&loaded), ["Equipment", "Pump", "Valve"]);
    }

    #[test]
    fn meta_class_selection() {
        let flat = "ex:EquipmentClass a owl:Class .\n\
            ex:Pump a rdfs:Class, ex:EquipmentClass .\n\
            ex:Valve a rdfs:Class, ex:EquipmentClass .\n\
            ex:Site a rdfs:Class, ex:LocationClass .\n";
        let loaded = load_text(&[flat], &config(&["ex:EquipmentClass"], &[]));
        assert_eq!(selected(&loaded), ["EquipmentClass", "Pump", "Valve"]);
        let ontology = &loaded.ontology;
        let iri = |name: &str| format!("http://example.org/onto#{name}");
        assert!(ontology.is_a(&iri("Pump"), &iri("EquipmentClass")));
        assert!(!ontology.is_a(&iri("Site"), &iri("EquipmentClass")));
        assert!(ontology.classes[&iri("EquipmentClass")].meta);
        assert!(!ontology.classes[&iri("Pump")].meta);
    }

    #[test]
    fn exclude_removes_descendants() {
        let text = format!("{TREE}ex:GearPump a owl:Class ; rdfs:subClassOf ex:Pump .\n");
        let loaded = load_text(&[&text], &config(&["ex:Thing"], &["ex:Pump"]));
        assert_eq!(
            selected(&loaded),
            ["Equipment", "Project", "Thing", "Valve"]
        );
    }

    #[test]
    fn empty_include_selects_everything() {
        let loaded = load_text(&[TREE], &config(&[], &[]));
        assert_eq!(selected(&loaded).len(), 5);
    }

    #[test]
    fn label_and_definition_fallbacks() {
        let text = "ex:A a owl:Class ; rdfs:label \"a label\"@en ; skos:prefLabel \"A\"@en ; \
            rdfs:comment \"comment\"@en .\n\
            ex:B a owl:Class ; rdfs:label \"B\"@en ; skos:definition \"def\"@en ; \
            rdfs:comment \"comment\"@en .\n";
        let loaded = load_text(&[text], &config(&[], &[]));
        let classes = &loaded.ontology.classes;
        let a = &classes["http://example.org/onto#A"];
        let b = &classes["http://example.org/onto#B"];
        assert_eq!(a.labels["en"], "A");
        assert_eq!(a.definition["en"], "comment");
        assert_eq!(b.labels["en"], "B");
        assert_eq!(b.definition["en"], "def");
    }

    #[test]
    fn untagged_literal_counts_for_every_language_and_empty_is_dropped() {
        let text = "ex:A a owl:Class ; skos:prefLabel \"Pomp\"@nl, \"pump\" ; \
            skos:altLabel \"  \", \"impeller pump\" .\n";
        let loaded = load_text(&[text], &config(&[], &[]));
        let a = &loaded.ontology.classes["http://example.org/onto#A"];
        assert_eq!(a.labels["nl"], "Pomp");
        assert_eq!(a.labels["en"], "pump");
        assert_eq!(a.alt_labels["en"], ["impeller pump"]);
        assert_eq!(a.alt_labels["nl"], ["impeller pump"]);
        assert!(messages(&loaded, Severity::Warning).is_empty());
    }

    #[test]
    fn missing_label_is_a_warning() {
        let text = "ex:A a owl:Class ; skos:prefLabel \"A\"@en .\n";
        let loaded = load_text(&[text], &config(&[], &[]));
        assert_eq!(
            messages(&loaded, Severity::Warning),
            ["class http://example.org/onto#A: no label in nl"]
        );
        assert!(!loaded.has_errors());
    }

    #[test]
    fn prefix_conflict_is_an_error() {
        let other = "@prefix ex: <http://example.org/other#> .\nex:B a owl:Class .\n";
        let loaded = load_text(&[TREE, other], &config(&[], &[]));
        let errors = messages(&loaded, Severity::Error);
        assert_eq!(errors.len(), 1);
        assert!(errors[0].contains("prefix ex:"), "{errors:?}");
        assert!(errors[0].starts_with("file1.ttl"), "{errors:?}");
    }

    #[test]
    fn unknown_include_is_an_error() {
        let loaded = load_text(&[TREE], &config(&["ex:Missing"], &[]));
        assert!(loaded.has_errors());
        assert!(loaded.first_error().unwrap().contains("ex:Missing"));
    }

    #[test]
    fn unknown_prefix_in_include_is_an_error() {
        let loaded = load_text(&[TREE], &config(&["zz:Thing"], &[]));
        assert!(loaded.first_error().unwrap().contains("unknown prefix zz:"));
    }

    #[test]
    fn cycle_is_an_error() {
        let text = "ex:A a owl:Class ; rdfs:subClassOf ex:C .\n\
            ex:B a owl:Class ; rdfs:subClassOf ex:A .\n\
            ex:C a owl:Class ; rdfs:subClassOf ex:B .\n";
        let loaded = load_text(&[text], &config(&[], &[]));
        let errors = messages(&loaded, Severity::Error);
        assert_eq!(errors.len(), 1, "{errors:?}");
        assert!(errors[0].starts_with("subclass cycle:"), "{errors:?}");
    }

    #[test]
    fn unloaded_import_is_a_warning() {
        let base = "<http://example.org/base> a owl:Ontology .\n";
        let local = "<http://example.org/local> a owl:Ontology ; \
            owl:imports <http://example.org/base>, <http://example.org/missing> .\n";
        let loaded = load_text(&[base, local], &config(&[], &[]));
        assert_eq!(
            messages(&loaded, Severity::Warning),
            ["file1.ttl: imports http://example.org/missing, which no loaded file declares"]
        );
    }

    #[test]
    fn parse_error_names_file_and_line() {
        let loaded = load_text(
            &[TREE, "ex:A a owl:Class .\nex:B a \"x .\n"],
            &config(&[], &[]),
        );
        let errors = messages(&loaded, Severity::Error);
        assert_eq!(errors.len(), 1, "{errors:?}");
        assert!(errors[0].starts_with("file1.ttl:6:"), "{errors:?}");
    }

    #[test]
    fn unknown_superclass_and_range_are_warnings() {
        let text = "ex:A a owl:Class ; rdfs:subClassOf ex:Gone, owl:Thing ; \
            skos:prefLabel \"a\" .\n\
            ex:p a owl:ObjectProperty ; rdfs:domain ex:A ; rdfs:range ex:Nowhere ; \
            skos:prefLabel \"p\" .\n";
        let loaded = load_text(&[text], &config(&[], &[]));
        let warnings = messages(&loaded, Severity::Warning);
        assert_eq!(warnings.len(), 2, "{warnings:?}");
        assert!(warnings.iter().any(|w| w.contains("unknown superclass")));
        assert!(warnings.iter().any(|w| w.contains("unknown range")));
    }

    #[test]
    fn property_selection_by_domain_range_and_super_property() {
        let text = format!(
            "{TREE}ex:locatedIn a owl:ObjectProperty ; rdfs:domain ex:Pump ; rdfs:range ex:Valve .\n\
             ex:partOf a owl:ObjectProperty ; rdfs:domain ex:Pump ; rdfs:range ex:Project .\n\
             ex:relatesTo a owl:ObjectProperty .\n\
             ex:mentions a owl:ObjectProperty ; rdfs:subPropertyOf ex:relatesTo .\n"
        );
        let loaded = load_text(&[&text], &config(&["ex:Equipment", "ex:relatesTo"], &[]));
        let selected: Vec<&str> = loaded
            .ontology
            .properties
            .values()
            .filter(|p| p.selected)
            .map(|p| p.iri.trim_start_matches("http://example.org/onto#"))
            .collect();
        assert_eq!(selected, ["locatedIn", "mentions", "relatesTo"]);
    }

    #[test]
    fn resolves_prefixed_and_full_names() {
        let loaded = load_text(&[TREE], &config(&[], &[]));
        let ontology = &loaded.ontology;
        assert_eq!(
            ontology.resolve("ex:Pump").unwrap(),
            "http://example.org/onto#Pump"
        );
        assert_eq!(
            ontology.resolve("http://example.org/onto#Pump").unwrap(),
            "http://example.org/onto#Pump"
        );
        assert_eq!(
            ontology.resolve("<http://example.org/onto#Pump>").unwrap(),
            "http://example.org/onto#Pump"
        );
        assert!(ontology.class("ex:Pump").is_ok());
        assert!(ontology.class("ex:Nope").is_err());
        assert!(ontology.is_a(
            "http://example.org/onto#Pump",
            "http://example.org/onto#Thing"
        ));
    }

    #[test]
    fn version_changes_with_content_and_selection() {
        let a = load_text(&[TREE], &config(&[], &[]));
        let b = load_text(&[TREE], &config(&[], &[]));
        let c = load_text(&[TREE], &config(&["ex:Pump"], &[]));
        let d = load_text(&[&format!("{TREE}\n")], &config(&[], &[]));
        assert_eq!(a.ontology.version, b.ontology.version);
        assert_ne!(a.ontology.version, c.ontology.version);
        assert_ne!(a.ontology.version, d.ontology.version);
    }

    const UNITS: &str = "@prefix qudt: <http://qudt.org/schema/qudt/> .\n\
        @prefix unit: <http://qudt.org/vocab/unit/> .\n\
        @prefix qk: <http://qudt.org/vocab/quantitykind/> .\n\
        @prefix qkdv: <http://qudt.org/vocab/dimensionvector/> .\n\
        @prefix xsd: <http://www.w3.org/2001/XMLSchema#> .\n\
        @prefix z: <urn:zontologize#> .\n\
        qk:Pressure a qudt:QuantityKind ; qudt:hasDimensionVector qkdv:P .\n\
        qk:Power a qudt:QuantityKind ; qudt:hasDimensionVector qkdv:W .\n\
        unit:BAR a qudt:Unit ; qudt:symbol \"bar\" ; qudt:ucumCode \"bar\" ; \
            qudt:conversionMultiplier 100000.0 ; qudt:hasDimensionVector qkdv:P ; \
            rdfs:label \"Bar\"@en .\n\
        unit:BAR_G a qudt:Unit ; qudt:symbol \"barg\" ; skos:altLabel \"bar(g)\" ; \
            qudt:conversionMultiplier 100000.0 ; qudt:conversionOffset 1.01325 ; \
            qudt:hasDimensionVector qkdv:P .\n\
        unit:OLD a qudt:Unit ; qudt:symbol \"old\" ; qudt:deprecated true ; \
            qudt:conversionMultiplier 1.0 ; qudt:hasDimensionVector qkdv:P .\n\
        unit:NOFACTOR a qudt:Unit ; qudt:symbol \"nf\" ; qudt:hasDimensionVector qkdv:P .\n\
        ex:pressure a owl:DatatypeProperty ; rdfs:domain ex:Pump ; rdfs:range xsd:decimal ; \
            qudt:hasQuantityKind qk:Pressure ; z:similarityTolerance 0.1 ; \
            skos:prefLabel \"pressure\"@en ; skos:altLabel \"druk\"@nl .\n\
        ex:power a owl:DatatypeProperty ; rdfs:domain ex:Pump ; qudt:hasQuantityKind qk:Power .\n\
        ex:stages a owl:DatatypeProperty ; rdfs:domain ex:Pump ; rdfs:range xsd:integer .\n\
        ex:material a owl:DatatypeProperty ; rdfs:domain ex:Pump ; rdfs:range xsd:string .\n\
        ex:built a owl:DatatypeProperty ; rdfs:domain ex:Pump ; rdfs:range xsd:date .\n\
        ex:projectCode a owl:DatatypeProperty ; rdfs:domain ex:Project .\n";

    fn property<'a>(loaded: &'a Loaded, name: &str) -> &'a Property {
        &loaded.ontology.properties[&format!("http://example.org/onto#{name}")]
    }

    #[test]
    fn units_and_quantity_kinds_load() {
        let loaded = load_text(&[TREE, UNITS], &config(&["ex:Equipment"], &[]));
        let units = &loaded.ontology.units;
        assert_eq!(
            units.keys().collect::<Vec<_>>(),
            [
                "http://qudt.org/vocab/unit/BAR",
                "http://qudt.org/vocab/unit/BAR_G"
            ]
        );
        let bar = &units["http://qudt.org/vocab/unit/BAR"];
        assert_eq!(bar.symbols, ["bar"]);
        assert_eq!(bar.multiplier, 100000.0);
        assert_eq!(bar.offset, 0.0);
        assert_eq!(bar.labels["en"], "Bar");
        let gauge = &units["http://qudt.org/vocab/unit/BAR_G"];
        assert_eq!(gauge.offset, 1.01325);
        assert_eq!(gauge.alt_labels["nl"], ["bar(g)"]);
        assert_eq!(loaded.ontology.quantity_kinds.len(), 2);
        assert!(
            messages(&loaded, Severity::Warning)
                .iter()
                .any(|m| m.starts_with("1 units without"))
        );
    }

    #[test]
    fn attribute_properties_get_kind_dimension_and_selection() {
        let loaded = load_text(&[TREE, UNITS], &config(&["ex:Equipment"], &[]));
        let pressure = property(&loaded, "pressure");
        assert!(pressure.is_attribute());
        assert_eq!(pressure.value, Some(ValueKind::Number));
        assert_eq!(
            pressure.dimension.as_deref(),
            Some("http://qudt.org/vocab/dimensionvector/P")
        );
        assert_eq!(pressure.tolerance, Some(0.1));
        assert_eq!(pressure.alt_labels["nl"], ["druk"]);
        assert_eq!(loaded.ontology.fitting_units(pressure).len(), 2);
        assert_eq!(property(&loaded, "power").value, Some(ValueKind::Number));
        assert_eq!(property(&loaded, "stages").value, Some(ValueKind::Number));
        assert!(property(&loaded, "stages").dimension.is_none());
        assert_eq!(property(&loaded, "material").value, Some(ValueKind::Text));
        assert_eq!(property(&loaded, "built").value, None);
        assert!(!property(&loaded, "projectCode").selected);
    }

    #[test]
    fn attribute_checks() {
        let loaded = load_text(&[TREE, UNITS], &config(&["ex:Equipment"], &[]));
        let errors = messages(&loaded, Severity::Error);
        assert!(
            errors
                .iter()
                .any(|m| m.contains("power: no loaded unit fits")),
            "{errors:?}"
        );
        let missing = format!(
            "{TREE}@prefix qudt: <http://qudt.org/schema/qudt/> .\n\
             @prefix z: <urn:zontologize#> .\n\
             ex:speed a owl:DatatypeProperty ; rdfs:domain ex:Pump ; \
             qudt:hasQuantityKind ex:Speed ; z:similarityTolerance -1 ."
        );
        let loaded = load_text(&[&missing], &config(&["ex:Equipment"], &[]));
        let errors = messages(&loaded, Severity::Error);
        assert!(errors.iter().any(|m| m.contains("quantity kind")));
        assert!(errors.iter().any(|m| m.contains("not a positive number")));
        let shared = format!(
            "{UNITS}unit:PSI a qudt:Unit ; qudt:symbol \"bar\" ; \
             qudt:conversionMultiplier 6894.76 ; qudt:hasDimensionVector qkdv:P ."
        );
        let loaded = load_text(&[TREE, &shared], &config(&["ex:Equipment"], &[]));
        assert!(
            messages(&loaded, Severity::Warning)
                .iter()
                .any(|m| m.contains("symbol \"bar\" belongs to 2 units"))
        );
    }
}
