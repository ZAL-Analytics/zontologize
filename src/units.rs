use std::collections::{BTreeMap, BTreeSet};

use aho_corasick::{AhoCorasick, Anchored, Input, MatchKind, StartKind};
use anyhow::{Context, Result};
use regex::Regex;
use sha2::{Digest, Sha256};
use unicode_normalization::UnicodeNormalization;

use crate::ontology::{Ontology, Unit};

#[derive(Debug, Clone, PartialEq)]
pub enum Lookup<'a> {
    Found(&'a Unit),
    Ambiguous,
    Unknown,
}

pub struct UnitIndex<'a> {
    exact: BTreeMap<(String, String), BTreeSet<&'a str>>,
    folded: BTreeMap<(String, String), BTreeSet<&'a str>>,
    texts: BTreeMap<String, BTreeSet<String>>,
    units: BTreeMap<&'a str, &'a Unit>,
}

pub fn normalise(text: &str) -> String {
    let nfkc: String = text.nfkc().collect();
    let mut out = String::with_capacity(nfkc.len());
    for word in nfkc.split_whitespace() {
        if !out.is_empty() {
            out.push(' ');
        }
        out.push_str(word);
    }
    out.replace('−', "-")
}

pub fn unit_texts(unit: &Unit) -> Vec<String> {
    let mut texts: Vec<String> = unit.symbols.clone();
    texts.extend(unit.labels.values().cloned());
    texts.extend(unit.alt_labels.values().flatten().cloned());
    let mut seen = BTreeSet::new();
    texts
        .into_iter()
        .map(|t| normalise(&t))
        .filter(|t| !t.is_empty() && seen.insert(t.clone()))
        .collect()
}

impl<'a> UnitIndex<'a> {
    pub fn new(ontology: &'a Ontology) -> UnitIndex<'a> {
        let dimensions: BTreeSet<&str> = ontology
            .properties
            .values()
            .filter(|p| p.is_attribute())
            .filter_map(|p| p.dimension.as_deref())
            .collect();
        let mut index = UnitIndex {
            exact: BTreeMap::new(),
            folded: BTreeMap::new(),
            texts: BTreeMap::new(),
            units: BTreeMap::new(),
        };
        for unit in ontology.units.values() {
            if !dimensions.contains(unit.dimension.as_str()) {
                continue;
            }
            index.units.insert(&unit.iri, unit);
            for text in unit_texts(unit) {
                let dimension = unit.dimension.clone();
                index
                    .folded
                    .entry((dimension.clone(), text.to_lowercase()))
                    .or_default()
                    .insert(&unit.iri);
                index
                    .texts
                    .entry(dimension.clone())
                    .or_default()
                    .insert(text.clone());
                index
                    .exact
                    .entry((dimension, text))
                    .or_default()
                    .insert(&unit.iri);
            }
        }
        index
    }

    pub fn lookup(&self, dimension: &str, text: &str) -> Lookup<'a> {
        let text = normalise(text);
        let pick = |iris: &BTreeSet<&'a str>| -> Lookup<'a> {
            match iris.len() {
                1 => iris
                    .first()
                    .and_then(|iri| self.units.get(iri))
                    .map_or(Lookup::Unknown, |u| Lookup::Found(u)),
                _ => Lookup::Ambiguous,
            }
        };
        if let Some(iris) = self.exact.get(&(dimension.to_string(), text.clone())) {
            return pick(iris);
        }
        match self
            .folded
            .get(&(dimension.to_string(), text.to_lowercase()))
        {
            Some(iris) => pick(iris),
            None => Lookup::Unknown,
        }
    }

    pub fn symbols(&self, dimension: &str, limit: usize) -> Vec<String> {
        let mut out: Vec<String> = self
            .units
            .values()
            .filter(|u| u.dimension == dimension)
            .flat_map(|u| u.symbols.first().cloned())
            .collect();
        out.sort_by_key(|s| (s.chars().count(), s.clone()));
        out.dedup();
        out.truncate(limit);
        out
    }

    pub fn fingerprint(&self) -> String {
        let mut hasher = Sha256::new();
        for ((dimension, text), iris) in &self.exact {
            for part in [dimension.as_str(), text.as_str()]
                .into_iter()
                .chain(iris.iter().copied())
            {
                hasher.update(part.as_bytes());
                hasher.update([0]);
            }
        }
        for unit in self.units.values() {
            hasher.update(unit.iri.as_bytes());
            hasher.update(unit.multiplier.to_le_bytes());
            hasher.update(unit.offset.to_le_bytes());
        }
        crate::ontology::hex(&hasher.finalize())
    }

