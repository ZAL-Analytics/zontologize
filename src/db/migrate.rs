use anyhow::{Context, Result, bail};
use sqlx::migrate::Migrator;
use sqlx::postgres::PgConnection;

use super::{EXTENSIONS, extension_version, quote_identifier, schema_exists};

pub static MIGRATOR: Migrator = sqlx::migrate!();

pub struct MigrationState {
    pub version: i64,
    pub description: String,
    pub applied: bool,
}

pub async fn applied_versions(conn: &mut PgConnection, schema: &str) -> Result<Vec<i64>> {
    let table = format!("{}._sqlx_migrations", quote_identifier(schema));
    let exists: bool = sqlx::query_scalar("select to_regclass($1) is not null")
        .bind(&table)
        .fetch_one(&mut *conn)
        .await?;
    if !exists {
        return Ok(Vec::new());
    }
    let versions =
        sqlx::query_scalar("select version from _sqlx_migrations where success order by version")
            .fetch_all(&mut *conn)
            .await?;
    Ok(versions)
}

pub async fn states(conn: &mut PgConnection, schema: &str) -> Result<Vec<MigrationState>> {
    let applied = applied_versions(conn, schema).await?;
    Ok(MIGRATOR
        .iter()
        .map(|migration| MigrationState {
            version: migration.version,
            description: migration.description.to_string(),
            applied: applied.contains(&migration.version),
        })
        .collect())
}

pub fn create_extension_sql(name: &str) -> String {
    format!("create extension if not exists {name};")
}

pub async fn run(conn: &mut PgConnection, schema: &str) -> Result<Vec<i64>> {
    let mut missing = Vec::new();
    for name in EXTENSIONS {
        if extension_version(conn, name).await?.is_none() {
            missing.push(create_extension_sql(name));
        }
    }
    if !missing.is_empty() {
        bail!(
            "missing extensions; a database owner or superuser has to run:\n  {}",
            missing.join("\n  ")
        );
    }
    if !schema_exists(conn, schema).await? {
        let sql = format!("create schema {}", quote_identifier(schema));
        sqlx::raw_sql(sqlx::AssertSqlSafe(sql))
            .execute(&mut *conn)
            .await
            .with_context(|| format!("cannot create schema {schema}"))?;
    }
    let before = applied_versions(conn, schema).await?;
    MIGRATOR
        .run(&mut *conn)
        .await
        .with_context(|| format!("migrations failed in schema {schema}"))?;
    let after = applied_versions(conn, schema).await?;
    Ok(after.into_iter().filter(|v| !before.contains(v)).collect())
}
