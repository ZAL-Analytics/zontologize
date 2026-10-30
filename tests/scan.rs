pub mod common;

use common::{Env, folder_source, source, stage};

async fn env_with_folder(name: &str) -> Env {
    let env = Env::new(name).await;
    env.write_config(&folder_source("docs", &env.data(), "current"));
    env
}

#[tokio::test]
#[ignore]
async fn first_run_records_every_file() {
    let env = env_with_folder("first").await;
    env.file("a/one.txt", "first file");
    env.file("a/two.txt", "second file");
    env.file("b/archive.zip", "not read");
    env.file("b/~$lock.docx", "ignored");
    let stats = env.scan().await;
    assert_eq!(source(&stats, "docs", "files"), 3);
    assert_eq!(source(&stats, "docs", "new"), 3);
    assert_eq!(source(&stats, "docs", "skipped"), 1);
    assert_eq!(source(&stats, "docs", "hashed"), 2);
    assert_eq!(
        env.count("select count(*) from documents where status = 'present' and sha256 is not null")
            .await,
        2
    );
    assert_eq!(
        env.count(
            "select count(*) from documents where status = 'skipped' and sha256 is null \
             and reason = 'extension .zip'"
        )
        .await,
        1
    );
    env.finish().await;
}

#[tokio::test]
#[ignore]
async fn unchanged_rerun_reads_nothing() {
    let env = env_with_folder("unchanged").await;
    env.file("a/one.txt", "first file");
    env.scan().await;
    let stats = env.scan().await;
    assert_eq!(source(&stats, "docs", "unchanged"), 1);
    assert_eq!(source(&stats, "docs", "hashed"), 0);
    assert_eq!(stage(&stats, "extract"), 0);
    assert_eq!(stage(&stats, "chunk"), 0);
    assert_eq!(stage(&stats, "rules"), 0);
    env.finish().await;
}

#[tokio::test]
#[ignore]
async fn changed_file_is_hashed_and_extracted_again_alone() {
    let env = env_with_folder("changed").await;
    let path = env.file("a/one.txt", "first version");
    env.file("a/two.txt", "stays the same");
    env.scan().await;
    let before: String =
        sqlx::query_scalar("select sha256 from documents where path = 'a/one.txt'")
            .fetch_one(&env.pool)
            .await
            .unwrap();
    std::fs::write(&path, "second version, longer").unwrap();
    let stats = env.scan().await;
    assert_eq!(source(&stats, "docs", "changed"), 1);
    assert_eq!(source(&stats, "docs", "hashed"), 1);
    assert_eq!(stage(&stats, "extract"), 1);
    let after: String = sqlx::query_scalar("select sha256 from documents where path = 'a/one.txt'")
        .fetch_one(&env.pool)
        .await
        .unwrap();
    assert_ne!(before, after);
    env.finish().await;
}

#[tokio::test]
#[ignore]
async fn deleted_file_is_marked_with_its_run() {
    let env = env_with_folder("deleted").await;
    let path = env.file("a/one.txt", "will go");
    env.file("a/two.txt", "stays");
    env.scan().await;
    std::fs::remove_file(path).unwrap();
    let stats = env.scan().await;
    assert_eq!(source(&stats, "docs", "deleted"), 1);
    let run: i64 = sqlx::query_scalar("select max(id) from runs")
        .fetch_one(&env.pool)
        .await
        .unwrap();
    let deleted_run: Option<i64> = sqlx::query_scalar(
        "select deleted_run_id from documents where path = 'a/one.txt' and status = 'deleted'",
    )
    .fetch_one(&env.pool)
    .await
    .unwrap();
    assert_eq!(deleted_run, Some(run));
    env.finish().await;
}

#[tokio::test]
#[ignore]
async fn moved_file_keeps_its_id() {
    let env = env_with_folder("moved").await;
    let path = env.file("a/one.txt", "moving file");
    env.scan().await;
    let id: i64 = sqlx::query_scalar("select id from documents where path = 'a/one.txt'")
        .fetch_one(&env.pool)
        .await
        .unwrap();
    std::fs::create_dir_all(env.data().join("b")).unwrap();
    std::fs::rename(path, env.data().join("b/one.txt")).unwrap();
    let stats = env.scan().await;
    assert_eq!(source(&stats, "docs", "moved"), 1);
    assert_eq!(source(&stats, "docs", "deleted"), 0);
    assert_eq!(stage(&stats, "extract"), 0);
    let moved: i64 = sqlx::query_scalar(
        "select id from documents where path = 'b/one.txt' and status = 'present'",
    )
    .fetch_one(&env.pool)
    .await
    .unwrap();
    assert_eq!(moved, id);
    assert_eq!(env.count("select count(*) from documents").await, 1);
    env.finish().await;
}

#[tokio::test]
#[ignore]
async fn duplicate_file_shares_a_sha256_and_is_extracted_once() {
    let env = env_with_folder("duplicate").await;
    env.file("a/one.txt", "same bytes");
    env.file("b/copy.txt", "same bytes");
    let stats = env.scan().await;
    assert_eq!(stage(&stats, "extract"), 1);
    assert_eq!(
        env.count("select count(distinct sha256) from documents")
            .await,
        1
    );
    assert_eq!(
        env.count("select count(distinct content_id) from documents")
            .await,
        1
    );
    env.finish().await;
}

