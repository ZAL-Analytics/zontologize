use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use sqlx::PgPool;
use sqlx::types::chrono::{DateTime, Utc};
use tokio::sync::watch;
use tokio::task::JoinSet;

use super::budget::{self, Defaults, Estimate};
use super::openrouter::Prices;
use super::{CallError, ModelName, Provider, ProviderKind, Reply, Request, Resume, Windows};
use crate::config::Config;
use crate::db;
use crate::db::llm::Call;

const PROGRESS_SECONDS: u64 = 60;
pub const GONE: &str = "the item no longer exists";
const WAIT_MARGIN_SECONDS: u64 = 60;
const UNKNOWN_RESET_WAIT_MINUTES: u64 = 30;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    Finished,
    Paused,
    DryRun,
}

pub struct Options {
    pub claude_program: PathBuf,
    pub max_usd: Option<f64>,
    pub max_requests: Option<u64>,
    pub dry_run: bool,
    pub no_wait: bool,
    pub new: bool,
    pub yes: bool,
    pub sample: Option<usize>,
    pub seed: Option<String>,
    pub extracted: bool,
    pub backoff: Duration,
}

impl Options {
    pub fn new(claude_program: PathBuf) -> Options {
        Options {
            claude_program,
            max_usd: None,
            max_requests: None,
            dry_run: false,
            no_wait: false,
            new: false,
            yes: false,
            sample: None,
            seed: None,
            extracted: false,
            backoff: Duration::from_secs(2),
        }
    }
}

impl Options {
    pub fn limits(&self) -> Limits {
        Limits {
            claude_program: self.claude_program.clone(),
            max_usd: self.max_usd,
            max_requests: self.max_requests,
            dry_run: self.dry_run,
            no_wait: self.no_wait,
            backoff: self.backoff,
        }
    }
}

pub struct Limits {
    pub claude_program: PathBuf,
    pub max_usd: Option<f64>,
    pub max_requests: Option<u64>,
    pub dry_run: bool,
    pub no_wait: bool,
    pub backoff: Duration,
}

pub struct Work {
    pub ord: i32,
    pub chars: usize,
}

struct Sent {
    ord: i32,
    key: String,
    chars: usize,
    result: Result<Reply, CallError>,
    seconds: f64,
}

pub(crate) trait Handler {
    fn task(&self) -> &'static str;
    fn label(&self) -> String;
    async fn request(&self, ord: i32) -> Result<Option<(String, Request)>>;
    fn accepts(&self, ord: i32, response: &str) -> Result<(), String>;
    async fn answered(&self, ord: i32, response: &str) -> Result<()>;
    async fn report(&self) -> Result<()>;
}

pub(crate) struct Runner<'a> {
    pub pool: &'a PgPool,
    pub config: &'a Config,
    pub name: &'a ModelName,
    pub job_id: i64,
    pub resume_after: Option<DateTime<Utc>>,
}

#[derive(Clone, Copy)]
struct Caps {
    max_usd: f64,
    max_requests: u64,
}

struct Sender {
    provider: Arc<Provider>,
    concurrency: usize,
    caps: Caps,
    backoff: Duration,
}

enum Stop {
    Done,
    Cap(String),
    Interrupted,
    Pause(Resume, String),
    Fatal(String),
}

pub fn percent(value: f64) -> String {
    format!("{:.0}%", value * 100.0)
}

pub fn utc(time: DateTime<Utc>) -> String {
    time.format("%Y-%m-%d %H:%M UTC").to_string()
}

