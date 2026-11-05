pub mod api;
pub mod attributes;
pub mod classify;
pub mod contents;
pub mod documents;
pub mod embed;
pub mod entities;
pub mod facts;
pub mod fit;
pub mod graph;
pub mod llm;
pub mod migrate;
pub mod ontology;
pub mod pool;
pub mod resolve;
pub mod review;
pub mod runs;
pub mod status;

use std::str::FromStr;

use anyhow::{Context, Result};
use sqlx::postgres::{PgConnectOptions, PgConnection};
use sqlx::{ConnectOptions, Connection};

pub const EXTENSIONS: [&str; 2] = ["vector", "age"];

const VECTOR_PATH: &str = "select set_config('search_path', \
    current_setting('search_path') || ', ' || quote_ident(n.nspname), false) \
    from pg_extension e join pg_namespace n on n.oid = e.extnamespace \
    where e.extname = 'vector' and n.nspname <> all(current_schemas(false))";

pub async fn connect(url: Option<&str>, schema: &str) -> Result<PgConnection> {
    let options = match url {
        Some(url) => PgConnectOptions::from_str(url).context("database URL is not valid")?,
        None => PgConnectOptions::new(),
    };
    let mut conn = options
        .options([("search_path", schema)])
        .connect()
        .await
        .context("cannot connect to PostgreSQL (DATABASE_URL or PG* variables)")?;
    add_vector_path(&mut conn).await?;
    Ok(conn)
}

pub async fn add_vector_path(conn: &mut PgConnection) -> Result<(), sqlx::Error> {
    sqlx::query(VECTOR_PATH).execute(&mut *conn).await?;
    Ok(())
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

pub async fn schema_exists_in(pool: &sqlx::PgPool, schema: &str) -> Result<bool> {
    let exists = sqlx::query_scalar("select exists(select 1 from pg_namespace where nspname = $1)")
        .bind(schema)
        .fetch_one(pool)
        .await?;
    Ok(exists)
}

pub fn quote_identifier(name: &str) -> String {
    format!("\"{}\"", name.replace('"', "\"\""))
}

pub fn copy_field(out: &mut String, value: Option<&str>) {
    let Some(value) = value else {
        out.push_str("\\N");
        return;
    };
    for c in value.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '\t' => out.push_str("\\t"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\0' => {}
            c => out.push(c),
        }
    }
}

pub fn copy_row(out: &mut String, fields: &[Option<&str>]) {
    for (i, field) in fields.iter().enumerate() {
        if i > 0 {
            out.push('\t');
        }
        copy_field(out, *field);
    }
    out.push('\n');
}

pub async fn close(conn: PgConnection) {
    let _ = conn.close().await;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn copy_rows_escape_text_format() {
        let mut out = String::new();
        copy_row(&mut out, &[Some("a\tb\\c\nd\0"), None, Some("")]);
        assert_eq!(out, "a\\tb\\\\c\\nd\t\\N\t\n");
    }

    #[test]
    fn quotes_identifiers() {
        assert_eq!(quote_identifier("zontologize"), "\"zontologize\"");
        assert_eq!(quote_identifier("a\"b"), "\"a\"\"b\"");
    }
}
