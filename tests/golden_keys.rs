pub mod common;

use std::path::PathBuf;

use common::repo;
use zontologize::attributes::{self, Matcher, Subject};
use zontologize::config::{self, Config};
use zontologize::embed;
use zontologize::facts::{self, Known};
use zontologize::llm::prompt;
use zontologize::llm::shortlist::{shortlist, shortlist_lines};
use zontologize::llm::{ModelName, Request};
use zontologize::ontology::{self, Ontology};
use zontologize::scan::chunk::ChunkSettings;
use zontologize::scan::extract::extract_key;
use zontologize::units::UnitIndex;
use zontologize::{classify, fit, rules};

const ACME: &str = "https://example.org/acme#";
const TEXT: &str = "Inspection report P-2021-0042 for the Eastport site. The booster pump runs at \
    45 kW with a discharge pressure of 6,5 bar and 1450 rpm. Material: stainless steel.";

fn acme() -> PathBuf {
    repo("examples/acme")
}

fn config() -> Config {
    config::load(Some(&acme())).unwrap()
}

fn acme_ontology(config: &Config) -> Ontology {
    let loaded = ontology::load(Some(&acme()), &config.ontology).unwrap();
    assert!(!loaded.has_errors(), "{:?}", loaded.findings);
    loaded.ontology
}

fn iri(name: &str) -> String {
    format!("{ACME}{name}")
}

fn model(name: &str) -> ModelName {
    ModelName::parse(name).unwrap()
}

#[test]
fn scan_stage_keys_are_unchanged() {
    let config = config();
    assert_eq!(
        extract_key(&config.extract, &config.ocr),
        "extract-v2 xberg-1.3.10 paddleocr small nl+en markdown embedded-0-10mb-ocr-false visual-plain-words"
    );
    assert_eq!(
        ChunkSettings::new(&config.chunk, config.extract.format).key,
        "chunk-v2 2000 200 markdown simple"
    );
    let ontology = acme_ontology(&config);
    let rules = rules::load(Some(&acme()), &ontology, &config.ontology.document_class).unwrap();
    assert_eq!(rules.key, "rules-v5-091696416c030a1a");
}

#[test]
fn ontology_version_is_unchanged() {
    let config = config();
    assert_eq!(
        acme_ontology(&config).version,
        "66188ea54f4dc65094b739db0e95b7fb338381029485bb1400f9325bbb28ec88"
    );
}

#[test]
fn embed_keys_are_unchanged() {
    let config = config();
    let model = embed::model::parse(&config.models.embed, &config.embed.models).unwrap();
    let fingerprint = model.fingerprint("rev");
    assert_eq!(
        fingerprint,
        "504dd5d55bdd8304a8faea174290e96f19b5478ed99143a73819af873e163e21"
    );
    assert_eq!(
        embed::model::text_key(&fingerprint, embed::STAGE_VERSION, TEXT),
        "a806e6cd1fc9526803cb77fba4f4a65fdeadafee88b6b0aea3826e511c6e0c7a"
    );
}

#[test]
fn request_key_is_unchanged() {
    let request = Request {
        task: "any".to_string(),
        system: "system".to_string(),
        user: "user".to_string(),
        schema: serde_json::json!({"type": "object", "properties": {"a": {"type": "string"}}}),
    };
    assert_eq!(
        request.key(&model("claude-code:sonnet")),
        "cc408bf230e85ab69a5c369c462d92947947ed97d4a278db968bbf94aa4f1bf7"
    );
    assert_eq!(
        request.key(&model("ollama:qwen3.5:4b")),
        "740492e996ecbde6d5c792e8b7e9f1c23252bddb79b3d3d7cecbc5bf4a538213"
    );
    assert_eq!(
        request.key(&model("openrouter:x/y")),
        "55cf82459f3a4ac6879577e69ab0bd6aa397f66ccac38a1803c5f89a2c2a5b91"
    );
}

