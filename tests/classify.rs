pub mod common;

use common::{Env, folder_source};
use zontologize::classify::run;
use zontologize::llm::job::{Options, Outcome};

const ORDER: &str = "Work order 17 for the north site. Replace the seal of the booster pump and \
    test the valve afterwards.";
const INSPECTION: &str = "Inspection report of the cooling unit. All parts were checked and no \
    leaks were found during the visit.";

fn options() -> Options {
    let mut options = Options::new(common::repo(
        "tests/fixtures/llm/claude-code/fake-claude-extract.sh",
    ));
    options.backoff = std::time::Duration::from_millis(1);
    options.no_wait = true;
    options
}

async fn classify(env: &Env) -> anyhow::Result<Outcome> {
    run(Some(&env.config()), Some(&common::test_url()), &options()).await
}

async fn doc(env: &Env, path: &str) -> (Option<String>, Option<String>, String, String) {
    sqlx::query_as(
        "select d.doc_type, d.type_method, e.class_iri, e.method from documents d \
         join entities e on e.document_id = d.id and e.key = d.id::text where d.path = $1",
    )
    .bind(path)
    .fetch_one(&env.pool)
    .await
    .unwrap()
}

#[tokio::test]
#[ignore]
async fn classify_types_only_what_rules_left_open() {
    let env = Env::new("classify_gaps").await;
    env.file("misc/order.txt", ORDER);
    env.file("Projects/x/03 Reports/order.txt", ORDER);
    env.file("misc/inspection.txt", INSPECTION);
    env.copy_acme_rules();
    env.write_config(&format!(
        "[models]\nclassify = \"claude-code:haiku\"\n\
         [providers.claude-code]\nconcurrency = 1\n{}",
        folder_source("docs", &env.data(), "current")
    ));
    env.scan().await;
    assert_eq!(classify(&env).await.unwrap(), Outcome::Finished);
    assert_eq!(env.count("select count(*) from llm_calls").await, 1);
    let (doc_type, method, class, entity_method) = doc(&env, "misc/order.txt").await;
    assert_eq!(doc_type.as_deref(), Some("work_order"));
    assert_eq!(method.as_deref(), Some("llm:claude-code:haiku"));
    assert_eq!(class, "https://example.org/acme#WorkOrder");
    assert_eq!(entity_method, "llm:claude-code:haiku");
    let (doc_type, method, _, entity_method) = doc(&env, "Projects/x/03 Reports/order.txt").await;
    assert_eq!(doc_type.as_deref(), Some("report"));
    assert_eq!(method.as_deref(), Some("path"));
    assert_eq!(entity_method, "path");
    assert_eq!(
        env.count("select count(*) from segments where doc_type = 'inspection_report'")
            .await,
        1
    );
    assert_eq!(classify(&env).await.unwrap(), Outcome::Finished);
    assert_eq!(env.count("select count(*) from llm_calls").await, 1);
    env.scan().await;
    let (doc_type, ..) = doc(&env, "misc/order.txt").await;
    assert_eq!(doc_type.as_deref(), Some("work_order"));
    env.finish().await;
}
