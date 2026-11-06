use std::collections::BTreeMap;

use super::{Answer, Built};
use crate::facts::check::Haystack;
use crate::ontology::{Ontology, ValueKind};
use crate::text::nfkc_squash;
use crate::units::{Lookup, UnitIndex, is_reading, readings, to_si};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reason {
    PropertyNotAllowed,
    RefNotAllowed,
    QuoteNotFound,
    NoNumber,
    NumberNotInQuote,
    MinAboveMax,
    NoUnit,
    UnitNotInText,
    UnknownUnit,
    AmbiguousUnit,
    NoText,
    TextNotFound,
    TextTooLong,
    Duplicate,
    NotOfType,
}

impl Reason {
    pub fn as_str(self) -> &'static str {
        match self {
            Reason::PropertyNotAllowed => "property not allowed",
            Reason::RefNotAllowed => "ref not allowed for the property",
            Reason::QuoteNotFound => "quote not in the chunk",
            Reason::NoNumber => "no number",
            Reason::NumberNotInQuote => "number not in the quote",
            Reason::MinAboveMax => "min above max",
            Reason::NoUnit => "no unit",
            Reason::UnitNotInText => "unit not in the text",
            Reason::UnknownUnit => "unknown unit",
            Reason::AmbiguousUnit => "ambiguous unit",
            Reason::NoText => "no text value",
            Reason::TextNotFound => "text not in the chunk",
            Reason::TextTooLong => "text too long",
            Reason::Duplicate => "duplicate",
            Reason::NotOfType => "number does not fit the datatype",
        }
    }
}

impl std::fmt::Display for Reason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

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
    fn drop(&mut self, reason: Reason, about: &str) {
        let about: String = about
            .chars()
            .take(crate::facts::check::MAX_DROP_TEXT)
            .collect();
        *self.drops.entry(format!("{reason}|{about}")).or_default() += 1;
    }
}

pub struct Limits {
    pub max_text_chars: usize,
}

fn slice(text: &str, start: usize, end: usize) -> String {
    text.chars().skip(start).take(end - start).collect()
}

