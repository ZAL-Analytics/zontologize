use std::collections::{BTreeMap, HashMap};

use anyhow::Result;
use serde::{Deserialize, Serialize};
use sqlx::pool::PoolConnection;
use sqlx::types::Json;
use sqlx::types::chrono::{DateTime, Utc};
use sqlx::{FromRow, PgPool, Postgres};

use crate::llm::budget::Calibration;
use crate::llm::{Usage, Window, Windows};

#[derive(Debug, Clone)]
pub struct Call<'a> {
    pub job_id: Option<i64>,
    pub provider: &'a str,
    pub model: &'a str,
    pub task: &'a str,
    pub cache_key: Option<&'a str>,
    pub input_chars: i64,
    pub usage: Usage,
    pub subscription: bool,
    pub windows: Windows,
    pub seconds: f64,
    pub status: &'a str,
    pub error: Option<&'a str>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct JobParams {
    pub sample: usize,
    pub seed: String,
}

#[derive(Debug, Clone, FromRow)]
pub struct Job {
    pub id: i64,
    pub task: String,
    pub provider: String,
    pub model: String,
    pub ontology_version_id: Option<i64>,
    pub params: Json<JobParams>,
    pub status: String,
    pub pause_reason: Option<String>,
    pub resume_after: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, FromRow)]
pub struct Item {
    pub ord: i32,
    pub chunk_id: i64,
    pub cache_key: Option<String>,
    pub status: String,
}

pub async fn cached(pool: &PgPool, keys: &[String]) -> Result<HashMap<String, String>> {
    let rows: Vec<(String, String)> =
        sqlx::query_as("select key, response::text from llm_cache where key = any($1)")
            .bind(keys)
            .fetch_all(pool)
            .await?;
    Ok(rows.into_iter().collect())
}

pub async fn record(
    pool: &PgPool,
    call: &Call<'_>,
    response: Option<&str>,
    item: Option<(i32, &str)>,
) -> Result<()> {
    let mut tx = pool.begin().await?;
    let window = |w: Option<Window>| (w.map(|w| w.used), w.map(|w| w.resets_at));
    let (five_use, five_reset) = window(call.windows.five_hour);
    let (seven_use, seven_reset) = window(call.windows.seven_day);
    sqlx::query(
        "insert into llm_calls (job_id, provider, model, task, cache_key, input_chars, \
         input_tokens, cached_input_tokens, output_tokens, cost_usd, subscription, \
         five_hour_use, five_hour_resets_at, seven_day_use, seven_day_resets_at, seconds, \
         status, error) \
         values ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15, $16, $17, $18)",
    )
    .bind(call.job_id)
    .bind(call.provider)
    .bind(call.model)
    .bind(call.task)
    .bind(call.cache_key)
    .bind(call.input_chars)
    .bind(call.usage.input_tokens)
    .bind(call.usage.cached_input_tokens)
    .bind(call.usage.output_tokens)
    .bind(call.usage.cost_usd)
    .bind(call.subscription)
    .bind(five_use)
    .bind(five_reset)
    .bind(seven_use)
    .bind(seven_reset)
    .bind(call.seconds)
    .bind(call.status)
    .bind(call.error)
    .execute(&mut *tx)
    .await?;
    if let (Some(response), Some(key)) = (response, call.cache_key) {
        sqlx::query(
            "insert into llm_cache (key, provider, model, response, input_tokens, output_tokens, \
             cost_usd) values ($1, $2, $3, $4::jsonb, $5, $6, $7) on conflict (key) do nothing",
        )
        .bind(key)
        .bind(call.provider)
        .bind(call.model)
        .bind(response)
        .bind(call.usage.input_tokens)
        .bind(call.usage.output_tokens)
        .bind(call.usage.cost_usd)
        .execute(&mut *tx)
        .await?;
    }
    if let (Some(job), Some((ord, status))) = (call.job_id, item) {
        sqlx::query(
            "update llm_job_items set status = $3, error = $4 where job_id = $1 and ord = $2",
        )
        .bind(job)
        .bind(ord)
        .bind(status)
        .bind(call.error)
        .execute(&mut *tx)
        .await?;
    }
    tx.commit().await?;
    Ok(())
}

