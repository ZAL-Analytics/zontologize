use std::path::Path;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use xberg::{
    ConcurrencyConfig, ExtractInput, ExtractionConfig, ExtractionMethod, OcrConfig, OcrElement,
    OcrElementConfig, OutputFormat, PaddleOcrConfig, PageConfig, PageContent,
};

use crate::config::{self, ModelTier, OcrBackend, TextFormat};

pub const STAGE_VERSION: u32 = 1;
const XBERG_VERSION: &str = "1.3.10";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Method {
    Layer,
    Ocr,
}

impl Method {
    pub fn as_str(self) -> &'static str {
        match self {
            Method::Layer => "layer",
            Method::Ocr => "ocr",
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Page {
    pub page_no: u32,
    pub text: String,
    pub method: Method,
    pub ocr_confidence: Option<f64>,
}

#[derive(Debug, Clone)]
pub struct ExtractSettings {
    xberg: ExtractionConfig,
    timeout: Duration,
    pub key: String,
}

impl ExtractSettings {
    pub fn new(extract: &config::ExtractConfig, ocr: &config::OcrConfig) -> Self {
        let paddle_ocr_settings = match ocr.backend {
            OcrBackend::Paddleocr => Some(PaddleOcrConfig {
                model_tier: tier_name(ocr.model_tier).to_string(),
                ..PaddleOcrConfig::default()
            }),
            OcrBackend::Tesseract => None,
        };
        let xberg = ExtractionConfig {
            use_cache: false,
            ocr: Some(OcrConfig {
                backend: backend_name(ocr.backend).to_string(),
                language: ocr.languages.clone(),
                paddle_ocr_settings,
                element_config: Some(OcrElementConfig {
                    include_elements: true,
                    ..OcrElementConfig::default()
                }),
                ..OcrConfig::default()
            }),
            pages: Some(PageConfig {
                extract_pages: true,
                ..PageConfig::default()
            }),
            concurrency: Some(ConcurrencyConfig {
                max_threads: Some(extract.threads),
                ..ConcurrencyConfig::default()
            }),
            output_format: match extract.format {
                TextFormat::Markdown => OutputFormat::Markdown,
                TextFormat::Plain => OutputFormat::Plain,
            },
            ..ExtractionConfig::default()
        };
        ExtractSettings {
            xberg,
            timeout: Duration::from_secs(extract.timeout_secs),
            key: extract_key(extract, ocr),
        }
    }
}

pub fn extract_key(extract: &config::ExtractConfig, ocr: &config::OcrConfig) -> String {
    format!(
        "extract-v{STAGE_VERSION} xberg-{XBERG_VERSION} {} {} {} {}",
        backend_name(ocr.backend),
        tier_name(ocr.model_tier),
        ocr.languages.join("+"),
        extract.format.as_str()
    )
}

pub fn backend_name(backend: OcrBackend) -> &'static str {
    match backend {
        OcrBackend::Paddleocr => "paddleocr",
        OcrBackend::Tesseract => "tesseract",
    }
}

pub fn tier_name(tier: ModelTier) -> &'static str {
    match tier {
        ModelTier::Small => "small",
        ModelTier::Medium => "medium",
    }
}

pub async fn extract_file(path: &Path, settings: &ExtractSettings) -> Result<Vec<Page>> {
    let input = ExtractInput::from_uri(path.to_string_lossy().into_owned());
    let result = tokio::time::timeout(settings.timeout, xberg::extract(input, &settings.xberg))
        .await
        .with_context(|| {
            format!(
                "extraction timed out after {:?}: {}",
                settings.timeout,
                path.display()
            )
        })?
        .with_context(|| format!("extraction failed: {}", path.display()))?;
    if result.results.is_empty() {
        let reasons: Vec<String> = result.errors.iter().map(|e| format!("{e:?}")).collect();
        bail!(
            "no text extracted from {}: {}",
            path.display(),
            reasons.join("; ")
        );
    }
    let mut pages = Vec::new();
    for document in result.results {
        let offset = pages.len() as u32;
        let document_pages = document_pages(
            document.pages,
            document.content,
            document.extraction_method,
            document.ocr_elements.unwrap_or_default(),
        );
        pages.extend(document_pages.into_iter().map(|mut page| {
            page.page_no += offset;
            page
        }));
    }
    Ok(pages)
}

pub fn is_timeout(error: &anyhow::Error) -> bool {
    error
        .downcast_ref::<tokio::time::error::Elapsed>()
        .is_some()
}

fn document_pages(
    pages: Option<Vec<PageContent>>,
    content: String,
    extraction_method: Option<ExtractionMethod>,
    elements: Vec<OcrElement>,
) -> Vec<Page> {
    match pages {
        Some(pages) if !pages.is_empty() => pages
            .into_iter()
            .map(|page| to_page(page, &elements))
            .collect(),
        _ => {
            let method = match extraction_method {
                Some(ExtractionMethod::Ocr) => Method::Ocr,
                _ => Method::Layer,
            };
            let ocr_confidence = match method {
                Method::Ocr => mean_recognition(&elements, None),
                Method::Layer => None,
            };
            vec![Page {
                page_no: 1,
                text: content,
                method,
                ocr_confidence,
            }]
        }
    }
}