pub(crate) async fn run<H: Handler>(
    runner: &Runner<'_>,
    handler: &H,
    queue: VecDeque<Work>,
    limits: &Limits,
) -> Result<Outcome> {
    let pool = runner.pool;
    let config = runner.config;
    let name = runner.name;
    let provider = Provider::new(name, config, limits.claude_program.clone())?;
    let concurrency = super::concurrency(name, config);
    let prices = match &provider {
        Provider::OpenRouter(openrouter) => Some(openrouter.prices().await?),
        Provider::Ollama(_) => Some(Prices {
            input: 0.0,
            output: 0.0,
        }),
        Provider::ClaudeCode(_) => None,
    };
    let calibration = db::llm::calibration(
        pool,
        name.provider.as_str(),
        &name.model,
        handler.task(),
        config.budget.calibration_calls,
    )
    .await?;
    let chars: u64 = queue.iter().map(|w| w.chars as u64).sum();
    let estimate = budget::estimate(
        queue.len() as u64,
        chars,
        &calibration,
        prices,
        Defaults {
            chars_per_token: config.models.chars_per_token,
            output_tokens: config.budget.default_output_tokens as f64,
        },
        concurrency,
    );
    let (spent, sent) = db::llm::spend(pool, runner.job_id).await?;
    let caps = Caps {
        max_usd: limits.max_usd.unwrap_or(config.budget.max_usd),
        max_requests: limits.max_requests.unwrap_or(config.budget.max_requests),
    };
    print_estimate(runner, &provider, &estimate, (spent, sent), caps).await?;
    if limits.dry_run {
        return Ok(Outcome::DryRun);
    }
    let (stop_tx, mut stop_rx) = watch::channel(false);
    tokio::spawn(async move {
        if tokio::signal::ctrl_c().await.is_ok() {
            let _ = stop_tx.send(true);
        }
    });
    let sender = Sender {
        provider: Arc::new(provider),
        concurrency,
        caps,
        backoff: limits.backoff,
    };
    let next_cost = estimate
        .usd
        .map_or(0.0, |usd| usd / estimate.requests.max(1) as f64);
    let mut queue = queue;
    let mut first = Some(runner.resume_after);
    loop {
        if let Some(resume_after) = first.take() {
            let mut waits = Vec::new();
            if let Some(at) = resume_after.filter(|at| *at > Utc::now()) {
                waits.push(at);
            }
            if name.provider == ProviderKind::ClaudeCode
                && let Some(windows) = db::llm::last_windows(pool).await?
                && let Some(at) = budget::window_full(
                    &windows,
                    config.providers.claude_code.max_window_use,
                    Utc::now(),
                )
            {
                waits.push(at);
            }
            if let Some(at) = waits.into_iter().max() {
                let stop = Stop::Pause(Resume::At(at), "waiting for the usage window".to_string());
                match pause(
                    runner,
                    handler,
                    stop,
                    limits,
                    &sender.provider,
                    next_cost,
                    &mut stop_rx,
                )
                .await?
                {
                    Some(outcome) => return Ok(outcome),
                    None => continue,
                }
            }
        }
        db::llm::set_job(pool, runner.job_id, "running", None, None).await?;
        let stop = send_all(runner, handler, &sender, &mut queue, &mut stop_rx).await?;
        if let Stop::Done = stop {
            db::llm::set_job(pool, runner.job_id, "finished", None, None).await?;
            handler.report().await?;
            println!("{}: finished", handler.label());
            return Ok(Outcome::Finished);
        }
        if let Some(outcome) = pause(
            runner,
            handler,
            stop,
            limits,
            &sender.provider,
            next_cost,
            &mut stop_rx,
        )
        .await?
        {
            return Ok(outcome);
        }
    }
}

