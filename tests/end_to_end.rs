pub mod common;

use std::path::Path;

use common::{Env, repo, stage};

fn copy_dir(from: &Path, to: &Path) {
    std::fs::create_dir_all(to).unwrap();
    for entry in std::fs::read_dir(from).unwrap() {
        let entry = entry.unwrap();
        let target = to.join(entry.file_name());
        if entry.file_type().unwrap().is_dir() {
            copy_dir(&entry.path(), &target);
        } else {
            std::fs::copy(entry.path(), target).unwrap();
        }
    }
}

#[tokio::test]
#[ignore]
async fn acme_example_end_to_end() {
    let env = Env::new("end_to_end").await;
    copy_dir(&repo("examples/acme"), &env.config());
    copy_dir(&repo("tests/fixtures/acme"), &env.data());
    let text = std::fs::read_to_string(env.config().join("zontologize.toml"))
        .unwrap()
        .replace(
            "schema = \"zontologize\"",
            &format!("schema = \"{}\"", env.schema),
        )
        .replace(
            "graph = \"zontologize_graph\"",
            &format!("graph = \"{}_graph\"", env.schema),
        )
        .replace(
            "api_schema = \"zontologize_api\"",
            &format!("api_schema = \"{}_api\"", env.schema),
        )
        .replace(
            "../../tests/fixtures/acme",
            &env.data().display().to_string(),
        )
        .replace(
            "out = \"fit\"",
            &format!("out = \"{}\"", env.dir.join("fit").display()),
        )
        .replace(
            "out = \"extract\"",
            &format!("out = \"{}\"", env.dir.join("extract").display()),
        )
        .replace(
            "out = \"eval\"",
            &format!("out = \"{}\"", env.dir.join("eval").display()),
        );
    std::fs::write(env.config().join("zontologize.toml"), text).unwrap();

    let stats = env.scan().await;
    assert_eq!(stats["sources"]["projects"]["files"].as_u64(), Some(9));
    assert_eq!(stats["sources"]["archive"]["files"].as_u64(), Some(3));
    assert_eq!(stage(&stats, "extract"), 10);
    assert_eq!(
        env.count(
            "select count(*) from documents where status = 'skipped' and sha256 is null \
             and path like '%.dwg'"
        )
        .await,
        1
    );
    let shared: Vec<(String, String)> = sqlx::query_as(
        "select e.class_iri, e.key from entities e \
         join mentions m on m.entity_id = e.id \
         left join chunks c on c.id = m.chunk_id \
         join documents d on d.id = m.document_id or d.content_id = c.content_id \
         where e.class_iri = 'https://example.org/acme#Project' \
         group by 1, 2 having count(distinct d.source_id) > 1 order by 2",
    )
    .fetch_all(&env.pool)
    .await
    .unwrap();
    assert_eq!(
        shared,
        [
            (
                "https://example.org/acme#Project".to_string(),
                "P-2021-0042".to_string()
            ),
            (
                "https://example.org/acme#Project".to_string(),
                "P-2022-0107".to_string()
            )
        ]
    );
    assert_eq!(
        env.count("select count(*) from mentions where document_id is null and chunk_id is null")
            .await,
        0
    );
    assert_eq!(
        env.count(
            "select count(*) from mentions m join chunks c on c.id = m.chunk_id \
             where c.page_from is null"
        )
        .await,
        0
    );

    let segments: Vec<(i32, i32, Option<String>)> = sqlx::query_as(
        "select s.page_from, s.page_to, s.doc_type from segments s \
         join documents d on d.content_id = s.content_id \
         where d.path like '%Certificates 0003.pdf' order by s.ord",
    )
    .fetch_all(&env.pool)
    .await
    .unwrap();
    assert_eq!(
        segments,
        [
            (1, 2, Some("certificate".to_string())),
            (3, 3, Some("certificate".to_string()))
        ]
    );
    assert_eq!(
        env.count(
            "select count(*) from relations r join entities s on s.id = r.src_id \
             join entities d on d.id = r.dst_id \
             where r.property_iri = 'https://example.org/acme#partOf' \
             and s.method = 'page_rule' and s.class_iri = 'https://example.org/acme#Certificate' \
             and d.document_id = (select id from documents where path like '%Certificates 0003.pdf')"
        )
        .await,
        2
    );
    let scanned: (String, f64) = sqlx::query_as(
        "select e.key, m.confidence from mentions m join entities e on e.id = m.entity_id \
         join chunks c on c.id = m.chunk_id join documents d on d.content_id = c.content_id \
         where d.path like '%Brief 0004.pdf' and m.method = 'regex-ocr'",
    )
    .fetch_one(&env.pool)
    .await
    .unwrap();
    assert_eq!(scanned.0, "P-2021-0042");
    assert!(scanned.1 > 0.0 && scanned.1 < 1.0, "{}", scanned.1);

    let report = env.data().join(
        "projects/Projects/2021/P-2021-0042 Northwind Pumps - Eastport/01 Correspondence/Letter P-2021-0042 0001.txt",
    );
    std::fs::write(
        &report,
        "Letter P-2021-0042\n\nThe compressor was serviced.\n",
    )
    .unwrap();
    let rules = std::fs::read_to_string(env.config().join("rules.toml"))
        .unwrap()
        .replace("doc_type = \"invoice\"", "doc_type = \"quotation\"");
    env.write_rules(&rules);
    let stats = env.scan().await;
    assert_eq!(stats["sources"]["projects"]["changed"].as_u64(), Some(1));
    assert_eq!(stage(&stats, "extract"), 1);
    assert_eq!(stage(&stats, "chunk"), 1);
    assert_eq!(stage(&stats, "rules"), 12 + 10);
    assert_eq!(
        env.count("select count(*) from documents where doc_type = 'quotation'")
            .await,
        2
    );

    let stats = env.scan().await;
    assert_eq!(stage(&stats, "extract"), 0);
    assert_eq!(stage(&stats, "chunk"), 0);
    assert_eq!(stage(&stats, "rules"), 0);

    let facts = "select (select count(*) from entities) + (select count(*) from mentions) \
        + (select count(*) from class_mentions) + (select count(*) from relations)";
    let before = env.count(facts).await;
    let mut options =
        zontologize::fit::run::Options::new(repo("tests/fixtures/llm/claude-code/fake-claude.sh"));
    options.no_wait = true;
    let outcome =
        zontologize::fit::run::run(Some(&env.config()), Some(&common::test_url()), &options)
            .await
            .unwrap();
    assert_eq!(outcome, zontologize::fit::run::Outcome::Finished);
    let report = std::fs::read_to_string(env.dir.join("fit/fit-report.md")).unwrap();
    assert!(report.contains("https://example.org/acme#"), "{report}");
    assert!(env.dir.join("fit/proposals.ttl").exists());
    assert_eq!(env.count(facts).await, before);

    let mut options = zontologize::llm::job::Options::new(repo(
        "tests/fixtures/llm/claude-code/fake-claude-extract.sh",
    ));
    options.no_wait = true;
    let url = common::test_url();
    let outcome = zontologize::classify::run(Some(&env.config()), Some(&url), &options)
        .await
        .unwrap();
    assert_eq!(outcome, zontologize::llm::job::Outcome::Finished);
    assert_eq!(
        env.count(
            "select count(*) from documents where path like '%Service visit%' \
             and doc_type = 'work_order' and type_method like 'llm:%'"
        )
        .await,
        1
    );
    let outcome = zontologize::facts::run::run(Some(&env.config()), Some(&url), &options)
        .await
        .unwrap();
    assert_eq!(outcome, zontologize::llm::job::Outcome::Finished);
    assert_eq!(
        env.count(
            "select count(*) from relations where method like 'llm:%' \
             and property_iri = 'https://example.org/acme#atSite'"
        )
        .await,
        1
    );
    assert_eq!(env.count("select count(*) from relation_terms").await, 1);
    assert!(env.dir.join("extract/extract-report.md").exists());
    assert!(env.dir.join("extract/relation-proposals.ttl").exists());
    let model_facts = "select (select count(*) from mentions where method like 'llm:%') \
        + (select count(*) from relations where method like 'llm:%') \
        + (select count(*) from relation_terms) + (select count(*) from llm_types)";
    let calls = env.count("select count(*) from llm_calls").await;
    let kept = env.count(model_facts).await;
    env.scan().await;
    let outcome = zontologize::facts::run::run(Some(&env.config()), Some(&url), &options)
        .await
        .unwrap();
    assert_eq!(outcome, zontologize::llm::job::Outcome::Finished);
    assert_eq!(env.count("select count(*) from llm_calls").await, calls);
    assert_eq!(env.count(model_facts).await, kept);

    graph_export_and_views(&env, &url).await;
    if std::env::var("ZONTOLOGIZE_TEST_EMBED_MODEL").is_ok() {
        embed_resolve_and_review(&env, &url).await;
    }
    env.finish().await;
}

