pub mod common;

use common::{Env, folder_source};
use zontologize::attributes::run::{Options, Outcome, run};

const SHEET: &str = "Data sheet for pump P-101 at the north site. Rated power 7,5 kW at 1450 rpm. \
    Discharge pressure 8 - 10 barg. Casing material: cast iron. Delivered with the base frame.";
const NOTES: &str = "Meeting notes about the yearly budget. The team talked about holidays, the \
    canteen menu and the new parking rules for visitors, with 12 kW of new lights.";
const RULES: &str = "[[id_patterns]]\nclass = \"acme:Pump\"\npattern = '\\bP-\\d{3}\\b'\n\
    key_case = \"upper\"\non = \"text\"\n";

fn options() -> Options {
    let mut options = Options::new(common::repo(
        "tests/fixtures/llm/claude-code/fake-claude-extract.sh",
    ));
    options.backoff = std::time::Duration::from_millis(1);
    options.no_wait = true;
    options
}

async fn setup(name: &str) -> Env {
    let env = Env::new(name).await;
    env.file("site/sheet.txt", SHEET);
    env.file("office/notes.txt", NOTES);
    env.write_rules(RULES);
    let out = env.dir.join("attributes");
    env.write_config(&format!(
        "[models]\nattributes = \"claude-code:haiku\"\n\
         [providers.claude-code]\nconcurrency = 1\n\
         [llm_extract]\nmin_chars = 50\n\
         [attributes]\nout = \"{}\"\n{}",
        out.display(),
        folder_source("docs", &env.data(), "current")
    ));
    env.scan().await;
    env
}

async fn attributes(env: &Env, options: &Options) -> anyhow::Result<Outcome> {
    run(Some(&env.config()), Some(&common::test_url()), options).await
}

#[tokio::test]
#[ignore]
async fn attributes_are_written_with_si_bounds_and_a_rerun_sends_nothing() {
    let env = setup("attributes_end").await;
    let mut dry = options();
    dry.dry_run = true;
    assert_eq!(attributes(&env, &dry).await.unwrap(), Outcome::DryRun);
    assert_eq!(env.count("select count(*) from llm_calls").await, 0);

    assert_eq!(
        attributes(&env, &options()).await.unwrap(),
        Outcome::Finished
    );
    assert_eq!(env.count("select count(*) from llm_calls").await, 1);
    assert_eq!(env.count("select count(*) from attributes").await, 4);
    assert_eq!(
        env.count(
            "select count(*) from attributes a join entities e on e.id = a.entity_id \
             where e.key = 'P-101' and a.property_iri = 'https://example.org/acme#ratedPower' \
             and a.si_low = 7500 and a.si_high = 7500 and a.unit_iri = \
             'http://qudt.org/vocab/unit/KiloW' and a.method = 'llm:claude-code:haiku'"
        )
        .await,
        1
    );
    assert_eq!(
        env.count(
            "select count(*) from attributes where unit_iri = 'https://example.org/acme#BAR_G' \
             and round(si_low) = 901325 and round(si_high) = 1101325"
        )
        .await,
        1
    );
    assert_eq!(
        env.count("select count(*) from attributes where value_text = 'cast iron' and low is null")
            .await,
        1
    );
    assert_eq!(
        env.count(
            "select count(*) from attributes a join chunks c on c.id = a.chunk_id \
             where a.span_start < 0 or a.span_end <= a.span_start or a.span_end > length(c.text)"
        )
        .await,
        0
    );
    let report = std::fs::read_to_string(env.dir.join("attributes/attributes-report.md")).unwrap();
    assert!(
        report.contains("| number not in the quote | 1 |"),
        "{report}"
    );

    assert_eq!(
        attributes(&env, &options()).await.unwrap(),
        Outcome::Finished
    );
    assert_eq!(env.count("select count(*) from llm_calls").await, 1);
    sqlx::query("update contents set rules_key = null")
        .execute(&env.pool)
        .await
        .unwrap();
    env.scan().await;
    assert_eq!(env.count("select count(*) from attributes").await, 4);

    env.file(
        "site/sheet.txt",
        &SHEET.replace("base frame", "base frame and coupling"),
    );
    env.scan().await;
    assert_eq!(env.count("select count(*) from attributes").await, 0);
    assert_eq!(
        attributes(&env, &options()).await.unwrap(),
        Outcome::Finished
    );
    assert_eq!(env.count("select count(*) from llm_calls").await, 2);
    assert_eq!(env.count("select count(*) from attributes").await, 4);
    env.finish().await;
}

