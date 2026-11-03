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
        "local:multilingual-e5-base",
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
