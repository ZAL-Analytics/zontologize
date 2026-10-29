pub mod migrate;

use std::str::FromStr;

use anyhow::{Context, Result};
use sqlx::postgres::{PgConnectOptions, PgConnection};
use sqlx::{ConnectOptions, Connection};

pub const EXTENSIONS: [&str; 2] = ["vector", "age"];

pub async fn connect(url: Option<&str>, schema: &str) -> Result<PgConnection> {
    let options = match url {
        Some(url) => PgConnectOptions::from_str(url).context("database URL is not valid")?,
        None => PgConnectOptions::new(),
    };
    options
        .options([("search_path", schema)])
        .connect()
        .await
        .context("cannot connect to PostgreSQL (DATABASE_URL or PG* variables)")
}

pub async fn server_version(conn: &mut PgConnection) -> Result<String> {
    let version: String = sqlx::query_scalar("select version()")
        .fetch_one(&mut *conn)
        .await?;
    Ok(version)
}

pub async fn extension_version(conn: &mut PgConnection, name: &str) -> Result<Option<String>> {
    let version = sqlx::query_scalar("select extversion from pg_extension where extname = $1")
        .bind(name)
        .fetch_optional(&mut *conn)
        .await?;
    Ok(version)
}

pub async fn schema_exists(conn: &mut PgConnection, schema: &str) -> Result<bool> {
    let exists = sqlx::query_scalar("select exists(select 1 from pg_namespace where nspname = $1)")
        .bind(schema)
        .fetch_one(&mut *conn)
        .await?;
    Ok(exists)
}

pub fn quote_identifier(name: &str) -> String {
    format!("\"{}\"", name.replace('"', "\"\""))
}

pub async fn close(conn: PgConnection) {
    let _ = conn.close().await;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quotes_identifiers() {
        assert_eq!(quote_identifier("zontologize"), "\"zontologize\"");
        assert_eq!(quote_identifier("a\"b"), "\"a\"\"b\"");
    }
}