async fn graph_export_and_views(env: &Env, url: &str) {
    let config = Some(env.config());
    zontologize::resolve::run(config.as_deref(), Some(url))
        .await
        .unwrap();
    let graph = zontologize::graph::sync(config.as_deref(), Some(url))
        .await
        .unwrap();
    assert_eq!(
        graph.vertices as i64,
        env.count("select count(*) from entities").await
    );
    let text = zontologize::export::run(config.as_deref(), Some(url))
        .await
        .unwrap();
    assert!(text.contains("owl:Ontology"));
    let api = format!("\"{}_api\"", env.schema);
    let documents = env
        .count(&format!(
            "select count(*) from {api}.documents d join {api}.chunks c on c.content_id = d.content_id"
        ))
        .await;
    assert!(documents > 0);
    let snapshot = "select (select count(*) from relations) + (select count(*) from aliases) \
        + (select count(*) from entities where canonical_id is not null)";
    let before = env.count(snapshot).await;
    env.scan().await;
    let again = zontologize::resolve::run(config.as_deref(), Some(url))
        .await
        .unwrap();
    assert_eq!(again.written(), 0, "{again:?}");
    let synced = zontologize::graph::sync(config.as_deref(), Some(url))
        .await
        .unwrap();
    assert_eq!(
        (synced.vertices, synced.edges),
        (graph.vertices, graph.edges)
    );
    assert_eq!(env.count(snapshot).await, before);
}

