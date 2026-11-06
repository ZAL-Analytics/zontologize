use anyhow::{Context, Result};
use sqlx::PgConnection;

use super::{identifier, schema_exists};

const TEMPLATE: &str = include_str!("api.sql");
const CURRENT: &str = "select v.id from {schema}.ontology_versions v \
    join {schema}.runs r on r.id = v.last_run_id \
    where r.status = 'finished' order by r.id desc limit 1";

fn render(schema: &str, api: &str) -> Result<String> {
    let quoted_schema = identifier(schema)?;
    let quoted_api = identifier(api)?;
    Ok(TEMPLATE
        .replace("{current}", CURRENT)
        .replace("{schema_text}", &format!("'{schema}'"))
        .replace("{schema}", &quoted_schema)
        .replace("{api}", &quoted_api))
}

pub async fn create(conn: &mut PgConnection, schema: &str, api: &str) -> Result<()> {
    if !schema_exists(conn, api).await? {
        let sql = format!("create schema {}", identifier(api)?);
        sqlx::raw_sql(sqlx::AssertSqlSafe(sql))
            .execute(&mut *conn)
            .await
            .with_context(|| format!("cannot create schema {api}"))?;
    }
    sqlx::raw_sql(sqlx::AssertSqlSafe(render(schema, api)?))
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
        let sql = render("my_schema", "my_api").unwrap();
        assert!(!sql.contains('{'), "{sql}");
        assert!(sql.contains("\"my_schema\".documents"));
        assert!(sql.contains("\"my_api\".documents"));
        assert!(sql.contains("hashtext('my_schema')"));
    }

    #[test]
    fn names_are_checked() {
        assert!(render("x'); drop schema y; --", "my_api").is_err());
        assert!(render("my_schema", "My\"Api").is_err());
    }
}
