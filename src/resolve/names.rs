use std::collections::BTreeMap;

use crate::ontology::Ontology;
use crate::rules::ids::{IdPatterns, On};

pub fn plain(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in crate::text::strip_accents(text).chars() {
        if c.is_alphanumeric() {
            out.extend(c.to_lowercase());
        } else {
            out.push(' ');
        }
    }
    crate::text::squash(&out)
}

pub struct Normaliser {
    strip: Vec<String>,
}

impl Normaliser {
    pub fn new(strip_words: &[String]) -> Normaliser {
        let mut strip: Vec<String> = strip_words
            .iter()
            .map(|w| plain(w))
            .filter(|w| !w.is_empty())
            .collect();
        strip.sort_by_key(|w| std::cmp::Reverse(w.len()));
        Normaliser { strip }
    }

    pub fn key(&self, name: &str) -> String {
        let mut padded = format!(" {} ", plain(name));
        for word in &self.strip {
            let needle = format!(" {word} ");
            while padded.contains(&needle) {
                padded = padded.replace(&needle, " ");
            }
        }
        crate::text::squash(&padded)
    }
}

const TAG_PREFIX_CHARS: usize = 3;

fn digit_tokens(key: &str) -> Vec<String> {
    let words: Vec<&str> = key.split(' ').collect();
    let mut tokens = Vec::new();
    for (i, word) in words.iter().enumerate() {
        if !word.chars().any(|c| c.is_ascii_digit()) {
            continue;
        }
        let prefix = match i.checked_sub(1).map(|j| words[j]) {
            Some(before)
                if before.chars().count() <= TAG_PREFIX_CHARS
                    && before.chars().all(char::is_alphabetic) =>
            {
                before
            }
            _ => "",
        };
        tokens.push(format!("{prefix}{word}"));
    }
    tokens.sort_unstable();
    tokens
}

pub fn digits_agree(a: &str, b: &str) -> bool {
    digit_tokens(a) == digit_tokens(b)
}

fn first_token(key: &str) -> &str {
    key.split(' ').next().unwrap_or("")
}

pub fn similarity(a: &str, b: &str) -> f64 {
    strsim::jaro_winkler(a, b)
}

pub fn compatible(ontology: &Ontology, a: &str, b: &str) -> bool {
    a == b || ontology.is_a(a, b) || ontology.is_a(b, a)
}

pub fn id_keys(ids: &IdPatterns, name: &str) -> Vec<(String, String)> {
    let mut keys: Vec<(String, String)> = ids
        .find(name, On::Text)
        .into_iter()
        .map(|hit| (hit.class, hit.key))
        .collect();
    keys.sort();
    keys.dedup();
    keys
}

pub fn blocks<T>(
    items: &[T],
    key: impl Fn(&T) -> &str,
    chars: usize,
) -> BTreeMap<String, Vec<usize>> {
    let mut out: BTreeMap<String, Vec<usize>> = BTreeMap::new();
    for (i, item) in items.iter().enumerate() {
        let first = first_token(key(item));
        if !first.is_empty() {
            let prefix: String = first.chars().take(chars.max(1)).collect();
            out.entry(prefix).or_default().push(i);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rules::tests::rules;

    fn ontology() -> Ontology {
        let ttl = "@prefix ex: <http://example.org/onto#> .\n\
            @prefix owl: <http://www.w3.org/2002/07/owl#> .\n\
            @prefix rdfs: <http://www.w3.org/2000/01/rdf-schema#> .\n\
            ex:Organisation a owl:Class .\n\
            ex:Customer a owl:Class ; rdfs:subClassOf ex:Organisation .\n\
            ex:Supplier a owl:Class ; rdfs:subClassOf ex:Organisation .\n";
        let config = crate::config::OntologyConfig {
            files: Vec::new(),
            languages: vec!["en".to_string()],
            include: vec!["ex:Organisation".to_string()],
            exclude: Vec::new(),
            document_class: String::new(),
        };
        crate::ontology::load_sources(&[("o.ttl".to_string(), ttl.as_bytes().to_vec())], &config)
            .ontology
    }

    #[test]
    fn accents_case_and_punctuation_go() {
        assert_eq!(plain("Société Générale, S.A."), "societe generale s a");
        assert_eq!(plain("  Ångström--Pumps  "), "angstrom pumps");
    }

    #[test]
    fn strip_words_are_removed_as_whole_words() {
        let normaliser = Normaliser::new(&["B.V.".to_string(), "GmbH".to_string()]);
        assert_eq!(normaliser.key("Northwind Pumps B.V."), "northwind pumps");
        assert_eq!(normaliser.key("Bvba Northwind GmbH"), "bvba northwind");
        assert_eq!(normaliser.key("B.V."), "");
    }

    #[test]
    fn numbers_must_agree() {
        assert!(!digits_agree(&plain("P-101"), &plain("P-102")));
        assert!(digits_agree(&plain("Pump P-101"), &plain("P 101 pump")));
        assert!(digits_agree("acme pumps", "acme pump"));
        assert!(!digits_agree(&plain("XX-372"), &plain("XY-372")));
        assert!(digits_agree(&plain("P-101"), &plain("P 101")));
        assert!(digits_agree(&plain("P-101"), &plain("p101")));
        assert!(digits_agree(&plain("Pump 101"), &plain("pump 101")));
        let a = plain("Pump P-101");
        let b = plain("Pump P-102");
        assert!(similarity(&a, &b) > 0.9);
    }

    #[test]
    fn an_id_inside_a_name_gives_the_key() {
        let rules =
            rules("[[id_patterns]]\nclass = \"ex:Project\"\npattern = '\\bP-\\d{4}-\\d{4}\\b'\n")
                .unwrap();
        assert_eq!(
            id_keys(&rules.ids, "Project P-2021-0042 at Eastport"),
            [(
                "http://example.org/onto#Project".to_string(),
                "P-2021-0042".to_string()
            )]
        );
        assert!(id_keys(&rules.ids, "Eastport").is_empty());
    }

    #[test]
    fn a_class_and_its_ancestor_match_but_siblings_do_not() {
        let ontology = ontology();
        let customer = ontology.class("ex:Customer").unwrap().iri.clone();
        let organisation = ontology.class("ex:Organisation").unwrap().iri.clone();
        let supplier = ontology.class("ex:Supplier").unwrap().iri.clone();
        assert!(compatible(&ontology, &customer, &organisation));
        assert!(compatible(&ontology, &organisation, &customer));
        assert!(!compatible(&ontology, &customer, &supplier));
    }

    #[test]
    fn blocks_group_by_the_start_of_the_first_token() {
        let names = [
            "acme pumps",
            "acme pump",
            "northwind",
            "havenstad",
            "havenstadt",
        ];
        let blocks = blocks(&names, |n| n, 4);
        assert_eq!(blocks["acme"], [0, 1]);
        assert_eq!(blocks["nort"], [2]);
        assert_eq!(blocks["have"], [3, 4]);
    }
}
