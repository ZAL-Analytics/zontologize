use std::path::PathBuf;

use zontologize::config;
use zontologize::scan::extract::{ExtractSettings, Method, Page, Word, extract_pages};

fn fixture(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/extract")
        .join(name)
}

fn settings() -> ExtractSettings {
    let config = config::load(None).unwrap();
    ExtractSettings::new(&config.extract, &config.ocr, config.gpu).unwrap()
}

fn normalize(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

#[tokio::test]
async fn txt_is_one_layer_page() {
    let pages = extract_pages(&fixture("note.txt"), &settings())
        .await
        .unwrap();
    assert_eq!(pages.len(), 1);
    assert_eq!(pages[0].page_no, 1);
    assert_eq!(pages[0].method, Method::Layer);
    assert!(
        pages[0].text.contains("Werkbon P-2021-0042"),
        "{}",
        pages[0].text
    );
}

#[tokio::test]
async fn docx_text_is_extracted() {
    let pages = extract_pages(&fixture("letter.docx"), &settings())
        .await
        .unwrap();
    assert_eq!(pages[0].page_no, 1);
    assert_eq!(pages[0].method, Method::Layer);
    assert!(pages[0].text.contains("PO-41237"), "{}", pages[0].text);
}

#[tokio::test]
async fn text_pdf_uses_the_layer() {
    let pages = extract_pages(&fixture("report.pdf"), &settings())
        .await
        .unwrap();
    assert_eq!(pages.len(), 1);
    assert_eq!(pages[0].page_no, 1);
    assert_eq!(pages[0].method, Method::Layer);
    assert_eq!(pages[0].ocr_confidence, None);
    assert!(
        normalize(&pages[0].text).contains("inspected on 12-03-2019 by Daan Rekzil"),
        "{}",
        pages[0].text
    );
}

#[tokio::test]
async fn missing_file_names_the_path() {
    let error = extract_pages(&fixture("missing.pdf"), &settings())
        .await
        .unwrap_err();
    assert!(format!("{error:#}").contains("missing.pdf"), "{error:#}");
}

#[tokio::test]
#[ignore]
async fn scan_pdf_goes_through_ocr() {
    let pages = extract_pages(&fixture("scan.pdf"), &settings())
        .await
        .unwrap();
    assert_eq!(pages.len(), 1);
    assert_eq!(pages[0].method, Method::Ocr);
    let confidence = pages[0].ocr_confidence.unwrap();
    assert!((0.5..=1.0).contains(&confidence), "{confidence}");
    let text = normalize(&pages[0].text);
    assert!(text.contains("P-2022-0158"), "{text}");
    assert!(text.contains("Grumoren"), "{text}");
}

fn assert_words(page: &Page, least_share: f64) {
    let chars: Vec<char> = page.text.chars().collect();
    let width = page.width.unwrap();
    let height = page.height.unwrap();
    assert!(width > 0.0 && height > 0.0, "{width} {height}");
    assert!(!page.words.is_empty(), "{}", page.text);
    for Word(start, end, x0, y0, x1, y1) in &page.words {
        let slice: String = chars[*start as usize..*end as usize].iter().collect();
        assert!(!slice.trim().is_empty(), "{start}..{end}");
        assert!((0.0..=1.0).contains(x0) && (0.0..=1.0).contains(y1));
        assert!(x0 < x1 && y0 < y1, "{x0} {y0} {x1} {y1}");
    }
    let found: usize = page.words.iter().map(|w| (w.1 - w.0) as usize).sum();
    let letters = page.text.chars().filter(|c| !c.is_whitespace()).count();
    let share = found as f64 / letters as f64;
    assert!(share >= least_share, "{share}");
}

#[tokio::test]
async fn text_pdf_has_word_boxes() {
    let pages = extract_pages(&fixture("report.pdf"), &settings())
        .await
        .unwrap();
    assert_words(&pages[0], 0.9);
    assert!((pages[0].height.unwrap() - 841.89).abs() < 0.01);
    let first = pages[0].words[0];
    assert!(first.3 < 0.2, "{first:?}");
}

#[tokio::test]
async fn office_pages_have_no_words() {
    let pages = extract_pages(&fixture("letter.docx"), &settings())
        .await
        .unwrap();
    assert!(pages[0].words.is_empty());
    assert_eq!(pages[0].width, None);
}

#[tokio::test]
#[ignore]
async fn scan_pdf_has_word_boxes() {
    let pages = extract_pages(&fixture("scan.pdf"), &settings())
        .await
        .unwrap();
    assert_words(&pages[0], 0.9);
}

#[tokio::test]
#[ignore]
async fn png_has_word_boxes_in_its_pixels() {
    let pages = extract_pages(&fixture("plate.png"), &settings())
        .await
        .unwrap();
    assert_eq!(pages[0].method, Method::Ocr);
    assert_words(&pages[0], 0.9);
    assert_eq!(
        (pages[0].width, pages[0].height),
        (Some(827.0), Some(400.0))
    );
}

#[tokio::test]
#[ignore]
async fn png_goes_through_ocr() {
    let pages = extract_pages(&fixture("scan.png"), &settings())
        .await
        .unwrap();
    assert_eq!(pages.len(), 1);
    assert_eq!(pages[0].method, Method::Ocr);
    let text = normalize(&pages[0].text);
    assert!(text.contains("Grumoren"), "{text}");
}
