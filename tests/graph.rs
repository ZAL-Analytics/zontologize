pub mod common;

use common::{Env, folder_source, repo};
use zontologize::config::OntologyConfig;

async fn acme(name: &str) -> Env {
    let env = Env::new(name).await;
    env.copy_acme_rules();
    env.write_config(&format!(
        "{}{}",
        folder_source("projects", &repo("tests/fixtures/acme/projects"), "current"),
        folder_source("archive", &repo("tests/fixtures/acme/archive"), "legacy")
    ));
    env.scan().await;
    env
}

#[tokio::test]
#[ignore]
async fn the_export_loads_again_with_the_same_selection() {
    let env = acme("export").await;
    let text = zontologize::export::run(Some(&env.config()), Some(&common::test_url()))
        .await
        .unwrap();
    let version = zontologize::db::ontology::current(&env.pool)
        .await
        .unwrap()
        .unwrap();
    let stored = zontologize::db::ontology::selected(&env.pool, version)
        .await
        .unwrap();
    let config = OntologyConfig {
        files: Vec::new(),
        languages: vec!["en".to_string(), "nl".to_string()],
        include: Vec::new(),
        exclude: Vec::new(),
        document_class: String::new(),
    };
    let loaded = zontologize::ontology::load_sources(
        &[("export.ttl".to_string(), text.into_bytes())],
        &config,
    );
    let ontology = loaded.ontology;
    assert!(!stored.classes.is_empty());
    for class in &stored.classes {
        let again = &ontology.classes[&class.iri];
        assert_eq!(again.labels, class.labels, "{}", class.iri);
        assert_eq!(again.alt_labels, class.alt_labels, "{}", class.iri);
        assert_eq!(again.parents, class.parents, "{}", class.iri);
    }
    for property in &stored.properties {
        let again = &ontology.properties[&property.iri];
        assert_eq!(again.domain, property.domain, "{}", property.iri);
        assert_eq!(again.range, property.range, "{}", property.iri);
        assert_eq!(again.labels, property.labels, "{}", property.iri);
    }
    for iri in [
        "http://purl.org/dc/terms/references",
        "http://purl.org/dc/terms/replaces",
    ] {
        assert!(ontology.properties.contains_key(iri), "{iri}");
    }
    env.finish().await;
}

async fn sync(env: &Env) -> zontologize::graph::Stats {
    zontologize::graph::sync(Some(&env.config()), Some(&common::test_url()))
        .await
        .unwrap()
}

async fn cypher(env: &Env, query: &str) -> i64 {
    let mut conn = zontologize::db::connect(Some(&common::test_url()), &env.schema)
        .await
        .unwrap();
    sqlx::raw_sql("load 'age'; set search_path = ag_catalog, public")
        .execute(&mut conn)
        .await
        .unwrap();
    let sql = format!(
        "select count(*) from ag_catalog.cypher('{}_graph', $$ {query} $$) as (n ag_catalog.agtype)",
        env.schema
    );
    let n = sqlx::query_scalar(sqlx::AssertSqlSafe(sql))
        .fetch_one(&mut conn)
        .await
        .unwrap();
    zontologize::db::close(conn).await;
    n
}

async fn vertex_ids(env: &Env) -> Vec<i64> {
    sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
        "select (id::text)::bigint from \"{}_graph\"._ag_label_vertex order by 1",
        env.schema
    )))
    .fetch_all(&env.pool)
    .await
    .unwrap()
}

