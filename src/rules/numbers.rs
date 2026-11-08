use regex::Regex;
use serde::Deserialize;

use crate::ontology::Ontology;

const NUMBER_GROUP: &str = "n";
const NUMBER_SLOT: &str = "{n}";

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NumberKeySpec {
    class: String,
    pattern: String,
    key: String,
}

#[derive(Debug, Clone)]
pub struct NumberKey {
    pub class: String,
    regex: Regex,
    key: String,
}

impl NumberKey {
    pub fn key_of(&self, name: &str) -> Option<String> {
        let captures = self.regex.captures(name.trim())?;
        let number = captures.name(NUMBER_GROUP)?.as_str().to_uppercase();
        Some(self.key.replace(NUMBER_SLOT, &number))
    }
}

pub fn compile(specs: &[NumberKeySpec], ontology: &Ontology) -> Result<Vec<NumberKey>, String> {
    let mut out = Vec::new();
    for (index, spec) in specs.iter().enumerate() {
        let class = ontology
            .class(&spec.class)
            .map_err(|e| format!("number_keys[{index}].class: {e}"))?
            .iri
            .clone();
        let regex =
            Regex::new(&spec.pattern).map_err(|e| format!("number_keys[{index}].pattern: {e}"))?;
        if !regex.capture_names().flatten().any(|n| n == NUMBER_GROUP) {
            return Err(format!("number_keys[{index}].pattern has no named group n"));
        }
        if !spec.key.contains(NUMBER_SLOT) {
            return Err(format!("number_keys[{index}].key does not contain {{n}}"));
        }
        out.push(NumberKey {
            class,
            regex,
            key: spec.key.clone(),
        });
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rule(pattern: &str, key: &str) -> NumberKey {
        NumberKey {
            class: "urn:test#Project".to_string(),
            regex: Regex::new(pattern).unwrap(),
            key: key.to_string(),
        }
    }

    const P: &str = r"(?i)^(?:P-?)?(?P<n>\d{5})(?:-[a-z])?(?:\s|$)";
    const W: &str = r"(?i)^(?:W-?)?(?P<n>\d{8})(?:\s|$)";

    #[test]
    fn spellings_of_one_project_get_one_key() {
        let p = rule(P, "P-{n}");
        for name in [
            "P-12345",
            "12345",
            "12345 Northwind",
            "P-12345-B",
            "p-12345-b",
            "P-12345 Booster Skid",
        ] {
            assert_eq!(p.key_of(name).as_deref(), Some("P-12345"), "{name}");
        }
        assert_eq!(p.key_of("P-12346").as_deref(), Some("P-12346"));
        assert_eq!(p.key_of("123450"), None);
        assert_eq!(p.key_of("Northwind 12345"), None);
    }

    #[test]
    fn eight_digits_are_work_order_numbers() {
        let w = rule(W, "W-{n}");
        assert_eq!(w.key_of("20210042 Contoso").as_deref(), Some("W-20210042"));
        assert_eq!(w.key_of("w-20210042").as_deref(), Some("W-20210042"));
        assert_eq!(w.key_of("W-2021004"), None);
    }
}