#[tokio::test]
#[ignore]
async fn interrupted_walk_marks_nothing_deleted() {
    let env = env_with_folder("interrupted").await;
    env.file("open/one.txt", "readable");
    env.file("locked/two.txt", "hidden later");
    env.scan().await;
    let locked = env.data().join("locked");
    let mut permissions = std::fs::metadata(&locked).unwrap().permissions();
    std::os::unix::fs::PermissionsExt::set_mode(&mut permissions, 0o000);
    std::fs::set_permissions(&locked, permissions.clone()).unwrap();
    let readable = std::fs::read_dir(&locked).is_ok();
    let stats = env.scan().await;
    std::os::unix::fs::PermissionsExt::set_mode(&mut permissions, 0o755);
    std::fs::set_permissions(&locked, permissions).unwrap();
    if readable {
        eprintln!("skipped: running with rights that ignore directory permissions");
        env.finish().await;
        return;
    }
    assert!(!stats["sources"]["docs"]["complete"].as_bool().unwrap());
    assert_eq!(source(&stats, "docs", "deleted"), 0);
    assert_eq!(
        env.count("select count(*) from documents where status = 'deleted'")
            .await,
        0
    );
    env.finish().await;
}

#[tokio::test]
#[ignore]
async fn failed_file_gets_an_error_row_and_the_run_goes_on() {
    let env = env_with_folder("failed").await;
    env.file("a/broken.pdf", "this is not a pdf");
    env.file("a/fine.txt", "readable text");
    let stats = env.scan().await;
    assert_eq!(stats["extract"]["errors"].as_u64(), Some(1));
    assert_eq!(
        env.count("select count(*) from contents where status = 'error' and error <> ''")
            .await,
        1
    );
    assert_eq!(
        env.count("select count(*) from contents where status = 'done'")
            .await,
        1
    );
    let stats = env.scan().await;
    assert_eq!(stage(&stats, "extract"), 0);
    env.finish().await;
}

#[tokio::test]
#[ignore]
async fn full_text_search_finds_a_chunk_and_chunk_change_skips_extraction() {
    let env = env_with_folder("fulltext").await;
    env.file("a/one.txt", "The heat exchanger was cleaned on site.");
    env.file("a/two.txt", "Unrelated text about invoices.");
    env.scan().await;
    assert_eq!(
        env.count(
            "select count(*) from chunks where tsv @@ plainto_tsquery('simple', 'heat exchanger')"
        )
        .await,
        1
    );
    let chunks_before = env.count("select max(id) from chunks").await;
    env.write_config(&format!(
        "[chunk]\nmax_chars = 500\noverlap = 50\ntext_search = \"simple\"\n{}",
        folder_source("docs", &env.data(), "current")
    ));
    let stats = env.scan().await;
    assert_eq!(stage(&stats, "extract"), 0);
    assert_eq!(stage(&stats, "chunk"), 2);
    assert_eq!(stage(&stats, "rules"), 2);
    assert!(env.count("select min(id) from chunks").await > chunks_before);
    env.finish().await;
}

#[tokio::test]
#[ignore]
async fn ontology_version_is_stored_once_per_content() {
    let env = env_with_folder("ontology").await;
    env.file("a/one.txt", "text");
    env.scan().await;
    env.scan().await;
    assert_eq!(env.count("select count(*) from ontology_versions").await, 1);
    let local = env.config().join("local.ttl");
    std::fs::write(
        &local,
        "@prefix acme: <https://example.org/acme#> .\n\
         @prefix owl: <http://www.w3.org/2002/07/owl#> .\n\
         @prefix rdfs: <http://www.w3.org/2000/01/rdf-schema#> .\n\
         acme:Blower a owl:Class ; rdfs:subClassOf acme:Equipment ; rdfs:label \"blower\" .\n",
    )
    .unwrap();
    let text = std::fs::read_to_string(env.config().join("zontologize.toml")).unwrap();
    let text = text.replace("acme.ttl\"]", "acme.ttl\", \"local.ttl\"]");
    std::fs::write(env.config().join("zontologize.toml"), text).unwrap();
    let stats = env.scan().await;
    assert_eq!(env.count("select count(*) from ontology_versions").await, 2);
    assert_eq!(stage(&stats, "extract"), 0);
    assert_eq!(stage(&stats, "chunk"), 0);
    assert_eq!(stage(&stats, "rules"), 2);
    assert_eq!(
        env.count(
            "select count(*) from classes c join ontology_versions v on v.id = c.ontology_version_id \
             where c.iri = 'https://example.org/acme#Blower' and c.selected"
        )
        .await,
        1
    );
    env.finish().await;
}

#[tokio::test]
#[ignore]
async fn ontology_error_stops_scan_before_any_write() {
    let env = env_with_folder("ontology_error").await;
    env.file("a/one.txt", "text");
    let text = std::fs::read_to_string(env.config().join("zontologize.toml")).unwrap();
    std::fs::write(
        env.config().join("zontologize.toml"),
        text.replace("acme:Site", "acme:Nowhere"),
    )
    .unwrap();
    let error = zontologize::scan::run(Some(&env.config()), &[], Some(&common::test_url()))
        .await
        .unwrap_err();
    assert!(format!("{error:#}").contains("acme:Nowhere"), "{error:#}");
    assert_eq!(env.count("select count(*) from runs").await, 0);
    env.finish().await;
}
