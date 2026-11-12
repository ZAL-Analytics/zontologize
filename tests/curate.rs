pub mod common;

use common::{Env, folder_source, repo};
use zontologize::curate::run::{Options, Outcome, run};

const CURATE_LINKS: &str = "select count(*) from relations \
    where property_iri = 'http://www.w3.org/2002/07/owl#sameAs' and method like 'curate:%'";
const SNAPSHOT: &str = "select coalesce((select string_agg(concat_ws(',', id, src_id, dst_id, \
    method, status, confidence), ';' order by id) from relations), '') || '|' || \
    coalesce((select string_agg(concat_ws(',', entity_id, class_iri, name, method, status, \
    confidence, created_at), ';' order by entity_id) from entity_classes), '')";

async fn setup(name: &str) -> Env {
    let env = Env::new(name).await;
    env.copy_acme_rules();
    env.write_config(&format!(
        "[resolve]\nstrip_words = [\"b.v.\"]\n\
         [models]\ncurate = \"claude-code:haiku\"\n\
         [providers.claude-code]\nconcurrency = 1\n\
         [curate]\nout = \"{}\"\n{}{}",
        env.dir.join("curate").display(),
        folder_source("projects", &repo("tests/fixtures/acme/projects"), "current"),
        folder_source("archive", &repo("tests/fixtures/acme/archive"), "legacy")
    ));
    env.scan().await;
    zontologize::resolve::run(Some(&env.config()), Some(&common::test_url()))
        .await
        .unwrap();
    env
}

fn options() -> Options {
    let mut options = Options::new(repo("tests/fixtures/llm/claude-code/fake-claude-curate.sh"));
    options.backoff = Some(std::time::Duration::from_millis(1));
    options.no_wait = true;
    options
}

async fn curate(env: &Env, options: &Options) -> Outcome {
    run(Some(&env.config()), Some(&common::test_url()), options)
        .await
        .unwrap()
}

async fn id(env: &Env, name: &str) -> i64 {
    env.count(&format!("select id from entities where name = '{name}'"))
        .await
}

async fn canonical(env: &Env, name: &str) -> i64 {
    env.count(&format!(
        "select canonical_id from entities where name = '{name}'"
    ))
    .await
}

async fn snapshot(env: &Env) -> String {
    sqlx::query_scalar(SNAPSHOT)
        .fetch_one(&env.pool)
        .await
        .unwrap()
}

#[tokio::test]
#[ignore]
async fn curate_merges_a_proposed_pair_and_a_rerun_changes_nothing() {
    let env = setup("curate_merge").await;
    assert_ne!(
        canonical(&env, "Zuidwal Technick").await,
        canonical(&env, "Zuidwal Techniek").await
    );
    let before = snapshot(&env).await;
    let mut dry = options();
    dry.dry_run = true;
    assert_eq!(curate(&env, &dry).await, Outcome::DryRun);
    assert_eq!(env.count("select count(*) from llm_jobs").await, 0);
    assert_eq!(snapshot(&env).await, before);

    assert_eq!(curate(&env, &options()).await, Outcome::Finished);
    assert_eq!(
        canonical(&env, "Zuidwal Technick").await,
        canonical(&env, "Zuidwal Techniek").await
    );
    assert_eq!(
        env.count(&format!("{CURATE_LINKS} and status = 'auto'"))
            .await,
        1
    );
    assert_eq!(
        env.count(
            "select count(*) from entity_classes where name = 'Zuidwal Techniek' \
             and status = 'auto' and method = 'curate:llm:claude-code:haiku'"
        )
        .await,
        2
    );
    let report = std::fs::read_to_string(env.dir.join("curate/curate-report.md")).unwrap();
    assert!(report.contains("| auto | 1 |"), "{report}");
    assert!(report.contains("Zuidwal Techniek"), "{report}");

    let calls = env.count("select count(*) from llm_calls").await;
    assert!(calls >= 1);
    let after = snapshot(&env).await;
    assert_eq!(curate(&env, &options()).await, Outcome::Finished);
    assert_eq!(env.count("select count(*) from llm_calls").await, calls);
    assert_eq!(snapshot(&env).await, after);
    env.finish().await;
}

