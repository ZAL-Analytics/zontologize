pub mod common;

use common::{Env, FakeEmbed, folder_source, repo};

const RESOLVE: &str = "[resolve]\nstrip_words = [\"b.v.\"]\n";

async fn setup(name: &str) -> Env {
    let env = Env::new(name).await;
    env.copy_acme_rules();
    env.write_config(&format!(
        "{RESOLVE}{}{}",
        folder_source("projects", &repo("tests/fixtures/acme/projects"), "current"),
        folder_source("archive", &repo("tests/fixtures/acme/archive"), "legacy")
    ));
    env.scan().await;
    env
}

async fn resolve(env: &Env) -> zontologize::resolve::Stats {
    zontologize::resolve::run(Some(&env.config()), Some(&common::test_url()))
        .await
        .unwrap()
}

async fn canonical(env: &Env, name: &str) -> i64 {
    sqlx::query_scalar("select canonical_id from entities where name = $1")
        .bind(name)
        .fetch_one(&env.pool)
        .await
        .unwrap()
}

async fn same_as(env: &Env, status: &str) -> i64 {
    sqlx::query_scalar(
        "select count(*) from relations where property_iri = 'http://www.w3.org/2002/07/owl#sameAs' \
         and status = $1",
    )
    .bind(status)
    .fetch_one(&env.pool)
    .await
    .unwrap()
}

#[tokio::test]
#[ignore]
async fn duplicates_share_a_canonical_id_and_reviews_stick() {
    let env = setup("resolve_names").await;
    let stats = resolve(&env).await;
    assert!(stats.embed_skipped.is_some(), "{stats:?}");
    let first = canonical(&env, "Northwind Pumps").await;
    assert_eq!(canonical(&env, "Northwind Pumps B.V.").await, first);
    assert_eq!(same_as(&env, "auto").await, 1, "{stats:?}");
    assert_ne!(
        canonical(&env, "Zuidwal Technick").await,
        canonical(&env, "Zuidwal Techniek").await
    );
    assert!(same_as(&env, "proposed").await >= 1, "{stats:?}");
    let alias: i64 = env
        .count(
            "select count(*) from aliases a join entities e on e.id = a.entity_id \
             where a.method = 'resolve' and e.id = e.canonical_id \
             and a.alias in ('Northwind Pumps', 'Northwind Pumps B.V.')",
        )
        .await;
    assert_eq!(alias, 1);

    let again = resolve(&env).await;
    assert_eq!(again.written(), 0, "{again:?}");

    sqlx::query(
        "update relations set status = 'rejected' where method = 'resolve:fuzzy' \
         and src_id in (select id from entities where name like 'Zuidwal%')",
    )
    .execute(&env.pool)
    .await
    .unwrap();
    let rejected = same_as(&env, "rejected").await;
    assert_eq!(rejected, 1);
    let again = resolve(&env).await;
    assert_eq!(again.written(), 0, "{again:?}");
    assert_eq!(same_as(&env, "rejected").await, 1);

    sqlx::query("update contents set rules_key = null")
        .execute(&env.pool)
        .await
        .unwrap();
    sqlx::query("update documents set rules_key = null")
        .execute(&env.pool)
        .await
        .unwrap();
    env.scan().await;
    assert_eq!(canonical(&env, "Northwind Pumps B.V.").await, first);
    env.finish().await;
}

#[tokio::test]
#[ignore]
async fn documents_get_reference_and_revision_links() {
    let env = setup("resolve_documents").await;
    resolve(&env).await;
    let replaces: Vec<(String, String)> = sqlx::query_as(
        "select a.name, b.name from relations r join entities a on a.id = r.src_id \
         join entities b on b.id = r.dst_id \
         where r.property_iri = 'http://purl.org/dc/terms/replaces'",
    )
    .fetch_all(&env.pool)
    .await
    .unwrap();
    assert_eq!(
        replaces,
        [(
            "DWG-310-A2 rev B.txt".to_string(),
            "DWG-310-A2 rev A.txt".to_string()
        )]
    );
    let references: Vec<String> = sqlx::query_scalar(
        "select distinct a.name from relations r join entities a on a.id = r.src_id \
         where r.property_iri = 'http://purl.org/dc/terms/references' and r.chunk_id is not null \
         order by 1",
    )
    .fetch_all(&env.pool)
    .await
    .unwrap();
    assert!(
        references
            .iter()
            .any(|n| n.starts_with("Inspection report")),
        "{references:?}"
    );
    assert!(
        references.iter().any(|n| n.starts_with("Letter")),
        "{references:?}"
    );
    let again = resolve(&env).await;
    assert_eq!(again.written(), 0, "{again:?}");
    env.finish().await;
}

