use anyhow::Result;
use sqlx::PgPool;

pub async fn start(pool: &PgPool, config_hash: &str) -> Result<(i64, u64)> {
    let aborted = sqlx::query(
        "update runs set status = 'aborted', finished_at = now() where status = 'running'",
    )
    .execute(pool)
    .await?
    .rows_affected();
    let id = sqlx::query_scalar(
        "insert into runs (started_at, config_hash, status) values (now(), $1, 'running') \
         returning id",
    )
    .bind(config_hash)
    .fetch_one(pool)
    .await?;
    Ok((id, aborted))
}

pub async fn finish(pool: &PgPool, id: i64, status: &str, stats: &serde_json::Value) -> Result<()> {
    sqlx::query("update runs set status = $2, finished_at = now(), stats = $3 where id = $1")
        .bind(id)
        .bind(status)
        .bind(sqlx::types::Json(stats))
        .execute(pool)
        .await?;
    Ok(())
}

pub struct WriterLock {
    conn: Option<sqlx::PgConnection>,
}

impl WriterLock {
    pub async fn release(mut self) {
        if let Some(conn) = self.conn.take() {
            super::close(conn).await;
        }
    }
}

pub async fn take_lock(conn: &mut sqlx::PgConnection, schema: &str) -> Result<()> {
    let taken: bool =
        sqlx::query_scalar("select pg_try_advisory_lock(hashtext('zontologize'), hashtext($1))")
            .bind(schema)
            .fetch_one(&mut *conn)
            .await?;
    if !taken {
        anyhow::bail!(
            "another zontologize run is writing to schema {schema}; try again when it is done"
        );
    }
    Ok(())
}

pub async fn lock(url: Option<&str>, schema: &str) -> Result<WriterLock> {
    let mut conn = super::connect(url, schema).await?;
    if let Err(error) = take_lock(&mut conn, schema).await {
        super::close(conn).await;
        return Err(error);
    }
    Ok(WriterLock { conn: Some(conn) })
}
