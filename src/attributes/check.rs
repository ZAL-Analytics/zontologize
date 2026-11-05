use std::collections::BTreeMap;

use super::{Answer, Built};
use crate::facts::check::Haystack;
use crate::ontology::{Ontology, ValueKind};
use crate::units::{Lookup, UnitIndex, is_reading, normalise, readings, to_si};

pub const PROPERTY_NOT_ALLOWED: &str = "property not allowed";
pub const REF_NOT_ALLOWED: &str = "ref not allowed for the property";
pub const QUOTE_NOT_FOUND: &str = "quote not in the chunk";
pub const NO_NUMBER: &str = "no number";
pub const NUMBER_NOT_IN_QUOTE: &str = "number not in the quote";
pub const MIN_ABOVE_MAX: &str = "min above max";
pub const NO_UNIT: &str = "no unit";
pub const UNIT_NOT_IN_TEXT: &str = "unit not in the text";
pub const UNKNOWN_UNIT: &str = "unknown unit";
pub const AMBIGUOUS_UNIT: &str = "ambiguous unit";
pub const NO_TEXT: &str = "no text value";
pub const TEXT_NOT_FOUND: &str = "text not in the chunk";
pub const TEXT_TOO_LONG: &str = "text too long";
pub const DUPLICATE: &str = "duplicate";

#[derive(Debug, Clone, PartialEq)]
pub struct Value {
    pub entity_id: i64,
    pub property: String,
    pub value_text: String,
    pub unit_text: Option<String>,
    pub unit_iri: Option<String>,
    pub low: Option<f64>,
    pub high: Option<f64>,
    pub si_low: Option<f64>,
    pub si_high: Option<f64>,
    pub dimension: Option<String>,
    pub start: usize,
    pub end: usize,
}

#[derive(Debug, Clone, Default)]
pub struct Checked {
    pub values: Vec<Value>,
    pub drops: BTreeMap<String, i64>,
}

impl Checked {
    fn drop(&mut self, reason: &str, about: &str) {
        *self.drops.entry(format!("{reason}|{about}")).or_default() += 1;
    }
}

pub struct Limits {
    pub max_text_chars: usize,
}

fn slice(text: &str, start: usize, end: usize) -> String {
    text.chars().skip(start).take(end - start).collect()
}

fn find_exact(haystack: &str, needle: &str) -> bool {
    !needle.is_empty() && normalise(haystack).contains(&normalise(needle))
}

pub fn check(
    answer: Answer,
    built: &Built,
    text: &str,
    context: &str,
    ontology: &Ontology,
    units: &UnitIndex,
    limits: &Limits,
) -> Checked {
    let mut checked = Checked::default();
    let haystack = Haystack::new(text);
    for out in answer.values {
        let Some(property) = built
            .properties
            .iter()
            .find(|p| p.eq_ignore_ascii_case(out.property.trim()))
            .and_then(|iri| ontology.properties.get(iri))
        else {
            checked.drop(PROPERTY_NOT_ALLOWED, &out.property);
            continue;
        };
        let iri = property.iri.as_str();
        let reference = out.reference.trim();
        let allowed = built
            .refs
            .get(iri)
            .is_some_and(|refs| refs.iter().any(|r| r == reference));
        let Some(subject) = built.subjects.iter().find(|s| s.reference == reference) else {
            checked.drop(REF_NOT_ALLOWED, iri);
            continue;
        };
        if !allowed {
            checked.drop(REF_NOT_ALLOWED, iri);
            continue;
        }
        let Some((start, end)) = haystack.find(&out.evidence) else {
            checked.drop(QUOTE_NOT_FOUND, iri);
            continue;
        };
        let quote = slice(text, start, end);
        let mut value = Value {
            entity_id: subject.entity_id,
            property: iri.to_string(),
            value_text: quote.clone(),
            unit_text: None,
            unit_iri: None,
            low: None,
            high: None,
            si_low: None,
            si_high: None,
            dimension: property.dimension.clone(),
            start,
            end,
        };
        if property.value == Some(ValueKind::Text) {
            let wanted = out.text.trim();
            if wanted.is_empty() {
                checked.drop(NO_TEXT, iri);
                continue;
            }
            if wanted.chars().count() > limits.max_text_chars {
                checked.drop(TEXT_TOO_LONG, iri);
                continue;
            }
            let Some((start, end)) = haystack.find(wanted) else {
                checked.drop(TEXT_NOT_FOUND, iri);
                continue;
            };
            value.value_text = slice(text, start, end);
            value.start = start;
            value.end = end;
        } else {
            if out.min.is_none() && out.max.is_none() {
                checked.drop(NO_NUMBER, iri);
                continue;
            }
            let found = readings(&quote);
            if [out.min, out.max]
                .into_iter()
                .flatten()
                .any(|v| !v.is_finite() || !is_reading(v, &found))
            {
                checked.drop(NUMBER_NOT_IN_QUOTE, iri);
                continue;
            }
            if let (Some(min), Some(max)) = (out.min, out.max)
                && min > max
            {
                checked.drop(MIN_ABOVE_MAX, iri);
                continue;
            }
            value.low = out.min;
            value.high = out.max;
            let unit_text = out.unit.trim();
            match &property.dimension {
                Some(dimension) => {
                    if unit_text.is_empty() {
                        checked.drop(NO_UNIT, iri);
                        continue;
                    }
                    if !find_exact(text, unit_text) && !find_exact(context, unit_text) {
                        checked.drop(UNIT_NOT_IN_TEXT, iri);
                        continue;
                    }
                    let unit = match units.lookup(dimension, unit_text) {
                        Lookup::Found(unit) => unit,
                        Lookup::Ambiguous => {
                            checked.drop(AMBIGUOUS_UNIT, unit_text);
                            continue;
                        }
                        Lookup::Unknown => {
                            checked.drop(UNKNOWN_UNIT, unit_text);
                            continue;
                        }
                    };
                    value.unit_text = Some(unit_text.to_string());
                    value.unit_iri = Some(unit.iri.clone());
                    value.si_low = out.min.map(|v| to_si(unit, v));
                    value.si_high = out.max.map(|v| to_si(unit, v));
                }
                None => {
                    value.unit_text = (!unit_text.is_empty()).then(|| unit_text.to_string());
                    value.si_low = out.min;
                    value.si_high = out.max;
                }
            }
        }
        if checked.values.contains(&value) {
            checked.drop(DUPLICATE, iri);
            continue;
        }
        checked.values.push(value);
    }
    checked
}

