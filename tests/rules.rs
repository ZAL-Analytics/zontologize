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
