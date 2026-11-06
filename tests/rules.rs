pub mod common;

use common::{Env, folder_source, stage};

async fn two_sources(name: &str) -> Env {
    let env = Env::new(name).await;
    let projects = env.data().join("projects");
    let archive = env.data().join("archive");
    env.write_config(&format!(
        "{}{}",
        folder_source("projects", &projects, "current"),
        folder_source("archive", &archive, "legacy")
    ));
    env.copy_acme_rules();
    env.file(
        "projects/Projects/2023/P-2023-0311 Northwind Pumps - Eastport/04 Reports/Inspection report P-2023-0311 0001.txt",
        "The heat exchanger at Eastport was replaced. NW Pumps approved drawing DWG-410-C2.",
    );
    env.file(
        "archive/Archief/Scans/2023/Werkbon P-2023-0311 0000.txt",
        "De pomp is vervangen volgens tekening DWG-410-C2.",
    );
    env
}

#[tokio::test]
#[ignore]
async fn shared_identifier_links_two_sources_through_one_entity() {
    let env = two_sources("shared").await;
    env.scan().await;
    let sources = env
        .count(
            "select count(distinct d.source_id) from entities e \
             join mentions m on m.entity_id = e.id \
             left join chunks c on c.id = m.chunk_id \
             join documents d on d.id = m.document_id or d.content_id = c.content_id \
             where e.class_iri = 'https://example.org/acme#Project' and e.key = 'P-2023-0311'",
        )
        .await;
    assert_eq!(sources, 2);
    assert_eq!(
        env.count(
            "select count(*) from entities where class_iri = 'https://example.org/acme#Drawing' \
             and key = 'DWG-410-C2'"
        )
        .await,
        1
    );
    assert_eq!(
        env.count("select count(*) from mentions where document_id is null and chunk_id is null")
            .await,
        0
    );
    assert_eq!(
        env.count(
            "select count(*) from relations r join entities s on s.id = r.src_id \
             join entities d on d.id = r.dst_id \
             where r.property_iri = 'https://example.org/acme#describes' \
             and d.key = 'P-2023-0311' and s.document_id is not null"
        )
        .await,
        2
    );
    env.finish().await;
}

#[tokio::test]
#[ignore]
async fn chunk_gets_gazetteer_entity_and_class_mentions() {
    let env = two_sources("gazetteer").await;
    env.scan().await;
    let gazetteer = env
        .count(
            "select count(*) from mentions m join entities e on e.id = m.entity_id \
             join chunks c on c.id = m.chunk_id \
             where m.method = 'gazetteer' and e.key = 'northwind pumps' \
             and substr(c.text, m.span_start + 1, m.span_end - m.span_start) = 'NW Pumps'",
        )
        .await;
    assert_eq!(gazetteer, 1);
    for class in ["HeatExchanger", "Pump", "Drawing"] {
        let count = env
            .count(&format!(
                "select count(*) from class_mentions where chunk_id is not null \
                 and class_iri = 'https://example.org/acme#{class}'"
            ))
            .await;
        assert!(count >= 1, "{class}");
    }
    assert_eq!(
        env.count(
            "select count(*) from aliases a join entities e on e.id = a.entity_id \
             where e.key = 'northwind pumps' and a.alias = 'NW Pumps'"
        )
        .await,
        1
    );
    env.finish().await;
}

#[tokio::test]
#[ignore]
async fn rule_change_reruns_rules_only() {
    let env = two_sources("rule_change").await;
    env.scan().await;
    let rules = std::fs::read_to_string(env.config().join("rules.toml")).unwrap();
    env.write_rules(&rules.replace("doc_type = \"work_order\"", "doc_type = \"report\""));
    let stats = env.scan().await;
    assert_eq!(stage(&stats, "extract"), 0);
    assert_eq!(stage(&stats, "chunk"), 0);
    assert_eq!(stage(&stats, "rules"), 4);
    assert_eq!(
        env.count("select count(*) from documents where doc_type = 'report'")
            .await,
        1
    );
    assert_eq!(
        env.count(
            "select count(*) from entities where class_iri = 'https://example.org/acme#WorkOrder'"
        )
        .await,
        0
    );
    let stats = env.scan().await;
    assert_eq!(stage(&stats, "rules"), 0);
    env.finish().await;
}