#[tokio::test]
#[ignore]
async fn extracted_sends_only_chunks_with_an_extract_answer() {
    let env = setup("attributes_extracted").await;
    let mut extracted = options();
    extracted.extracted = true;
    extracted.dry_run = true;
    assert_eq!(attributes(&env, &extracted).await.unwrap(), Outcome::DryRun);
    extracted.dry_run = false;
    assert_eq!(
        attributes(&env, &extracted).await.unwrap(),
        Outcome::Finished
    );
    assert_eq!(env.count("select count(*) from llm_calls").await, 0);
    sqlx::query("update chunks set llm_key = 'k' where text like 'Data sheet%'")
        .execute(&env.pool)
        .await
        .unwrap();
    assert_eq!(
        attributes(&env, &extracted).await.unwrap(),
        Outcome::Finished
    );
    assert_eq!(env.count("select count(*) from llm_calls").await, 1);
    env.finish().await;
}

#[tokio::test]
#[ignore]
async fn attributes_are_reviewed_and_reviews_set_their_confidence() {
    let env = setup("attributes_review").await;
    assert_eq!(
        attributes(&env, &options()).await.unwrap(),
        Outcome::Finished
    );
    let options = zontologize::review::Options {
        n: Some(10),
        method: "llm:".to_string(),
        class: "acme:ratedPower".to_string(),
        seed: "seed".to_string(),
    };
    let mut input: &[u8] = b"y\n";
    let tally = zontologize::review::sample(
        Some(&env.config()),
        Some(&common::test_url()),
        &options,
        &mut input,
    )
    .await
    .unwrap();
    assert_eq!((tally.right, tally.wrong), (1, 0));
    assert_eq!(
        env.count(
            "select count(*) from fact_reviews where kind = 'attribute' \
             and property_iri = 'https://example.org/acme#ratedPower'"
        )
        .await,
        1
    );
    sqlx::query(
        "insert into fact_reviews (kind, fact_id, class_iri, property_iri, method, verdict, \
         snapshot) select 'attribute', -g, 'https://example.org/acme#Pump', \
         'https://example.org/acme#ratedPower', 'llm:claude-code:haiku', 'right', 'x' \
         from generate_series(1, 29) g",
    )
    .execute(&env.pool)
    .await
    .unwrap();
    env.file(
        "site/sheet.txt",
        &SHEET.replace("base frame", "base frame and coupling"),
    );
    env.scan().await;
    assert_eq!(
        attributes(&env, &self::options()).await.unwrap(),
        Outcome::Finished
    );
    let wilson = zontologize::review::wilson_lower(30, 30);
    assert_eq!(
        env.count(&format!(
            "select count(*) from attributes where property_iri = \
             'https://example.org/acme#ratedPower' and abs(confidence - {wilson}) < 1e-9"
        ))
        .await,
        1
    );
    assert_eq!(
        env.count("select count(*) from attributes where abs(confidence - 0.7) < 1e-9")
            .await,
        3
    );
    env.finish().await;
}

#[tokio::test]
#[ignore]
async fn the_view_hides_deleted_files_and_marks_replaced_documents() {
    let env = setup("attributes_view").await;
    assert_eq!(
        attributes(&env, &options()).await.unwrap(),
        Outcome::Finished
    );
    let api = format!("\"{}_api\"", env.schema);
    let view = format!("select count(*) from {api}.attributes");
    assert_eq!(env.count(&view).await, 4);
    assert_eq!(env.count(&format!("{view} where superseded")).await, 0);
    sqlx::query(
        "insert into relations (src_id, property_iri, dst_id, method, status, confidence, \
         document_id) select n.id, 'http://purl.org/dc/terms/replaces', o.id, 'resolve:revisions', \
         'auto', 1, n.document_id from entities n, entities o, documents dn, documents dold \
         where n.document_id = dn.id and n.key = dn.id::text and dn.path like 'office/%' \
         and o.document_id = dold.id and o.key = dold.id::text and dold.path like 'site/%'",
    )
    .execute(&env.pool)
    .await
    .unwrap();
    assert_eq!(env.count(&format!("{view} where superseded")).await, 4);
    assert_eq!(
        env.count(&format!(
            "select count(*) from {api}.units where iri = 'http://qudt.org/vocab/unit/KiloW'"
        ))
        .await,
        1
    );
    sqlx::query("update documents set status = 'deleted' where path like 'site/%'")
        .execute(&env.pool)
        .await
        .unwrap();
    assert_eq!(env.count(&view).await, 0);
    env.finish().await;
}