pub async fn known_keys(pool: &PgPool, keys: &[String]) -> Result<(Vec<String>, Vec<String>)> {
    let cached = sqlx::query_scalar("select key from llm_cache where key = any($1)")
        .bind(keys)
        .fetch_all(pool)
        .await?;
    let invalid = sqlx::query_scalar(
        "select distinct cache_key from llm_job_items where status = 'invalid' \
         and cache_key = any($1)",
    )
    .bind(keys)
    .fetch_all(pool)
    .await?;
    Ok((cached, invalid))
}

pub async fn calibration(
    pool: &PgPool,
    provider: &str,
    model: &str,
    task: &str,
    limit: i64,
) -> Result<Calibration> {
    let (calls, chars, input_tokens, output_tokens, cost_usd, seconds): (
        i64,
        f64,
        f64,
        f64,
        f64,
        f64,
    ) = sqlx::query_as(
        "with recent as (select * from llm_calls where provider = $1 and model = $2 \
         and task = $3 and status = 'ok' order by id desc limit $4) \
         select count(*), coalesce(sum(input_chars), 0)::float8, \
         coalesce(sum(input_tokens), 0)::float8, coalesce(sum(output_tokens), 0)::float8, \
         coalesce(sum(cost_usd), 0)::float8, coalesce(sum(seconds), 0)::float8 from recent",
    )
    .bind(provider)
    .bind(model)
    .bind(task)
    .bind(limit)
    .fetch_one(pool)
    .await?;
    Ok(Calibration {
        calls,
        chars,
        input_tokens,
        output_tokens,
        cost_usd,
        seconds,
        five_hour_per_usd: use_per_usd(pool, "five_hour", limit).await?,
        seven_day_per_usd: use_per_usd(pool, "seven_day", limit).await?,
    })
}

async fn use_per_usd(pool: &PgPool, window: &str, limit: i64) -> Result<Option<f64>> {
    let (used, reset) = match window {
        "five_hour" => ("five_hour_use", "five_hour_resets_at"),
        _ => ("seven_day_use", "seven_day_resets_at"),
    };
    let sql = format!(
        "with recent as (select id, cost_usd, {used} as used, {reset} as reset from llm_calls \
         where provider = 'claude-code' and subscription and status = 'ok' \
         and {used} is not null order by id desc limit $1), \
         pairs as (select used - lag(used) over (order by id) as rise, cost_usd, reset, \
         lag(reset) over (order by id) as previous from recent) \
         select case when sum(rise) > 0 then sum(rise) / nullif(sum(cost_usd), 0) end \
         from pairs where reset = previous and rise >= 0"
    );
    let value: Option<f64> = sqlx::query_scalar(sqlx::AssertSqlSafe(sql))
        .bind(limit)
        .fetch_one(pool)
        .await?;
    Ok(value)
}

type WindowRow = (
    Option<f64>,
    Option<DateTime<Utc>>,
    Option<f64>,
    Option<DateTime<Utc>>,
);

pub async fn last_windows(pool: &PgPool) -> Result<Option<Windows>> {
    let row: Option<WindowRow> = sqlx::query_as(
        "select five_hour_use, five_hour_resets_at, seven_day_use, seven_day_resets_at \
         from llm_calls where provider = 'claude-code' and five_hour_use is not null \
         order by id desc limit 1",
    )
    .fetch_optional(pool)
    .await?;
    let window = |used: Option<f64>, resets_at: Option<DateTime<Utc>>| {
        Some(Window {
            used: used?,
            resets_at: resets_at?,
        })
    };
    Ok(row.map(|(fu, fr, su, sr)| Windows {
        five_hour: window(fu, fr),
        seven_day: window(su, sr),
    }))
}

const JOB_COLUMNS: &str = "id, task, provider, model, ontology_version_id, params, status, \
    pause_reason, resume_after";

pub async fn open_job(pool: &PgPool, task: &str) -> Result<Option<Job>> {
    let job = sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "select {JOB_COLUMNS} from llm_jobs where task = $1 and status in ('running', 'paused') \
         order by id desc limit 1"
    )))
    .bind(task)
    .fetch_optional(pool)
    .await?;
    Ok(job)
}

