use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use sqlx::PgPool;
use sqlx::types::chrono::{DateTime, Utc};
use tokio::sync::watch;
use tokio::task::JoinSet;

use super::report::{self, Entry, Summary};
use super::{Answer, TASK, check, counts_by_chunk, shortlist, shortlist_lines, split_sample};
use crate::config::{self, Config};
use crate::db::fit::Chunk;
use crate::db::llm::{Call, Item, Job, JobParams};
use crate::llm::budget::{self, Defaults, Estimate};
use crate::llm::openrouter::Prices;
use crate::llm::prompt::{self, Prompt};
use crate::llm::{CallError, ModelName, Provider, ProviderKind, Reply, Request, Resume, Windows};
use crate::ontology::{self, Ontology};
use crate::{db, llm};

const USER_VARIABLES: [&str; 5] = ["path", "pages", "languages", "shortlist", "chunk"];
const PROGRESS_SECONDS: u64 = 60;
const WAIT_MARGIN_SECONDS: u64 = 60;
const UNKNOWN_RESET_WAIT_MINUTES: u64 = 30;

pub struct Options {
    pub claude_program: PathBuf,
    pub sample: Option<usize>,
    pub seed: Option<String>,
    pub max_usd: Option<f64>,
    pub max_requests: Option<u64>,
    pub dry_run: bool,
    pub no_wait: bool,
    pub new: bool,
    pub backoff: Duration,
}