#[tokio::test]
#[ignore]
async fn rules_rerun_keeps_relation_verdicts() {
    let env = two_sources("rule_verdicts").await;
    env.scan().await;
    sqlx::query(
        "insert into relations (src_id, property_iri, dst_id, method, confidence, chunk_id, \
         status) select e.id, 'https://example.org/acme#partOf', e.id, 'regex', 1.0, \
         (select min(id) from chunks), 'confirmed' from entities e limit 1",
    )
    .execute(&env.pool)
    .await
    .unwrap();
    sqlx::query(
        "update relations set status = case when id % 2 = 0 then 'confirmed' else 'rejected' end",
    )
    .execute(&env.pool)
    .await
    .unwrap();
    let total = env.count("select count(*) from relations").await;
    let rejected = env
        .count("select count(*) from relations where status = 'rejected'")
        .await;
    assert!(total > 2 && rejected > 0, "{total} {rejected}");
    sqlx::query("update documents set rules_key = null")
        .execute(&env.pool)
        .await
        .unwrap();
    sqlx::query("update contents set rules_key = null")
        .execute(&env.pool)
        .await
        .unwrap();
    let stats = env.scan().await;
    assert!(stage(&stats, "rules") > 0);
    assert_eq!(env.count("select count(*) from relations").await, total);
    assert_eq!(
        env.count("select count(*) from relations where status = 'rejected'")
            .await,
        rejected
    );
    env.finish().await;
}

#[tokio::test]
#[ignore]
async fn deleted_document_loses_its_facts() {
    let env = two_sources("deleted_facts").await;
    env.scan().await;
    std::fs::remove_file(
        env.data()
            .join("archive/Archief/Scans/2023/Werkbon P-2023-0311 0000.txt"),
    )
    .unwrap();
    env.scan().await;
    assert_eq!(
        env.count(
            "select count(*) from mentions m join documents d on d.id = m.document_id \
             where d.status = 'deleted'"
        )
        .await,
        0
    );
    assert_eq!(
        env.count(
            "select count(*) from mentions m join chunks k on k.id = m.chunk_id \
             where not exists (select 1 from documents d where d.content_id = k.content_id \
             and d.status = 'present')"
        )
        .await,
        0
    );
    assert_eq!(
        env.count(
            "select count(*) from contents c where c.rules_key is not null and not exists \
             (select 1 from documents d where d.content_id = c.id and d.status = 'present')"
        )
        .await,
        0
    );
    assert_eq!(
        env.count(
            "select count(*) from entities where class_iri = 'https://example.org/acme#WorkOrder'"
        )
        .await,
        0
    );
    assert_eq!(
        env.count(
            "select count(*) from entities where class_iri = 'https://example.org/acme#Project'"
        )
        .await,
        1
    );
    env.finish().await;
}

#[tokio::test]
#[ignore]
async fn facts_from_ocr_pages_carry_the_ocr_confidence() {
    let env = two_sources("ocr_confidence").await;
    env.scan().await;
    sqlx::query(
        "update pages set method = 'ocr', ocr_confidence = 0.8 where content_id = \
         (select content_id from documents where path like 'Archief/%')",
    )
    .execute(&env.pool)
    .await
    .unwrap();
    sqlx::query("update contents set rules_key = null")
        .execute(&env.pool)
        .await
        .unwrap();
    env.scan().await;
    let scanned: (f64, f64) = sqlx::query_as(
        "select min(m.confidence), max(m.confidence) from mentions m \
         join chunks c on c.id = m.chunk_id join documents d on d.content_id = c.content_id \
         where d.path like 'Archief/%'",
    )
    .fetch_one(&env.pool)
    .await
    .unwrap();
    assert!((scanned.0 - 0.8).abs() < 1e-9 && (scanned.1 - 0.8).abs() < 1e-9);
    let layer: f64 = sqlx::query_scalar(
        "select min(m.confidence) from mentions m \
         join chunks c on c.id = m.chunk_id join documents d on d.content_id = c.content_id \
         where d.path like 'Projects/%'",
    )
    .fetch_one(&env.pool)
    .await
    .unwrap();
    assert_eq!(layer, 1.0);
    env.finish().await;
}

#[tokio::test]
#[ignore]
async fn confused_identifier_in_ocr_text_links_to_the_known_entity() {
    let env = two_sources("ocr_ids").await;
    env.file(
        "archive/Archief/Scans/2023/Brief 0001.txt",
        "Betreft order P-2O23-O3l1 en order P-2O99-OOO1.",
    );
    env.scan().await;
    sqlx::query(
        "update pages set method = 'ocr', ocr_confidence = 0.9 where content_id = \
         (select content_id from documents where path like '%Brief 0001.txt')",
    )
    .execute(&env.pool)
    .await
    .unwrap();
    sqlx::query("update contents set rules_key = null")
        .execute(&env.pool)
        .await
        .unwrap();
    let stats = env.scan().await;
    assert_eq!(stats["ocr_ids"]["accepted"].as_u64().unwrap(), 1);
    assert_eq!(stats["ocr_ids"]["rejected"].as_u64().unwrap(), 1);
    let hit: (String, String, f64) = sqlx::query_as(
        "select e.key, m.method, m.confidence from mentions m join entities e on e.id = m.entity_id \
         where m.method = 'regex-ocr'",
    )
    .fetch_one(&env.pool)
    .await
    .unwrap();
    assert_eq!(hit.0, "P-2023-0311");
    assert_eq!(hit.1, "regex-ocr");
    assert!((hit.2 - 0.9).abs() < 1e-9);
    assert_eq!(
        env.count("select count(*) from entities where key = 'P-2099-0001'")
            .await,
        0
    );
    env.finish().await;
}