#[tokio::test]
#[ignore]
async fn embedding_matches_are_proposed() {
    let env = setup("resolve_embed").await;
    env.embed(
        "local:arctic-embed-m-v2.0",
        FakeEmbed::new(8),
        &zontologize::embed::run::Options::default(),
    )
    .await;
    sqlx::query(
        "update embeddings set embedding = '[1,0,0,0,0,0,0,0]' where owner_kind = 'entity'",
    )
    .execute(&env.pool)
    .await
    .unwrap();
    sqlx::query(
        "create index embeddings_test on embeddings \
         using hnsw ((embedding::halfvec(8)) halfvec_cosine_ops) where owner_kind = 'entity'",
    )
    .execute(&env.pool)
    .await
    .unwrap();
    let stats = resolve(&env).await;
    assert!(stats.embed_skipped.is_none(), "{stats:?}");
    assert!(
        stats.added.get("resolve:embed").copied().unwrap_or(0) > 0,
        "{stats:?}"
    );
    let auto_embed = env
        .count("select count(*) from relations where method = 'resolve:embed' and status <> 'proposed'")
        .await;
    assert_eq!(auto_embed, 0);
    env.finish().await;
}

async fn review(env: &Env, answers: &str, seed: &str) -> zontologize::review::MergeTally {
    let options = zontologize::review::MergeOptions {
        n: Some(3),
        class: String::new(),
        seed: seed.to_string(),
    };
    let mut input = tokio::io::BufReader::new(answers.as_bytes());
    zontologize::review::merges(
        Some(&env.config()),
        Some(&common::test_url()),
        &options,
        &mut input,
    )
    .await
    .unwrap()
}

async fn proposed_order(env: &Env, seed: &str) -> Vec<i64> {
    zontologize::db::review::proposed_merges(&env.pool, "", seed, 10)
        .await
        .unwrap()
        .into_iter()
        .map(|m| m.id)
        .collect()
}

#[tokio::test]
#[ignore]
async fn review_merges_confirms_rejects_and_quits() {
    let env = setup("resolve_review").await;
    resolve(&env).await;
    let order = proposed_order(&env, "s").await;
    assert!(order.len() >= 2, "{order:?}");
    assert_eq!(order, proposed_order(&env, "s").await);
    let tally = review(&env, "y\nn\nq\n", "s").await;
    assert_eq!(tally.confirmed, 1);
    assert_eq!(tally.rejected, 1);
    let (src, dst, status): (i64, i64, String) =
        sqlx::query_as("select src_id, dst_id, status from relations where id = $1")
            .bind(order[0])
            .fetch_one(&env.pool)
            .await
            .unwrap();
    assert_eq!(status, "confirmed");
    let canon: Vec<Option<i64>> =
        sqlx::query_scalar("select canonical_id from entities where id = any($1) order by id")
            .bind([src, dst])
            .fetch_all(&env.pool)
            .await
            .unwrap();
    assert_eq!(canon[0], canon[1]);
    let (src, dst, status): (i64, i64, String) =
        sqlx::query_as("select src_id, dst_id, status from relations where id = $1")
            .bind(order[1])
            .fetch_one(&env.pool)
            .await
            .unwrap();
    assert_eq!(status, "rejected");
    let canon: Vec<Option<i64>> =
        sqlx::query_scalar("select canonical_id from entities where id = any($1) order by id")
            .bind([src, dst])
            .fetch_all(&env.pool)
            .await
            .unwrap();
    assert_ne!(canon[0], canon[1]);
    let again = resolve(&env).await;
    assert_eq!(again.written(), 0, "{again:?}");
    assert_eq!(same_as(&env, "confirmed").await, 1);
    assert_eq!(same_as(&env, "rejected").await, 1);
    let tally = review(&env, "q\n", "s").await;
    assert_eq!(tally, zontologize::review::MergeTally::default());
    env.finish().await;
}

