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