#[tokio::test]
#[ignore]
async fn the_graph_mirrors_entities_and_relations() {
    let env = acme("graph").await;
    zontologize::resolve::run(Some(&env.config()), Some(&common::test_url()))
        .await
        .unwrap();
    let stats = sync(&env).await;
    let api = format!("\"{}_api\"", env.schema);
    let canonical = |view: &str| format!("select count(*) from {api}.{view}");
    let entities = env.count(&canonical("canonical_entities")).await;
    let relations = env.count(&canonical("canonical_relations")).await;
    assert_eq!(stats.vertices as i64, entities);
    assert_eq!(stats.edges as i64, relations);
    assert!(relations > 0);
    let described = env
        .count(&format!(
            "select count(*) from {api}.canonical_relations r join entities p on p.id = r.dst_id \
                 where r.property_iri like '%#describes' and p.class_iri like '%#Project'"
        ))
        .await;
    assert!(described > 0);
    let found = cypher(&env, "match (d)-[:describes]->(p:Project) return p.name").await;
    assert_eq!(found, described);
    let indexed = env
        .count(&format!(
            "select count(*) from pg_indexes where schemaname = '{}_graph' \
             and indexdef like '%USING gin (properties)%'",
            env.schema
        ))
        .await;
    let vertex_labels = env
        .count("select count(*) from graph_labels where kind = 'vertex'")
        .await;
    assert_eq!(indexed, vertex_labels);
    let mapped = env
        .count("select count(*) from graph_labels where kind = 'vertex' and label = 'Project'")
        .await;
    assert_eq!(mapped, 1);

    let unchanged = sync(&env).await;
    assert!(unchanged.unchanged, "{unchanged:?}");
    assert_eq!(unchanged.vertices, stats.vertices);
    assert!(!stats.unchanged);
    let with_chunk = env
        .count(&format!(
            "select count(*) from {api}.canonical_relations where chunk_id is not null"
        ))
        .await;
    assert!(with_chunk > 0);
    let traced = cypher(
        &env,
        "match ()-[r]->() where r.chunk_id is not null and r.span_start is not null return r",
    )
    .await;
    let spanned = env
        .count(&format!(
            "select count(*) from {api}.canonical_relations \
                 where chunk_id is not null and span_start is not null"
        ))
        .await;
    assert_eq!(traced, spanned);
    let documents = env
        .count(&format!(
            "select count(*) from {api}.canonical_entities where document_id is not null"
        ))
        .await;
    assert!(documents > 0);
    let found = cypher(&env, "match (v) where v.document_id is not null return v").await;
    assert_eq!(found, documents);

    let ids = vertex_ids(&env).await;
    let mut reader = zontologize::db::connect(Some(&common::test_url()), &env.schema)
        .await
        .unwrap();
    let edge_count = format!(
        "select count(*) from \"{}_graph\"._ag_label_edge",
        env.schema
    );
    sqlx::raw_sql("begin isolation level repeatable read")
        .execute(&mut reader)
        .await
        .unwrap();
    let before: i64 = sqlx::query_scalar(sqlx::AssertSqlSafe(edge_count.clone()))
        .fetch_one(&mut reader)
        .await
        .unwrap();
    sqlx::query(
        "update relations set status = 'rejected' \
         where id = (select min(id) from relations where property_iri like '%#describes')",
    )
    .execute(&env.pool)
    .await
    .unwrap();
    let again = sync(&env).await;
    assert_eq!(
        again.edges as i64,
        env.count(&canonical("canonical_relations")).await
    );
    assert!((again.edges as i64) < relations);
    let during: i64 = sqlx::query_scalar(sqlx::AssertSqlSafe(edge_count))
        .fetch_one(&mut reader)
        .await
        .unwrap();
    assert_eq!(during, before);
    sqlx::raw_sql("rollback")
        .execute(&mut reader)
        .await
        .unwrap();
    zontologize::db::close(reader).await;
    assert_eq!(vertex_ids(&env).await, ids);

    common::drop_graph(&format!("{}_graph", env.schema)).await;
    let rebuilt = sync(&env).await;
    assert_eq!(rebuilt.vertices as i64, entities);
    assert_eq!(vertex_ids(&env).await, ids);
    env.finish().await;
}