#[tokio::test]
#[ignore]
async fn a_low_confidence_merge_stays_proposed_for_review() {
    let env = setup("curate_proposed").await;
    let prompts = env.config().join("prompts");
    std::fs::create_dir_all(&prompts).unwrap();
    let user = std::fs::read_to_string(repo("prompts/curate-user.md")).unwrap();
    std::fs::write(
        prompts.join("curate-user.md"),
        format!("Weigh the evidence.\n{user}"),
    )
    .unwrap();
    assert_eq!(curate(&env, &options()).await, Outcome::Finished);
    assert_eq!(
        env.count(&format!("{CURATE_LINKS} and status = 'proposed'"))
            .await,
        1
    );
    assert_ne!(
        canonical(&env, "Zuidwal Technick").await,
        canonical(&env, "Zuidwal Techniek").await
    );
    let merges = zontologize::db::review::proposed_merges(&env.pool, "", "seed", 100)
        .await
        .unwrap();
    assert!(
        merges
            .iter()
            .any(|m| m.method == "curate:llm:claude-code:haiku"),
        "{merges:?}"
    );
    env.finish().await;
}

#[tokio::test]
#[ignore]
async fn confirmed_links_and_rejected_rows_stay_untouched() {
    let env = setup("curate_settled").await;
    let (a, b) = (
        id(&env, "Zuidwal Technick").await,
        id(&env, "Zuidwal Techniek").await,
    );
    let (low, high) = (a.min(b), a.max(b));
    sqlx::query(sqlx::AssertSqlSafe(format!(
        "insert into relations (src_id, property_iri, dst_id, method, status, confidence, \
         chunk_id) select {low}, 'http://www.w3.org/2002/07/owl#sameAs', {high}, 'review', \
         'confirmed', 1.0, min(chunk_id) from mentions where entity_id = {low}"
    )))
    .execute(&env.pool)
    .await
    .unwrap();
    sqlx::query(sqlx::AssertSqlSafe(format!(
        "insert into entity_classes (entity_id, class_iri, method, status, confidence, \
         ontology_version_id) select id, 'https://example.org/acme#Site', 'curate:person', \
         'rejected', 0.5, ontology_version_id from entities where id = {high}"
    )))
    .execute(&env.pool)
    .await
    .unwrap();
    let settled = "select string_agg(concat_ws(',', id, status, method), ';') from relations \
                   where status = 'confirmed'";
    let confirmed: String = sqlx::query_scalar(settled)
        .fetch_one(&env.pool)
        .await
        .unwrap();
    assert_eq!(curate(&env, &options()).await, Outcome::Finished);
    let after: String = sqlx::query_scalar(settled)
        .fetch_one(&env.pool)
        .await
        .unwrap();
    assert_eq!(after, confirmed);
    assert_eq!(env.count(CURATE_LINKS).await, 0);
    assert_eq!(
        env.count(&format!(
            "select count(*) from entity_classes where entity_id = {high} and status = 'rejected' \
             and method = 'curate:person' and class_iri = 'https://example.org/acme#Site'"
        ))
        .await,
        1
    );
    assert_eq!(
        env.count(&format!(
            "select count(*) from entity_classes where entity_id = {low} and status = 'auto'"
        ))
        .await,
        1
    );
    assert_eq!(canonical(&env, "Zuidwal Techniek").await, low);
    env.finish().await;
}

#[tokio::test]
#[ignore]
async fn class_checks_send_a_name_hit_and_a_relation_misfit() {
    let env = setup("curate_class_checks").await;
    let config = env.config().join("zontologize.toml");
    let text = std::fs::read_to_string(&config).unwrap();
    std::fs::write(
        &config,
        format!("{text}\n[curate.not_in_class]\n\"acme:Site\" = [\"(?i)port$\"]\n"),
    )
    .unwrap();
    let (project, site) = (
        id(&env, "P-2023-0005 Northwind Pumps B.V.").await,
        id(&env, "Eastport").await,
    );
    sqlx::query(sqlx::AssertSqlSafe(format!(
        "insert into entity_classes (entity_id, class_iri, method, status, confidence, \
         ontology_version_id) select id, 'https://example.org/acme#Site', 'test', 'auto', 1.0, \
         ontology_version_id from entities where id = {project}"
    )))
    .execute(&env.pool)
    .await
    .unwrap();
    assert_eq!(curate(&env, &options()).await, Outcome::Finished);
    let row = |id: i64, class: &str| {
        format!(
            "select count(*) from entity_classes where entity_id = {id} and status = 'auto' \
             and method = 'curate:llm:claude-code:haiku' and class_iri {class}"
        )
    };
    assert_eq!(
        env.count(&row(project, "= 'https://example.org/acme#Project'"))
            .await,
        1
    );
    assert_eq!(env.count(&row(site, "is null")).await, 1);
    let report = std::fs::read_to_string(env.dir.join("curate/curate-report.md")).unwrap();
    assert!(report.contains("| relation check | 1 |"), "{report}");
    assert!(report.contains("| name pattern | 1 |"), "{report}");
    let calls = env.count("select count(*) from llm_calls").await;
    assert_eq!(curate(&env, &options()).await, Outcome::Finished);
    assert_eq!(env.count("select count(*) from llm_calls").await, calls);
    env.finish().await;
}