    pub fn finder(&self) -> Result<UnitFinder> {
        let mut owners: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
        for (dimension, texts) in &self.texts {
            for text in texts {
                owners
                    .entry(text.to_lowercase())
                    .or_default()
                    .insert(dimension.clone());
            }
        }
        let patterns: Vec<&String> = owners.keys().collect();
        let automaton = AhoCorasick::builder()
            .match_kind(MatchKind::LeftmostLongest)
            .start_kind(StartKind::Anchored)
            .build(&patterns)
            .context("cannot build the unit matcher")?;
        Ok(UnitFinder {
            automaton,
            owners: owners.into_values().collect(),
        })
    }
}

pub struct UnitFinder {
    automaton: AhoCorasick,
    owners: Vec<BTreeSet<String>>,
}

impl UnitFinder {
    pub fn dimensions(&self, lowered: &str) -> BTreeSet<String> {
        let mut found = BTreeSet::new();
        let bytes = lowered.as_bytes();
        for (i, byte) in bytes.iter().enumerate() {
            if !byte.is_ascii_digit() {
                continue;
            }
            let next = bytes.get(i + 1).copied();
            let after = bytes.get(i + 2).copied();
            if next.is_some_and(|b| b.is_ascii_digit())
                || (matches!(next, Some(b'.' | b',')) && after.is_some_and(|b| b.is_ascii_digit()))
            {
                continue;
            }
            let mut start = i + 1;
            if matches!(next, Some(b' ' | b'\t')) {
                start += 1;
            }
            if start >= bytes.len() {
                continue;
            }
            let input = Input::new(lowered)
                .span(start..lowered.len())
                .anchored(Anchored::Yes);
            let Some(hit) = self.automaton.find(input) else {
                continue;
            };
            let boundary = lowered[hit.end()..]
                .chars()
                .next()
                .is_none_or(|c| !c.is_alphanumeric());
            if boundary {
                found.extend(self.owners[hit.pattern().as_usize()].iter().cloned());
            }
        }
        found
    }
}

pub fn to_si(unit: &Unit, value: f64) -> f64 {
    (value + unit.offset) * unit.multiplier
}

fn number_token() -> &'static Regex {
    static TOKEN: std::sync::OnceLock<Regex> = std::sync::OnceLock::new();
    TOKEN.get_or_init(|| {
        Regex::new(r"[-−]?\d+(?:[.,' \u{a0}\u{202f}]\d+)*").expect("number token pattern")
    })
}

fn grouped(groups: &[&str], separators: &[char], decimal: Option<char>) -> Option<f64> {
    let (whole, fraction) = match decimal {
        Some(mark) if separators.last() == Some(&mark) => {
            (&groups[..groups.len() - 1], groups.last().copied())
        }
        Some(_) => return None,
        None => (groups, None),
    };
    let thousands = &separators[..whole.len().saturating_sub(1)];
    if let Some(first) = thousands.first()
        && (thousands.iter().any(|s| s != first)
            || Some(*first) == decimal
            || whole[1..].iter().any(|g| g.len() != 3))
    {
        return None;
    }
    let mut text: String = whole.concat();
    if let Some(fraction) = fraction {
        text.push('.');
        text.push_str(fraction);
    }
    text.parse().ok()
}

pub fn readings(quote: &str) -> Vec<f64> {
    let mut out: Vec<f64> = Vec::new();
    for found in number_token().find_iter(quote) {
        let token = found.as_str();
        let negative = token.starts_with(['-', '−']);
        let body = token.trim_start_matches(['-', '−']);
        let groups: Vec<&str> = body.split(|c: char| !c.is_ascii_digit()).collect();
        let separators: Vec<char> = body.chars().filter(|c| !c.is_ascii_digit()).collect();
        let mut values: Vec<f64> = groups.iter().filter_map(|g| g.parse().ok()).collect();
        for (i, separator) in separators.iter().enumerate() {
            if matches!(separator, '.' | ',') {
                let text = format!("{}.{}", groups[i], groups[i + 1]);
                values.extend(text.parse::<f64>().ok());
            }
        }
        if groups.len() > 1 {
            for decimal in [None, Some('.'), Some(',')] {
                values.extend(grouped(&groups, &separators, decimal));
            }
        }
        for value in values {
            out.push(value);
            if negative {
                out.push(-value);
            }
        }
    }
    out
}