#[tokio::test]
#[ignore]
async fn merged_entities_are_one_vertex_with_remapped_edges() {
    let env = acme("canonical").await;
    sqlx::raw_sql(
        "insert into entities (class_iri, key, name, method, confidence, ontology_version_id)
             select 'https://example.org/acme#Site', k, n, 'rules', 1, (select max(id) from ontology_versions)
             from (values ('gt-east', 'Eastport Terminal'), ('gt-east-bv', 'Eastport Terminal BV'),
                          ('gt-west', 'Westhaven')) v(k, n);
         update entities set canonical_id = (select id from entities where key = 'gt-east')
             where key in ('gt-east', 'gt-east-bv');
         insert into relations (src_id, property_iri, dst_id, method, status, confidence, document_id)
             select (select id from entities where key = s), p, (select id from entities where key = d),
                 'rules', st, c, (select min(id) from documents)
             from (values ('gt-east-bv', 'https://example.org/acme#locatedNear', 'gt-west', 'auto', 0.6),
                          ('gt-east', 'https://example.org/acme#locatedNear', 'gt-west', 'auto', 0.9),
                          ('gt-east', 'http://www.w3.org/2002/07/owl#sameAs', 'gt-east-bv', 'auto', 1.0),
                          ('gt-west', 'https://example.org/acme#locatedNear', 'gt-east-bv', 'auto', 0.5))
                 v(s, p, d, st, c);",
    )
    .execute(&env.pool)
    .await
    .unwrap();
    sync(&env).await;
    let root = "match (v) where v.name = 'Eastport Terminal' and v.members = 2 \
                and 'Eastport Terminal BV' in v.aliases return v";
    assert_eq!(cypher(&env, root).await, 1);
    let member = "match (v) where v.name = 'Eastport Terminal BV' return v";
    assert_eq!(cypher(&env, member).await, 0);
    let merged = "match (a)-[r]->(b) where a.name = 'Eastport Terminal' and b.name = 'Westhaven' \
                  return r";
    assert_eq!(cypher(&env, merged).await, 1);
    let best = "match (a)-[r]->(b) where a.name = 'Eastport Terminal' and b.name = 'Westhaven' \
                and r.support = 2 and r.confidence = 0.9 return r";
    assert_eq!(cypher(&env, best).await, 1);
    let remapped = "match (a)-[r]->(b) where a.name = 'Westhaven' and b.name = 'Eastport Terminal' \
                    return r";
    assert_eq!(cypher(&env, remapped).await, 1);
    assert_eq!(cypher(&env, "match (a)-[r]->(a) return r").await, 0);

    sqlx::query(
        "insert into aliases (entity_id, alias, method) \
         select id, 'West Haven', 'rules' from entities where key = 'gt-west'",
    )
    .execute(&env.pool)
    .await
    .unwrap();
    let again = sync(&env).await;
    assert!(!again.unchanged, "{again:?}");
    let alias = "match (v) where v.name = 'Westhaven' and 'West Haven' in v.aliases return v";
    assert_eq!(cypher(&env, alias).await, 1);
    env.finish().await;
}