fn has_unit(haystack: &str, unit: &str) -> bool {
    let haystack = nfkc_squash(haystack).to_lowercase();
    let unit = nfkc_squash(unit).to_lowercase();
    if unit.is_empty() {
        return false;
    }
    haystack.match_indices(&unit).any(|(at, _)| {
        let before = haystack[..at].chars().next_back();
        let after = haystack[at + unit.len()..].chars().next();
        !before.is_some_and(char::is_alphabetic) && !after.is_some_and(char::is_alphanumeric)
    })
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
            .find(|p| *p == out.property.trim())
            .and_then(|iri| ontology.properties.get(iri))
        else {
            checked.drop(Reason::PropertyNotAllowed, &out.property);
            continue;
        };
        let iri = property.iri.as_str();
        let reference = out.reference.trim();
        let allowed = built
            .refs
            .get(iri)
            .is_some_and(|refs| refs.iter().any(|r| r == reference));
        let Some(subject) = built.subjects.iter().find(|s| s.reference == reference) else {
            checked.drop(Reason::RefNotAllowed, iri);
            continue;
        };
        if !allowed {
            checked.drop(Reason::RefNotAllowed, iri);
            continue;
        }
        let Some((start, end)) = haystack.find(&out.evidence) else {
            checked.drop(Reason::QuoteNotFound, iri);
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
                checked.drop(Reason::NoText, iri);
                continue;
            }
            if wanted.chars().count() > limits.max_text_chars {
                checked.drop(Reason::TextTooLong, iri);
                continue;
            }
            let Some((from, to)) = Haystack::new(&quote).find(wanted) else {
                checked.drop(Reason::TextNotFound, iri);
                continue;
            };
            value.value_text = slice(&quote, from, to);
            value.start = start + from;
            value.end = start + to;
        } else {
            if out.min.is_none() && out.max.is_none() {
                checked.drop(Reason::NoNumber, iri);
                continue;
            }
            let found = readings(&nfkc_squash(&quote));
            if [out.min, out.max]
                .into_iter()
                .flatten()
                .any(|v| !v.is_finite() || !is_reading(v, &found))
            {
                checked.drop(Reason::NumberNotInQuote, iri);
                continue;
            }
            if let (Some(min), Some(max)) = (out.min, out.max)
                && min > max
            {
                checked.drop(Reason::MinAboveMax, iri);
                continue;
            }
            if [out.min, out.max]
                .into_iter()
                .flatten()
                .any(|v| !property.accepts_number(v))
            {
                checked.drop(Reason::NotOfType, iri);
                continue;
            }
            value.low = out.min;
            value.high = out.max;
            let unit_text = out.unit.trim();
            match &property.quantity_kind {
                Some(kind) => {
                    if unit_text.is_empty() {
                        checked.drop(Reason::NoUnit, iri);
                        continue;
                    }
                    if !has_unit(text, unit_text) && !has_unit(context, unit_text) {
                        checked.drop(Reason::UnitNotInText, iri);
                        continue;
                    }
                    let unit = match units.lookup(kind, unit_text) {
                        Lookup::Found(unit) => unit,
                        Lookup::Ambiguous => {
                            checked.drop(Reason::AmbiguousUnit, unit_text);
                            continue;
                        }
                        Lookup::Unknown => {
                            checked.drop(Reason::UnknownUnit, unit_text);
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
            checked.drop(Reason::Duplicate, iri);
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

    #[test]
    fn drop_reason_texts_are_stored_unchanged() {
        let texts: Vec<&str> = [
            Reason::PropertyNotAllowed,
            Reason::RefNotAllowed,
            Reason::QuoteNotFound,
            Reason::NoNumber,
            Reason::NumberNotInQuote,
            Reason::MinAboveMax,
            Reason::NoUnit,
            Reason::UnitNotInText,
            Reason::UnknownUnit,
            Reason::AmbiguousUnit,
            Reason::NoText,
            Reason::TextNotFound,
            Reason::TextTooLong,
            Reason::Duplicate,
            Reason::NotOfType,
        ]
        .iter()
        .map(|r| r.as_str())
        .collect();
        assert_eq!(
            texts,
            [
                "property not allowed",
                "ref not allowed for the property",
                "quote not in the chunk",
                "no number",
                "number not in the quote",
                "min above max",
                "no unit",
                "unit not in the text",
                "unknown unit",
                "ambiguous unit",
                "no text value",
                "text not in the chunk",
                "text too long",
                "duplicate",
                "number does not fit the datatype"
            ]
        );
        let mut checked = Checked::default();
        checked.drop(Reason::NoUnit, "x");
        assert!(checked.drops.contains_key("no unit|x"));
    }

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
        assert_eq!(reasons(&checked), [Reason::Duplicate.as_str()]);
    }

    #[test]
    fn each_drop_reason() {
        let cases = [
            (
                value("r1", "power", Some(7.5), Some(7.5), "kW", "", "not there"),
                Reason::QuoteNotFound.as_str(),
            ),
            (
                value("r9", "power", Some(7.5), Some(7.5), "kW", "", "7,5 kW"),
                Reason::RefNotAllowed.as_str(),
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
                Reason::RefNotAllowed.as_str(),
            ),
            (
                value("r1", "power", None, None, "kW", "", "7,5 kW"),
                Reason::NoNumber.as_str(),
            ),
            (
                value("r1", "power", Some(75.0), Some(75.0), "kW", "", "7,5 kW"),
                Reason::NumberNotInQuote.as_str(),
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
                Reason::MinAboveMax.as_str(),
            ),
            (
                value("r1", "power", Some(7.5), Some(7.5), "", "", "7,5 kW"),
                Reason::NoUnit.as_str(),
            ),
            (
                value("r1", "power", Some(7.5), Some(7.5), "MW", "", "7,5 kW"),
                Reason::UnitNotInText.as_str(),
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
                Reason::UnknownUnit.as_str(),
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
                Reason::NoText.as_str(),
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
                Reason::TextNotFound.as_str(),
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
                Reason::TextTooLong.as_str(),
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
            [Reason::PropertyNotAllowed.as_str()]
        );
    }

    #[test]
    fn units_need_word_boundaries_and_fold_case() {
        let barrels = run(
            "P-101 pressure 10 barrels",
            "",
            vec![value(
                "r1",
                "pressure",
                Some(10.0),
                Some(10.0),
                "bar",
                "",
                "pressure 10 barrels",
            )],
        );
        assert_eq!(reasons(&barrels), [Reason::UnitNotInText.as_str()]);
        let folded = run(
            "P-101 power 7,5 kw",
            "",
            vec![value(
                "r1",
                "power",
                Some(7.5),
                Some(7.5),
                "KW",
                "",
                "7,5 kw",
            )],
        );
        assert_eq!(folded.values.len(), 1, "{:?}", folded.drops);
    }

    #[test]
    fn text_values_come_from_the_quote() {
        let checked = run(
            CHUNK,
            "",
            vec![value(
                "r1",
                "material",
                None,
                None,
                "",
                "cast iron GG25",
                "3 stages",
            )],
        );
        assert_eq!(reasons(&checked), [Reason::TextNotFound.as_str()]);
    }

    #[test]
    fn integer_ranges_reject_fractions_and_signs() {
        let chunk = "P-101 has 3,5 stages or -2 stages";
        for (number, evidence) in [(3.5, "3,5 stages"), (-2.0, "-2 stages")] {
            let checked = run(
                chunk,
                "",
                vec![value(
                    "r1",
                    "stages",
                    Some(number),
                    Some(number),
                    "",
                    "",
                    evidence,
                )],
            );
            assert_eq!(reasons(&checked), [Reason::NotOfType.as_str()], "{number}");
        }
    }

    #[test]
    fn quotes_are_read_after_nfkc() {
        let checked = run(
            "P-101 power \u{ff17},\u{ff15} kW",
            "",
            vec![value(
                "r1",
                "power",
                Some(7.5),
                Some(7.5),
                "kW",
                "",
                "\u{ff17},\u{ff15} kW",
            )],
        );
        assert_eq!(checked.values.len(), 1, "{:?}", checked.drops);
    }

    #[test]
    fn property_iris_are_case_sensitive() {
        let mut wrong = value("r1", "power", Some(7.5), Some(7.5), "kW", "", "7,5 kW");
        wrong["property"] = serde_json::json!("http://example.org/ex#Power");
        assert_eq!(
            reasons(&run(CHUNK, "", vec![wrong])),
            [Reason::PropertyNotAllowed.as_str()]
        );
    }

    #[test]
    fn ambiguous_case_is_dropped() {
        let checked = run(
            "P-101 power 5 mw",
            "",
            vec![value("r1", "power", Some(5.0), Some(5.0), "mw", "", "5 mw")],
        );
        assert_eq!(reasons(&checked), [Reason::AmbiguousUnit.as_str()]);
    }
}