#[tokio::test]
#[ignore]
async fn one_name_in_disjoint_classes_is_sent_and_moved() {
    let env = setup("curate_disjoint").await;
    let (customer, site) = (
        id(&env, "Northwind Pumps B.V.").await,
        id(&env, "Eastport").await,
    );
    sqlx::query(sqlx::AssertSqlSafe(format!(
        "insert into aliases (entity_id, alias, method) values ({customer}, 'Eastport', 'test')"
    )))
    .execute(&env.pool)
    .await
    .unwrap();
    let mut dry = options();
    dry.dry_run = true;
    assert_eq!(curate(&env, &dry).await, Outcome::DryRun);
    assert_eq!(curate(&env, &options()).await, Outcome::Finished);
    assert_eq!(
        env.count(&format!(
            "select count(*) from entity_classes where entity_id = {site} and status = 'auto' \
             and class_iri = 'https://example.org/acme#Customer'"
        ))
        .await,
        1
    );
    let report = std::fs::read_to_string(env.dir.join("curate/curate-report.md")).unwrap();
    assert!(report.contains("| disjoint classes | 1 |"), "{report}");
    assert_eq!(curate(&env, &options()).await, Outcome::Finished);
    let report = std::fs::read_to_string(env.dir.join("curate/curate-report.md")).unwrap();
    assert!(report.contains("| disjoint classes | 0 |"), "{report}");
    env.finish().await;
}

async fn review_classes(env: &Env, answers: &str) -> zontologize::review::ClassTally {
    let options = zontologize::review::ClassOptions {
        n: Some(10),
        class: String::new(),
        seed: "seed".to_string(),
    };
    let mut input = tokio::io::BufReader::new(answers.as_bytes());
    zontologize::review::classes(
        Some(&env.config()),
        Some(&common::test_url()),
        &options,
        &mut input,
    )
    .await
    .unwrap()
}

#[tokio::test]
#[ignore]
async fn review_classes_records_answers_on_proposed_rows() {
    let env = setup("curate_review_classes").await;
    let (a, b) = (
        id(&env, "Zuidwal Technick").await,
        id(&env, "Zuidwal Techniek").await,
    );
    sqlx::query(sqlx::AssertSqlSafe(format!(
        "insert into entity_classes (entity_id, class_iri, method, status, confidence, \
         ontology_version_id) select id, case when id = {a} then 'https://example.org/acme#Site' \
         end, 'curate:llm:claude-code:haiku', 'proposed', 0.6, ontology_version_id \
         from entities where id in ({a}, {b})"
    )))
    .execute(&env.pool)
    .await
    .unwrap();
    let none = zontologize::review::ClassTally::default();
    assert_eq!(review_classes(&env, "q\n").await, none);
    let order = zontologize::db::review::proposed_classes(&env.pool, "", "seed", 10)
        .await
        .unwrap();
    assert_eq!(order.len(), 2);
    let answers = if order[0].entity_id == a {
        "y\nn\n"
    } else {
        "n\ny\n"
    };
    let tally = review_classes(&env, answers).await;
    assert_eq!((tally.confirmed, tally.rejected, tally.skipped), (1, 1, 0));
    let row = |id: i64, status: &str| {
        format!(
            "select count(*) from entity_classes where entity_id = {id} and status = '{status}'"
        )
    };
    assert_eq!(env.count(&row(a, "confirmed")).await, 1);
    assert_eq!(env.count(&row(b, "rejected")).await, 1);
    assert_eq!(review_classes(&env, "y\ny\n").await, none);
    let api = format!("\"{}_api\".canonical_entities", env.schema);
    assert_eq!(
        env.count(&format!(
            "select count(*) from {api} where id = {a} \
             and class_iri = 'https://example.org/acme#Site'"
        ))
        .await,
        1
    );
    assert_eq!(
        env.count(&format!("select count(*) from {api} where id = {b}"))
            .await,
        1
    );
    env.finish().await;
}
