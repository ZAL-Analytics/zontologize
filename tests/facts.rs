pub mod common;

use common::{Env, folder_source};

const TEXT: &str = "Inspection report for the north site. The booster pump showed wear on the \
    impeller and the valve on the return line was replaced after the visit.";

async fn setup(name: &str) -> Env {
    let env = Env::new(name).await;
    env.file("site/report.txt", TEXT);
    env.write_config(&folder_source("docs", &env.data(), "current"));
    env.scan().await;
    env
}

async fn add_model_facts(env: &Env) {
    sqlx::query(
        "insert into entities (class_iri, key, name, method, confidence, ontology_version_id) \
         select 'https://example.org/acme#Pump', 'booster pump', 'booster pump', 'llm:x', 0.7, \
         max(id) from ontology_versions",
    )
    .execute(&env.pool)
    .await
    .unwrap();
    for sql in [
        "insert into mentions (entity_id, chunk_id, span_start, span_end, method, confidence) \
         select e.id, c.id, 0, 5, 'llm:x', 0.7 from entities e, chunks c \
         where e.method = 'llm:x'",
        "insert into relations (src_id, property_iri, dst_id, method, confidence, chunk_id) \
         select e.id, 'https://example.org/acme#partOf', e.id, 'llm:x', 0.7, c.id \
         from entities e, chunks c where e.method = 'llm:x'",
        "insert into relation_terms (chunk_id, src_class, label, lang, dst_class, span_start, \
         span_end, method) select id, 'a', 'b', 'en', 'c', 0, 5, 'llm:x' from chunks",
        "update chunks set llm_key = 'k'",
    ] {
        sqlx::query(sql).execute(&env.pool).await.unwrap();
    }
}

async fn model_facts(env: &Env) -> i64 {
    env.count(
        "select (select count(*) from mentions where method = 'llm:x') \
         + (select count(*) from relations where method = 'llm:x') \
         + (select count(*) from relation_terms) \
         + (select count(*) from chunks where llm_key is not null)",
    )
    .await
}

#[tokio::test]
#[ignore]
async fn rules_rerun_keeps_model_facts_and_unused_contents_lose_them() {
    let env = setup("facts_rules").await;
    add_model_facts(&env).await;
    assert_eq!(model_facts(&env).await, 4);
    sqlx::query("update contents set rules_key = null")
        .execute(&env.pool)
        .await
        .unwrap();
    env.scan().await;
    assert!(
        env.count("select count(*) from contents where rules_key is not null")
            .await
            > 0
    );
    assert_eq!(model_facts(&env).await, 4);
    std::fs::remove_file(env.data().join("site/report.txt")).unwrap();
    env.scan().await;
    assert_eq!(model_facts(&env).await, 0);
    env.finish().await;
}

#[tokio::test]
#[ignore]
async fn facts_of_classes_the_ontology_drops_are_removed() {
    let env = setup("facts_unselected").await;
    add_model_facts(&env).await;
    let path = env.config().join("zontologize.toml");
    let text = std::fs::read_to_string(&path).unwrap();
    let text = text.replacen(
        "document_class = ",
        "exclude = [\"acme:Pump\"]\ndocument_class = ",
        1,
    );
    std::fs::write(&path, text).unwrap();
    env.scan().await;
    assert_eq!(
        env.count("select count(*) from mentions where method = 'llm:x'")
            .await,
        0
    );
    assert_eq!(
        env.count("select count(*) from entities where class_iri like '%#Pump'")
            .await,
        0
    );
    assert_eq!(
        env.count("select count(*) from relations where method = 'llm:x'")
            .await,
        0
    );
    env.finish().await;
}

use zontologize::facts::run::{Options, Outcome, run};

const BOOSTER: &str = "Inspection report for project P-2024-0001 at the north site. The booster \
    pump was installed at the north site by the team last week, and the valve was checked.";
const NOTES: &str = "Meeting notes about the yearly budget. The team talked about holidays, the \
    canteen menu and the new parking rules for visitors.";
const MODEL_FACTS: &str = "select (select count(*) from mentions where method like 'llm:%') \
    + (select count(*) from relations where method like 'llm:%') \
    + (select count(*) from relation_terms)";

fn options() -> Options {
    let mut options = Options::new(common::repo(
        "tests/fixtures/llm/claude-code/fake-claude-extract.sh",
    ));
    options.backoff = std::time::Duration::from_millis(1);
    options.no_wait = true;
    options
}