#[cfg(test)]
mod tests {
    use super::super::tests::{built_for, input, iri, ontology, subject};
    use super::*;

    fn answer(json: serde_json::Value) -> Answer {
        serde_json::from_value(json).unwrap()
    }

    fn value(
        reference: &str,
        property: &str,
        min: Option<f64>,
        max: Option<f64>,
        unit: &str,
        text: &str,
        evidence: &str,
    ) -> serde_json::Value {
        serde_json::json!({
            "ref": reference, "property": iri(property), "min": min, "max": max,
            "unit": unit, "text": text, "evidence": evidence,
        })
    }

    fn run(chunk: &str, context: &str, values: Vec<serde_json::Value>) -> Checked {
        let onto = ontology();
        let pump = subject("r1", 10, "Pump", "P-101");
        let sheet = subject("doc", 13, "Sheet", "");
        let mut chunk_input = input(chunk, vec![pump], Some(sheet));
        chunk_input.context = context.to_string();
        let built = built_for(&onto, &chunk_input).expect("eligible");
        let units = UnitIndex::new(&onto);
        check(
            answer(serde_json::json!({ "values": values })),
            &built,
            chunk,
            context,
            &onto,
            &units,
            &Limits { max_text_chars: 20 },
        )
    }

    fn reasons(checked: &Checked) -> Vec<String> {
        checked
            .drops
            .keys()
            .map(|k| k.split('|').next().unwrap_or("").to_string())
            .collect()
    }

    const CHUNK: &str = "P-101 rated power 7,5 kW. Discharge pressure 8 - 10 barg. Material: cast iron GG25. 3 stages.";

    #[test]
    fn good_values_are_kept_with_si_bounds() {
        let checked = run(
            CHUNK,
            "",
            vec![
                value(
                    "r1",
                    "power",
                    Some(7.5),
                    Some(7.5),
                    "kW",
                    "",
                    "rated power 7,5 kW",
                ),
                value("doc", "power", Some(7.5), Some(7.5), "kW", "", "7,5 kW"),
                value(
                    "r1",
                    "pressure",
                    Some(8.0),
                    Some(10.0),
                    "barg",
                    "",
                    "8 - 10 barg",
                ),
                value(
                    "r1",
                    "material",
                    None,
                    None,
                    "",
                    "cast iron GG25",
                    "Material: cast iron GG25",
                ),
                value("r1", "stages", Some(3.0), Some(3.0), "", "", "3 stages"),
            ],
        );
        assert!(checked.drops.is_empty(), "{:?}", checked.drops);
        let power = &checked.values[0];
        assert_eq!(power.entity_id, 10);
        assert_eq!(power.si_low, Some(7500.0));
        assert_eq!(power.value_text, "rated power 7,5 kW");
        assert_eq!(checked.values[1].entity_id, 13);
        let pressure = &checked.values[2];
        assert!((pressure.si_low.unwrap() - 901325.0).abs() < 1e-6);
        assert!((pressure.si_high.unwrap() - 1101325.0).abs() < 1e-6);
        assert_eq!(
            pressure.unit_iri.as_deref(),
            Some("http://example.org/ex#barg")
        );
        let material = &checked.values[3];
        assert_eq!(material.value_text, "cast iron GG25");
        assert_eq!(material.low, None);
        assert_eq!(checked.values[4].si_low, Some(3.0));
        assert_eq!(checked.values[4].dimension, None);
    }