async fn print_estimate(
    runner: &Runner<'_>,
    provider: &Provider,
    estimate: &Estimate,
    (spent, sent): (f64, i64),
    caps: Caps,
) -> Result<()> {
    let Caps {
        max_usd,
        max_requests,
    } = caps;
    let name = runner.name;
    let basis = if estimate.calibrated {
        "learned from recorded calls"
    } else {
        "from defaults until 20 calls are recorded"
    };
    println!(
        "estimate ({basis}): {} requests, {:.0} input and {:.0} output tokens",
        estimate.requests, estimate.input_tokens, estimate.output_tokens
    );
    match estimate.usd {
        Some(usd) => println!("estimate: ${usd:.4} with {name}"),
        None => println!("estimate: USD unknown until calls with {name} are recorded"),
    }
    if let Some(seconds) = estimate.seconds {
        println!("estimate: {:.0} minutes", seconds / 60.0);
    }
    if spent > 0.0 || sent > 0 {
        println!("job so far: {sent} requests, ${spent:.4}");
    }
    let caps = match (max_usd > 0.0, max_requests > 0) {
        (false, false) => "none".to_string(),
        (true, false) => format!("${max_usd:.4}"),
        (false, true) => format!("{max_requests} requests"),
        (true, true) => format!("${max_usd:.4} and {max_requests} requests"),
    };
    let fits_usd = max_usd <= 0.0 || estimate.usd.is_none_or(|usd| spent + usd <= max_usd);
    let fits_requests = max_requests == 0 || sent as u64 + estimate.requests <= max_requests;
    println!(
        "caps: {caps}; the job {} them",
        if fits_usd && fits_requests {
            "fits"
        } else {
            "stops at"
        }
    );
    match provider {
        Provider::ClaudeCode(_) => {
            let max_use = runner.config.providers.claude_code.max_window_use;
            match db::llm::last_windows(runner.pool).await? {
                Some(windows) => print_windows(&windows, estimate, max_use),
                None => println!(
                    "windows: no reading yet; run zontologize models check to see the usage windows"
                ),
            }
        }
        Provider::OpenRouter(openrouter) => {
            let credit = openrouter.credit().await?;
            match credit.remaining_usd {
                Some(remaining) => println!(
                    "openrouter: ${remaining:.4} left on the key{}",
                    match estimate.usd {
                        Some(usd) if usd > remaining => "; the job needs more and waits for funds",
                        _ => "",
                    }
                ),
                None => println!("openrouter: no limit on the key"),
            }
        }
        Provider::Ollama(_) => {}
    }
    Ok(())
}

fn print_windows(windows: &Windows, estimate: &Estimate, max_use: f64) {
    let rows = [
        ("5-hour", windows.five_hour, estimate.five_hour),
        ("7-day", windows.seven_day, estimate.seven_day),
    ];
    for (label, window, share) in rows {
        let Some(window) = window else {
            continue;
        };
        let used = if window.resets_at > Utc::now() {
            window.used
        } else {
            0.0
        };
        let mut line = format!(
            "windows: {label} at {} (resets {}), stop at {}",
            percent(used),
            utc(window.resets_at),
            percent(max_use)
        );
        match share {
            Some(share) => line.push_str(&format!(
                "; the job needs about {} over {} window(s)",
                percent(share),
                budget::windows_needed(share, used, max_use)
            )),
            None => line.push_str("; share unknown until window readings are recorded"),
        }
        println!("{line}");
    }
}