#[tokio::test]
#[ignore]
async fn the_class_overlay_moves_renames_and_drops_clusters() {
    let env = acme("overlay").await;
    sqlx::raw_sql(
        "insert into entities (class_iri, key, name, method, confidence, ontology_version_id)
             select 'https://example.org/acme#Site', k, n, 'rules', 1, (select max(id) from ontology_versions)
             from (values ('ov-east', 'Eastport Terminal'), ('ov-east-bv', 'Eastport Terminal BV'),
                          ('ov-west', 'Westhaven')) v(k, n);
         update entities set canonical_id = (select id from entities where key = 'ov-east')
             where key in ('ov-east', 'ov-east-bv');
         insert into relations (src_id, property_iri, dst_id, method, status, confidence, document_id)
             select (select id from entities where key = 'ov-west'), 'https://example.org/acme#partOf',
                 (select id from entities where key = 'ov-east-bv'), 'rules', 'auto', 0.8,
                 (select min(id) from documents);",
    )
    .execute(&env.pool)
    .await
    .unwrap();
    let api = format!("\"{}_api\"", env.schema);
    let set = |status: &str, class: Option<&str>, name: Option<&str>| {
        let class = class.map_or("null".to_string(), |c| {
            format!("'https://example.org/acme#{c}'")
        });
        let name = name.map_or("null".to_string(), |n| format!("'{n}'"));
        format!(
            "insert into entity_classes (entity_id, class_iri, name, method, status, confidence, \
             ontology_version_id) select id, {class}, {name}, 'curate:test', '{status}', 0.9, \
             ontology_version_id from entities where key = 'ov-east' \
             on conflict (entity_id) do update set class_iri = excluded.class_iri, \
             name = excluded.name, status = excluded.status"
        )
    };
    let root_class = format!(
        "select count(*) from {api}.canonical_entities where key = 'ov-east' \
         and class_iri = 'https://example.org/acme#Project'"
    );
    sync(&env).await;
    sqlx::raw_sql(sqlx::AssertSqlSafe(set("proposed", Some("Project"), None)))
        .execute(&env.pool)
        .await
        .unwrap();
    let proposed = sync(&env).await;
    assert!(!proposed.unchanged);
    assert_eq!(env.count(&root_class).await, 0);
    sqlx::raw_sql(sqlx::AssertSqlSafe(set(
        "auto",
        Some("Project"),
        Some("Eastport"),
    )))
    .execute(&env.pool)
    .await
    .unwrap();
    let moved = sync(&env).await;
    assert!(!moved.unchanged, "{moved:?}");
    assert_eq!(env.count(&root_class).await, 1);
    assert_eq!(
        cypher(
            &env,
            "match (v:Project) where v.name = 'Eastport' and v.members = 2 return v"
        )
        .await,
        1
    );
    assert_eq!(
        cypher(&env, "match (v:Site) where v.key = 'ov-east' return v").await,
        0
    );
    assert_eq!(
        cypher(
            &env,
            "match (a)-[r]->(b:Project) where a.name = 'Westhaven' return r"
        )
        .await,
        1
    );
    sqlx::raw_sql(sqlx::AssertSqlSafe(set("auto", None, None)))
        .execute(&env.pool)
        .await
        .unwrap();
    let dropped = sync(&env).await;
    assert!(!dropped.unchanged);
    assert_eq!(
        env.count(&format!(
            "select count(*) from {api}.canonical_entities where key = 'ov-east'"
        ))
        .await,
        0
    );
    assert_eq!(
        cypher(&env, "match (v) where v.key = 'ov-east' return v").await,
        0
    );
    assert_eq!(
        cypher(
            &env,
            "match (a)-[r]->(b) where a.name = 'Westhaven' return r"
        )
        .await,
        0
    );
    env.finish().await;
}

async fn view_columns(env: &Env) -> Vec<String> {
    sqlx::query_scalar(
        "select table_name || '.' || column_name from information_schema.columns \
         where table_schema = $1 order by table_name, ordinal_position",
    )
    .bind(format!("{}_api", env.schema))
    .fetch_all(&env.pool)
    .await
    .unwrap()
}

#[tokio::test]
#[ignore]
async fn the_views_keep_their_contract() {
    let env = acme("views").await;
    let api = format!("\"{}_api\"", env.schema);
    let actual = view_columns(&env).await;
    let expected: Vec<String> = std::fs::read_to_string(repo("tests/fixtures/api-columns.txt"))
        .unwrap()
        .lines()
        .map(str::to_string)
        .collect();
    for view in expected.iter().map(|c| c.split('.').next().unwrap()) {
        let have: Vec<&String> = actual
            .iter()
            .filter(|c| c.starts_with(&format!("{view}.")))
            .collect();
        let want: Vec<&String> = expected
            .iter()
            .filter(|c| c.starts_with(&format!("{view}.")))
            .collect();
        assert!(
            have.len() >= want.len() && have[..want.len()] == want[..],
            "{view}: {have:?}"
        );
    }
    for view in actual.iter().map(|c| c.split('.').next().unwrap()) {
        env.count(&format!("select count(*) from {api}.{view}"))
            .await;
    }
    let version = env
        .count(&format!("select {api}.contract_version()::bigint"))
        .await;
    assert_eq!(version, 6);

    let documents = env
        .count(&format!("select count(*) from {api}.documents"))
        .await;
    sqlx::query(
        "update documents set status = 'deleted' where id = (select min(id) from documents)",
    )
    .execute(&env.pool)
    .await
    .unwrap();
    let left = env
        .count(&format!("select count(*) from {api}.documents"))
        .await;
    assert_eq!(left, documents - 1);
    let skipped = env
        .count("select count(*) from documents where status = 'skipped'")
        .await;
    let shown = env
        .count(&format!(
            "select count(*) from {api}.documents where status = 'skipped'"
        ))
        .await;
    assert_eq!(shown, skipped);

    let relations = env
        .count(&format!("select count(*) from {api}.relations"))
        .await;
    sqlx::query(
        "update relations set status = 'rejected' where id = (select min(id) from relations)",
    )
    .execute(&env.pool)
    .await
    .unwrap();
    let left = env
        .count(&format!("select count(*) from {api}.relations"))
        .await;
    assert_eq!(left, relations - 1);

    let ancestors = env
        .count(&format!(
            "select count(*) from {api}.class_ancestors where iri like '%#Project' and depth = 0"
        ))
        .await;
    assert_eq!(ancestors, 1);

    let running = format!("select running::int::bigint from {api}.state");
    assert_eq!(env.count(&running).await, 0);
    let lock = zontologize::db::runs::lock(Some(&common::test_url()), &env.schema)
        .await
        .unwrap();
    assert_eq!(env.count(&running).await, 1);
    lock.release().await;
    assert_eq!(env.count(&running).await, 0);
    let scanned = env
        .count(&format!(
            "select (last_scan_finished_at is not null)::int::bigint from {api}.state"
        ))
        .await;
    assert_eq!(scanned, 1);
    env.finish().await;
}