async fn embed_resolve_and_review(env: &Env, url: &str) {
    let config = Some(env.config());
    let options = zontologize::embed::run::Options::default();
    let stats = zontologize::embed::run::run(config.as_deref(), Some(url), &options)
        .await
        .unwrap();
    assert!(
        stats.chunks.embedded > 0 && stats.entities.embedded > 0,
        "{stats:?}"
    );
    assert!(stats.classes.embedded > 0, "{stats:?}");
    assert_eq!(
        env.count(
            "select count(*) from pg_indexes where schemaname = current_schema() \
             and indexname like 'embeddings_m%'"
        )
        .await,
        2
    );
    let eval = zontologize::embed::eval::Options {
        sample: Some(20),
        ..Default::default()
    };
    let report = zontologize::embed::eval::run(config.as_deref(), Some(url), &eval)
        .await
        .unwrap();
    let report = std::fs::read_to_string(report).unwrap();
    assert!(report.contains("Question R@10"), "{report}");
    let stats = zontologize::resolve::run(config.as_deref(), Some(url))
        .await
        .unwrap();
    assert!(stats.embed_skipped.is_none(), "{stats:?}");
    let canonical: Vec<Option<i64>> = sqlx::query_scalar(
        "select distinct canonical_id from entities where name like 'Northwind Pumps%'",
    )
    .fetch_all(&env.pool)
    .await
    .unwrap();
    assert_eq!(canonical.len(), 1, "{canonical:?}");
    assert_eq!(
        env.count(
            "select count(*) from relations where property_iri = 'http://purl.org/dc/terms/replaces'"
        )
        .await,
        1
    );
    let options = zontologize::review::MergeOptions {
        n: Some(3),
        class: String::new(),
        seed: "e2e".to_string(),
    };
    let mut input = tokio::io::BufReader::new("y\nn\nq\n".as_bytes());
    let tally = zontologize::review::merges(config.as_deref(), Some(url), &options, &mut input)
        .await
        .unwrap();
    assert_eq!((tally.confirmed, tally.rejected), (1, 1));

    env.scan().await;
    let options = zontologize::embed::run::Options::default();
    let stats = zontologize::embed::run::run(config.as_deref(), Some(url), &options)
        .await
        .unwrap();
    let embedded = stats.chunks.embedded + stats.entities.embedded + stats.classes.embedded;
    assert_eq!(embedded, 0, "{stats:?}");
    let stats = zontologize::resolve::run(config.as_deref(), Some(url))
        .await
        .unwrap();
    assert_eq!(stats.written(), 0, "{stats:?}");
}
