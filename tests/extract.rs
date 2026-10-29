use std::path::PathBuf;

use zontologize::config;
use zontologize::scan::extract::{ExtractSettings, Method, extract_file};

fn fixture(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/extract")
        .join(name)
}

fn settings() -> ExtractSettings {
    let config = config::load(None).unwrap();
    ExtractSettings::new(&config.extract, &config.ocr)
}

fn normalize(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

#[tokio::test]
async fn txt_is_one_layer_page() {
    let pages = extract_file(&fixture("note.txt"), &settings())
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
    let pages = extract_file(&fixture("letter.docx"), &settings())
        .await
        .unwrap();
    assert_eq!(pages[0].page_no, 1);
    assert_eq!(pages[0].method, Method::Layer);
    assert!(pages[0].text.contains("PO-41237"), "{}", pages[0].text);
}

#[tokio::test]
async fn text_pdf_uses_the_layer() {
    let pages = extract_file(&fixture("report.pdf"), &settings())
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
    let error = extract_file(&fixture("missing.pdf"), &settings())
        .await
        .unwrap_err();
    assert!(format!("{error:#}").contains("missing.pdf"), "{error:#}");
}

#[tokio::test]
#[ignore]
async fn scan_pdf_goes_through_ocr() {
    let pages = extract_file(&fixture("scan.pdf"), &settings())
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