pub async fn create_job(
    pool: &PgPool,
    task: &str,
    provider: &str,
    model: &str,
    ontology_version_id: i64,
    params: &JobParams,
    chunk_ids: &[i64],
) -> Result<Job> {
    let mut tx = pool.begin().await?;
    let job: Job = sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "insert into llm_jobs (task, provider, model, ontology_version_id, params) \
         values ($1, $2, $3, $4, $5) returning {JOB_COLUMNS}"
    )))
    .bind(task)
    .bind(provider)
    .bind(model)
    .bind(ontology_version_id)
    .bind(Json(params))
    .fetch_one(&mut *tx)
    .await?;
    sqlx::query(
        "insert into llm_job_items (job_id, ord, chunk_id) \
         select $1, ord::integer, chunk_id from unnest($2::bigint[]) with ordinality as t(chunk_id, ord)",
    )
    .bind(job.id)
    .bind(chunk_ids)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(job)
}

pub async fn replace_items(
    pool: &PgPool,
    job: i64,
    chunk_ids: &[i64],
    keys: &[String],
) -> Result<Vec<i32>> {
    let mut tx = pool.begin().await?;
    sqlx::query("delete from llm_job_items where job_id = $1 and status in ('pending', 'error')")
        .bind(job)
        .execute(&mut *tx)
        .await?;
    let ords = sqlx::query_scalar(
        "insert into llm_job_items (job_id, ord, chunk_id, cache_key) \
         select $1, (select coalesce(max(ord), 0) from llm_job_items where job_id = $1) \
         + t.n::integer, t.chunk_id, t.key \
         from unnest($2::bigint[], $3::text[]) with ordinality as t(chunk_id, key, n) \
         order by t.n returning ord",
    )
    .bind(job)
    .bind(chunk_ids)
    .bind(keys)
    .fetch_all(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(ords)
}

pub async fn add_stats(pool: &PgPool, job: i64, stats: &BTreeMap<String, i64>) -> Result<()> {
    if job == 0 || stats.is_empty() {
        return Ok(());
    }
    let names: Vec<&str> = stats.keys().map(String::as_str).collect();
    let counts: Vec<i64> = stats.values().copied().collect();
    sqlx::query(
        "update llm_jobs set params = params || jsonb_build_object('stats', \
         coalesce(params -> 'stats', '{}'::jsonb) || (select jsonb_object_agg(t.name, \
         coalesce((params -> 'stats' ->> t.name)::bigint, 0) + t.count) \
         from unnest($2::text[], $3::bigint[]) as t(name, count))) where id = $1",
    )
    .bind(job)
    .bind(&names)
    .bind(&counts)
    .execute(pool)
    .await?;
    Ok(())
}

pub async fn task_stats(
    pool: &PgPool,
    task: &str,
    provider: &str,
    model: &str,
) -> Result<BTreeMap<String, i64>> {
    let rows: Vec<(String, i64)> = sqlx::query_as(
        "select s.key, sum(s.value::bigint)::bigint from llm_jobs j, \
         jsonb_each_text(coalesce(j.params -> 'stats', '{}'::jsonb)) as s \
         where j.task = $1 and j.provider = $2 and j.model = $3 group by 1 order by 1",
    )
    .bind(task)
    .bind(provider)
    .bind(model)
    .fetch_all(pool)
    .await?;
    Ok(rows.into_iter().collect())
}

pub async fn set_job(
    pool: &PgPool,
    id: i64,
    status: &str,
    reason: Option<&str>,
    resume_after: Option<DateTime<Utc>>,
) -> Result<()> {
    sqlx::query(
        "update llm_jobs set status = $2, pause_reason = $3, resume_after = $4, \
         updated_at = now() where id = $1",
    )
    .bind(id)
    .bind(status)
    .bind(reason)
    .bind(resume_after)
    .execute(pool)
    .await?;
    Ok(())
}

pub async fn items(pool: &PgPool, job: i64) -> Result<Vec<Item>> {
    let items = sqlx::query_as(
        "select ord, chunk_id, cache_key, status from llm_job_items where job_id = $1 order by ord",
    )
    .bind(job)
    .fetch_all(pool)
    .await?;
    Ok(items)
}

pub async fn set_item_keys(pool: &PgPool, job: i64, ords: &[i32], keys: &[String]) -> Result<()> {
    sqlx::query(
        "update llm_job_items i set cache_key = t.key from unnest($2::integer[], $3::text[]) \
         as t(ord, key) where i.job_id = $1 and i.ord = t.ord",
    )
    .bind(job)
    .bind(ords)
    .bind(keys)
    .execute(pool)
    .await?;
    Ok(())
}

pub async fn set_items_status(
    pool: &PgPool,
    job: i64,
    ords: &[i32],
    status: &str,
    error: Option<&str>,
) -> Result<()> {
    sqlx::query(
        "update llm_job_items set status = $3, error = $4 where job_id = $1 and ord = any($2)",
    )
    .bind(job)
    .bind(ords)
    .bind(status)
    .bind(error)
    .execute(pool)
    .await?;
    Ok(())
}

pub async fn spend(pool: &PgPool, job: i64) -> Result<(f64, i64)> {
    let row = sqlx::query_as(
        "select coalesce(sum(cost_usd) filter (where not subscription), 0)::float8, count(*) \
         from llm_calls where job_id = $1",
    )
    .bind(job)
    .fetch_one(pool)
    .await?;
    Ok(row)
}

pub async fn lock(conn: &mut PoolConnection<Postgres>, job: i64) -> Result<bool> {
    let locked = sqlx::query_scalar(
        "select pg_try_advisory_lock(hashtextextended(current_schema() || ':llm_job:' || $1, 0))",
    )
    .bind(job)
    .fetch_one(&mut **conn)
    .await?;
    Ok(locked)
}

#[derive(Debug, FromRow)]
pub struct ModelUsage {
    pub provider: String,
    pub model: String,
    pub calls: i64,
    pub input_tokens: i64,
    pub output_tokens: i64,
    pub cost_usd: f64,
    pub week_calls: i64,
    pub week_cost_usd: f64,
}

pub async fn usage(pool: &PgPool) -> Result<Vec<ModelUsage>> {
    let rows = sqlx::query_as(
        "select provider, model, count(*) as calls, \
         coalesce(sum(input_tokens), 0)::bigint as input_tokens, \
         coalesce(sum(output_tokens), 0)::bigint as output_tokens, \
         coalesce(sum(cost_usd), 0)::float8 as cost_usd, \
         count(*) filter (where created_at > now() - interval '7 days') as week_calls, \
         coalesce(sum(cost_usd) filter (where created_at > now() - interval '7 days'), 0)::float8 \
         as week_cost_usd \
         from llm_calls group by 1, 2 order by 1, 2",
    )
    .fetch_all(pool)
    .await?;
    Ok(rows)
}

#[derive(Debug, FromRow)]
pub struct JobState {
    pub id: i64,
    pub task: String,
    pub provider: String,
    pub model: String,
    pub status: String,
    pub pause_reason: Option<String>,
    pub resume_after: Option<DateTime<Utc>>,
    pub done: i64,
    pub pending: i64,
    pub invalid: i64,
    pub error: i64,
}

pub async fn jobs(pool: &PgPool, limit: i64) -> Result<Vec<JobState>> {
    let rows = sqlx::query_as(
        "select j.id, j.task, j.provider, j.model, j.status, j.pause_reason, j.resume_after, \
         count(*) filter (where i.status = 'done') as done, \
         count(*) filter (where i.status = 'pending') as pending, \
         count(*) filter (where i.status = 'invalid') as invalid, \
         count(*) filter (where i.status = 'error') as error \
         from llm_jobs j left join llm_job_items i on i.job_id = j.id \
         group by j.id order by j.id desc limit $1",
    )
    .bind(limit)
    .fetch_all(pool)
    .await?;
    Ok(rows)
}

pub async fn totals(pool: &PgPool, job: i64) -> Result<(i64, i64, f64)> {
    let row = sqlx::query_as(
        "select coalesce(sum(input_tokens), 0)::bigint, coalesce(sum(output_tokens), 0)::bigint, \
         coalesce(sum(cost_usd), 0)::float8 from llm_calls where job_id = $1",
    )
    .bind(job)
    .fetch_one(pool)
    .await?;
    Ok(row)
}