#[test]
fn facts_cache_key_is_unchanged() {
    let config = config();
    let ontology = acme_ontology(&config);
    let system = prompt::load(None, "extract-system").unwrap();
    let user = prompt::load(None, "extract-user").unwrap();
    let model = model("claude-code:sonnet");
    let context = facts::Context {
        ontology: &ontology,
        languages: &config.ontology.languages,
        max_shortlist: config.llm_extract.max_shortlist,
        max_known: config.llm_extract.max_known,
        definition_chars: config.llm_extract.definition_chars,
        exclude: &[],
        check_key: "",
        system: &system,
        user: &user,
        model: &model,
    };
    let input = facts::ChunkInput {
        id: 7,
        text: TEXT.to_string(),
        path: "Projects/P-2021-0042/report.pdf".to_string(),
        pages: "1-2".to_string(),
        counts: vec![(iri("Equipment"), 3), (iri("Site"), 1)],
        known: vec![Known {
            entity_id: 11,
            class: iri("Site"),
            name: "Eastport".to_string(),
        }],
        doc_class: Some(iri("Document")),
    };
    let built = facts::build(&context, &input);
    assert_eq!(
        built.cache_key,
        built.request.stage_key(&model, facts::CACHE_VERSION)
    );
    assert_eq!(
        built.cache_key,
        "6e5cc2187b0a27434eebf8058756c91bb50fa45426d6f7cfe0999c54ceefbe99"
    );
}

#[test]
fn attributes_cache_key_is_unchanged() {
    let config = config();
    let ontology = acme_ontology(&config);
    let units = UnitIndex::new(&ontology);
    let matcher = Matcher::new(&ontology, &units).unwrap();
    let system = prompt::load(None, "attributes-system").unwrap();
    let user = prompt::load(None, "attributes-user").unwrap();
    let model = model("claude-code:sonnet");
    let check = attributes::check_key(&units, 120);
    let context = attributes::Context {
        ontology: &ontology,
        units: &units,
        languages: &config.ontology.languages,
        max_properties: 30,
        max_subjects: config.attributes.max_subjects,
        unit_symbols: config.attributes.unit_symbols,
        check_key: &check,
        system: &system,
        user: &user,
        model: &model,
    };
    let input = attributes::ChunkInput {
        id: 7,
        text: TEXT.to_string(),
        context: "Datasheet booster pump".to_string(),
        path: "Projects/P-2021-0042/sheet.pdf".to_string(),
        pages: "1".to_string(),
        known: vec![Subject {
            reference: "e1".to_string(),
            entity_id: 11,
            class: iri("Pump"),
            name: "booster pump".to_string(),
        }],
        doc: None,
    };
    let mut hits = matcher.hits(&input.text);
    hits.properties
        .extend(matcher.hits(&input.context).properties);
    let built = attributes::build(&context, &input, &hits).unwrap();
    assert_eq!(
        built.cache_key,
        built.request.stage_key(&model, attributes::CACHE_VERSION)
    );
    assert_eq!(
        built.cache_key,
        "47f1098fb396922e5425dc5199559253cc82266575539813ec9fef61312e798d"
    );
}

#[test]
fn classify_and_fit_cache_keys_are_unchanged() {
    let config = config();
    let ontology = acme_ontology(&config);
    let rules = rules::load(Some(&acme()), &ontology, &config.ontology.document_class).unwrap();
    let languages = &config.ontology.languages;
    let system = prompt::load(None, "classify-system").unwrap();
    let user = prompt::load(None, "classify-user").unwrap();
    let lines = classify::doc_type_lines(
        &rules,
        &ontology,
        languages,
        config.classify.definition_chars,
    );
    let names: Vec<String> = rules.doc_types.keys().cloned().collect();
    let request = Request {
        task: classify::TASK.to_string(),
        system: system.text.clone(),
        user: user.fill(&[
            ("doc_types", lines.trim_end()),
            ("path", "Projects/P-2021-0042/report.pdf"),
            ("pages", "1-2"),
            ("text", TEXT),
        ]),
        schema: classify::schema(&names),
    };
    assert_eq!(
        request.stage_key(&model("claude-code:haiku"), classify::STAGE_VERSION),
        "cb4990c79b5bcc83362c3af15e438fc552dbaddb4080653d81ab9740703782cf"
    );

    let system = prompt::load(None, "fit-system").unwrap();
    let user = prompt::load(None, "fit-user").unwrap();
    let list = shortlist(
        &ontology,
        &[(iri("Equipment"), 2)],
        &ontology.roots,
        &[],
        config.fit.max_shortlist,
    );
    let lines = shortlist_lines(&ontology, &list, languages, config.fit.definition_chars);
    let request = Request {
        task: "fit".to_string(),
        system: system.text.clone(),
        user: user.fill(&[
            ("path", "Projects/P-2021-0042/report.pdf"),
            ("pages", "1-2"),
            ("languages", &languages.join(", ")),
            ("shortlist", lines.trim_end()),
            ("chunk", TEXT),
        ]),
        schema: fit::schema(&list, languages),
    };
    assert_eq!(
        request.key(&model("claude-code:sonnet")),
        "ea7258b7843831b723f0b20b0d41dd0d0a6eae4e3e00f5e56b168a28a43eb2a0"
    );
}
