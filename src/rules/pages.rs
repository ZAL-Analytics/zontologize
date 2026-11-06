use std::collections::{BTreeMap, BTreeSet};

use regex::{RegexSet, RegexSetBuilder};
use serde::Deserialize;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PageTypeSpec {
    doc_type: String,
    pattern: String,
    #[serde(default)]
    starts: bool,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PagesSpec {
    pub head_chars: Option<usize>,
    pub part_property: Option<String>,
}

pub fn doc_types_of(specs: &[PageTypeSpec]) -> impl Iterator<Item = &str> {
    specs.iter().map(|s| s.doc_type.as_str())
}

struct PageType {
    doc_type: String,
    starts: bool,
}

pub struct PageTypes {
    set: RegexSet,
    types: Vec<PageType>,
    head_chars: usize,
}

pub struct PageInput<'a> {
    pub page_no: i32,
    pub text: &'a str,
    pub keys: BTreeSet<String>,
    pub ocr_confidence: Option<f64>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Segment {
    pub ord: i32,
    pub page_from: i32,
    pub page_to: i32,
    pub doc_type: Option<String>,
    pub confidence: f64,
}

impl PageTypes {
    pub fn compile(
        specs: &[PageTypeSpec],
        doc_types: &BTreeMap<String, String>,
        head_chars: usize,
    ) -> Result<PageTypes, String> {
        if !specs.is_empty() && head_chars == 0 {
            return Err(
                "pages.head_chars must be greater than 0 when page_types are set".to_string(),
            );
        }
        let mut types = Vec::new();
        for (index, spec) in specs.iter().enumerate() {
            if !doc_types.contains_key(&spec.doc_type) {
                return Err(format!(
                    "page_types[{index}].doc_type: {:?} is not a key of [doc_types]",
                    spec.doc_type
                ));
            }
            types.push(PageType {
                doc_type: spec.doc_type.clone(),
                starts: spec.starts,
            });
        }
        let set = RegexSetBuilder::new(specs.iter().map(|s| &s.pattern))
            .case_insensitive(true)
            .build()
            .map_err(|e| format!("page_types: {e}"))?;
        Ok(PageTypes {
            set,
            types,
            head_chars,
        })
    }

    pub fn is_empty(&self) -> bool {
        self.types.is_empty()
    }

    fn classify(&self, text: &str) -> Option<&PageType> {
        let end = text
            .char_indices()
            .nth(self.head_chars)
            .map_or(text.len(), |(i, _)| i);
        let first = self.set.matches(&text[..end]).iter().next()?;
        Some(&self.types[first])
    }

    pub fn segments(&self, pages: &[PageInput]) -> Vec<Segment> {
        let mut segments: Vec<Segment> = Vec::new();
        let mut current_type: Option<String> = None;
        let mut last_keys: BTreeSet<String> = BTreeSet::new();
        for page in pages {
            let found = self.classify(page.text);
            let mut start = segments.is_empty();
            if let Some(found) = found {
                if found.starts || current_type.as_deref() != Some(found.doc_type.as_str()) {
                    start = true;
                }
                current_type = Some(found.doc_type.clone());
            }
            if !page.keys.is_empty() && !last_keys.is_empty() && page.keys.is_disjoint(&last_keys) {
                start = true;
            }
            if start {
                last_keys.clear();
            }
            if !page.keys.is_empty() {
                last_keys.clone_from(&page.keys);
            }
            let confidence = page.ocr_confidence.unwrap_or(1.0);
            match segments.last_mut() {
                Some(segment) if !start => {
                    segment.page_to = page.page_no;
                    segment.confidence = segment.confidence.min(confidence);
                }
                _ => segments.push(Segment {
                    ord: segments.len() as i32,
                    page_from: page.page_no,
                    page_to: page.page_no,
                    doc_type: current_type.clone(),
                    confidence,
                }),
            }
        }
        segments
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn page_types(specs: &str) -> PageTypes {
        #[derive(Deserialize)]
        struct File {
            page_types: Vec<PageTypeSpec>,
        }
        let file: File = toml::from_str(specs).unwrap();
        let doc_types = BTreeMap::from([
            ("certificate".to_string(), "ex:Certificate".to_string()),
            ("report".to_string(), "ex:Report".to_string()),
            ("letter".to_string(), "ex:Letter".to_string()),
        ]);
        PageTypes::compile(&file.page_types, &doc_types, 40).unwrap()
    }

    const SPECS: &str = r#"
[[page_types]]
doc_type = "certificate"
pattern = 'certificate of conformity'
starts = true

[[page_types]]
doc_type = "report"
pattern = 'test report'

[[page_types]]
doc_type = "letter"
pattern = 'dear|certificate'
"#;

    fn page<'a>(page_no: i32, text: &'a str, keys: &[&str]) -> PageInput<'a> {
        PageInput {
            page_no,
            text,
            keys: keys.iter().map(|k| k.to_string()).collect(),
            ocr_confidence: None,
        }
    }

    fn ranges(segments: &[Segment]) -> Vec<(i32, i32, Option<&str>)> {
        segments
            .iter()
            .map(|s| (s.page_from, s.page_to, s.doc_type.as_deref()))
            .collect()
    }

    #[test]
    fn first_matching_rule_wins_and_case_is_ignored() {
        let types = page_types(SPECS);
        let segments = types.segments(&[page(1, "CERTIFICATE OF CONFORMITY no 1", &[])]);
        assert_eq!(ranges(&segments), [(1, 1, Some("certificate"))]);
    }

    #[test]
    fn pages_without_a_match_take_the_previous_type() {
        let types = page_types(SPECS);
        let pages = [
            page(1, "Test report for a made-up pump", &[]),
            page(2, "measurements continue", &[]),
            page(3, "more measurements", &[]),
        ];
        assert_eq!(ranges(&types.segments(&pages)), [(1, 3, Some("report"))]);
    }

    #[test]
    fn starts_splits_a_run_of_the_same_type() {
        let types = page_types(SPECS);
        let pages = [
            page(1, "Certificate of conformity A", &[]),
            page(2, "annex", &[]),
            page(3, "Certificate of conformity B", &[]),
        ];
        assert_eq!(
            ranges(&types.segments(&pages)),
            [(1, 2, Some("certificate")), (3, 3, Some("certificate"))]
        );
    }

    #[test]
    fn type_change_and_id_change_split() {
        let types = page_types(SPECS);
        let pages = [
            page(1, "Dear customer", &["P-1"]),
            page(2, "regards", &[]),
            page(3, "second page of the order", &["P-2"]),
            page(4, "Test report", &["P-2"]),
        ];
        assert_eq!(
            ranges(&types.segments(&pages)),
            [
                (1, 2, Some("letter")),
                (3, 3, Some("letter")),
                (4, 4, Some("report"))
            ]
        );
    }

    #[test]
    fn empty_id_set_does_not_split() {
        let types = page_types(SPECS);
        let pages = [
            page(1, "Dear customer", &["P-1"]),
            page(2, "no identifiers here", &[]),
            page(3, "still P-1", &["P-1"]),
        ];
        assert_eq!(ranges(&types.segments(&pages)), [(1, 3, Some("letter"))]);
    }

    #[test]
    fn head_limits_where_patterns_look() {
        let types = page_types(SPECS);
        let late = format!("{} test report", "x".repeat(60));
        let pages = [page(1, &late, &[])];
        assert_eq!(ranges(&types.segments(&pages)), [(1, 1, None)]);
    }

    #[test]
    fn segment_confidence_is_the_lowest_page_confidence() {
        let types = page_types(SPECS);
        let mut first = page(1, "Test report", &[]);
        first.ocr_confidence = Some(0.9);
        let mut second = page(2, "page two", &[]);
        second.ocr_confidence = Some(0.7);
        let segments = types.segments(&[first, second]);
        assert_eq!(segments.len(), 1);
        assert!((segments[0].confidence - 0.7).abs() < 1e-9);
    }

    #[test]
    fn unknown_doc_type_is_an_error() {
        #[derive(Deserialize)]
        struct File {
            page_types: Vec<PageTypeSpec>,
        }
        let file: File =
            toml::from_str("[[page_types]]\ndoc_type = \"nope\"\npattern = 'x'\n").unwrap();
        let error = PageTypes::compile(&file.page_types, &BTreeMap::new(), 10)
            .err()
            .unwrap();
        assert!(error.contains("page_types[0].doc_type"), "{error}");
    }
}