impl Options {
    pub fn new(claude_program: PathBuf) -> Options {
        Options {
            claude_program,
            sample: None,
            seed: None,
            max_usd: None,
            max_requests: None,
            dry_run: false,
            no_wait: false,
            new: false,
            backoff: Duration::from_secs(2),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    Finished,
    Paused,
    DryRun,
}

struct Prepared {
    chunk: Chunk,
    shortlist: Vec<String>,
    request: Request,
    key: String,
}

struct Work {
    ord: i32,
    key: String,
    request: Request,
}

#[derive(Clone, Copy)]
struct Caps {
    max_usd: f64,
    max_requests: u64,
}

enum Stop {
    Done,
    Cap(String),
    Interrupted,
    Pause(Resume, String),
    Fatal(String),
}

struct Fit<'a> {
    pool: &'a PgPool,
    config: &'a Config,
    name: &'a ModelName,
    ontology: &'a Ontology,
    job: &'a Job,
    prepared: &'a HashMap<i32, Prepared>,
    out: &'a Path,
}

pub async fn run(
    config_dir: Option<&Path>,
    url: Option<&str>,
    options: &Options,
) -> Result<Outcome> {
    let config = config::load(config_dir)?;
    if config.models.fit.is_empty() {
        bail!("[models] fit is empty; set it to provider:model, for example claude-code:haiku");
    }
    let name =
        ModelName::parse(&config.models.fit).map_err(|e| anyhow::anyhow!("models.fit: {e}"))?;
    if config.ontology.files.is_empty() {
        bail!("[ontology] files is empty");
    }
    let loaded = ontology::load(config_dir, &config.ontology)?;
    if let Some(error) = loaded.first_error() {
        bail!("ontology: {error}; run zontologize ontology check");
    }
    let system = prompt::load(config_dir, "fit-system")?;
    system.check(&[])?;
    let user = prompt::load(config_dir, "fit-user")?;
    user.check(&USER_VARIABLES)?;
    let pool = db::pool::connect(
        url,
        &config.database.schema,
        config.database.max_connections,
    )
    .await?;
    let result = run_with(
        &pool,
        &config,
        &name,
        &loaded.ontology,
        &system,
        &user,
        options,
    )
    .await;
    pool.close().await;
    result
}

fn pages(chunk: &Chunk) -> String {
    match (chunk.page_from, chunk.page_to) {
        (Some(from), Some(to)) if from != to => format!("{from}-{to}"),
        (Some(from), _) => from.to_string(),
        _ => "-".to_string(),
    }
}

async fn run_with(
    pool: &PgPool,
    config: &Config,
    name: &ModelName,
    ontology: &Ontology,
    system: &Prompt,
    user: &Prompt,
    options: &Options,
) -> Result<Outcome> {
    let Some(version_id) = db::fit::ontology_version(pool, &ontology.version).await? else {
        bail!("the ontology changed since the last scan; run zontologize scan first");
    };
    let params = JobParams {
        sample: options.sample.unwrap_or(config.fit.sample),
        seed: options
            .seed
            .clone()
            .unwrap_or_else(|| config.fit.seed.clone()),
    };
    let mut job = db::llm::open_job(pool, TASK).await?;
    if let Some(open) = &job
        && options.new
        && !options.dry_run
    {
        db::llm::set_job(pool, open.id, "abandoned", Some("abandoned by --new"), None).await?;
        println!("fit job {}: abandoned", open.id);
        job = None;
    }
    if let Some(open) = &job
        && !options.new
    {
        let mut differences = Vec::new();
        if open.provider != name.provider.as_str() || open.model != name.model {
            differences.push(format!("model {}:{}", open.provider, open.model));
        }
        if open.params.0 != params {
            differences.push(format!(
                "sample {} with seed {:?}",
                open.params.0.sample, open.params.0.seed
            ));
        }
        if open.ontology_version_id != Some(version_id) {
            differences.push("another ontology version".to_string());
        }
        if !differences.is_empty() {
            bail!(
                "fit job {} is unfinished and was started with {}; run with --new to abandon it and start over",
                open.id,
                differences.join(", ")
            );
        }
    }
    let job = if options.new && options.dry_run {
        None
    } else {
        job
    };
    let items = match &job {
        Some(open) => db::llm::items(pool, open.id).await?,
        None => draw(pool, config, &params).await?,
    };
    let job = match job {
        Some(open) => open,
        None if options.dry_run => Job {
            id: 0,
            task: TASK.to_string(),
            provider: name.provider.as_str().to_string(),
            model: name.model.clone(),
            ontology_version_id: Some(version_id),
            params: sqlx::types::Json(params.clone()),
            status: "running".to_string(),
            pause_reason: None,
            resume_after: None,
        },
        None => {
            let ids: Vec<i64> = items.iter().map(|i| i.chunk_id).collect();
            let created = db::llm::create_job(
                pool,
                TASK,
                name.provider.as_str(),
                &name.model,
                version_id,
                &params,
                &ids,
            )
            .await?;
            println!("fit job {}: new, {} chunks sampled", created.id, ids.len());
            created
        }
    };
    let mut lock = pool.acquire().await?;
    if job.id > 0 && !db::llm::lock(&mut lock, job.id).await? {
        bail!("fit job {} is running in another process", job.id);
    }
    let prepared = prepare(pool, config, ontology, system, user, name, &items).await?;
    let gone: Vec<i32> = items
        .iter()
        .filter(|i| i.status != "done" && !prepared.contains_key(&i.ord))
        .map(|i| i.ord)
        .collect();
    let todo: Vec<&Item> = items
        .iter()
        .filter(|i| (i.status == "pending" || i.status == "error") && prepared.contains_key(&i.ord))
        .collect();
    let keys: Vec<String> = todo.iter().map(|i| prepared[&i.ord].key.clone()).collect();
    let cache = db::llm::cached(pool, &keys).await?;
    let hits: Vec<i32> = todo
        .iter()
        .filter(|i| cache.contains_key(&prepared[&i.ord].key))
        .map(|i| i.ord)
        .collect();
    let queue: VecDeque<Work> = todo
        .iter()
        .filter(|i| !cache.contains_key(&prepared[&i.ord].key))
        .map(|i| {
            let p = &prepared[&i.ord];
            Work {
                ord: i.ord,
                key: p.key.clone(),
                request: p.request.clone(),
            }
        })
        .collect();
    if job.id > 0 {
        let ords: Vec<i32> = todo.iter().map(|i| i.ord).collect();
        db::llm::set_item_keys(pool, job.id, &ords, &keys).await?;
        db::llm::set_items_status(pool, job.id, &hits, "done", None).await?;
        db::llm::set_items_status(pool, job.id, &gone, "error", Some("chunk no longer exists"))
            .await?;
    }
    let done = items.iter().filter(|i| i.status == "done").count();
    let label = if job.id > 0 {
        format!("fit job {}", job.id)
    } else {
        "fit (dry run)".to_string()
    };
    println!(
        "{label}: {} chunks, {done} answered before, {} from the cache, {} to send, {} gone",
        items.len(),
        hits.len(),
        queue.len(),
        gone.len()
    );
    let provider = Provider::new(name, config, options.claude_program.clone())?;
    let concurrency = llm::concurrency(name, config);
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
        TASK,
        config.budget.calibration_calls,
    )
    .await?;
    let chars: u64 = queue.iter().map(|w| w.request.chars() as u64).sum();
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
    let (spent, sent) = db::llm::spend(pool, job.id).await?;
    let caps = Caps {
        max_usd: options.max_usd.unwrap_or(config.budget.max_usd),
        max_requests: options.max_requests.unwrap_or(config.budget.max_requests),
    };
    print_estimate(
        pool,
        config,
        name,
        &provider,
        &estimate,
        (spent, sent),
        caps,
    )
    .await?;
    if options.dry_run {
        return Ok(Outcome::DryRun);
    }
    let out = PathBuf::from(&config.fit.out);
    let context = Fit {
        pool,
        config,
        name,
        ontology,
        job: &job,
        prepared: &prepared,
        out: &out,
    };
    let (stop_tx, mut stop_rx) = watch::channel(false);
    tokio::spawn(async move {
        if tokio::signal::ctrl_c().await.is_ok() {
            let _ = stop_tx.send(true);
        }
    });
    let provider = Arc::new(provider);
    let next_cost = estimate
        .usd
        .map_or(0.0, |usd| usd / estimate.requests.max(1) as f64);
    let mut queue = queue;
    let mut first = Some(job.resume_after);
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
                match pause(&context, stop, options, &provider, next_cost, &mut stop_rx).await? {
                    Some(outcome) => return Ok(outcome),
                    None => continue,
                }
            }
        }
        db::llm::set_job(pool, job.id, "running", None, None).await?;
        let stop = send_all(
            &context,
            &provider,
            &mut queue,
            concurrency,
            options,
            caps,
            &mut stop_rx,
        )
        .await?;
        if let Stop::Done = stop {
            db::llm::set_job(pool, job.id, "finished", None, None).await?;
            write_report(&context).await?;
            println!("fit job {}: finished; wrote {}", job.id, out.display());
            return Ok(Outcome::Finished);
        }
        if let Some(outcome) =
            pause(&context, stop, options, &provider, next_cost, &mut stop_rx).await?
        {
            return Ok(outcome);
        }
    }
}