async fn setup_extract(name: &str) -> Env {
    let env = Env::new(name).await;
    env.file("site/report.txt", BOOSTER);
    env.file("office/notes.txt", NOTES);
    env.copy_acme_rules();
    let out = env.dir.join("extract");
    env.write_config(&format!(
        "[models]\nextract = \"claude-code:haiku\"\n\
         [providers.claude-code]\nconcurrency = 1\n\
         [llm_extract]\nmin_chars = 50\nmin_evidence = 1\nout = \"{}\"\n{}",
        out.display(),
        folder_source("docs", &env.data(), "current")
    ));
    env.scan().await;
    env
}

async fn extract(env: &Env, options: &Options) -> anyhow::Result<Outcome> {
    run(Some(&env.config()), Some(&common::test_url()), options).await
}

#[tokio::test]
#[ignore]
async fn extract_writes_checked_facts_and_a_rerun_sends_nothing() {
    let env = setup_extract("facts_end").await;
    let mut dry = options();
    dry.dry_run = true;
    assert_eq!(extract(&env, &dry).await.unwrap(), Outcome::DryRun);
    assert_eq!(env.count("select count(*) from llm_jobs").await, 0);
    assert_eq!(env.count("select count(*) from llm_calls").await, 0);

    assert_eq!(extract(&env, &options()).await.unwrap(), Outcome::Finished);
    assert_eq!(env.count("select count(*) from llm_calls").await, 2);
    assert_eq!(
        env.count(
            "select count(*) from mentions m join entities e on e.id = m.entity_id \
             where m.method = 'llm:claude-code:haiku' and e.class_iri in \
             ('https://example.org/acme#Site', 'https://example.org/acme#Pump')"
        )
        .await,
        2
    );
    assert_eq!(
        env.count(
            "select count(*) from relations where method like 'llm:%' \
             and property_iri = 'https://example.org/acme#atSite' and span_start is not null"
        )
        .await,
        1
    );
    assert_eq!(
        env.count(
            "select count(*) from relations r join entities e on e.id = r.src_id \
             where r.method like 'llm:%' and r.property_iri = 'https://example.org/acme#describes' \
             and e.document_id is not null"
        )
        .await,
        1
    );
    assert_eq!(
        env.count("select count(*) from relation_terms where label = 'installed at'")
            .await,
        1
    );
    assert_eq!(
        env.count(
            "select count(*) from mentions m join chunks c on c.id = m.chunk_id \
             where m.method like 'llm:%' and (m.span_start < 0 or m.span_end <= m.span_start \
             or m.span_end > length(c.text))"
        )
        .await,
        0
    );
    let report = std::fs::read_to_string(env.dir.join("extract/extract-report.md")).unwrap();
    assert!(report.contains("installed at"), "{report}");
    let turtle = std::fs::read_to_string(env.dir.join("extract/relation-proposals.ttl")).unwrap();
    assert!(turtle.contains("installedAt"), "{turtle}");

    let facts = env.count(MODEL_FACTS).await;
    assert_eq!(extract(&env, &options()).await.unwrap(), Outcome::Finished);
    assert_eq!(env.count("select count(*) from llm_calls").await, 2);
    sqlx::query("update contents set rules_key = null")
        .execute(&env.pool)
        .await
        .unwrap();
    env.scan().await;
    assert_eq!(env.count(MODEL_FACTS).await, facts);
    env.finish().await;
}

#[tokio::test]
#[ignore]
async fn a_cap_pauses_and_new_files_send_only_their_chunks() {
    let env = setup_extract("facts_cap").await;
    let mut capped = options();
    capped.max_requests = Some(1);
    assert_eq!(extract(&env, &capped).await.unwrap(), Outcome::Paused);
    assert_eq!(env.count("select count(*) from llm_calls").await, 1);
    assert_eq!(extract(&env, &options()).await.unwrap(), Outcome::Finished);
    assert_eq!(env.count("select count(*) from llm_calls").await, 2);
    assert_eq!(env.count("select count(*) from llm_jobs").await, 1);
    env.file(
        "office/more.txt",
        &format!("{NOTES} A second page of notes."),
    );
    env.scan().await;
    assert_eq!(extract(&env, &options()).await.unwrap(), Outcome::Finished);
    assert_eq!(env.count("select count(*) from llm_calls").await, 3);
    env.finish().await;
}

