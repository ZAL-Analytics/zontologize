use std::path::PathBuf;

fn dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("examples/test-data")
}

#[test]
fn the_test_data_config_loads_with_people() {
    let config = zontologize::config::load(Some(&dir())).unwrap();
    assert_eq!(config.database.schema, "zontologize_test");
    assert_eq!(config.sources.len(), 1);
    let loaded = zontologize::ontology::load(Some(&dir()), &config.ontology).unwrap();
    assert!(!loaded.has_errors(), "{:?}", loaded.findings);
    let person = loaded.ontology.class("acme:Person").unwrap();
    assert_eq!(person.iri, "https://example.org/acme#Person");
    let rules = zontologize::rules::load(
        Some(&dir()),
        &loaded.ontology,
        &config.ontology.document_class,
    )
    .unwrap();
    assert!(rules.persons.is_some());
}