async fn add_pump(env: &Env, key: &str, values: &[(&str, f64)]) -> i64 {
    let id: i64 = sqlx::query_scalar(
        "insert into entities (class_iri, key, name, method, confidence, ontology_version_id) \
         select 'https://example.org/acme#Pump', $1, $1, 'regex', 1, max(id) \
         from ontology_versions returning id",
    )
    .bind(key)
    .fetch_one(&env.pool)
    .await
    .unwrap();
    sqlx::query(
        "insert into mentions (entity_id, chunk_id, span_start, span_end, method, confidence) \
         select $1, min(id), 0, 1, 'llm:x', 0.7 from chunks",
    )
    .bind(id)
    .execute(&env.pool)
    .await
    .unwrap();
    for (property, si) in values {
        sqlx::query(
            "insert into attributes (entity_id, property_iri, value_text, low, high, si_low, \
             si_high, chunk_id, span_start, span_end, method, confidence) \
             select $1, 'https://example.org/acme#' || $2, 'x', $3, $3, $3, $3, min(id), 0, 1, \
             'llm:x', 0.7 from chunks",
        )
        .bind(id)
        .bind(property)
        .bind(si)
        .execute(&env.pool)
        .await
        .unwrap();
    }
    id
}

#[tokio::test]
#[ignore]
async fn resolve_proposes_similar_equipment_and_keeps_rejections() {
    let env = setup("attributes_similar").await;
    let a = add_pump(
        &env,
        "P-201",
        &[("ratedPower", 7500.0), ("dischargePressure", 1.0e6)],
    )
    .await;
    let b = add_pump(
        &env,
        "P-202",
        &[("ratedPower", 7600.0), ("dischargePressure", 1.0e6)],
    )
    .await;
    let far = add_pump(
        &env,
        "P-203",
        &[("ratedPower", 90000.0), ("dischargePressure", 4.0e6)],
    )
    .await;
    let config = env.config();
    let url = common::test_url();
    let resolve = || zontologize::resolve::run(Some(&config), Some(&url));
    resolve().await.unwrap();
    let links = "select count(*) from relations where property_iri = 'urn:zontologize#similarTo' \
         and method = 'resolve:similar' and status = 'proposed'";
    assert_eq!(env.count(links).await, 1);
    assert_eq!(
        env.count(&format!(
            "select count(*) from relations where src_id = {a} and dst_id = {b} \
             and confidence > 0.8 and props -> 'shared' is not null and chunk_id is not null"
        ))
        .await,
        1
    );
    assert_eq!(
        env.count(&format!(
            "select count(*) from relations where {far} in (src_id, dst_id) \
             and method = 'resolve:similar'"
        ))
        .await,
        0
    );
    let again = resolve().await.unwrap();
    assert_eq!(again.added.get("resolve:similar").copied().unwrap_or(0), 0);
    sqlx::query("update relations set status = 'rejected' where method = 'resolve:similar'")
        .execute(&env.pool)
        .await
        .unwrap();
    resolve().await.unwrap();
    assert_eq!(env.count(links).await, 0);
    sqlx::query("update relations set status = 'proposed' where method = 'resolve:similar'")
        .execute(&env.pool)
        .await
        .unwrap();
    sqlx::query("update contents set rules_key = null")
        .execute(&env.pool)
        .await
        .unwrap();
    env.scan().await;
    assert_eq!(env.count(links).await, 1);
    let graph = zontologize::graph::sync(Some(&env.config()), Some(&common::test_url()))
        .await
        .unwrap();
    assert!(graph.edges >= 1);
    assert_eq!(
        env.count("select count(*) from graph_labels where kind = 'edge' and label = 'similarTo'")
            .await,
        1
    );
    env.finish().await;
}
