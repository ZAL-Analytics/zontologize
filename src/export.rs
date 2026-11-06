use std::collections::BTreeMap;

use anyhow::{Context, Result, bail};
use oxrdf::{Literal, NamedNode, Triple};
use oxttl::TurtleSerializer;

use crate::db;
use crate::db::ontology::Stored;
use crate::db::resolve::{REFERENCES, REPLACES};
use crate::ns::{DCTERMS, OWL, QUDT, RDF, RDF_TYPE, RDFS, SKOS, XSD_DOUBLE};
use crate::ontology::{Class, LangMap, Property, PropertyKind, Unit};

pub async fn run(config_dir: Option<&std::path::Path>, url: Option<&str>) -> Result<String> {
    let config = crate::config::load(config_dir)?;
    let pool = db::pool::connect(url, &config.database.schema, 1).await?;
    let result = async {
        let Some(version) = db::ontology::current(&pool).await? else {
            bail!("no scan has finished yet; run zontologize scan first");
        };
        let stored = db::ontology::selected(&pool, version).await?;
        turtle(&stored, &config.export.ontology_iri)
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

struct Triples {
    triples: Vec<Triple>,
}

impl Triples {
    fn add(&mut self, s: &str, p: &str, o: Value) -> Result<()> {
        let object: oxrdf::Term = match o {
            Value::Node(iri) => node(&iri)?.into(),
            Value::Text(text, lang) => literal(&text, &lang).into(),
            Value::Number(number) => {
                Literal::new_typed_literal(number.to_string(), node(XSD_DOUBLE)?).into()
            }
        };
        self.triples.push(Triple::new(node(s)?, node(p)?, object));
        Ok(())
    }

    fn a(&mut self, s: &str, kind: String) -> Result<()> {
        self.add(s, RDF_TYPE, Value::Node(kind))
    }

    fn nodes(&mut self, s: &str, p: &str, iris: &[String]) -> Result<()> {
        for iri in iris {
            self.add(s, p, Value::Node(iri.clone()))?;
        }
        Ok(())
    }

    fn labels(&mut self, s: &str, p: &str, labels: &LangMap<String>) -> Result<()> {
        for (lang, label) in labels {
            self.add(s, p, Value::Text(label.clone(), lang.clone()))?;
        }
        Ok(())
    }

    fn alt_labels(&mut self, s: &str, labels: &LangMap<Vec<String>>) -> Result<()> {
        for (lang, labels) in labels {
            for label in labels {
                self.add(
                    s,
                    &format!("{SKOS}altLabel"),
                    Value::Text(label.clone(), lang.clone()),
                )?;
            }
        }
        Ok(())
    }
}

pub fn turtle(stored: &Stored, ontology_iri: &str) -> Result<String> {
    let mut serializer = TurtleSerializer::new();
    for (prefix, iri) in [
        ("rdf", RDF),
        ("rdfs", RDFS),
        ("owl", OWL),
        ("skos", SKOS),
        ("dcterms", DCTERMS),
        ("qudt", QUDT),
    ] {
        serializer = serializer.with_prefix(prefix, iri)?;
    }
    let mut out = Triples {
        triples: Vec::new(),
    };
    out.a(ontology_iri, format!("{OWL}Ontology"))?;
    out.add(
        ontology_iri,
        &format!("{OWL}versionInfo"),
        Value::Text(stored.sha256.clone(), String::new()),
    )?;
    classes(&mut out, &stored.classes)?;
    properties(&mut out, &stored.properties)?;
    quantity_kinds(&mut out, &stored.properties)?;
    units(&mut out, &stored.units)?;
    for (iri, label) in [(REFERENCES, "references"), (REPLACES, "replaces")] {
        out.a(iri, format!("{OWL}ObjectProperty"))?;
        out.add(
            iri,
            &format!("{SKOS}prefLabel"),
            Value::Text(label.to_string(), "en".to_string()),
        )?;
    }
    let mut writer = serializer.for_writer(Vec::new());
    for triple in &out.triples {
        writer.serialize_triple(triple)?;
    }
    let bytes = writer.finish()?;
    Ok(String::from_utf8(bytes)?)
}

fn classes(out: &mut Triples, classes: &[Class]) -> Result<()> {
    for class in classes {
        out.a(&class.iri, format!("{OWL}Class"))?;
        out.nodes(&class.iri, &format!("{RDFS}subClassOf"), &class.parents)?;
        for kind in &class.types {
            out.a(&class.iri, kind.clone())?;
        }
        out.labels(&class.iri, &format!("{SKOS}prefLabel"), &class.labels)?;
        out.alt_labels(&class.iri, &class.alt_labels)?;
        out.labels(&class.iri, &format!("{SKOS}definition"), &class.definition)?;
    }
    Ok(())
}

fn properties(out: &mut Triples, properties: &[Property]) -> Result<()> {
    for property in properties {
        let kind = match property.kind {
            PropertyKind::Object => format!("{OWL}ObjectProperty"),
            PropertyKind::Datatype => format!("{OWL}DatatypeProperty"),
            PropertyKind::Rdf => format!("{RDF}Property"),
        };
        out.a(&property.iri, kind)?;
        out.nodes(&property.iri, &format!("{RDFS}domain"), &property.domain)?;
        out.nodes(&property.iri, &format!("{RDFS}range"), &property.range)?;
        out.nodes(
            &property.iri,
            &format!("{RDFS}subPropertyOf"),
            &property.parents,
        )?;
        out.labels(&property.iri, &format!("{SKOS}prefLabel"), &property.labels)?;
        out.alt_labels(&property.iri, &property.alt_labels)?;
        if let Some(kind) = &property.quantity_kind {
            out.add(
                &property.iri,
                &format!("{QUDT}hasQuantityKind"),
                Value::Node(kind.clone()),
            )?;
        }
        if let Some(tolerance) = property.tolerance {
            out.add(
                &property.iri,
                crate::ontology::TOLERANCE,
                Value::Number(tolerance),
            )?;
        }
    }
    Ok(())
}

fn quantity_kinds(out: &mut Triples, properties: &[Property]) -> Result<()> {
    let mut kinds = BTreeMap::new();
    for property in properties {
        if let (Some(kind), Some(dimension)) = (&property.quantity_kind, &property.dimension) {
            kinds.insert(kind.clone(), dimension.clone());
        }
    }
    for (kind, dimension) in kinds {
        out.a(&kind, format!("{QUDT}QuantityKind"))?;
        out.add(
            &kind,
            &format!("{QUDT}hasDimensionVector"),
            Value::Node(dimension),
        )?;
    }
    Ok(())
}

fn units(out: &mut Triples, units: &[Unit]) -> Result<()> {
    for unit in units {
        out.a(&unit.iri, format!("{QUDT}Unit"))?;
        for symbol in &unit.symbols {
            out.add(
                &unit.iri,
                &format!("{QUDT}symbol"),
                Value::Text(symbol.clone(), String::new()),
            )?;
        }
        out.labels(&unit.iri, &format!("{RDFS}label"), &unit.labels)?;
        out.alt_labels(&unit.iri, &unit.alt_labels)?;
        out.add(
            &unit.iri,
            &format!("{QUDT}hasDimensionVector"),
            Value::Node(unit.dimension.clone()),
        )?;
        out.add(
            &unit.iri,
            &format!("{QUDT}conversionMultiplier"),
            Value::Number(unit.multiplier),
        )?;
        if unit.offset != 0.0 {
            out.add(
                &unit.iri,
                &format!("{QUDT}conversionOffset"),
                Value::Number(unit.offset),
            )?;
        }
    }
    Ok(())
}

enum Value {
    Node(String),
    Text(String, String),
    Number(f64),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn export_uses_the_configured_iri_and_typed_numbers() {
        let stored = Stored {
            sha256: "abc".to_string(),
            classes: Vec::new(),
            properties: Vec::new(),
            units: vec![Unit {
                iri: "http://example.org/unit/BAR".to_string(),
                symbols: vec!["bar".to_string()],
                labels: LangMap::new(),
                alt_labels: LangMap::new(),
                dimension: "http://example.org/dim/P".to_string(),
                quantity_kinds: Vec::new(),
                multiplier: 100000.0,
                offset: 1.5,
            }],
        };
        let text = turtle(&stored, "https://example.org/acme/ontology").unwrap();
        assert!(
            text.contains("<https://example.org/acme/ontology> a owl:Ontology"),
            "{text}"
        );
        assert!(
            text.contains(&format!("\"100000\"^^<{XSD_DOUBLE}>")),
            "{text}"
        );
        assert!(text.contains(&format!("\"1.5\"^^<{XSD_DOUBLE}>")), "{text}");
        let loaded = crate::ontology::load_sources(
            &[("export.ttl".to_string(), text.into_bytes())],
            &crate::config::OntologyConfig {
                files: Vec::new(),
                languages: vec!["en".to_string()],
                include: Vec::new(),
                exclude: Vec::new(),
                document_class: String::new(),
            },
        );
        let unit = &loaded.ontology.units["http://example.org/unit/BAR"];
        assert_eq!(unit.multiplier, 100000.0);
        assert_eq!(unit.offset, 1.5);
    }
}