pub fn is_reading(value: f64, readings: &[f64]) -> bool {
    readings
        .iter()
        .any(|r| (r - value).abs() <= 1e-9 * value.abs().max(1.0))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::OntologyConfig;

    const TTL: &str = r#"
        @prefix ex: <http://example.org/onto#> .
        @prefix owl: <http://www.w3.org/2002/07/owl#> .
        @prefix rdfs: <http://www.w3.org/2000/01/rdf-schema#> .
        @prefix skos: <http://www.w3.org/2004/02/skos/core#> .
        @prefix qudt: <http://qudt.org/schema/qudt/> .
        @prefix unit: <http://qudt.org/vocab/unit/> .
        @prefix qk: <http://qudt.org/vocab/quantitykind/> .
        @prefix qkdv: <http://qudt.org/vocab/dimensionvector/> .
        ex:Pump a owl:Class .
        qk:Pressure a qudt:QuantityKind ; qudt:hasDimensionVector qkdv:P .
        qk:Power a qudt:QuantityKind ; qudt:hasDimensionVector qkdv:W .
        qk:Temperature a qudt:QuantityKind ; qudt:hasDimensionVector qkdv:T .
        unit:BAR a qudt:Unit ; qudt:symbol "bar" ; rdfs:label "Bar"@en ;
            qudt:conversionMultiplier 100000.0 ; qudt:hasDimensionVector qkdv:P .
        unit:BAR_G a qudt:Unit ; qudt:symbol "barg" ; skos:altLabel "bar(g)" ;
            qudt:conversionMultiplier 100000.0 ; qudt:conversionOffset 1.01325 ;
            qudt:hasDimensionVector qkdv:P .
        unit:PSI a qudt:Unit ; qudt:symbol "psi" ; qudt:symbol "lbf/in²" ;
            qudt:conversionMultiplier 6894.757 ; qudt:hasDimensionVector qkdv:P .
        unit:KiloW a qudt:Unit ; qudt:symbol "kW" ;
            qudt:conversionMultiplier 1000.0 ; qudt:hasDimensionVector qkdv:W .
        unit:MegaW a qudt:Unit ; qudt:symbol "MW" ;
            qudt:conversionMultiplier 1000000.0 ; qudt:hasDimensionVector qkdv:W .
        unit:MilliW a qudt:Unit ; qudt:symbol "mW" ;
            qudt:conversionMultiplier 0.001 ; qudt:hasDimensionVector qkdv:W .
        unit:HP_A a qudt:Unit ; qudt:symbol "hp" ;
            qudt:conversionMultiplier 745.7 ; qudt:hasDimensionVector qkdv:W .
        unit:HP_B a qudt:Unit ; qudt:symbol "hp" ;
            qudt:conversionMultiplier 735.5 ; qudt:hasDimensionVector qkdv:W .
        unit:DEG_C a qudt:Unit ; qudt:symbol "°C" ;
            qudt:conversionMultiplier 1.0 ; qudt:conversionOffset 273.15 ;
            qudt:hasDimensionVector qkdv:T .
        ex:pressure a owl:DatatypeProperty ; rdfs:domain ex:Pump ; qudt:hasQuantityKind qk:Pressure .
        ex:power a owl:DatatypeProperty ; rdfs:domain ex:Pump ; qudt:hasQuantityKind qk:Power .
        ex:temperature a owl:DatatypeProperty ; rdfs:domain ex:Pump ;
            qudt:hasQuantityKind qk:Temperature .
    "#;

    const P: &str = "http://qudt.org/vocab/dimensionvector/P";
    const W: &str = "http://qudt.org/vocab/dimensionvector/W";
    const T: &str = "http://qudt.org/vocab/dimensionvector/T";

    fn ontology() -> Ontology {
        let config = OntologyConfig {
            files: Vec::new(),
            languages: vec!["en".to_string(), "nl".to_string()],
            include: vec!["http://example.org/onto#Pump".to_string()],
            exclude: Vec::new(),
            document_class: String::new(),
        };
        let loaded = crate::ontology::load_sources(&[("t.ttl".to_string(), TTL.into())], &config);
        assert!(!loaded.has_errors(), "{:?}", loaded.findings);
        loaded.ontology
    }

    fn found(index: &UnitIndex, dimension: &str, text: &str) -> Option<String> {
        match index.lookup(dimension, text) {
            Lookup::Found(unit) => Some(unit.iri.rsplit('/').next().unwrap().to_string()),
            _ => None,
        }
    }

    #[test]
    fn lookup_is_exact_first_then_unique_case_insensitive() {
        let onto = ontology();
        let index = UnitIndex::new(&onto);
        assert_eq!(found(&index, W, "mW").as_deref(), Some("MilliW"));
        assert_eq!(found(&index, W, "MW").as_deref(), Some("MegaW"));
        assert_eq!(index.lookup(W, "mw"), Lookup::Ambiguous);
        assert_eq!(found(&index, W, "KW").as_deref(), Some("KiloW"));
        assert_eq!(index.lookup(W, "hp"), Lookup::Ambiguous);
        assert_eq!(found(&index, P, "BARG").as_deref(), Some("BAR_G"));
        assert_eq!(found(&index, P, "bar(g)").as_deref(), Some("BAR_G"));
        assert_eq!(found(&index, P, "lbf/in2").as_deref(), Some("PSI"));
        assert_eq!(found(&index, P, " bar ").as_deref(), Some("BAR"));
        assert_eq!(index.lookup(P, "kW"), Lookup::Unknown);
        assert_eq!(found(&index, T, "℃").as_deref(), Some("DEG_C"));
    }

    #[test]
    fn si_values_use_offset_and_multiplier() {
        let onto = ontology();
        let units = &onto.units;
        let gauge = &units["http://qudt.org/vocab/unit/BAR_G"];
        assert!((to_si(gauge, 0.0) - 101325.0).abs() < 1e-6);
        let celsius = &units["http://qudt.org/vocab/unit/DEG_C"];
        assert!((to_si(celsius, 20.0) - 293.15).abs() < 1e-9);
        let kilowatt = &units["http://qudt.org/vocab/unit/KiloW"];
        assert_eq!(to_si(kilowatt, 7.5), 7500.0);
    }

    #[test]
    fn number_readings() {
        let has = |quote: &str, value: f64| is_reading(value, &readings(quote));
        assert!(has("7,5 kW", 7.5));
        assert!(has("7.5 kW", 7.5));
        assert!(has("1.450 kW", 1.45));
        assert!(has("1.450 kW", 1450.0));
        assert!(has("-0,5 barg", -0.5));
        assert!(has("−0,5 barg", -0.5));
        assert!(has("1 450 rpm", 1450.0));
        assert!(has("1.234,5 m3/h", 1234.5));
        assert!(has("1,234.5 m3/h", 1234.5));
        assert!(has("-1..10 barg", -1.0));
        assert!(has("-1..10 barg", 10.0));
        assert!(has("10-12 bar", 12.0));
        assert!(has("1585  / 2320 m3/hr", 2320.0));
        assert!(!has("7,5 kW", 75.0));
        assert!(!has("1.450 kW", 14.5));
        assert!(!has("1,23,4", 1234.0));
        assert!(!has("no digits", 0.0));
    }

    #[test]
    fn patterns_find_numbers_with_fitting_units() {
        let onto = ontology();
        let index = UnitIndex::new(&onto);
        let finder = index.finder().unwrap();
        let hits = |text: &str| -> Vec<String> {
            finder
                .dimensions(&normalise(text).to_lowercase())
                .iter()
                .map(|d| d.rsplit('/').next().unwrap().to_string())
                .collect()
        };
        assert_eq!(hits("rated 7,5 kW at 1450 rpm"), ["W"]);
        assert_eq!(hits("design 10 barg, 80 °C"), ["P", "T"]);
        assert!(hits("10 barrels").is_empty());
        assert!(hits("kW only").is_empty());
        assert_eq!(hits("1.450 KW"), ["W"]);
        assert_eq!(hits("Druk: 8-10 BAR(G)"), ["P"]);
        assert!(hits("P-101 at 10").is_empty());
        assert_eq!(index.symbols(W, 2), ["MW", "hp"]);
    }
}