    #[test]
    fn a_unit_in_the_header_or_the_context_is_accepted() {
        let chunk = "| Power [kW] |\n| 250 |";
        let checked = run(
            chunk,
            "",
            vec![value(
                "r1",
                "power",
                Some(250.0),
                Some(250.0),
                "kW",
                "",
                "250",
            )],
        );
        assert_eq!(checked.values.len(), 1, "{:?}", checked.drops);
        let checked = run(
            "| 250 | power of P-101 |",
            "| Power [kW] | Name |",
            vec![value(
                "r1",
                "power",
                Some(250.0),
                Some(250.0),
                "kW",
                "",
                "250",
            )],
        );
        assert_eq!(checked.values.len(), 1, "{:?}", checked.drops);
    }

    #[test]
    fn two_duty_points_are_two_values() {
        let checked = run(
            "P-101 power 242 / 246 kW",
            "",
            vec![
                value(
                    "r1",
                    "power",
                    Some(242.0),
                    Some(242.0),
                    "kW",
                    "",
                    "242 / 246 kW",
                ),
                value(
                    "r1",
                    "power",
                    Some(246.0),
                    Some(246.0),
                    "kW",
                    "",
                    "242 / 246 kW",
                ),
                value(
                    "r1",
                    "power",
                    Some(246.0),
                    Some(246.0),
                    "kW",
                    "",
                    "242 / 246 kW",
                ),
            ],
        );
        assert_eq!(checked.values.len(), 2);
        assert_eq!(reasons(&checked), [DUPLICATE]);
    }

    #[test]
    fn each_drop_reason() {
        let cases = [
            (
                value("r1", "power", Some(7.5), Some(7.5), "kW", "", "not there"),
                QUOTE_NOT_FOUND,
            ),
            (
                value("r9", "power", Some(7.5), Some(7.5), "kW", "", "7,5 kW"),
                REF_NOT_ALLOWED,
            ),
            (
                value(
                    "doc",
                    "pressure",
                    Some(8.0),
                    Some(8.0),
                    "barg",
                    "",
                    "8 - 10 barg",
                ),
                REF_NOT_ALLOWED,
            ),
            (
                value("r1", "power", None, None, "kW", "", "7,5 kW"),
                NO_NUMBER,
            ),
            (
                value("r1", "power", Some(75.0), Some(75.0), "kW", "", "7,5 kW"),
                NUMBER_NOT_IN_QUOTE,
            ),
            (
                value(
                    "r1",
                    "pressure",
                    Some(10.0),
                    Some(8.0),
                    "barg",
                    "",
                    "8 - 10 barg",
                ),
                MIN_ABOVE_MAX,
            ),
            (
                value("r1", "power", Some(7.5), Some(7.5), "", "", "7,5 kW"),
                NO_UNIT,
            ),
            (
                value("r1", "power", Some(7.5), Some(7.5), "MW", "", "7,5 kW"),
                UNIT_NOT_IN_TEXT,
            ),
            (
                value(
                    "r1",
                    "pressure",
                    Some(8.0),
                    Some(8.0),
                    "stages",
                    "",
                    "8 - 10 barg",
                ),
                UNKNOWN_UNIT,
            ),
            (
                value(
                    "r1",
                    "material",
                    None,
                    None,
                    "",
                    "",
                    "Material: cast iron GG25",
                ),
                NO_TEXT,
            ),
            (
                value(
                    "r1",
                    "material",
                    None,
                    None,
                    "",
                    "steel",
                    "Material: cast iron GG25",
                ),
                TEXT_NOT_FOUND,
            ),
            (
                value(
                    "r1",
                    "material",
                    None,
                    None,
                    "",
                    "Material: cast iron GG25",
                    "Material: cast iron GG25",
                ),
                TEXT_TOO_LONG,
            ),
        ];
        for (out, reason) in cases {
            let checked = run(CHUNK, "", vec![out.clone()]);
            assert!(checked.values.is_empty(), "{out}");
            assert_eq!(reasons(&checked), [reason], "{out}");
        }
        let mut wrong = value("r1", "power", Some(7.5), Some(7.5), "kW", "", "7,5 kW");
        wrong["property"] = serde_json::json!("http://example.org/ex#nope");
        assert_eq!(
            reasons(&run(CHUNK, "", vec![wrong])),
            [PROPERTY_NOT_ALLOWED]
        );
    }

    #[test]
    fn ambiguous_case_is_dropped() {
        let checked = run(
            "P-101 power 5 mw",
            "",
            vec![value("r1", "power", Some(5.0), Some(5.0), "mw", "", "5 mw")],
        );
        assert_eq!(reasons(&checked), [AMBIGUOUS_UNIT]);
    }
}
