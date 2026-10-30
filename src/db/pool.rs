use std::str::FromStr;

use anyhow::{Context, Result};
use sqlx::PgPool;
use sqlx::postgres::{PgConnectOptions, PgPoolOptions};

pub async fn connect(url: Option<&str>, schema: &str, max_connections: u32) -> Result<PgPool> {
    let options = match url {
        Some(url) => PgConnectOptions::from_str(url).context("database URL is not valid")?,
        None => PgConnectOptions::new(),
    };
    PgPoolOptions::new()
        .max_connections(max_connections)
        .connect_with(options.options([("search_path", schema)]))
        .await
        .context("cannot connect to PostgreSQL (DATABASE_URL or PG* variables)")
}
