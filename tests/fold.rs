pub mod common;

use common::{Env, folder_source, repo};
use zontologize::db::entities::{Folded, fold_class};

const CUSTOMER: &str = "urn:test#Customer";
const ORGANISATION: &str = "urn:test#Organisation";

async fn setup() -> Env {
    let env = Env::new("fold").await;
    env.copy_acme_rules();
    env.write_config(&folder_source(
        "projects",
        &repo("tests/fixtures/acme/projects"),
        "current",
    ));
    env.scan().await;
    env
}

async fn entity(env: &Env, class: &str, key: &str, name: &str) -> i64 {
    sqlx::query_scalar(
        "insert into entities (class_iri, key, name, method, confidence, ontology_version_id) \
         select $1, $2, $3, 'test', 1.0, max(id) from ontology_versions returning id",
    )
    .bind(class)
    .bind(key)
    .bind(name)
    .fetch_one(&env.pool)
    .await
    .unwrap()
}

async fn document(env: &Env) -> i64 {
    sqlx::query_scalar("select min(id) from documents")
        .fetch_one(&env.pool)
        .await
        .unwrap()
}

async fn count(env: &Env, sql: &'static str, id: i64) -> i64 {
    sqlx::query_scalar(sql)
        .bind(id)
        .fetch_one(&env.pool)
        .await
        .unwrap()
}

#[tokio::test]
#[ignore]
async fn a_class_folds_into_entities_with_the_same_key_or_moves() {
    let env = setup().await;
    let doc = document(&env).await;
    let organisation = entity(&env, ORGANISATION, "northwind", "Northwind").await;
    let customer = entity(&env, CUSTOMER, "northwind", "Northwind Ltd").await;
    let alone = entity(&env, CUSTOMER, "contoso", "Contoso").await;
    let project = entity(&env, "urn:test#Project", "P-1", "P-1").await;
    sqlx::query("insert into aliases (entity_id, alias, method) values ($1, 'NW', 'test')")
        .bind(customer)
        .execute(&env.pool)
        .await
        .unwrap();
    sqlx::query(
        "insert into mentions (entity_id, document_id, span_start, span_end, method, confidence) \
         values ($1, $2, 0, 4, 'test', 1.0)",
    )
    .bind(customer)
    .bind(doc)
    .execute(&env.pool)
    .await
    .unwrap();
    sqlx::query(
        "insert into relations (src_id, property_iri, dst_id, method, confidence, document_id) \
         values ($1, 'urn:test#forCustomer', $2, 'test', 1.0, $3), \
         ($2, 'http://www.w3.org/2002/07/owl#sameAs', $4, 'test', 1.0, $3)",
    )
    .bind(project)
    .bind(customer)
    .bind(doc)
    .bind(organisation)
    .execute(&env.pool)
    .await
    .unwrap();

    let mut conn = env.pool.acquire().await.unwrap();
    let folded = fold_class(&mut conn, CUSTOMER, ORGANISATION).await.unwrap();
    drop(conn);

    assert_eq!(
        folded,
        Folded {
            merged: 1,
            moved: 1
        }
    );
    assert_eq!(
        count(
            &env,
            "select count(*) from entities where id = $1",
            customer
        )
        .await,
        0
    );
    let aliases = "select count(*) from aliases where entity_id = $1 \
        and alias in ('NW', 'Northwind Ltd')";
    assert_eq!(count(&env, aliases, organisation).await, 2);
    let mentions = "select count(*) from mentions where entity_id = $1";
    assert_eq!(count(&env, mentions, organisation).await, 1);
    let targets = "select count(*) from relations where dst_id = $1 \
        and property_iri = 'urn:test#forCustomer'";
    assert_eq!(count(&env, targets, organisation).await, 1);
    let loops = "select count(*) from relations where src_id = $1 and dst_id = $1";
    assert_eq!(count(&env, loops, organisation).await, 0);
    let class: String = sqlx::query_scalar("select class_iri from entities where id = $1")
        .bind(alone)
        .fetch_one(&env.pool)
        .await
        .unwrap();
    assert_eq!(class, ORGANISATION);
}