#[tokio::test]
#[ignore]
async fn the_database_runs_the_pinned_age_version() {
    let mut conn = zontologize::db::connect(Some(&common::test_url()), "public")
        .await
        .unwrap();
    let version = zontologize::db::extension_version(&mut conn, "age")
        .await
        .unwrap();
    zontologize::db::close(conn).await;
    zontologize::db::graph::check_age(version.as_deref()).unwrap();
}

#[tokio::test]
#[ignore]
async fn a_label_no_longer_used_is_emptied() {
    let env = acme("label_drop").await;
    sync(&env).await;
    let labels = format!(
        "select count(*) from ag_catalog.ag_label l join ag_catalog.ag_graph g on g.graphid = l.graph \
         where g.name = '{}_graph'",
        env.schema
    );
    let before = env.count(&labels).await;
    let class: String = sqlx::query_scalar(
        "select class_iri from entities where document_id is null group by 1 order by count(*) limit 1",
    )
    .fetch_one(&env.pool)
    .await
    .unwrap();
    let removed = sqlx::query(
        "delete from entities where class_iri = (select class_iri from entities \
         where document_id is null group by 1 order by count(*) limit 1)",
    )
    .execute(&env.pool)
    .await
    .unwrap()
    .rows_affected();
    assert!(removed > 0);
    sync(&env).await;
    assert_eq!(env.count(&labels).await, before);
    let left = cypher(
        &env,
        &format!("match (v) where v.class = '{class}' return v"),
    )
    .await;
    assert_eq!(left, 0);
    env.finish().await;
}

#[tokio::test]
#[ignore]
async fn mentions_map_to_their_page() {
    let env = Env::new("mention_pages").await;
    env.copy_acme_rules();
    env.write_config(&folder_source("docs", &env.data(), "current"));
    std::fs::create_dir_all(env.data().join("a")).unwrap();
    std::fs::copy(
        repo("tests/fixtures/extract/two-pages.pdf"),
        env.data().join("a/two-pages.pdf"),
    )
    .unwrap();
    env.scan().await;
    let api = format!("\"{}_api\"", env.schema);
    let rows: Vec<(i32, String, String)> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "select mp.page_no, substr(p.text, mp.span_start + 1, mp.span_end - mp.span_start), e.name \
         from {api}.mention_pages mp \
         join {api}.pages p on p.content_id = mp.content_id and p.page_no = mp.page_no \
         join {api}.entities e on e.id = mp.entity_id"
    )))
    .fetch_all(&env.pool)
    .await
    .unwrap();
    let order = rows
        .iter()
        .find(|row| row.2 == "P-2021-0042")
        .unwrap_or_else(|| panic!("{rows:?}"));
    assert_eq!(order.0, 2);
    assert_eq!(order.1, "P-2021-0042");
    for (_, slice, name) in &rows {
        assert!(
            slice.eq_ignore_ascii_case(name) || !slice.is_empty(),
            "{rows:?}"
        );
    }
    env.finish().await;
}
