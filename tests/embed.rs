pub mod common;

use common::{Env, FakeEmbed, folder_source};
use zontologize::embed::run::Options;

const DEFAULT: &str = "local:arctic-embed-m-v2.0";
const OTHER: &str = "local:multilingual-e5-small";

const REPORT: &str = "Inspection report for the north site. The booster pump showed wear on the \
    impeller and the valve on the return line was replaced after the visit.";
const LETTER: &str = "Dear customer, we confirm the order for two spare seals and one gasket set \
    for the cooling water pump, to be delivered next month.";
const JUNK: &str = "12 34 56 78 90 12 34 56 78 90 12 34 56 78 90 12 34 56 78 90 12 34 56 78 90";

async fn setup(name: &str) -> Env {
    let env = Env::new(name).await;
    env.file("site/report.txt", REPORT);
    env.file("site/copy-of-report.txt", &format!("{REPORT}\n\n"));
    env.file("office/letter.txt", LETTER);
    env.file("office/table.txt", JUNK);
    env.write_config(&folder_source("docs", &env.data(), "current"));
    env.scan().await;
    env
}

#[tokio::test]
#[ignore]
async fn embed_is_incremental_and_skips_junk() {
    let env = setup("embed_incremental").await;
    let fake = FakeEmbed::new(8);
    let texts = fake.texts.clone();
    let stats = env.embed(DEFAULT, fake, &Options::default()).await;
    assert_eq!(stats.chunks.eligible, 3, "{stats:?}");
    assert_eq!(stats.chunks.embedded, 2, "{stats:?}");
    assert_eq!(stats.chunks.copied, 1, "{stats:?}");
    assert_eq!(stats.junk, 1, "{stats:?}");
    assert!(stats.classes.embedded > 0, "{stats:?}");
    let report_calls = texts
        .lock()
        .unwrap()
        .iter()
        .filter(|t| t.contains("booster pump"))
        .count();
    assert_eq!(report_calls, 1);
    assert_eq!(
        env.count("select count(*) from embeddings where owner_kind = 'chunk'")
            .await,
        3
    );
    assert_eq!(
        env.count(
            "select count(*) from pg_indexes where schemaname = current_schema() and indexname like 'embeddings_m%_chunk'"
        )
        .await,
        1
    );

    let stats = env
        .embed(DEFAULT, FakeEmbed::new(8), &Options::default())
        .await;
    assert_eq!(
        stats.chunks.embedded + stats.entities.embedded + stats.classes.embedded,
        0
    );
    assert_eq!(stats.chunks.kept, 3);

    env.file("office/second.txt", "A second letter about the maintenance contract for the compressor station and its yearly inspection.");
    env.scan().await;
    let stats = env
        .embed(DEFAULT, FakeEmbed::new(8), &Options::default())
        .await;
    assert_eq!(stats.chunks.embedded, 1, "{stats:?}");
    assert_eq!(stats.chunks.kept, 3, "{stats:?}");

    std::fs::remove_file(env.data().join("office/second.txt")).unwrap();
    env.scan().await;
    let stats = env
        .embed(DEFAULT, FakeEmbed::new(8), &Options::default())
        .await;
    assert_eq!(stats.chunks.deleted, 1, "{stats:?}");
    env.finish().await;
}

#[tokio::test]
#[ignore]
async fn minutes_stop_the_run_and_the_next_one_goes_on() {
    let env = setup("embed_minutes").await;
    let options = Options {
        minutes: Some(0),
        ..Options::default()
    };
    let stats = env.embed(DEFAULT, FakeEmbed::new(8), &options).await;
    assert!(stats.stopped);
    assert_eq!(stats.chunks.embedded, 0);
    assert_eq!(
        env.count("select count(*) from pg_indexes where schemaname = current_schema() and indexname like 'embeddings_m%'")
            .await,
        0
    );
    let stats = env
        .embed(DEFAULT, FakeEmbed::new(8), &Options::default())
        .await;
    assert!(!stats.stopped);
    assert_eq!(stats.chunks.embedded, 2);
    env.finish().await;
}

#[tokio::test]
#[ignore]
async fn an_alias_change_re_embeds_that_entity_only() {
    let env = setup("embed_entities").await;
    sqlx::query(
        "insert into entities (class_iri, key, name, method, confidence, ontology_version_id) \
         select 'https://example.org/acme#Customer', k, k, 'gazetteer', 1, max(id) \
         from ontology_versions, unnest(array['Acme Pumps', 'Northwind']) k group by k",
    )
    .execute(&env.pool)
    .await
    .unwrap();
    let stats = env
        .embed(DEFAULT, FakeEmbed::new(8), &Options::default())
        .await;
    let entities = stats.entities.eligible;
    assert!(entities >= 2, "{stats:?}");
    assert_eq!(stats.entities.embedded, entities);
    sqlx::query(
        "insert into aliases (entity_id, alias, method) select id, 'Acme', 'gazetteer' \
         from entities where name = 'Acme Pumps'",
    )
    .execute(&env.pool)
    .await
    .unwrap();
    let fake = FakeEmbed::new(8);
    let texts = fake.texts.clone();
    let stats = env.embed(DEFAULT, fake, &Options::default()).await;
    assert_eq!(stats.entities.embedded, 1, "{stats:?}");
    assert_eq!(
        texts.lock().unwrap().as_slice(),
        ["Acme Pumps (customer); Acme"]
    );
    env.finish().await;
}

