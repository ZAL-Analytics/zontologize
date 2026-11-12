use std::path::Path;

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use xberg::{
    AccelerationConfig, ConcurrencyConfig, ExecutionProviderType, ExtractInput, ExtractionConfig,
    ExtractionMethod, OcrBoundingGeometry, OcrConfig, OcrElement, OcrElementConfig,
    OcrElementLevel, OutputFormat, PaddleOcrConfig, PageConfig, PageContent,
};
use xberg_native_pdf::PdfDocument;

use crate::config::{self, GpuConfig, GpuProvider, ModelTier, TextFormat};

pub const STAGE_VERSION: u32 = 2;
const XBERG_VERSION: &str = "1.3.10";
pub const OCR_BACKEND: &str = "paddleocr";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
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

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Page {
    pub page_no: u32,
    pub text: String,
    pub method: Method,
    pub ocr_confidence: Option<f64>,
    pub width: Option<f32>,
    pub height: Option<f32>,
    pub words: Vec<Word>,
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Word(pub u32, pub u32, pub f32, pub f32, pub f32, pub f32);

#[derive(Debug, Clone)]
pub struct ExtractSettings {
    xberg: ExtractionConfig,
    visual: ExtractionConfig,
    pub key: String,
}

fn acceleration(gpu: GpuConfig) -> AccelerationConfig {
    AccelerationConfig {
        provider: match gpu.provider {
            GpuProvider::Cpu => ExecutionProviderType::Cpu,
            GpuProvider::Cuda => ExecutionProviderType::Cuda,
        },
        device_id: gpu.device,
    }
}

impl ExtractSettings {
    pub fn new(
        extract: &config::ExtractConfig,
        ocr: &config::OcrConfig,
        gpu: GpuConfig,
    ) -> Result<Self> {
        gpu.usable()?;
        let paddle_ocr_settings = Some(PaddleOcrConfig {
            model_tier: tier_name(ocr.model_tier).to_string(),
            ..PaddleOcrConfig::default()
        });
        let xberg = ExtractionConfig {
            use_cache: false,
            ocr: Some(OcrConfig {
                backend: OCR_BACKEND.to_string(),
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
            max_archive_depth: extract.embedded_depth,
            max_embedded_file_bytes: Some(extract.max_embedded_mb * 1024 * 1024),
            ocr_embedded_images: Some(extract.ocr_embedded_images),
            output_format: match extract.format {
                TextFormat::Markdown => OutputFormat::Markdown,
                TextFormat::Plain => OutputFormat::Plain,
            },
            acceleration: Some(acceleration(gpu)),
            ..ExtractionConfig::default()
        };
        let mut visual = xberg.clone();
        visual.output_format = OutputFormat::Plain;
        if let Some(element_config) = visual
            .ocr
            .as_mut()
            .and_then(|ocr| ocr.element_config.as_mut())
        {
            element_config.min_level = OcrElementLevel::Word;
        }
        Ok(ExtractSettings {
            xberg,
            visual,
            key: extract_key(extract, ocr),
        })
    }
}

pub fn extract_key(extract: &config::ExtractConfig, ocr: &config::OcrConfig) -> String {
    crate::hash::stage_key(
        "extract",
        STAGE_VERSION,
        &[
            &format!("xberg-{XBERG_VERSION}"),
            OCR_BACKEND,
            tier_name(ocr.model_tier),
            &ocr.languages.join("+"),
            extract.format.as_str(),
            &format!(
                "embedded-{}-{}mb-ocr-{}",
                extract.embedded_depth, extract.max_embedded_mb, extract.ocr_embedded_images
            ),
            "visual-plain-words",
        ],
    )
}

pub fn tier_name(tier: ModelTier) -> &'static str {
    match tier {
        ModelTier::Small => "small",
        ModelTier::Medium => "medium",
    }
}

pub async fn extract_pages(path: &Path, settings: &ExtractSettings) -> Result<Vec<Page>> {
    let uri = path.to_string_lossy().into_owned();
    if uri.contains("://") {
        bail!("not a file path: {uri}");
    }
    let mime = xberg::detect_mime_type(uri.clone(), true).unwrap_or_default();
    let visual = mime == PDF_MIME || mime.starts_with("image/");
    let config = if visual {
        &settings.visual
    } else {
        &settings.xberg
    };
    let input = ExtractInput::from_uri(uri);
    let result = xberg::extract(input, config)
        .await
        .with_context(|| format!("extraction failed: {}", path.display()))?;
    if result.results.is_empty() {
        let reasons: Vec<String> = result.errors.iter().map(|e| format!("{e:?}")).collect();
        bail!(
            "no text extracted from {}: {}",
            path.display(),
            reasons.join("; ")
        );
    }
    let pdf = (visual && mime == PDF_MIME)
        .then(|| std::fs::read(path).ok())
        .flatten()
        .and_then(|bytes| PdfDocument::from_bytes(bytes).ok());
    let mut pages = Vec::new();
    for (index, document) in result.results.into_iter().enumerate() {
        let offset = pages.len() as u32;
        let elements = document.ocr_elements.unwrap_or_default();
        let frames = ocr_frames(|key| document.metadata.additional.get(key));
        let mut document_pages = document_pages(
            document.pages,
            document.content,
            document.extraction_method,
            &elements,
        );
        if visual {
            let layer = pdf
                .as_ref()
                .filter(|_| index == 0 && document.mime_type == PDF_MIME);
            add_words(&mut document_pages, &elements, &frames, layer);
        }
        pages.extend(document_pages.into_iter().map(|mut page| {
            page.page_no += offset;
            page
        }));
    }
    Ok(pages)
}

const PDF_MIME: &str = "application/pdf";

#[derive(Debug, Clone, Copy, PartialEq)]
struct Frame {
    page_no: Option<u32>,
    width: f32,
    height: f32,
}

fn ocr_frames<'a>(additional: impl Fn(&str) -> Option<&'a Value>) -> Vec<Frame> {
    let listed: Vec<Frame> = additional("ocr_page_coordinate_frames")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|frame| {
            Some(Frame {
                page_no: Some(frame.get("page_number")?.as_u64()? as u32),
                width: frame.get("width")?.as_f64()? as f32,
                height: frame.get("height")?.as_f64()? as f32,
            })
        })
        .collect();
    if !listed.is_empty() {
        return listed;
    }
    let dimension = |key: &str| additional(key).and_then(Value::as_f64);
    match (
        dimension("ocr_processed_image_width"),
        dimension("ocr_processed_image_height"),
    ) {
        (Some(width), Some(height)) if width > 0.0 && height > 0.0 => vec![Frame {
            page_no: None,
            width: width as f32,
            height: height as f32,
        }],
        _ => Vec::new(),
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
struct PdfPage {
    llx: f32,
    lly: f32,
    width: f32,
    height: f32,
    rotate: i32,
}

impl PdfPage {
    fn read(pdf: &PdfDocument, index: usize) -> Option<PdfPage> {
        let (llx, lly, urx, ury) = pdf.get_page_media_box(index).ok()?;
        let page = PdfPage {
            llx,
            lly,
            width: urx - llx,
            height: ury - lly,
            rotate: pdf.get_page_rotation(index).unwrap_or(0),
        };
        (page.width > 0.0 && page.height > 0.0).then_some(page)
    }

    fn shown(&self) -> (f32, f32) {
        match self.rotate {
            90 | 270 => (self.height, self.width),
            _ => (self.width, self.height),
        }
    }

    fn place(&self, x: f32, y: f32, width: f32, height: f32) -> [f32; 4] {
        let u0 = (x - self.llx) / self.width;
        let u1 = (x + width - self.llx) / self.width;
        let v0 = 1.0 - (y + height - self.lly) / self.height;
        let v1 = 1.0 - (y - self.lly) / self.height;
        let (a, b) = (turn(u0, v0, self.rotate), turn(u1, v1, self.rotate));
        [a.0.min(b.0), a.1.min(b.1), a.0.max(b.0), a.1.max(b.1)]
    }
}

fn turn(u: f32, v: f32, rotate: i32) -> (f32, f32) {
    match rotate {
        90 => (1.0 - v, u),
        180 => (1.0 - u, 1.0 - v),
        270 => (v, 1.0 - u),
        _ => (u, v),
    }
}

fn pixel_box(geometry: &OcrBoundingGeometry) -> [f32; 4] {
    match geometry {
        OcrBoundingGeometry::Rectangle {
            left,
            top,
            width,
            height,
        } => [
            *left as f32,
            *top as f32,
            (left + width) as f32,
            (top + height) as f32,
        ],
        OcrBoundingGeometry::Quadrilateral { points } => {
            let xs = || points.iter().map(|point| point.x as f32);
            let ys = || points.iter().map(|point| point.y as f32);
            [
                xs().fold(f32::INFINITY, f32::min),
                ys().fold(f32::INFINITY, f32::min),
                xs().fold(f32::NEG_INFINITY, f32::max),
                ys().fold(f32::NEG_INFINITY, f32::max),
            ]
        }
    }
}

fn page_frame(frames: &[Frame], page_no: u32, scale: Option<(f32, f32)>) -> Option<(f32, f32)> {
    if let Some(frame) = frames.iter().find(|frame| frame.page_no == Some(page_no)) {
        return Some((frame.width, frame.height));
    }
    let frame = frames.iter().find(|frame| frame.page_no.is_none())?;
    let (x, y) = scale.unwrap_or((1.0, 1.0));
    Some((frame.width * x, frame.height * y))
}

fn add_words(
    pages: &mut [Page],
    elements: &[OcrElement],
    frames: &[Frame],
    pdf: Option<&PdfDocument>,
) {
    let geometry: Vec<Option<PdfPage>> = pages
        .iter()
        .map(|page| pdf.and_then(|pdf| PdfPage::read(pdf, page.page_no.saturating_sub(1) as usize)))
        .collect();
    let first_ocr = pages
        .iter()
        .zip(&geometry)
        .find(|(page, _)| page.method == Method::Ocr)
        .and_then(|(_, geometry)| geometry.map(|g| g.shown()));
    for (page, geometry) in pages.iter_mut().zip(geometry) {
        let shown = geometry.map(|g| g.shown());
        let found: Vec<(String, [f32; 4])> = match page.method {
            Method::Ocr => {
                let scale = shown
                    .zip(first_ocr)
                    .map(|((w, h), (w1, h1))| (w / w1, h / h1));
                let Some((width, height)) = page_frame(frames, page.page_no, scale) else {
                    continue;
                };
                let (shown_width, shown_height) = shown.unwrap_or((width, height));
                page.width = Some(shown_width);
                page.height = Some(shown_height);
                elements
                    .iter()
                    .filter(|e| e.level == OcrElementLevel::Word && e.page_number == page.page_no)
                    .map(|e| {
                        let [x0, y0, x1, y1] = pixel_box(&e.geometry);
                        (
                            e.text.clone(),
                            [x0 / width, y0 / height, x1 / width, y1 / height],
                        )
                    })
                    .collect()
            }
            Method::Layer => {
                let (Some(pdf), Some(geometry)) = (pdf, geometry) else {
                    continue;
                };
                let (shown_width, shown_height) = geometry.shown();
                page.width = Some(shown_width);
                page.height = Some(shown_height);
                pdf.extract_words(page.page_no.saturating_sub(1) as usize)
                    .unwrap_or_default()
                    .into_iter()
                    .filter(|word| word.rotation_degrees == 0.0)
                    .map(|word| {
                        let b = word.bbox;
                        (word.text, geometry.place(b.x, b.y, b.width, b.height))
                    })
                    .collect()
            }
        };
        page.words = place_words(&page.text, &found);
    }
}

fn place_words(text: &str, found: &[(String, [f32; 4])]) -> Vec<Word> {
    let mut words = Vec::new();
    let mut byte = 0;
    let mut char = 0u32;
    for (word, [x0, y0, x1, y1]) in found {
        if word.trim().is_empty() {
            continue;
        }
        let Some(at) = text[byte..].find(word.as_str()) else {
            continue;
        };
        let start = char + text[byte..byte + at].chars().count() as u32;
        let end = start + word.chars().count() as u32;
        byte += at + word.len();
        char = end;
        words.push(Word(start, end, unit(*x0), unit(*y0), unit(*x1), unit(*y1)));
    }
    words
}

fn unit(value: f32) -> f32 {
    (value.clamp(0.0, 1.0) * 10_000.0).round() / 10_000.0
}

fn document_pages(
    pages: Option<Vec<PageContent>>,
    content: String,
    extraction_method: Option<ExtractionMethod>,
    elements: &[OcrElement],
) -> Vec<Page> {
    match pages {
        Some(pages) if !pages.is_empty() => pages
            .into_iter()
            .map(|page| to_page(page, elements))
            .collect(),
        _ => {
            let method = match extraction_method {
                Some(ExtractionMethod::Ocr) => Method::Ocr,
                _ => Method::Layer,
            };
            let ocr_confidence = match method {
                Method::Ocr => mean_recognition(elements, None),
                Method::Layer => None,
            };
            vec![Page {
                page_no: 1,
                text: content,
                method,
                ocr_confidence,
                width: None,
                height: None,
                words: Vec::new(),
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
        width: None,
        height: None,
        words: Vec::new(),
    }
}

fn mean_recognition(elements: &[OcrElement], page_no: Option<u32>) -> Option<f64> {
    let scores: Vec<f64> = elements
        .iter()
        .filter(|element| element.level == OcrElementLevel::Line)
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

    #[test]
    fn xberg_version_matches_cargo_lock() {
        let lock =
            std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/Cargo.lock")).unwrap();
        for name in ["xberg", "xberg-native-pdf"] {
            let version = lock
                .split("[[package]]")
                .find(|block| block.contains(&format!("\nname = \"{name}\"\n")))
                .and_then(|block| block.lines().find_map(|l| l.strip_prefix("version = ")))
                .unwrap();
            assert_eq!(version.trim_matches('"'), XBERG_VERSION, "{name}");
        }
    }

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
        extract.embedded_depth = 1;
        assert_ne!(base, extract_key(&extract, &config.ocr));
        let mut extract = config.extract.clone();
        extract.ocr_embedded_images = true;
        assert_ne!(base, extract_key(&extract, &config.ocr));
        let mut extract = config.extract.clone();
        extract.workers += 1;
        extract.worker_memory_mb += 1;
        assert_eq!(base, extract_key(&extract, &config.ocr));
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
            backend: "paddle-ocr".to_string(),
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
            &[],
        );
        assert_eq!(
            pages,
            [Page {
                page_no: 1,
                text: "mail body".to_string(),
                method: Method::Layer,
                ocr_confidence: None,
                width: None,
                height: None,
                words: Vec::new(),
            }]
        );
    }

    #[test]
    fn ocr_document_without_pages_uses_all_elements() {
        let pages = document_pages(
            Some(Vec::new()),
            "scan".to_string(),
            Some(ExtractionMethod::Ocr),
            &[element(1, 0.4), element(1, 0.6)],
        );
        assert_eq!(pages[0].method, Method::Ocr);
        assert!((pages[0].ocr_confidence.unwrap() - 0.5).abs() < 1e-9);
    }

    fn found(words: &[&str]) -> Vec<(String, [f32; 4])> {
        words
            .iter()
            .map(|word| (word.to_string(), [0.1, 0.2, 0.3, 0.4]))
            .collect()
    }

    #[test]
    fn forward_search_gives_char_offsets() {
        let text = "Ærø pump P-101 and pump P-102";
        let words = place_words(text, &found(&["pump", "P-101", "pump", "P-102"]));
        let chars: Vec<char> = text.chars().collect();
        let slices: Vec<String> = words
            .iter()
            .map(|w| chars[w.0 as usize..w.1 as usize].iter().collect())
            .collect();
        assert_eq!(slices, ["pump", "P-101", "pump", "P-102"]);
        assert_eq!((words[0].0, words[2].0), (4, 19));
    }

    #[test]
    fn forward_search_skips_missing_words() {
        let words = place_words("valve seven", &found(&["valve", "absent", "seven"]));
        assert_eq!(words.iter().map(|w| w.0).collect::<Vec<_>>(), [0, 6]);
    }

    #[test]
    fn boxes_clamp_and_round() {
        let words = place_words("a", &[("a".to_string(), [-0.2, 0.123456, 1.3, 0.5])]);
        assert_eq!(words, [Word(0, 1, 0.0, 0.1235, 1.0, 0.5)]);
    }

    fn page(rotate: i32) -> PdfPage {
        PdfPage {
            llx: 0.0,
            lly: 0.0,
            width: 200.0,
            height: 100.0,
            rotate,
        }
    }

    fn close(a: [f32; 4], b: [f32; 4]) {
        assert!(
            a.iter().zip(&b).all(|(x, y)| (x - y).abs() < 1e-5),
            "{a:?} {b:?}"
        );
    }

    #[test]
    fn layer_box_flips_y_from_the_baseline() {
        close(page(0).place(20.0, 50.0, 40.0, 10.0), [0.1, 0.4, 0.3, 0.5]);
        let shifted = PdfPage {
            llx: 10.0,
            lly: 20.0,
            ..page(0)
        };
        close(shifted.place(30.0, 70.0, 40.0, 10.0), [0.1, 0.4, 0.3, 0.5]);
    }

    #[test]
    fn layer_box_turns_with_the_page() {
        close(page(90).place(20.0, 50.0, 40.0, 10.0), [0.5, 0.1, 0.6, 0.3]);
        close(
            page(180).place(20.0, 50.0, 40.0, 10.0),
            [0.7, 0.5, 0.9, 0.6],
        );
        close(
            page(270).place(20.0, 50.0, 40.0, 10.0),
            [0.4, 0.7, 0.5, 0.9],
        );
        assert_eq!(page(90).shown(), (100.0, 200.0));
        assert_eq!(page(180).shown(), (200.0, 100.0));
    }

    #[test]
    fn quad_becomes_its_bounding_box() {
        let quad = OcrBoundingGeometry::Quadrilateral {
            points: [(10, 5), (50, 7), (52, 20), (9, 18)]
                .into_iter()
                .map(|(x, y)| xberg::OcrPoint { x, y })
                .collect(),
        };
        assert_eq!(pixel_box(&quad), [9.0, 5.0, 52.0, 20.0]);
    }

    #[test]
    fn ocr_frame_of_a_later_page_scales_from_the_first() {
        let values: HashMap<&str, Value> = [
            ("ocr_processed_image_width", Value::from(1240)),
            ("ocr_processed_image_height", Value::from(1754)),
        ]
        .into_iter()
        .collect();
        let frames = ocr_frames(|key| values.get(key));
        assert_eq!(page_frame(&frames, 1, None), Some((1240.0, 1754.0)));
        assert_eq!(
            page_frame(&frames, 2, Some((0.5, 2.0))),
            Some((620.0, 3508.0))
        );
    }

    #[test]
    fn listed_frames_win_per_page() {
        let values: HashMap<&str, Value> = [
            ("ocr_processed_image_width", Value::from(100)),
            ("ocr_processed_image_height", Value::from(100)),
            (
                "ocr_page_coordinate_frames",
                serde_json::json!([{"page_number": 2, "width": 1240, "height": 600, "unit": "pixel"}]),
            ),
        ]
        .into_iter()
        .collect();
        let frames = ocr_frames(|key| values.get(key));
        assert_eq!(
            page_frame(&frames, 2, Some((3.0, 3.0))),
            Some((1240.0, 600.0))
        );
        assert_eq!(page_frame(&frames, 1, None), None);
    }

    #[test]
    fn ocr_words_fill_the_page() {
        let mut word = element(1, 0.9);
        word.level = OcrElementLevel::Word;
        word.text = "Valve".to_string();
        word.geometry = OcrBoundingGeometry::Rectangle {
            left: 62,
            top: 30,
            width: 124,
            height: 60,
        };
        let frames = [Frame {
            page_no: None,
            width: 620.0,
            height: 300.0,
        }];
        let mut pages = [to_page(page_content(1, ocr_without_score()), &[])];
        pages[0].text = "Valve".to_string();
        add_words(&mut pages, &[word, element(1, 0.5)], &frames, None);
        assert_eq!(pages[0].words, [Word(0, 5, 0.1, 0.1, 0.3, 0.3)]);
        assert_eq!(
            (pages[0].width, pages[0].height),
            (Some(620.0), Some(300.0))
        );
    }

    #[test]
    fn mean_recognition_reads_lines_only() {
        let mut word = element(1, 0.1);
        word.level = OcrElementLevel::Word;
        assert_eq!(
            mean_recognition(&[word, element(1, 0.7)], Some(1)),
            Some(0.7)
        );
    }

    #[test]
    fn cpu_provider_sets_cpu_acceleration() {
        let config = crate::config::load(None).unwrap();
        let settings = ExtractSettings::new(&config.extract, &config.ocr, config.gpu).unwrap();
        assert_eq!(
            settings.xberg.acceleration,
            Some(AccelerationConfig {
                provider: ExecutionProviderType::Cpu,
                device_id: 0,
            })
        );
    }

    #[cfg(feature = "gpu")]
    #[test]
    fn cuda_provider_sets_device() {
        let config = crate::config::load(None).unwrap();
        let gpu = GpuConfig {
            provider: GpuProvider::Cuda,
            device: 1,
        };
        let settings = ExtractSettings::new(&config.extract, &config.ocr, gpu).unwrap();
        assert_eq!(
            settings.xberg.acceleration,
            Some(AccelerationConfig {
                provider: ExecutionProviderType::Cuda,
                device_id: 1,
            })
        );
    }
}
