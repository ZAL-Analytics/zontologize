pub mod common;

use common::Env;

#[tokio::test]
#[ignore]
async fn eval_entities_counts_duplicates_merges_and_classes() {
    let env = Env::new("eval").await;
    env.write_config("[eval]\nworld = \"world.csv\"\n");
    std::fs::write(
        env.config().join("world.csv"),
        "name,variant,class\n\
         Acme BV,Acme BV,acme:Customer\n\
         Acme BV,Acme B.V.,acme:Customer\n\
         Brix Ltd,Brix Ltd,acme:Customer\n\
         Eastport,Eastport,acme:Site\n\
         Westhaven,Westhaven,acme:Site\n",
    )
    .unwrap();
    sqlx::raw_sql(
        "insert into ontology_versions (sha256, files) values ('x', '[]');
         insert into entities (class_iri, key, name, method, confidence, ontology_version_id)
         values ('https://example.org/acme#Customer', 'acme bv', 'Acme BV', 'rules', 1, 1),
                ('https://example.org/acme#Customer', 'acme', 'Acme', 'rules', 1, 1),
                ('https://example.org/acme#Site', 'brix', 'Brix', 'rules', 1, 1),
                ('https://example.org/acme#Site', 'eastport', 'Eastport', 'rules', 1, 1),
                ('https://example.org/acme#Site', 'westhaven', 'Westhaven', 'rules', 1, 1);
         insert into aliases (entity_id, alias, method) values (2, 'Acme B.V.', 'gazetteer'),
                (3, 'Brix Ltd', 'gazetteer');
         update entities set canonical_id = 4 where id in (4, 5);",
    )
    .execute(&env.pool)
    .await
    .unwrap();
    let scores = zontologize::eval::run(Some(&env.config()), Some(&common::test_url()))
        .await
        .unwrap();
    let table = zontologize::eval::table(&scores);
    let customer = &scores[0];
    assert_eq!(customer.class, "acme:Customer", "{table}");
    assert_eq!(
        (customer.world, customer.found, customer.duplicates),
        (2, 2, 1),
        "{table}"
    );
    assert_eq!((customer.matched, customer.right_class), (3, 2), "{table}");
    let site = &scores[1];
    assert_eq!(
        (site.world, site.found, site.wrong_merges),
        (2, 2, 1),
        "{table}"
    );
    let total = &scores[2];
    assert_eq!((total.world, total.wrong_merges), (4, 1), "{table}");
    assert!(table.contains("acme:Customer"), "{table}");
    env.finish().await;
}