async fn send_all<H: Handler>(
    runner: &Runner<'_>,
    handler: &H,
    sender: &Sender,
    queue: &mut VecDeque<Work>,
    stop_rx: &mut watch::Receiver<bool>,
) -> Result<Stop> {
    let caps = sender.caps;
    let pool = runner.pool;
    let job = runner.job_id;
    let (mut spent, sent) = db::llm::spend(pool, job).await?;
    let mut sent = sent as u64;
    let mut set: JoinSet<Sent> = JoinSet::new();
    let mut stop: Option<Stop> = None;
    let mut last_progress = Instant::now();
    let (mut ok, mut invalid, mut failed) = (0usize, 0usize, 0usize);
    let max_use = runner.config.providers.claude_code.max_window_use;
    loop {
        while stop.is_none() && set.len() < sender.concurrency && !queue.is_empty() {
            if let Some(reason) = budget::cap_reached(spent, sent, caps.max_usd, caps.max_requests)
            {
                stop = Some(Stop::Cap(reason));
                break;
            }
            let Some(work) = queue.pop_front() else {
                break;
            };
            let Some((key, request)) = handler.request(work.ord).await? else {
                db::llm::set_items_status(pool, job, &[work.ord], "error", Some(GONE)).await?;
                continue;
            };
            sent += 1;
            let provider = sender.provider.clone();
            let backoff = sender.backoff;
            set.spawn(async move {
                let started = Instant::now();
                let (result, _) = super::send_with_retries(&provider, &request, backoff).await;
                Sent {
                    ord: work.ord,
                    key,
                    chars: request.chars(),
                    result,
                    seconds: started.elapsed().as_secs_f64(),
                }
            });
        }
        if set.is_empty() {
            return Ok(stop.unwrap_or(Stop::Done));
        }
        let joined = tokio::select! {
            joined = set.join_next() => joined,
            _ = stop_rx.changed() => {
                set.abort_all();
                while set.join_next().await.is_some() {}
                return Ok(Stop::Interrupted);
            }
        };
        let Some(joined) = joined else {
            continue;
        };
        let Sent {
            ord,
            key,
            chars,
            result,
            seconds,
        } = joined.context("a model call panicked")?;
        let mut call = Call {
            job_id: Some(job),
            provider: runner.name.provider.as_str(),
            model: &runner.name.model,
            task: handler.task(),
            cache_key: Some(&key),
            input_chars: chars as i64,
            usage: Default::default(),
            subscription: false,
            windows: Windows::default(),
            seconds,
            status: "ok",
            error: None,
        };
        match result {
            Ok(reply) => {
                call.usage = reply.usage;
                call.subscription = reply.subscription;
                call.windows = reply.windows;
                if !reply.subscription {
                    spent += reply.usage.cost_usd;
                }
                match handler.accepts(ord, &reply.response) {
                    Ok(()) => {
                        ok += 1;
                        db::llm::record(pool, &call, Some(&reply.response), Some((ord, "done")))
                            .await?;
                        handler.answered(ord, &reply.response).await?;
                    }
                    Err(error) => {
                        invalid += 1;
                        let message = format!("answer does not fit the schema: {error}");
                        call.status = "invalid";
                        call.error = Some(&message);
                        db::llm::record(pool, &call, None, Some((ord, "invalid"))).await?;
                    }
                }
                if reply.subscription
                    && stop.is_none()
                    && let Some(at) = budget::window_full(&reply.windows, max_use, Utc::now())
                {
                    stop = Some(Stop::Pause(
                        Resume::At(at),
                        format!("a Claude Code usage window reached {}", percent(max_use)),
                    ));
                }
            }
            Err(CallError::Pause { resume, reason }) => {
                call.status = "pause";
                call.error = Some(&reason);
                db::llm::record(pool, &call, None, None).await?;
                queue.push_front(Work { ord, chars });
                if stop.is_none() {
                    stop = Some(Stop::Pause(resume, reason.clone()));
                }
            }
            Err(CallError::Invalid(message)) => {
                invalid += 1;
                call.status = "invalid";
                call.error = Some(&message);
                db::llm::record(pool, &call, None, Some((ord, "invalid"))).await?;
            }
            Err(error @ CallError::Retry { .. }) => {
                failed += 1;
                let message = error.to_string();
                call.status = "error";
                call.error = Some(&message);
                db::llm::record(pool, &call, None, Some((ord, "error"))).await?;
            }
            Err(CallError::Fatal(message)) => {
                call.status = "error";
                call.error = Some(&message);
                db::llm::record(pool, &call, None, None).await?;
                queue.push_front(Work { ord, chars });
                set.abort_all();
                while set.join_next().await.is_some() {}
                return Ok(Stop::Fatal(message));
            }
        }
        if last_progress.elapsed() >= Duration::from_secs(PROGRESS_SECONDS) {
            last_progress = Instant::now();
            println!(
                "{}: {ok} answered, {invalid} invalid, {failed} errors, {} left, ${spent:.4}",
                handler.label(),
                queue.len() + set.len()
            );
        }
    }
}