fn to_page(page: PageContent, elements: &[OcrElement]) -> Page {
    let (method, ocr_confidence) = match &page.ocr_confidence {
        None => (Method::Layer, None),
        Some(confidence) => (
            Method::Ocr,
            confidence
                .score
                .or_else(|| mean_recognition(elements, Some(page.page_number))),
        ),
    };
    Page {
        page_no: page.page_number,
        text: page.content,
        method,
        ocr_confidence,
    }
}

fn mean_recognition(elements: &[OcrElement], page_no: Option<u32>) -> Option<f64> {
    let scores: Vec<f64> = elements
        .iter()
        .filter(|element| page_no.is_none_or(|n| element.page_number == n))
        .map(|element| element.confidence.recognition)
        .collect();
    if scores.is_empty() {
        return None;
    }
    Some(scores.iter().sum::<f64>() / scores.len() as f64)
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use xberg::{OcrBoundingGeometry, OcrConfidence, OcrElementLevel, PageOcrConfidence};

    use super::*;

    fn page_content(page_number: u32, ocr: Option<PageOcrConfidence>) -> PageContent {
        PageContent {
            page_number,
            content: format!("page {page_number}"),
            tables: Vec::new(),
            image_indices: Vec::new(),
            image_preprocessing: None,
            hierarchy: None,
            is_blank: None,
            layout_regions: None,
            speaker_notes: None,
            section_name: None,
            sheet_name: None,
            ocr_confidence: ocr,
            native_content: None,
        }
    }

    fn element(page_number: u32, recognition: f64) -> OcrElement {
        OcrElement {
            text: "word".to_string(),
            geometry: OcrBoundingGeometry::Rectangle {
                left: 0,
                top: 0,
                width: 1,
                height: 1,
            },
            confidence: OcrConfidence {
                detection: None,
                recognition,
            },
            level: OcrElementLevel::Line,
            rotation: None,
            page_number,
            parent_id: None,
            backend_metadata: HashMap::new(),
        }
    }

    fn ocr_without_score() -> Option<PageOcrConfidence> {
        Some(PageOcrConfidence {
            score: None,
            word_count: 0,
            backend: "paddle-ocr".to_string(),
        })
    }

    #[test]
    fn extract_key_changes_with_ocr_settings() {
        let config = config::load(None).unwrap();
        let base = extract_key(&config.extract, &config.ocr);
        let mut ocr = config.ocr.clone();
        ocr.model_tier = ModelTier::Medium;
        assert_ne!(base, extract_key(&config.extract, &ocr));
        let mut ocr = config.ocr.clone();
        ocr.languages.push("nl".to_string());
        assert_ne!(base, extract_key(&config.extract, &ocr));
        let mut extract = config.extract.clone();
        extract.format = TextFormat::Plain;
        assert_ne!(base, extract_key(&extract, &config.ocr));
        let mut extract = config.extract.clone();
        extract.threads += 1;
        assert_eq!(base, extract_key(&extract, &config.ocr));
    }

    #[test]
    fn page_without_ocr_is_layer() {
        let page = to_page(page_content(1, None), &[element(1, 0.9)]);
        assert_eq!(page.method, Method::Layer);
        assert_eq!(page.ocr_confidence, None);
    }

    #[test]
    fn ocr_page_takes_mean_of_its_own_elements() {
        let elements = [element(1, 0.5), element(2, 0.8), element(2, 0.6)];
        let page = to_page(page_content(2, ocr_without_score()), &elements);
        assert_eq!(page.method, Method::Ocr);
        let confidence = page.ocr_confidence.unwrap();
        assert!((confidence - 0.7).abs() < 1e-9, "{confidence}");
    }

    #[test]
    fn backend_score_wins_over_elements() {
        let ocr = Some(PageOcrConfidence {
            score: Some(0.42),
            word_count: 10,
            backend: "tesseract".to_string(),
        });
        let page = to_page(page_content(1, ocr), &[element(1, 0.9)]);
        assert_eq!(page.ocr_confidence, Some(0.42));
    }

    #[test]
    fn ocr_page_without_elements_has_no_confidence() {
        let page = to_page(page_content(1, ocr_without_score()), &[]);
        assert_eq!(page.method, Method::Ocr);
        assert_eq!(page.ocr_confidence, None);
    }

    #[test]
    fn document_without_pages_becomes_one_page() {
        let pages = document_pages(
            None,
            "mail body".to_string(),
            Some(ExtractionMethod::Native),
            Vec::new(),
        );
        assert_eq!(
            pages,
            [Page {
                page_no: 1,
                text: "mail body".to_string(),
                method: Method::Layer,
                ocr_confidence: None,
            }]
        );
    }

    #[test]
    fn ocr_document_without_pages_uses_all_elements() {
        let pages = document_pages(
            Some(Vec::new()),
            "scan".to_string(),
            Some(ExtractionMethod::Ocr),
            vec![element(1, 0.4), element(1, 0.6)],
        );
        assert_eq!(pages[0].method, Method::Ocr);
        assert!((pages[0].ocr_confidence.unwrap() - 0.5).abs() < 1e-9);
    }
}