#[tokio::test]
#[ignore]
async fn a_change_over_the_resend_share_needs_yes() {
    let env = setup_extract("facts_share").await;
    assert_eq!(extract(&env, &options()).await.unwrap(), Outcome::Finished);
    sqlx::query(
        "update relations set status = 'confirmed' where method like 'llm:%' \
         and property_iri = 'https://example.org/acme#atSite'",
    )
    .execute(&env.pool)
    .await
    .unwrap();
    let prompts = env.config().join("prompts");
    std::fs::create_dir_all(&prompts).unwrap();
    let user = std::fs::read_to_string(common::repo("prompts/extract-user.md")).unwrap();
    std::fs::write(
        prompts.join("extract-user.md"),
        format!("Read closely.\n{user}"),
    )
    .unwrap();
    let error = extract(&env, &options()).await.unwrap_err().to_string();
    assert!(error.contains("--yes"), "{error}");
    assert_eq!(env.count("select count(*) from llm_calls").await, 2);
    let mut yes = options();
    yes.yes = true;
    assert_eq!(extract(&env, &yes).await.unwrap(), Outcome::Finished);
    assert_eq!(env.count("select count(*) from llm_calls").await, 4);
    assert_eq!(
        env.count(
            "select count(*) from relations where property_iri = 'https://example.org/acme#atSite'"
        )
        .await,
        1
    );
    assert_eq!(
        env.count("select count(*) from relations where status = 'confirmed'")
            .await,
        1
    );
    env.finish().await;
}

#[tokio::test]
#[ignore]
async fn review_marks_facts_and_never_draws_them_twice() {
    let env = setup_extract("facts_review").await;
    assert_eq!(extract(&env, &options()).await.unwrap(), Outcome::Finished);
    let first = zontologize::db::review::sample(&env.pool, "llm:", "", "seed", 10)
        .await
        .unwrap();
    let again = zontologize::db::review::sample(&env.pool, "llm:", "", "seed", 10)
        .await
        .unwrap();
    assert_eq!(first.len(), 4);
    let ids = |facts: &[zontologize::db::review::Fact]| {
        facts
            .iter()
            .map(|f| (f.kind.clone(), f.id))
            .collect::<Vec<_>>()
    };
    assert_eq!(ids(&first), ids(&again));
    let options = zontologize::review::Options {
        n: Some(10),
        method: "llm:".to_string(),
        class: String::new(),
        seed: "seed".to_string(),
    };
    let mut input: &[u8] = b"y\nn\ny\nq\n";
    let tally = zontologize::review::sample(
        Some(&env.config()),
        Some(&common::test_url()),
        &options,
        &mut input,
    )
    .await
    .unwrap();
    assert_eq!((tally.right, tally.wrong), (2, 1));
    assert_eq!(env.count("select count(*) from fact_reviews").await, 3);
    let left = zontologize::db::review::sample(&env.pool, "llm:", "", "seed", 10)
        .await
        .unwrap();
    assert_eq!(left.len(), 1);
    let precision = zontologize::db::review::precision(&env.pool).await.unwrap();
    assert_eq!(precision.iter().map(|p| p.right).sum::<i64>(), 2);
    assert_eq!(precision.iter().map(|p| p.total).sum::<i64>(), 3);
    env.finish().await;
}

#[tokio::test]
#[ignore]
async fn sample_skips_repeated_texts_and_paths_narrow_the_chunks() {
    let env = setup_extract("facts_sample").await;
    env.file("copy/notes.txt", NOTES);
    env.scan().await;
    let mut sampled = options();
    sampled.sample = Some(10);
    sampled.dry_run = true;
    assert_eq!(extract(&env, &sampled).await.unwrap(), Outcome::DryRun);
    sampled.dry_run = false;
    sampled.sample = Some(1);
    assert_eq!(extract(&env, &sampled).await.unwrap(), Outcome::Finished);
    assert_eq!(env.count("select count(*) from llm_calls").await, 1);
    let text = std::fs::read_to_string(env.config().join("zontologize.toml"))
        .unwrap()
        .replace("[llm_extract]\n", "[llm_extract]\npaths = [\"site/**\"]\n");
    std::fs::write(env.config().join("zontologize.toml"), text).unwrap();
    let mut slice = options();
    slice.yes = true;
    assert_eq!(extract(&env, &slice).await.unwrap(), Outcome::Finished);
    assert_eq!(
        env.count(
            "select count(*) from chunks k join documents d on d.content_id = k.content_id \
             where k.llm_key is not null and d.path like 'site/%'"
        )
        .await,
        1
    );
    assert!(env.dir.join("extract/gazetteer-candidates.csv").exists());
    env.finish().await;
}
