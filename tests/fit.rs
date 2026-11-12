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
    options.backoff = Some(std::time::Duration::from_millis(1));
    options.no_wait = true;
    options
}

async fn setup(name: &str) -> Env {
    setup_with(name, "").await
}

async fn setup_with(name: &str, claude_code: &str) -> Env {
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
         [providers.claude-code]\nconcurrency = 1\n{claude_code}\n\
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

#[tokio::test]
#[ignore]
async fn an_item_that_keeps_pausing_is_given_up() {
    let env = setup("fit_pause_cap").await;
    let mut options = options("fake-claude-rejected.sh");
    options.no_wait = false;
    let outcome = tokio::time::timeout(std::time::Duration::from_secs(60), fit(&env, &options))
        .await
        .expect("the job keeps pausing on the same item")
        .unwrap();
    assert_eq!(outcome, Outcome::Finished);
    assert_eq!(
        env.count("select count(*) from llm_job_items where error like '%paused 3 times%'")
            .await,
        4
    );
    env.finish().await;
}

#[tokio::test]
#[ignore]
async fn a_database_error_does_not_leave_the_job_running() {
    let env = setup("fit_db_error").await;
    for sql in [
        "create function fail_call() returns trigger language plpgsql as \
         $$ begin raise exception 'acme write failed'; end $$",
        "create trigger fail_call before insert on llm_calls \
         for each row execute function fail_call()",
    ] {
        sqlx::query(sql).execute(&env.pool).await.unwrap();
    }
    let error = fit(&env, &options("fake-claude.sh")).await.unwrap_err();
    assert!(
        format!("{error:#}").contains("acme write failed"),
        "{error:#}"
    );
    assert_eq!(
        env.count("select count(*) from llm_jobs where status = 'running'")
            .await,
        0
    );
    assert_eq!(
        env.count(
            "select count(*) from llm_jobs where status = 'paused' \
             and pause_reason like '%acme write failed%'"
        )
        .await,
        1
    );
    env.finish().await;
}

#[tokio::test]
#[ignore]
async fn max_usd_stops_a_billed_job_but_not_a_subscription() {
    let env = setup("fit_max_usd").await;
    let mut billed = options("fake-claude-billed.sh");
    billed.max_usd = Some(0.002);
    assert_eq!(fit(&env, &billed).await.unwrap(), Outcome::Paused);
    assert_eq!(env.count("select count(*) from llm_calls").await, 2);
    assert_eq!(
        env.count("select count(*) from llm_jobs where pause_reason like '%--max-usd%'")
            .await,
        1
    );
    env.finish().await;
    let env = setup("fit_max_usd_subscription").await;
    let mut subscription = options("fake-claude.sh");
    subscription.max_usd = Some(0.002);
    assert_eq!(fit(&env, &subscription).await.unwrap(), Outcome::Finished);
    assert_eq!(env.count("select count(*) from llm_calls").await, 4);
    env.finish().await;
}

async fn two_accounts(name: &str) -> Env {
    setup_with(
        name,
        "config_dirs = [\"/nonexistent/acct-a\", \"/nonexistent/acct-b\"]",
    )
    .await
}

#[tokio::test]
#[ignore]
async fn a_limited_account_is_paused_and_the_job_goes_on_with_the_other() {
    let env = two_accounts("fit_accounts").await;
    assert_eq!(
        fit(&env, &options("fake-claude-accounts.sh"))
            .await
            .unwrap(),
        Outcome::Finished
    );
    assert_eq!(
        env.count(
            "select count(*) from llm_calls where status = 'pause' and account like '%acct-a' \
             and resume_at = to_timestamp(1893456000)"
        )
        .await,
        1
    );
    assert_eq!(
        env.count("select count(*) from llm_calls where status = 'ok' and account like '%acct-b'")
            .await,
        4
    );
    assert_eq!(
        env.count(
            "select count(*) from llm_calls where status = 'ok' and account not like '%acct-b'"
        )
        .await,
        0
    );
    let accounts = zontologize::db::llm::accounts(
        &env.pool,
        &[
            "/nonexistent/acct-a".to_string(),
            "/nonexistent/acct-b".to_string(),
        ],
    )
    .await
    .unwrap();
    assert!(accounts[0].paused_until.is_some(), "{accounts:?}");
    assert!(accounts[1].paused_until.is_none(), "{accounts:?}");
    assert!(accounts[1].last_used.is_some(), "{accounts:?}");
    env.finish().await;
}

#[tokio::test]
#[ignore]
async fn the_job_pauses_when_every_account_is_limited() {
    let env = two_accounts("fit_accounts_limited").await;
    assert_eq!(
        fit(&env, &options("fake-claude-rejected.sh"))
            .await
            .unwrap(),
        Outcome::Paused
    );
    assert_eq!(
        env.count("select count(distinct account) from llm_calls where status = 'pause'")
            .await,
        2
    );
    assert_eq!(
        env.count("select count(*) from llm_jobs where status = 'paused'")
            .await,
        1
    );
    env.finish().await;
}
