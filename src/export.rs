use anyhow::{Context, Result, bail};
use oxrdf::{Literal, NamedNode, Triple};
use oxttl::TurtleSerializer;

use crate::db;
use crate::db::ontology::Stored;
use crate::db::resolve::{REFERENCES, REPLACES};
use crate::ontology::PropertyKind;

const RDF: &str = "http://www.w3.org/1999/02/22-rdf-syntax-ns#";
const RDFS: &str = "http://www.w3.org/2000/01/rdf-schema#";
const OWL: &str = "http://www.w3.org/2002/07/owl#";
const SKOS: &str = "http://www.w3.org/2004/02/skos/core#";
const DCTERMS: &str = "http://purl.org/dc/terms/";
const ONTOLOGY_IRI: &str = "urn:zontologize:ontology";

pub async fn run(config_dir: Option<&std::path::Path>, url: Option<&str>) -> Result<String> {
    let config = crate::config::load(config_dir)?;
    let pool = db::pool::connect(url, &config.database.schema, 1).await?;
    let result = async {
        let Some(version) = db::ontology::current(&pool).await? else {
            bail!("no scan has finished yet; run zontologize scan first");
        };
        let stored = db::ontology::selected(&pool, version).await?;
        turtle(&stored)
    }
    .await;
    pool.close().await;
    result
}

fn node(iri: &str) -> Result<NamedNode> {
    NamedNode::new(iri).with_context(|| format!("{iri} is not a valid IRI"))
}

fn literal(text: &str, lang: &str) -> Literal {
    Literal::new_language_tagged_literal(text, lang)
        .unwrap_or_else(|_| Literal::new_simple_literal(text))
}

pub fn turtle(stored: &Stored) -> Result<String> {
    let mut serializer = TurtleSerializer::new();
    for (prefix, iri) in [
        ("rdf", RDF),
        ("rdfs", RDFS),
        ("owl", OWL),
        ("skos", SKOS),
        ("dcterms", DCTERMS),
    ] {
        serializer = serializer.with_prefix(prefix, iri)?;
    }
    let mut triples = Vec::new();
    let mut add = |s: &str, p: &str, o: Value| -> Result<()> {
        let object: oxrdf::Term = match o {
            Value::Node(iri) => node(&iri)?.into(),
            Value::Text(text, lang) => literal(&text, &lang).into(),
        };
        triples.push(Triple::new(node(s)?, node(p)?, object));
        Ok(())
    };
    let a = format!("{RDF}type");
    add(ONTOLOGY_IRI, &a, Value::Node(format!("{OWL}Ontology")))?;
    add(
        ONTOLOGY_IRI,
        &format!("{OWL}versionInfo"),
        Value::Text(stored.sha256.clone(), String::new()),
    )?;
    for class in &stored.classes {
        add(&class.iri, &a, Value::Node(format!("{OWL}Class")))?;
        for parent in &class.parents {
            add(
                &class.iri,
                &format!("{RDFS}subClassOf"),
                Value::Node(parent.clone()),
            )?;
        }
        for kind in &class.types {
            add(&class.iri, &a, Value::Node(kind.clone()))?;
        }
        for (lang, label) in &class.labels {
            add(
                &class.iri,
                &format!("{SKOS}prefLabel"),
                Value::Text(label.clone(), lang.clone()),
            )?;
        }
        for (lang, labels) in &class.alt_labels {
            for label in labels {
                add(
                    &class.iri,
                    &format!("{SKOS}altLabel"),
                    Value::Text(label.clone(), lang.clone()),
                )?;
            }
        }
        for (lang, text) in &class.definition {
            add(
                &class.iri,
                &format!("{SKOS}definition"),
                Value::Text(text.clone(), lang.clone()),
            )?;
        }
    }
    for property in &stored.properties {
        let kind = match property.kind {
            PropertyKind::Object => format!("{OWL}ObjectProperty"),
            PropertyKind::Datatype => format!("{OWL}DatatypeProperty"),
            PropertyKind::Rdf => format!("{RDF}Property"),
        };
        add(&property.iri, &a, Value::Node(kind))?;
        for domain in &property.domain {
            add(
                &property.iri,
                &format!("{RDFS}domain"),
                Value::Node(domain.clone()),
            )?;
        }
        for range in &property.range {
            add(
                &property.iri,
                &format!("{RDFS}range"),
                Value::Node(range.clone()),
            )?;
        }
        for parent in &property.parents {
            add(
                &property.iri,
                &format!("{RDFS}subPropertyOf"),
                Value::Node(parent.clone()),
            )?;
        }
        for (lang, label) in &property.labels {
            add(
                &property.iri,
                &format!("{SKOS}prefLabel"),
                Value::Text(label.clone(), lang.clone()),
            )?;
        }
    }
    for (iri, label) in [(REFERENCES, "references"), (REPLACES, "replaces")] {
        add(iri, &a, Value::Node(format!("{OWL}ObjectProperty")))?;
        add(
            iri,
            &format!("{SKOS}prefLabel"),
            Value::Text(label.to_string(), "en".to_string()),
        )?;
    }
    let mut writer = serializer.for_writer(Vec::new());
    for triple in &triples {
        writer.serialize_triple(triple)?;
    }
    let bytes = writer.finish()?;
    Ok(String::from_utf8(bytes)?)
}

enum Value {
    Node(String),
    Text(String, String),
}
