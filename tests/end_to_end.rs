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
            "../../tests/fixtures/acme",
            &env.data().display().to_string(),
        );
    std::fs::write(env.config().join("zontologize.toml"), text).unwrap();

    let stats = env.scan().await;
    assert_eq!(stats["sources"]["projects"]["files"].as_u64(), Some(5));
    assert_eq!(stats["sources"]["archive"]["files"].as_u64(), Some(3));
    assert_eq!(stage(&stats, "extract"), 6);
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
    assert_eq!(stage(&stats, "rules"), 8 + 6);
    assert_eq!(
        env.count("select count(*) from documents where doc_type = 'quotation'")
            .await,
        1
    );

    let stats = env.scan().await;
    assert_eq!(stage(&stats, "extract"), 0);
    assert_eq!(stage(&stats, "chunk"), 0);
    assert_eq!(stage(&stats, "rules"), 0);
    env.finish().await;
}