#[tokio::test]
#[ignore]
async fn a_second_writer_stops_and_leaves_the_running_run() {
    let env = setup("writer").await;
    let lock = zontologize::db::runs::lock(Some(&common::test_url()), &env.schema)
        .await
        .unwrap();
    let (run_id, _) = zontologize::db::runs::start(&env.pool, "held")
        .await
        .unwrap();
    let error = zontologize::resolve::run(Some(&env.config()), Some(&common::test_url()))
        .await
        .err()
        .unwrap();
    assert!(format!("{error:#}").contains("another zontologize run is writing"));
    let status: String = sqlx::query_scalar("select status from runs where id = $1")
        .bind(run_id)
        .fetch_one(&env.pool)
        .await
        .unwrap();
    assert_eq!(status, "running");
    lock.release().await;
    resolve(&env).await;
    env.finish().await;
}

#[tokio::test]
#[ignore]
async fn an_embedding_link_the_search_misses_is_kept_while_it_scores() {
    let env = Env::new("resolve_stable").await;
    env.copy_acme_rules();
    env.write_config(&format!(
        "[resolve]\nmax_candidates = 1\n{}{}",
        folder_source("projects", &repo("tests/fixtures/acme/projects"), "current"),
        folder_source("archive", &repo("tests/fixtures/acme/archive"), "legacy")
    ));
    env.scan().await;
    env.embed(
        "local:arctic-embed-m-v2.0",
        FakeEmbed::new(8),
        &zontologize::embed::run::Options::default(),
    )
    .await;
    let set = |vector: &'static str, id: Option<i64>| {
        let pool = env.pool.clone();
        async move {
            sqlx::query(
                "update embeddings set embedding = $1::halfvec \
                 where owner_kind = 'entity' and ($2::bigint is null or owner_id = $2)",
            )
            .bind(vector)
            .bind(id)
            .execute(&pool)
            .await
            .unwrap();
        }
    };
    set("[1,0,0,0,0,0,0,0]", None).await;
    resolve(&env).await;
    let (link, b): (i64, i64) = sqlx::query_as(
        "select r.id, r.dst_id from relations r \
         join entities a on a.id = r.src_id join entities b on b.id = r.dst_id \
         where r.method = 'resolve:embed' and a.class_iri = b.class_iri \
         and (select count(*) from entities c where c.class_iri = a.class_iri) >= 3 \
         order by r.id limit 1",
    )
    .fetch_one(&env.pool)
    .await
    .unwrap();
    set("[0.95,0.31,0,0,0,0,0,0]", Some(b)).await;
    resolve(&env).await;
    let kept = env
        .count(&format!("select count(*) from relations where id = {link}"))
        .await;
    assert_eq!(kept, 1);
    set("[0,1,0,0,0,0,0,0]", Some(b)).await;
    resolve(&env).await;
    let kept = env
        .count(&format!("select count(*) from relations where id = {link}"))
        .await;
    assert_eq!(kept, 0);
    env.finish().await;
}

#[tokio::test]
#[ignore]
async fn a_rules_rerun_keeps_links_and_reviews() {
    let env = setup("resolve_rerun").await;
    resolve(&env).await;
    let links = "select count(*) from relations where method like 'resolve:%'";
    let before = env.count(links).await;
    assert!(before > 0);
    sqlx::query(
        "update relations set status = 'confirmed' \
         where id = (select min(id) from relations where method = 'resolve:fuzzy')",
    )
    .execute(&env.pool)
    .await
    .unwrap();
    let rules = env.config().join("rules.toml");
    let text = std::fs::read_to_string(&rules).unwrap();
    std::fs::write(&rules, format!("{text}\n# changed\n")).unwrap();
    let stats = env.scan().await;
    assert!(common::stage(&stats, "rules") > 0);
    assert_eq!(env.count(links).await, before);
    assert_eq!(
        env.count("select count(*) from relations where status = 'confirmed'")
            .await,
        1
    );
    env.finish().await;
}
