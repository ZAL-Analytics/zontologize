use anyhow::{Context, Result, bail};
use sqlx::postgres::PgConnection;
use sqlx::types::chrono::{DateTime, Utc};
use sqlx::{ConnectOptions, PgPool};

use super::quote_identifier;

pub const COLUMNS: [&str; 6] = ["id", "path", "file", "sha256", "size", "modified"];

const CURSOR: &str = "zontologize_view_rows";

pub type ViewRow = (
    String,
    String,
    Option<String>,
    Option<String>,
    i64,
    DateTime<Utc>,
);

pub async fn check_view(pool: &PgPool, view: &str) -> Result<String> {
    let Some((schema, name)) = view.split_once('.') else {
        bail!("view {view:?} must be schema.view");
    };
    let columns: Vec<String> = sqlx::query_scalar(
        "select column_name::text from information_schema.columns \
         where table_schema = $1 and table_name = $2",
    )
    .bind(schema)
    .bind(name)
    .fetch_all(pool)
    .await?;
    if columns.is_empty() {
        bail!("view {view} does not exist or is not readable");
    }
    for column in COLUMNS {
        if !columns.iter().any(|c| c == column) {
            bail!("view {view} has no column {column}");
        }
    }
    Ok(format!(
        "{}.{}",
        quote_identifier(schema),
        quote_identifier(name)
    ))
}

pub struct ViewRows {
    conn: PgConnection,
    fetch: String,
}

pub async fn open(pool: &PgPool, qualified: &str, batch: usize) -> Result<ViewRows> {
    let mut conn = pool
        .connect_options()
        .connect()
        .await
        .context("cannot open a connection to read the view")?;
    sqlx::raw_sql("begin isolation level repeatable read read only")
        .execute(&mut conn)
        .await?;
    let declare = format!(
        "declare {CURSOR} no scroll cursor for \
         select id::text, path::text, file::text, sha256::text, size::bigint, \
         modified::timestamptz from {qualified} order by id::text"
    );
    sqlx::raw_sql(sqlx::AssertSqlSafe(declare))
        .execute(&mut conn)
        .await?;
    Ok(ViewRows {
        conn,
        fetch: format!("fetch forward {} from {CURSOR}", batch.max(1)),
    })
}

impl ViewRows {
    pub async fn next(&mut self) -> Result<Vec<ViewRow>> {
        let rows = sqlx::query_as(sqlx::AssertSqlSafe(self.fetch.clone()))
            .fetch_all(&mut self.conn)
            .await?;
        Ok(rows)
    }

    pub async fn close(self) {
        super::close(self.conn).await;
    }
}
