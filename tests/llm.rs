pub mod common;

use common::Env;
use sqlx::types::chrono::{TimeZone, Utc};
use zontologize::db::llm::{self, Call, JobParams};
use zontologize::llm::{Usage, Window, Windows};

fn call<'a>(key: Option<&'a str>, chars: i64, usage: Usage, five_hour: f64) -> Call<'a> {
    Call {
        job_id: None,
        provider: "claude-code",
        model: "haiku",
        task: "fit",
        cache_key: key,
        input_chars: chars,
        usage,
        subscription: true,
        windows: Windows {
            five_hour: Some(Window {
                used: five_hour,
                resets_at: Utc.with_ymd_and_hms(2030, 1, 1, 15, 0, 0).unwrap(),
            }),
            seven_day: None,
        },
        seconds: 2.0,
        status: "ok",
        error: None,
    }
}

#[tokio::test]
#[ignore]
async fn cache_round_trip_and_calibration() {
    let env = Env::new("llm_cache").await;
    let usage = Usage {
        input_tokens: 500,
        cached_input_tokens: 100,
        output_tokens: 50,
        cost_usd: 0.01,
    };
    for i in 0..10 {
        let key = format!("k{i}");
        let response = format!("{{\"n\": {i}}}");
        let used = 0.10 + 0.01 * i as f64;
        llm::record(
            &env.pool,
            &call(Some(&key), 2000, usage, used),
            Some(&response),
            None,
        )
        .await
        .unwrap();
    }
    llm::record(
        &env.pool,
        &call(Some("k0"), 2000, usage, 0.2),
        Some("{\"n\": 99}"),
        None,
    )
    .await
    .unwrap();
    let cached = llm::cached(
        &env.pool,
        &["k0".to_string(), "k3".to_string(), "x".to_string()],
    )
    .await
    .unwrap();
    assert_eq!(cached.len(), 2);
    assert_eq!(cached["k3"], "{\"n\": 3}");
    assert_eq!(cached["k0"], "{\"n\": 0}");
    let calibration = llm::calibration(&env.pool, "claude-code", "haiku", "fit", 200)
        .await
        .unwrap();
    assert_eq!(calibration.calls, 11);
    assert_eq!(calibration.input_tokens / calibration.chars, 0.25);
    assert_eq!(calibration.output_tokens, 550.0);
    let per_usd = calibration.five_hour_per_usd.unwrap();
    assert!((per_usd - 1.0).abs() < 1e-6, "{per_usd}");
    let windows = llm::last_windows(&env.pool).await.unwrap().unwrap();
    assert_eq!(windows.five_hour.unwrap().used, 0.2);
    env.finish().await;
}

#[tokio::test]
#[ignore]
async fn a_job_is_locked_by_one_process_only() {
    let env = Env::new("llm_lock").await;
    sqlx::query("insert into runs (started_at, config_hash) values (now(), 'x')")
        .execute(&env.pool)
        .await
        .unwrap();
    sqlx::query("insert into ontology_versions (sha256, files) values ('v', '[]')")
        .execute(&env.pool)
        .await
        .unwrap();
    let version: i64 = sqlx::query_scalar("select id from ontology_versions")
        .fetch_one(&env.pool)
        .await
        .unwrap();
    sqlx::query("insert into contents (sha256) values ('c')")
        .execute(&env.pool)
        .await
        .unwrap();
    let chunks: Vec<i64> = sqlx::query_scalar(
        "insert into chunks (content_id, ord, text, ts_config) \
         select (select id from contents), n, 'text', 'simple' from generate_series(1, 3) n \
         returning id",
    )
    .fetch_all(&env.pool)
    .await
    .unwrap();
    let params = JobParams {
        sample: 3,
        seed: "s".to_string(),
    };
    let job = llm::create_job(&env.pool, "fit", "ollama", "m", version, &params, &chunks)
        .await
        .unwrap();
    let items = llm::items(&env.pool, job.id).await.unwrap();
    assert_eq!(items.iter().map(|i| i.chunk_id).collect::<Vec<_>>(), chunks);
    assert_eq!(items[0].ord, 1);
    assert_eq!(
        llm::open_job(&env.pool, "fit").await.unwrap().unwrap().id,
        job.id
    );
    let mut first = env.pool.acquire().await.unwrap();
    let mut second = env.pool.acquire().await.unwrap();
    assert!(llm::lock(&mut first, job.id).await.unwrap());
    assert!(!llm::lock(&mut second, job.id).await.unwrap());
    assert!(llm::lock(&mut second, job.id + (1i64 << 32)).await.unwrap());
    drop(first);
    drop(second);
    env.finish().await;
}
