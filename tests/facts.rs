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
    options.backoff = Some(std::time::Duration::from_millis(1));
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
         [resolve]\nstrip_words = [\"B.V.\", \"BV\"]\n\
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
async fn a_check_change_replays_the_cache_without_yes() {
    let env = setup_extract("facts_check").await;
    assert_eq!(extract(&env, &options()).await.unwrap(), Outcome::Finished);
    let facts = env.count(MODEL_FACTS).await;
    sqlx::query("update chunks set llm_key = 'older check' where llm_key is not null")
        .execute(&env.pool)
        .await
        .unwrap();
    assert_eq!(extract(&env, &options()).await.unwrap(), Outcome::Finished);
    assert_eq!(env.count("select count(*) from llm_calls").await, 2);
    assert_eq!(env.count(MODEL_FACTS).await, facts);
    assert_eq!(
        env.count("select count(*) from chunks where llm_key = 'older check'")
            .await,
        0
    );
    env.finish().await;
}

async fn known_setup(name: &str) -> (Env, i64) {
    let env = Env::new(name).await;
    env.file(
        "Projects/2021/P-2021-0042 Brixwell Pumps - Kelmar/04 Reports/visit.txt",
        "Visit report. The pressure was checked on the return line and the readings were \
         written down for the next visit. Nothing else needs attention this time.",
    );
    env.copy_acme_rules();
    env.write_config(&folder_source("docs", &env.data(), "current"));
    env.scan().await;
    let chunk = env.count("select min(id) from chunks").await;
    (env, chunk)
}

async fn known_names(env: &Env, chunk: i64) -> Vec<String> {
    zontologize::db::facts::known(&env.pool, &[chunk], 10)
        .await
        .unwrap()
        .into_iter()
        .map(|k| k.name)
        .collect()
}

#[tokio::test]
#[ignore]
async fn known_offers_the_document_project_customer_and_site() {
    let (env, chunk) = known_setup("facts_known").await;
    let mut names = known_names(&env, chunk).await;
    names.sort();
    assert_eq!(
        names,
        ["Brixwell Pumps", "Kelmar", "P-2021-0042 Brixwell Pumps"]
    );
    sqlx::raw_sql(
        "insert into entities (class_iri, key, name, method, confidence, ontology_version_id)
         select class_iri, 'brixwell root', 'Brixwell Root', 'gazetteer', 1, ontology_version_id
         from entities where key = 'brixwell pumps';
         update entities set canonical_id = (select id from entities where key = 'brixwell root')
         where key in ('brixwell pumps', 'brixwell root');",
    )
    .execute(&env.pool)
    .await
    .unwrap();
    let mut names = known_names(&env, chunk).await;
    names.sort();
    assert_eq!(
        names,
        ["Brixwell Root", "Kelmar", "P-2021-0042 Brixwell Pumps"]
    );
    env.finish().await;
}

#[tokio::test]
#[ignore]
async fn known_leaves_out_model_mentions() {
    let (env, chunk) = known_setup("facts_known_model").await;
    sqlx::raw_sql(
        "insert into entities (class_iri, key, name, method, confidence, ontology_version_id)
         select class_iri, 'model made', 'Model Made', 'llm:x', 0.8, ontology_version_id
         from entities where key = 'kelmar';
         insert into mentions (entity_id, document_id, span_start, span_end, method, confidence)
         select (select id from entities where key = 'model made'), min(id), 0, 4, 'llm:x', 0.8
         from documents;",
    )
    .execute(&env.pool)
    .await
    .unwrap();
    assert!(
        !known_names(&env, chunk)
            .await
            .contains(&"Model Made".to_string())
    );
    env.finish().await;
}

#[tokio::test]
#[ignore]
async fn a_class_override_survives_an_extract_replay() {
    let env = setup_extract("facts_overlay").await;
    assert_eq!(extract(&env, &options()).await.unwrap(), Outcome::Finished);
    let pump = "select id from entities where method like 'llm:%' \
                and class_iri = 'https://example.org/acme#Pump'";
    let id = env.count(pump).await;
    sqlx::query(sqlx::AssertSqlSafe(format!(
        "insert into entity_classes (entity_id, class_iri, method, status, confidence, \
         ontology_version_id) select id, 'https://example.org/acme#Equipment', 'curate:test', \
         'auto', 0.9, ontology_version_id from entities where id = {id}"
    )))
    .execute(&env.pool)
    .await
    .unwrap();
    sqlx::query("update chunks set llm_key = null")
        .execute(&env.pool)
        .await
        .unwrap();
    assert_eq!(extract(&env, &options()).await.unwrap(), Outcome::Finished);
    assert_eq!(env.count("select count(*) from llm_calls").await, 2);
    assert_eq!(env.count(pump).await, id);
    assert_eq!(
        env.count(&format!(
            "select count(*) from entity_classes where entity_id = {id} and status = 'auto'"
        ))
        .await,
        1
    );
    assert_eq!(
        env.count(&format!(
            "select count(*) from \"{}_api\".canonical_entities where id = {id} \
             and class_iri = 'https://example.org/acme#Equipment'",
            env.schema
        ))
        .await,
        1
    );
    env.finish().await;
}

#[tokio::test]
#[ignore]
async fn a_replay_gives_extracted_entities_normalised_keys() {
    let env = setup_extract("facts_keys").await;
    assert_eq!(extract(&env, &options()).await.unwrap(), Outcome::Finished);
    sqlx::raw_sql(
        "update entities set key = key || ' b.v.' where method like 'llm:%';
         update chunks set llm_key = 'older check' where llm_key is not null;",
    )
    .execute(&env.pool)
    .await
    .unwrap();
    assert_eq!(extract(&env, &options()).await.unwrap(), Outcome::Finished);
    assert_eq!(env.count("select count(*) from llm_calls").await, 2);
    assert_eq!(
        env.count("select count(*) from entities where method like 'llm:%' and key like '% b.v.'")
            .await,
        0
    );
    assert!(
        env.count("select count(*) from entities where method like 'llm:%' and key = 'north site'")
            .await
            > 0
    );
    assert_eq!(
        env.count(
            "select count(*) from entities e where e.method like 'llm:%' \
             and not exists (select 1 from mentions m where m.entity_id = e.id)"
        )
        .await,
        0
    );
    env.finish().await;
}

const ACME_ROWS: &str = "
    insert into entities (class_iri, key, name, method, confidence, ontology_version_id)
    select 'https://example.org/acme#Organisation', k, n, 'llm:test', 0.8, max(ontology_version_id)
    from entities, (values (1, 'acme b.v.', 'Acme B.V.'), (2, 'acme bv', 'Acme BV'),
                           (3, 'zeta works', 'Zeta Works')) v(o, k, n)
    group by o, k, n order by o;
    insert into mentions (entity_id, document_id, span_start, span_end, method, confidence)
    select e.id, (select min(id) from documents), 0, 4, 'llm:test', 0.8
    from entities e where e.key in ('acme b.v.', 'acme bv');
    insert into attributes (entity_id, property_iri, value_text, chunk_id, span_start, span_end,
        method, confidence)
    select e.id, 'https://example.org/acme#ratedPower', e.name, (select min(id) from chunks), 0, 4,
        'llm:test', 0.8
    from entities e where e.key in ('acme b.v.', 'acme bv');
    insert into aliases (entity_id, alias, method)
    select e.id, 'Alias of ' || e.name, 'gazetteer' from entities e
    where e.key in ('acme b.v.', 'acme bv');
    insert into relations (src_id, property_iri, dst_id, method, status, confidence, document_id)
    select e.id, 'https://example.org/acme#partOf', z.id, 'llm:test', 'confirmed', 0.9,
        (select min(id) from documents)
    from entities e, entities z where e.key in ('acme b.v.', 'acme bv') and z.key = 'zeta works';
";

async fn acme_ids(env: &Env) -> Vec<(i64, String)> {
    sqlx::query_as("select id, key from entities where name like 'Acme%' order by id")
        .fetch_all(&env.pool)
        .await
        .unwrap()
}

#[tokio::test]
#[ignore]
async fn stored_model_entities_are_re_keyed_and_folded() {
    let env = setup_extract("facts_rekey").await;
    assert_eq!(extract(&env, &options()).await.unwrap(), Outcome::Finished);
    sqlx::raw_sql(ACME_ROWS).execute(&env.pool).await.unwrap();
    let before = acme_ids(&env).await;
    assert_eq!(extract(&env, &options()).await.unwrap(), Outcome::Finished);
    assert_eq!(env.count("select count(*) from llm_calls").await, 2);
    let kept = before[0].0;
    assert_eq!(acme_ids(&env).await, [(kept, "acme".to_string())]);
    let on_kept = |table: &str| format!("select count(*) from {table} where entity_id = {kept}");
    assert_eq!(env.count(&on_kept("mentions")).await, 2);
    assert_eq!(env.count(&on_kept("attributes")).await, 2);
    assert_eq!(
        env.count(&format!(
            "select count(*) from aliases where entity_id = {kept} and \
             (alias like 'Alias of Acme%' or (alias = 'Acme BV' and method = 'fold'))"
        ))
        .await,
        3
    );
    assert_eq!(
        env.count(&format!(
            "select count(*) from relations where src_id = {kept} and status = 'confirmed'"
        ))
        .await,
        2
    );
    assert_eq!(extract(&env, &options()).await.unwrap(), Outcome::Finished);
    assert_eq!(acme_ids(&env).await, [(kept, "acme".to_string())]);
    env.finish().await;
}

#[tokio::test]
#[ignore]
async fn a_rejected_same_as_keeps_rows_apart() {
    let env = setup_extract("facts_rekey_apart").await;
    assert_eq!(extract(&env, &options()).await.unwrap(), Outcome::Finished);
    sqlx::raw_sql(ACME_ROWS).execute(&env.pool).await.unwrap();
    let before = acme_ids(&env).await;
    sqlx::query(
        "insert into relations (src_id, property_iri, dst_id, method, status, confidence, document_id) \
         values ($1, 'http://www.w3.org/2002/07/owl#sameAs', $2, 'resolve:fuzzy', 'rejected', 0.9, \
         (select min(id) from documents))",
    )
    .bind(before[0].0)
    .bind(before[1].0)
    .execute(&env.pool)
    .await
    .unwrap();
    assert_eq!(extract(&env, &options()).await.unwrap(), Outcome::Finished);
    assert_eq!(
        acme_ids(&env).await,
        [
            (before[0].0, "acme".to_string()),
            (before[1].0, "acme bv".to_string())
        ]
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
