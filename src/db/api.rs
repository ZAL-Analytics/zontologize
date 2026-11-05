use anyhow::{Context, Result};
use sqlx::PgConnection;

use super::{quote_identifier, schema_exists};

const TEMPLATE: &str = include_str!("api.sql");
const CURRENT: &str = "select v.id from {schema}.ontology_versions v \
    join {schema}.runs r on r.id = v.last_run_id \
    where r.status = 'finished' order by r.id desc limit 1";

pub fn render(schema: &str, api: &str) -> String {
    TEMPLATE
        .replace("{current}", CURRENT)
        .replace("{schema_text}", &format!("'{schema}'"))
        .replace("{schema}", &quote_identifier(schema))
        .replace("{api}", &quote_identifier(api))
}

pub async fn create(conn: &mut PgConnection, schema: &str, api: &str) -> Result<()> {
    if !schema_exists(conn, api).await? {
        let sql = format!("create schema {}", quote_identifier(api));
        sqlx::raw_sql(sqlx::AssertSqlSafe(sql))
            .execute(&mut *conn)
            .await
            .with_context(|| format!("cannot create schema {api}"))?;
    }
    sqlx::raw_sql(sqlx::AssertSqlSafe(render(schema, api)))
        .execute(&mut *conn)
        .await
        .with_context(|| format!("cannot create the views in schema {api}"))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_placeholder_is_filled() {
        let sql = render("my_schema", "my_api");
        assert!(!sql.contains('{'), "{sql}");
        assert!(sql.contains("\"my_schema\".documents"));
        assert!(sql.contains("\"my_api\".documents"));
        assert!(sql.contains("hashtext('my_schema')"));
    }
}
