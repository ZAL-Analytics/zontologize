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
    conn: sqlx::PgConnection,
    schema: String,
}

impl WriterLock {
    pub async fn release(mut self) {
        let _ = unlock(&mut self.conn, &self.schema).await;
        super::close(self.conn).await;
    }
}

pub async fn release(lock: Option<WriterLock>) {
    if let Some(lock) = lock {
        lock.release().await;
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

pub async fn unlock(conn: &mut sqlx::PgConnection, schema: &str) -> Result<()> {
    sqlx::query("select pg_advisory_unlock(hashtext('zontologize'), hashtext($1))")
        .bind(schema)
        .execute(&mut *conn)
        .await?;
    Ok(())
}

pub async fn lock(url: Option<&str>, schema: &str) -> Result<WriterLock> {
    let mut conn = super::connect(url, schema).await?;
    if let Err(error) = take_lock(&mut conn, schema).await {
        super::close(conn).await;
        return Err(error);
    }
    Ok(WriterLock {
        conn,
        schema: schema.to_string(),
    })
}

pub async fn track<T, F>(pool: &PgPool, config_hash: &str, key: &str, work: F) -> Result<T>
where
    T: serde::Serialize,
    F: std::future::Future<Output = Result<T>>,
{
    let (run_id, _) = start(pool, config_hash).await?;
    let outcome = work.await;
    let (status, json) = match &outcome {
        Ok(stats) => {
            let mut map = serde_json::Map::new();
            map.insert(key.to_string(), serde_json::to_value(stats)?);
            ("finished", serde_json::Value::Object(map))
        }
        Err(error) => ("failed", serde_json::json!({"error": format!("{error:#}")})),
    };
    finish(pool, run_id, status, &json).await?;
    outcome
}

pub async fn scan_left_unfinished(pool: &PgPool) -> Result<bool> {
    let unfinished = sqlx::query_scalar(
        "select exists(select 1 from runs where status in ('failed', 'aborted') \
         and id > coalesce((select max(id) from runs where status = 'finished' \
         and stats ? 'rules'), 0))",
    )
    .fetch_one(pool)
    .await?;
    Ok(unfinished)
}