async fn draw(pool: &PgPool, config: &Config, params: &JobParams) -> Result<Vec<Item>> {
    let limit = params.sample as i64;
    let min_chars = config.fit.min_chars as i32;
    let matched = db::fit::sample(pool, &params.seed, min_chars, true, limit).await?;
    let unmatched = db::fit::sample(pool, &params.seed, min_chars, false, limit).await?;
    let ids = split_sample(
        &matched,
        &unmatched,
        params.sample,
        config.fit.unmatched_share,
    );
    if ids.is_empty() {
        bail!("no chunks to sample; run zontologize scan first");
    }
    Ok(ids
        .into_iter()
        .enumerate()
        .map(|(i, chunk_id)| Item {
            ord: i as i32 + 1,
            chunk_id,
            cache_key: None,
            status: "pending".to_string(),
        })
        .collect())
}

async fn prepare(
    pool: &PgPool,
    config: &Config,
    ontology: &Ontology,
    system: &Prompt,
    user: &Prompt,
    name: &ModelName,
    items: &[Item],
) -> Result<HashMap<i32, Prepared>> {
    let ids: Vec<i64> = items.iter().map(|i| i.chunk_id).collect();
    let mut chunks: HashMap<i64, Chunk> = db::fit::chunks(pool, &ids)
        .await?
        .into_iter()
        .map(|c| (c.id, c))
        .collect();
    let counts = counts_by_chunk(db::fit::class_counts(pool, &ids).await?);
    let languages = &config.ontology.languages;
    let mut prepared = HashMap::new();
    for item in items {
        let Some(chunk) = chunks.remove(&item.chunk_id) else {
            continue;
        };
        let list = shortlist(
            ontology,
            counts.get(&chunk.id).map(Vec::as_slice).unwrap_or(&[]),
            &ontology.roots,
            config.fit.max_shortlist,
        );
        let lines = shortlist_lines(ontology, &list, languages);
        let user_text = user.fill(&[
            ("path", &chunk.path),
            ("pages", &pages(&chunk)),
            ("languages", &languages.join(", ")),
            ("shortlist", lines.trim_end()),
            ("chunk", &chunk.text),
        ]);
        let request = Request {
            task: TASK.to_string(),
            system: system.text.clone(),
            user: user_text,
            schema: super::schema(&list, languages),
        };
        let key = request.key(name);
        prepared.insert(
            item.ord,
            Prepared {
                chunk,
                shortlist: list,
                request,
                key,
            },
        );
    }
    Ok(prepared)
}

fn percent(value: f64) -> String {
    format!("{:.0}%", value * 100.0)
}

fn utc(time: DateTime<Utc>) -> String {
    time.format("%Y-%m-%d %H:%M UTC").to_string()
}