async fn pause<H: Handler>(
    runner: &Runner<'_>,
    handler: &H,
    stop: Stop,
    limits: &Limits,
    provider: &Provider,
    next_cost: f64,
    stop_rx: &mut watch::Receiver<bool>,
) -> Result<Option<Outcome>> {
    let pool = runner.pool;
    let job = runner.job_id;
    let label = handler.label();
    let (reason, resume) = match stop {
        Stop::Done => return Ok(None),
        Stop::Cap(reason) => (reason, None),
        Stop::Interrupted => ("stopped by Ctrl+C".to_string(), None),
        Stop::Fatal(message) => {
            db::llm::set_job(pool, job, "paused", Some(&message), None).await?;
            handler.report().await?;
            bail!("{label}: {message}");
        }
        Stop::Pause(resume, reason) => (reason, Some(resume)),
    };
    let resume_after = match &resume {
        Some(Resume::At(at)) => Some(*at + Duration::from_secs(WAIT_MARGIN_SECONDS)),
        _ => None,
    };
    db::llm::set_job(pool, job, "paused", Some(&reason), resume_after).await?;
    handler.report().await?;
    if resume.is_none() {
        println!("{label}: paused, {reason}; run the same command to go on");
        return Ok(Some(Outcome::Paused));
    }
    if limits.no_wait {
        let when = resume_after.map_or("when funds are added".to_string(), |at| {
            format!("after {}", utc(at))
        });
        println!("{label}: paused, {reason}; run the same command {when}");
        return Ok(Some(Outcome::Paused));
    }
    let waited = match resume_after {
        Some(at) => {
            println!("{label}: {reason}; waiting until {}", utc(at));
            sleep_until(at, stop_rx).await
        }
        None => {
            wait_for_funds(runner.config, provider, next_cost, &label, &reason, stop_rx).await?
        }
    };
    if !waited {
        db::llm::set_job(pool, job, "paused", Some("stopped by Ctrl+C"), resume_after).await?;
        println!("{label}: paused, stopped by Ctrl+C; run the same command to go on");
        return Ok(Some(Outcome::Paused));
    }
    println!("{label}: going on");
    Ok(None)
}

async fn sleep_until(at: DateTime<Utc>, stop_rx: &mut watch::Receiver<bool>) -> bool {
    let wait = (at - Utc::now()).to_std().unwrap_or_default();
    tokio::select! {
        _ = tokio::time::sleep(wait) => true,
        _ = stop_rx.changed() => false,
    }
}

async fn wait_for_funds(
    config: &Config,
    provider: &Provider,
    next_cost: f64,
    label: &str,
    reason: &str,
    stop_rx: &mut watch::Receiver<bool>,
) -> Result<bool> {
    let Provider::OpenRouter(openrouter) = provider else {
        let at = Utc::now() + Duration::from_secs(UNKNOWN_RESET_WAIT_MINUTES * 60);
        println!(
            "{label}: {reason}; no reset time given, trying again at {}",
            utc(at)
        );
        return Ok(sleep_until(at, stop_rx).await);
    };
    let poll = config.providers.openrouter.poll_minutes.max(1);
    println!("{label}: {reason}; checking the key every {poll} minutes until funds are added");
    loop {
        let at = Utc::now() + Duration::from_secs(poll * 60);
        if !sleep_until(at, stop_rx).await {
            return Ok(false);
        }
        match openrouter.credit().await {
            Ok(credit) if credit.remaining_usd.is_none_or(|r| r > next_cost) => return Ok(true),
            Ok(_) => {}
            Err(error) => println!("{label}: cannot read the key: {error:#}"),
        }
    }
}
