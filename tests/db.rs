use std::str::FromStr;

use sqlx::postgres::{PgConnectOptions, PgConnection};
use sqlx::{ConnectOptions, Connection};
use zontologize::db;

const TEST_URL: &str = "ZONTOLOGIZE_TEST_DATABASE_URL";
const SCHEMA_PREFIX: &str = "zontologize_test_";

fn test_url() -> String {
    std::env::var(TEST_URL).unwrap_or_else(|_| panic!("{TEST_URL} is not set"))
}

fn schema_name(test: &str) -> String {
    format!("{SCHEMA_PREFIX}{test}_{}", std::process::id())
}

async fn admin() -> PgConnection {
    PgConnectOptions::from_str(&test_url())
        .unwrap()
        .connect()
        .await
        .unwrap()
}

async fn drop_schema(schema: &str) {
    let mut conn = admin().await;
    let sql = format!(
        "drop schema if exists {} cascade",
        db::quote_identifier(schema)
    );
    sqlx::raw_sql(sqlx::AssertSqlSafe(sql))
        .execute(&mut conn)
        .await
        .unwrap();
    conn.close().await.unwrap();
}

async fn tables_in(conn: &mut PgConnection, schema: &str) -> Vec<String> {
    sqlx::query_scalar(
        "select c.relname::text from pg_class c join pg_namespace n on n.oid = c.relnamespace \
         where n.nspname = $1 and c.relkind = 'r' order by 1",
    )
    .bind(schema)
    .fetch_all(conn)
    .await
    .unwrap()
}

async fn objects_outside_test_schemas(conn: &mut PgConnection) -> Vec<String> {
    sqlx::query_scalar(
        "select n.nspname || '.' || c.relname from pg_class c \
         join pg_namespace n on n.oid = c.relnamespace \
         where n.nspname not like $1 and n.nspname not like 'pg\\_toast%' \
         and n.nspname not like 'pg\\_temp%' order by 1",
    )
    .bind(format!("{SCHEMA_PREFIX}%"))
    .fetch_all(conn)
    .await
    .unwrap()
}

async fn migrate(schema: &str) -> anyhow::Result<Vec<i64>> {
    let mut conn = db::connect(Some(&test_url()), schema).await?;
    let result = db::migrate::run(&mut conn, schema).await;
    db::close(conn).await;
    result
}

#[tokio::test]
#[ignore]
async fn first_run_creates_schema_and_tables() {
    let schema = schema_name("first");
    drop_schema(&schema).await;
    let applied = migrate(&schema).await.unwrap();
    assert_eq!(applied, [1]);
    let mut conn = admin().await;
    assert_eq!(
        tables_in(&mut conn, &schema).await,
        ["_sqlx_migrations", "runs"]
    );
    conn.close().await.unwrap();
    drop_schema(&schema).await;
}

#[tokio::test]
#[ignore]
async fn second_run_is_a_no_op() {
    let schema = schema_name("second");
    drop_schema(&schema).await;
    migrate(&schema).await.unwrap();
    let applied = migrate(&schema).await.unwrap();
    assert!(applied.is_empty());
    drop_schema(&schema).await;
}

#[tokio::test]
#[ignore]
async fn pre_created_schema_is_used() {
    let schema = schema_name("precreated");
    drop_schema(&schema).await;
    let mut conn = admin().await;
    let sql = format!("create schema {}", db::quote_identifier(&schema));
    sqlx::raw_sql(sqlx::AssertSqlSafe(sql))
        .execute(&mut conn)
        .await
        .unwrap();
    let marker = format!(
        "create table {}.marker (id int)",
        db::quote_identifier(&schema)
    );
    sqlx::raw_sql(sqlx::AssertSqlSafe(marker))
        .execute(&mut conn)
        .await
        .unwrap();
    assert_eq!(migrate(&schema).await.unwrap(), [1]);
    assert_eq!(
        tables_in(&mut conn, &schema).await,
        ["_sqlx_migrations", "marker", "runs"]
    );
    conn.close().await.unwrap();
    drop_schema(&schema).await;
}

#[tokio::test]
#[ignore]
async fn nothing_is_created_outside_the_schema() {
    let schema = schema_name("outside");
    drop_schema(&schema).await;
    let mut conn = admin().await;
    let before = objects_outside_test_schemas(&mut conn).await;
    migrate(&schema).await.unwrap();
    let after = objects_outside_test_schemas(&mut conn).await;
    assert_eq!(before, after);
    conn.close().await.unwrap();
    drop_schema(&schema).await;
}

#[tokio::test]
#[ignore]
async fn database_without_extensions_names_the_sql() {
    let mut conn = admin().await;
    let database = format!("zontologize_test_noext_{}", std::process::id());
    let create = format!("create database {}", db::quote_identifier(&database));
    if let Err(error) = sqlx::raw_sql(sqlx::AssertSqlSafe(create))
        .execute(&mut conn)
        .await
    {
        eprintln!("skipped: cannot create a database without extensions: {error}");
        return;
    }
    let url = PgConnectOptions::from_str(&test_url())
        .unwrap()
        .database(&database);
    let mut target = url
        .options([("search_path", "zontologize")])
        .connect()
        .await
        .unwrap();
    let error = db::migrate::run(&mut target, "zontologize")
        .await
        .unwrap_err()
        .to_string();
    target.close().await.unwrap();
    let drop = format!("drop database {}", db::quote_identifier(&database));
    sqlx::raw_sql(sqlx::AssertSqlSafe(drop))
        .execute(&mut conn)
        .await
        .unwrap();
    conn.close().await.unwrap();
    assert!(
        error.contains("create extension if not exists age;"),
        "{error}"
    );
    assert!(
        error.contains("create extension if not exists vector;"),
        "{error}"
    );
}