async fn print_estimate(
    pool: &PgPool,
    config: &Config,
    name: &ModelName,
    provider: &Provider,
    estimate: &Estimate,
    (spent, sent): (f64, i64),
    caps: Caps,
) -> Result<()> {
    let Caps {
        max_usd,
        max_requests,
    } = caps;
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
            let max_use = config.providers.claude_code.max_window_use;
            match db::llm::last_windows(pool).await? {
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

async fn send_all(
    context: &Fit<'_>,
    provider: &Arc<Provider>,
    queue: &mut VecDeque<Work>,
    concurrency: usize,
    options: &Options,
    caps: Caps,
    stop_rx: &mut watch::Receiver<bool>,
) -> Result<Stop> {
    let pool = context.pool;
    let job = context.job;
    let (mut spent, sent) = db::llm::spend(pool, job.id).await?;
    let mut sent = sent as u64;
    let mut set: JoinSet<(Work, Result<Reply, CallError>, f64)> = JoinSet::new();
    let mut stop: Option<Stop> = None;
    let mut last_progress = Instant::now();
    let (mut ok, mut invalid, mut failed) = (0usize, 0usize, 0usize);
    let max_use = context.config.providers.claude_code.max_window_use;
    loop {
        while stop.is_none() && set.len() < concurrency && !queue.is_empty() {
            if let Some(reason) = budget::cap_reached(spent, sent, caps.max_usd, caps.max_requests)
            {
                stop = Some(Stop::Cap(reason));
                break;
            }
            let Some(work) = queue.pop_front() else {
                break;
            };
            sent += 1;
            let provider = provider.clone();
            let backoff = options.backoff;
            set.spawn(async move {
                let started = Instant::now();
                let (result, _) = llm::send_with_retries(&provider, &work.request, backoff).await;
                (work, result, started.elapsed().as_secs_f64())
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
        let (work, result, seconds) = joined.context("a model call panicked")?;
        let mut call = Call {
            job_id: Some(job.id),
            provider: context.name.provider.as_str(),
            model: &context.name.model,
            task: TASK,
            cache_key: Some(&work.key),
            input_chars: work.request.chars() as i64,
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
                match serde_json::from_str::<Answer>(&reply.response) {
                    Ok(_) => {
                        ok += 1;
                        db::llm::record(
                            pool,
                            &call,
                            Some(&reply.response),
                            Some((work.ord, "done")),
                        )
                        .await?;
                    }
                    Err(error) => {
                        invalid += 1;
                        let message = format!("answer does not fit the schema: {error}");
                        call.status = "invalid";
                        call.error = Some(&message);
                        db::llm::record(pool, &call, None, Some((work.ord, "invalid"))).await?;
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
                queue.push_front(work);
                if stop.is_none() {
                    stop = Some(Stop::Pause(resume, reason.clone()));
                }
            }
            Err(CallError::Invalid(message)) => {
                invalid += 1;
                call.status = "invalid";
                call.error = Some(&message);
                db::llm::record(pool, &call, None, Some((work.ord, "invalid"))).await?;
            }
            Err(error @ CallError::Retry { .. }) => {
                failed += 1;
                let message = error.to_string();
                call.status = "error";
                call.error = Some(&message);
                db::llm::record(pool, &call, None, Some((work.ord, "error"))).await?;
            }
            Err(CallError::Fatal(message)) => {
                call.status = "error";
                call.error = Some(&message);
                db::llm::record(pool, &call, None, None).await?;
                queue.push_front(work);
                set.abort_all();
                while set.join_next().await.is_some() {}
                return Ok(Stop::Fatal(message));
            }
        }
        if last_progress.elapsed() >= Duration::from_secs(PROGRESS_SECONDS) {
            last_progress = Instant::now();
            println!(
                "fit job {}: {ok} answered, {invalid} invalid, {failed} errors, {} left, ${spent:.4}",
                job.id,
                queue.len() + set.len()
            );
        }
    }
}

async fn pause(
    context: &Fit<'_>,
    stop: Stop,
    options: &Options,
    provider: &Provider,
    next_cost: f64,
    stop_rx: &mut watch::Receiver<bool>,
) -> Result<Option<Outcome>> {
    let pool = context.pool;
    let job = context.job.id;
    let (reason, resume) = match stop {
        Stop::Done => return Ok(None),
        Stop::Cap(reason) => (reason, None),
        Stop::Interrupted => ("stopped by Ctrl+C".to_string(), None),
        Stop::Fatal(message) => {
            db::llm::set_job(pool, job, "paused", Some(&message), None).await?;
            write_report(context).await?;
            bail!("fit job {job}: {message}");
        }
        Stop::Pause(resume, reason) => (reason, Some(resume)),
    };
    let resume_after = match &resume {
        Some(Resume::At(at)) => Some(*at + Duration::from_secs(WAIT_MARGIN_SECONDS)),
        _ => None,
    };
    db::llm::set_job(pool, job, "paused", Some(&reason), resume_after).await?;
    write_report(context).await?;
    if resume.is_none() {
        println!("fit job {job}: paused, {reason}; run the same command to go on");
        return Ok(Some(Outcome::Paused));
    }
    if options.no_wait {
        let when = resume_after.map_or("when funds are added".to_string(), |at| {
            format!("after {}", utc(at))
        });
        println!("fit job {job}: paused, {reason}; run the same command {when}");
        return Ok(Some(Outcome::Paused));
    }
    let waited = match resume_after {
        Some(at) => {
            println!("fit job {job}: {reason}; waiting until {}", utc(at));
            sleep_until(at, stop_rx).await
        }
        None => wait_for_funds(context.config, provider, next_cost, job, &reason, stop_rx).await?,
    };
    if !waited {
        db::llm::set_job(pool, job, "paused", Some("stopped by Ctrl+C"), resume_after).await?;
        println!("fit job {job}: paused, stopped by Ctrl+C; run the same command to go on");
        return Ok(Some(Outcome::Paused));
    }
    println!("fit job {job}: going on");
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
    job: i64,
    reason: &str,
    stop_rx: &mut watch::Receiver<bool>,
) -> Result<bool> {
    let Provider::OpenRouter(openrouter) = provider else {
        let at = Utc::now() + Duration::from_secs(UNKNOWN_RESET_WAIT_MINUTES * 60);
        println!(
            "fit job {job}: {reason}; no reset time given, trying again at {}",
            utc(at)
        );
        return Ok(sleep_until(at, stop_rx).await);
    };
    let poll = config.providers.openrouter.poll_minutes.max(1);
    println!(
        "fit job {job}: {reason}; checking the key every {poll} minutes until funds are added"
    );
    loop {
        let at = Utc::now() + Duration::from_secs(poll * 60);
        if !sleep_until(at, stop_rx).await {
            return Ok(false);
        }
        match openrouter.credit().await {
            Ok(credit) if credit.remaining_usd.is_none_or(|r| r > next_cost) => return Ok(true),
            Ok(_) => {}
            Err(error) => println!("fit job {job}: cannot read the key: {error:#}"),
        }
    }
}

async fn write_report(context: &Fit<'_>) -> Result<()> {
    let pool = context.pool;
    let job = context.job;
    let items = db::llm::items(pool, job.id).await?;
    let keys: Vec<String> = items
        .iter()
        .filter(|i| i.status == "done")
        .filter_map(|i| i.cache_key.clone())
        .collect();
    let cache = db::llm::cached(pool, &keys).await?;
    let languages = &context.config.ontology.languages;
    let mut entries = Vec::new();
    for item in items.iter().filter(|i| i.status == "done") {
        let (Some(prepared), Some(key)) = (context.prepared.get(&item.ord), &item.cache_key) else {
            continue;
        };
        let Some(Ok(answer)) = cache.get(key).map(|r| serde_json::from_str::<Answer>(r)) else {
            continue;
        };
        entries.push(Entry {
            chunk: &prepared.chunk,
            shortlist: &prepared.shortlist,
            checked: check(answer, &prepared.shortlist, &prepared.chunk.text, languages),
        });
    }
    let (input_tokens, output_tokens, cost_usd) = db::llm::totals(pool, job.id).await?;
    let count = |status: &str| items.iter().filter(|i| i.status == status).count();
    let summary = Summary {
        job: job.id,
        model: context.name.to_string(),
        ontology_version: context.ontology.version.clone(),
        items: items.len(),
        done: count("done"),
        invalid: count("invalid"),
        errors: count("error"),
        input_tokens,
        output_tokens,
        cost_usd,
    };
    let proposals = report::write(
        context.out,
        context.ontology,
        &entries,
        &summary,
        context.config.fit.min_evidence,
        &context.config.fit.proposal_namespace,
    )?;
    println!(
        "fit job {}: report over {} answers, {} proposals in {}",
        job.id,
        entries.len(),
        proposals.len(),
        context.out.display()
    );
    Ok(())
}
