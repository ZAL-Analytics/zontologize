pub mod common;

use common::{Env, folder_source, repo};
use zontologize::fit::run::{Options, Outcome, run};

const BOOSTER: &str = "Inspection report for the north site. The booster pump (pomp in the Dutch \
    manual) showed wear on the impeller. The valve on the return line was replaced and the \
    pressure sensor was checked. Next inspection of the booster pump in March.";
const OTHER: &str = "Meeting notes about the yearly budget. The team talked about holidays, the \
    canteen menu and the new parking rules for visitors. Nothing about any machine at all was \
    discussed in this meeting, which ended early.";

fn options(script: &str) -> Options {
    let mut options = Options::new(repo(&format!("tests/fixtures/llm/claude-code/{script}")));
    options.backoff = std::time::Duration::from_millis(1);
    options.no_wait = true;
    options
}

async fn setup(name: &str) -> Env {
    let env = Env::new(name).await;
    for n in 1..=3 {
        env.file(
            &format!("site/report {n}.txt"),
            &format!("{BOOSTER} Report {n}."),
        );
    }
    env.file("office/notes.txt", OTHER);
    let out = env.dir.join("fit");
    env.write_config(&format!(
        "[models]\nfit = \"claude-code:haiku\"\n\
         [providers.claude-code]\nconcurrency = 1\n\
         [fit]\nsample = 10\nmin_chars = 50\nmin_evidence = 2\nout = \"{}\"\n\
         proposal_namespace = \"urn:acme:proposal#\"\n{}",
        out.display(),
        folder_source("docs", &env.data(), "current")
    ));
    env.scan().await;
    env
}

async fn fit(env: &Env, options: &Options) -> anyhow::Result<Outcome> {
    run(Some(&env.config()), Some(&common::test_url()), options).await
}

async fn facts(env: &Env) -> i64 {
    env.count(
        "select (select count(*) from entities) + (select count(*) from mentions) \
         + (select count(*) from class_mentions) + (select count(*) from relations)",
    )
    .await
}

#[tokio::test]
#[ignore]
async fn fit_runs_to_the_end_and_a_rerun_sends_nothing() {
    let env = setup("fit_end").await;
    let before = facts(&env).await;
    let mut dry = options("fake-claude.sh");
    dry.dry_run = true;
    assert_eq!(fit(&env, &dry).await.unwrap(), Outcome::DryRun);
    assert_eq!(env.count("select count(*) from llm_jobs").await, 0);
    assert_eq!(env.count("select count(*) from llm_calls").await, 0);

    assert_eq!(
        fit(&env, &options("fake-claude.sh")).await.unwrap(),
        Outcome::Finished
    );
    assert_eq!(env.count("select count(*) from llm_job_items").await, 4);
    assert_eq!(env.count("select count(*) from llm_calls").await, 4);
    assert_eq!(env.count("select count(*) from llm_cache").await, 4);
    assert_eq!(
        env.count("select count(*) from llm_jobs where status = 'finished'")
            .await,
        1
    );
    let report = std::fs::read_to_string(env.dir.join("fit/fit-report.md")).unwrap();
    assert!(report.contains("acme#Pump"), "{report}");
    assert!(!report.contains("Partial"), "{report}");
    let turtle = std::fs::read_to_string(env.dir.join("fit/proposals.ttl")).unwrap();
    assert!(turtle.contains("BoosterPump"), "{turtle}");
    assert!(turtle.contains("\"pomp\"@nl"), "{turtle}");
    assert!(!turtle.contains("impeller"), "{turtle}");
    assert_eq!(facts(&env).await, before);

    let mut again = options("fake-claude.sh");
    again.new = true;
    assert_eq!(fit(&env, &again).await.unwrap(), Outcome::Finished);
    assert_eq!(env.count("select count(*) from llm_calls").await, 4);
    assert_eq!(
        env.count("select count(*) from llm_jobs where status = 'abandoned'")
            .await,
        0
    );
    env.finish().await;
}

#[tokio::test]
#[ignore]
async fn full_window_pauses_and_the_next_call_resumes() {
    let env = setup("fit_window").await;
    assert_eq!(
        fit(&env, &options("fake-claude-full.sh")).await.unwrap(),
        Outcome::Paused
    );
    assert_eq!(env.count("select count(*) from llm_calls").await, 1);
    assert_eq!(
        env.count(
            "select count(*) from llm_jobs where status = 'paused' \
             and resume_after > '2029-12-31'"
        )
        .await,
        1
    );
    assert!(
        std::fs::read_to_string(env.dir.join("fit/fit-report.md"))
            .unwrap()
            .contains("Partial: 1 of 4")
    );
    assert_eq!(
        fit(&env, &options("fake-claude.sh")).await.unwrap(),
        Outcome::Paused
    );
    assert_eq!(env.count("select count(*) from llm_calls").await, 1);
    sqlx::query(
        "update llm_calls set five_hour_resets_at = now() - interval '1 hour', \
         seven_day_resets_at = now() - interval '1 hour'",
    )
    .execute(&env.pool)
    .await
    .unwrap();
    sqlx::query("update llm_jobs set resume_after = now() - interval '1 minute'")
        .execute(&env.pool)
        .await
        .unwrap();
    assert_eq!(
        fit(&env, &options("fake-claude.sh")).await.unwrap(),
        Outcome::Finished
    );
    assert_eq!(env.count("select count(*) from llm_calls").await, 4);
    env.finish().await;
}

#[tokio::test]
#[ignore]
async fn caps_stop_a_job_and_a_changed_sample_needs_new() {
    let env = setup("fit_caps").await;
    let mut capped = options("fake-claude.sh");
    capped.max_requests = Some(1);
    assert_eq!(fit(&env, &capped).await.unwrap(), Outcome::Paused);
    assert_eq!(env.count("select count(*) from llm_calls").await, 1);
    assert_eq!(
        env.count("select count(*) from llm_jobs where pause_reason like '%--max-requests%'")
            .await,
        1
    );
    let mut other = options("fake-claude.sh");
    other.sample = Some(3);
    let error = fit(&env, &other).await.unwrap_err().to_string();
    assert!(error.contains("--new"), "{error}");
    assert_eq!(
        fit(&env, &options("fake-claude.sh")).await.unwrap(),
        Outcome::Finished
    );
    assert_eq!(env.count("select count(*) from llm_calls").await, 4);
    other.new = true;
    assert_eq!(fit(&env, &other).await.unwrap(), Outcome::Finished);
    assert_eq!(env.count("select count(*) from llm_jobs").await, 2);
    assert_eq!(env.count("select count(*) from llm_calls").await, 4);
    env.finish().await;
}

#[tokio::test]
#[ignore]
async fn missing_claude_binary_stops_the_job_with_its_path() {
    let env = setup("fit_missing").await;
    let error = fit(&env, &options("does-not-exist"))
        .await
        .unwrap_err()
        .to_string();
    assert!(error.contains("does-not-exist"), "{error}");
    assert_eq!(
        env.count("select count(*) from llm_jobs where status = 'paused'")
            .await,
        1
    );
    env.finish().await;
}