#[tokio::test]
#[ignore]
async fn bundle_with_two_headers_becomes_two_segments() {
    let env = two_sources("segments").await;
    env.file(
        "archive/Archief/Scans/2023/Bundel 0001.txt",
        "one page made up for the test",
    );
    env.scan().await;
    let content: i64 =
        sqlx::query_scalar("select content_id from documents where path like '%Bundel 0001.txt'")
            .fetch_one(&env.pool)
            .await
            .unwrap();
    sqlx::query("delete from pages where content_id = $1")
        .bind(content)
        .execute(&env.pool)
        .await
        .unwrap();
    for (page_no, text) in [
        (1, "Certificate of conformity for pump P-2023-0311"),
        (2, "annex with measurements"),
        (3, "Certificate of conformity for a second made-up pump"),
    ] {
        sqlx::query(
            "insert into pages (content_id, page_no, method, text) values ($1, $2, 'layer', $3)",
        )
        .bind(content)
        .bind(page_no)
        .bind(text)
        .execute(&env.pool)
        .await
        .unwrap();
    }
    sqlx::query("update contents set chunk_key = null, rules_key = null")
        .execute(&env.pool)
        .await
        .unwrap();
    let stats = env.scan().await;
    assert_eq!(stats["segments"]["split"].as_u64().unwrap(), 1);
    let segments: Vec<(i32, i32, Option<String>)> = sqlx::query_as(
        "select page_from, page_to, doc_type from segments where content_id = $1 order by ord",
    )
    .bind(content)
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
            "select count(*) from entities e join mentions m on m.entity_id = e.id \
             where e.method = 'page_rule' and e.class_iri = 'https://example.org/acme#Certificate' \
             and m.chunk_id is not null"
        )
        .await,
        2
    );
    assert_eq!(
        env.count(
            "select count(*) from relations r join entities s on s.id = r.src_id \
             join entities d on d.id = r.dst_id \
             where r.property_iri = 'https://example.org/acme#partOf' \
             and s.method = 'page_rule' and d.document_id is not null and r.chunk_id is not null"
        )
        .await,
        2
    );
    env.finish().await;
}

#[tokio::test]
#[ignore]
async fn a_quiet_scan_skips_cleanup_and_a_better_method_wins() {
    let env = two_sources("quiet").await;
    env.scan().await;
    sqlx::query(
        "insert into entities (class_iri, key, name, method, confidence, ontology_version_id) \
         select 'https://example.org/acme#Drawing', 'DWG-999-Z9', 'DWG-999-Z9', 'regex', 1.0, \
         max(id) from ontology_versions",
    )
    .execute(&env.pool)
    .await
    .unwrap();
    sqlx::query("update entities set method = 'ocr', confidence = 0.3 where key = 'DWG-410-C2'")
        .execute(&env.pool)
        .await
        .unwrap();
    let aliases = "select coalesce(string_agg(xmin::text, ',' order by entity_id, alias), '') \
                   from aliases where method = 'gazetteer'";
    let before: String = sqlx::query_scalar(aliases)
        .fetch_one(&env.pool)
        .await
        .unwrap();
    assert!(!before.is_empty());
    let orphan = "select count(*) from entities where key = 'DWG-999-Z9'";
    env.scan().await;
    assert_eq!(env.count(orphan).await, 1);
    let after: String = sqlx::query_scalar(aliases)
        .fetch_one(&env.pool)
        .await
        .unwrap();
    assert_eq!(after, before);
    env.file(
        "projects/Projects/2023/P-2023-0311 Northwind Pumps - Eastport/04 Reports/Inspection report P-2023-0311 0001.txt",
        "The heat exchanger at Eastport was replaced again. NW Pumps approved drawing DWG-410-C2.",
    );
    env.scan().await;
    assert_eq!(env.count(orphan).await, 0);
    let method: (String, f64) =
        sqlx::query_as("select method, confidence from entities where key = 'DWG-410-C2'")
            .fetch_one(&env.pool)
            .await
            .unwrap();
    assert_eq!(method, ("regex".to_string(), 1.0));
    env.finish().await;
}
