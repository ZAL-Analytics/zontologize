pub mod common;

use common::Env;

#[tokio::test]
#[ignore]
async fn stemmed_text_and_trigram_names_are_searchable() {
    let env = Env::new("search").await;
    let stemmed: bool = sqlx::query_scalar(
        "select (to_tsvector('dutch'::regconfig, 'twee lagers vervangen') \
         || to_tsvector('english'::regconfig, 'twee lagers vervangen')) \
         @@ (plainto_tsquery('dutch'::regconfig, 'lager') || plainto_tsquery('english'::regconfig, 'lager'))",
    )
    .fetch_one(&env.pool)
    .await
    .unwrap();
    assert!(stemmed);
    let column: String = sqlx::query_scalar(
        "select pg_get_expr(d.adbin, d.adrelid) from pg_attrdef d \
         join pg_attribute a on a.attrelid = d.adrelid and a.attnum = d.adnum \
         where d.adrelid = 'chunks'::regclass and a.attname = 'tsv_stem'",
    )
    .fetch_one(&env.pool)
    .await
    .unwrap();
    assert!(
        column.contains("dutch") && column.contains("english"),
        "{column}"
    );
    let close: f32 = sqlx::query_scalar("select similarity(lower('Nortwind'), lower('Northwind'))")
        .fetch_one(&env.pool)
        .await
        .unwrap();
    assert!(close > 0.3, "{close}");
    let indexes: i64 = sqlx::query_scalar(
        "select count(*) from pg_indexes where schemaname = current_schema() \
         and indexname in ('entities_name_trgm', 'aliases_alias_trgm', 'chunks_tsv_stem')",
    )
    .fetch_one(&env.pool)
    .await
    .unwrap();
    assert_eq!(indexes, 3);
    let sql = format!("select {}_api.contract_version()", env.schema);
    let contract: i32 = sqlx::query_scalar(sqlx::AssertSqlSafe(sql))
        .fetch_one(&env.pool)
        .await
        .unwrap();
    assert_eq!(contract, 6);
}