#[tokio::test]
#[ignore]
async fn two_models_sit_side_by_side_until_prune() {
    let env = setup("embed_models").await;
    env.embed(DEFAULT, FakeEmbed::new(8), &Options::default())
        .await;
    let other = Options {
        model: Some(OTHER.to_string()),
        ..Options::default()
    };
    env.embed(OTHER, FakeEmbed::new(4), &other).await;
    assert_eq!(env.count("select count(*) from embedding_models").await, 2);
    assert_eq!(
        env.count("select count(*) from embedding_models where current")
            .await,
        1
    );
    assert_eq!(
        env.count("select count(distinct vector_dims(embedding)) from embeddings")
            .await,
        2
    );
    for (model, dimensions) in [(DEFAULT, 8), (OTHER, 4)] {
        let id: i64 = sqlx::query_scalar("select id from embedding_models where name = $1")
            .bind(model)
            .fetch_one(&env.pool)
            .await
            .unwrap();
        let query = common::fake_vector("booster pump", dimensions);
        let text = zontologize::db::embed::vector_text(&query);
        let sql = format!(
            "explain select owner_id from embeddings where model_id = {id} \
             and owner_kind = 'chunk' order by embedding::halfvec({dimensions}) <=> \
             '{text}'::halfvec({dimensions}) limit 3"
        );
        let mut tx = env.pool.begin().await.unwrap();
        sqlx::query("set local enable_seqscan = off")
            .execute(&mut *tx)
            .await
            .unwrap();
        let plan: Vec<String> = sqlx::query_scalar(sqlx::AssertSqlSafe(sql))
            .fetch_all(&mut *tx)
            .await
            .unwrap();
        let plan = plan.join("\n");
        assert!(
            plan.contains(&format!("embeddings_m{id}_chunk")),
            "{model}: {plan}"
        );
    }
    let prune = Options {
        prune: true,
        ..Options::default()
    };
    let stats = env.embed(DEFAULT, FakeEmbed::new(8), &prune).await;
    assert_eq!(stats.pruned, [OTHER]);
    assert_eq!(env.count("select count(*) from embedding_models").await, 1);
    assert_eq!(
        env.count("select count(distinct vector_dims(embedding)) from embeddings")
            .await,
        1
    );
    env.finish().await;
}

#[tokio::test]
#[ignore]
async fn eval_report_lists_every_model_and_the_questions() {
    let env = Env::new("embed_eval").await;
    env.copy_acme_rules();
    let projects = common::repo("tests/fixtures/acme/projects");
    let archive = common::repo("tests/fixtures/acme/archive");
    env.write_config(&format!(
        "{}{}",
        folder_source("projects", &projects, "current"),
        folder_source("archive", &archive, "legacy")
    ));
    env.scan().await;
    std::fs::write(
        env.config().join("questions.toml"),
        "[[questions]]\ntext = \"What did the inspection find?\"\n\
         expect = [\"Projects/**/Inspection report*\"]\n",
    )
    .unwrap();
    let config = zontologize::config::load(Some(&env.config())).unwrap();
    let ontology = zontologize::embed::run::load_ontology(Some(&env.config()), &config).unwrap();
    let questions = zontologize::embed::eval::read_questions(Some(&env.config())).unwrap();
    let sample =
        zontologize::embed::eval::sample(&env.pool, &config, &ontology, &questions, 50, "s")
            .await
            .unwrap();
    assert!(!sample.neighbours.is_empty(), "{sample:?}");
    assert_eq!(sample.questions.len(), 1);
    assert!(!sample.questions[0].expected.is_empty());
    let again =
        zontologize::embed::eval::sample(&env.pool, &config, &ontology, &questions, 50, "s")
            .await
            .unwrap();
    assert_eq!(
        sample.chunks.iter().map(|c| c.id).collect::<Vec<_>>(),
        again.chunks.iter().map(|c| c.id).collect::<Vec<_>>()
    );
    let mut reports = Vec::new();
    for (name, dimensions) in [(DEFAULT, 8), (OTHER, 4)] {
        let model = zontologize::embed::model::parse(name, &config.embed.models).unwrap();
        let fake = FakeEmbed::new(dimensions);
        reports.push(
            zontologize::embed::eval::evaluate(&env.pool, &config, &model, Box::new(fake), &sample)
                .await
                .unwrap(),
        );
    }
    let kept = env.count("select count(*) from embeddings").await;
    assert_eq!(kept as usize, 2 * sample.chunks.len());
    let report = zontologize::embed::eval::report(&sample, "s", &reports);
    for name in [DEFAULT, OTHER] {
        assert!(report.contains(&format!("| {name} |")), "{report}");
    }
    assert!(report.contains("Question R@10"), "{report}");
    let model = zontologize::embed::model::parse(DEFAULT, &config.embed.models).unwrap();
    let again = zontologize::embed::eval::evaluate(
        &env.pool,
        &config,
        &model,
        Box::new(FakeEmbed::new(8)),
        &sample,
    )
    .await
    .unwrap();
    assert_eq!(again.rate, None);
    env.finish().await;
}
