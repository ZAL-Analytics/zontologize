use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::sync::{Mutex, PoisonError};
use std::time::Instant;

use anyhow::{Result, bail};
use globset::GlobSet;
use serde::de::DeserializeOwned;
use sqlx::PgPool;

use super::job::{self, Handler, Options, Outcome, Runner, Work};
use super::{ModelName, Request};
use crate::config::Config;
use crate::db;
use crate::db::facts::Confidence;
use crate::db::llm::{JobParams, Owner};
use crate::facts::sample::{Candidate, Settings, first_folder, letter_share, select, text_hash};

const KEY_BATCH: usize = 10_000;

pub(crate) struct Base<'a> {
    pub pool: &'a PgPool,
    pub config: &'a Config,
    pub name: &'a ModelName,
    pub task: &'static str,
    pub ontology_version: &'a str,
    pub sources: &'a [String],
    pub paths: &'a [String],
    pub read_batch: usize,
    pub seed: &'a str,
    pub resend_share: f64,
    pub confidence: f64,
}

pub(crate) struct Chunk {
    pub id: i64,
    pub key: Option<String>,
    pub source: String,
    pub document_id: i64,
    pub doc_type: Option<String>,
    pub path: String,
    pub text: String,
    pub ocr_confidence: Option<f64>,
    pub matched: bool,
}

pub(crate) struct Keys<'a> {
    pub chunk_id: i64,
    pub key: &'a str,
    pub cache_key: &'a str,
    pub request: &'a Request,
}

struct Todo {
    chunk_id: i64,
    cache_key: String,
    chars: usize,
    resend: bool,
    order: (usize, i64, i64),
}

#[derive(Default)]
pub(crate) struct Plan {
    pub scanned: usize,
    pub eligible: usize,
    pub done: usize,
    todo: Vec<Todo>,
    pub stale: Vec<i64>,
}

pub(crate) struct Counts {
    pub hits: usize,
    pub send: usize,
    pub invalid: usize,
    pub resend: usize,
}

struct Row {
    id: i64,
    key: Option<String>,
    order: (usize, i64, i64),
}

pub(crate) trait Stage {
    type Item;
    type Answer: DeserializeOwned;
    type Checked;
    const SAMPLED: &'static str;
    const WRITES: &'static str;
    const OWNER: Owner = Owner::Chunk;

    fn base(&self) -> &Base<'_>;
    fn keys(item: &Self::Item) -> Keys<'_>;
    fn drops(checked: &Self::Checked) -> &BTreeMap<String, i64>;
    async fn read(
        &self,
        after: i64,
        paths: Option<&GlobSet>,
        sampling: bool,
        plan: &mut Plan,
    ) -> Result<Option<(i64, Vec<Chunk>)>>;
    async fn prepare(&self, ids: &[i64]) -> Result<Vec<Self::Item>>;
    fn check(&self, item: Self::Item, answer: Self::Answer) -> Self::Checked;
    async fn write(
        &self,
        checked: &[Self::Checked],
        method: &str,
        confidence: &Confidence,
        version_id: i64,
    ) -> Result<()>;
    fn summary(&self, plan: &Plan, counts: &Counts, started: Instant);
    async fn clear(&self, stale: &[i64], dry_run: bool) -> Result<()>;
    async fn tidy(&self) -> Result<()>;
    async fn report(&self, job: Option<i64>) -> Result<()>;
}

fn path_filter(patterns: &[String]) -> Result<Option<GlobSet>> {
    if patterns.is_empty() {
        return Ok(None);
    }
    crate::scan::walk::globs(patterns).map(Some)
}

fn candidate(chunk: &Chunk) -> Candidate {
    Candidate {
        id: chunk.id,
        document_id: chunk.document_id,
        stratum: format!(
            "{}\u{1f}{}\u{1f}{}",
            chunk.source,
            chunk.doc_type.as_deref().unwrap_or(""),
            first_folder(&chunk.path)
        ),
        text_hash: text_hash(&chunk.text),
        letters: letter_share(&chunk.text),
        ocr_confidence: chunk.ocr_confidence,
        matched: chunk.matched,
    }
}

async fn compare<S: Stage>(stage: &S, rows: &[Row], plan: &mut Plan) -> Result<()> {
    let ids: Vec<i64> = rows.iter().map(|r| r.id).collect();
    let by_id: HashMap<i64, &Row> = rows.iter().map(|r| (r.id, r)).collect();
    let prepared = stage.prepare(&ids).await?;
    let built: HashSet<i64> = prepared.iter().map(|p| S::keys(p).chunk_id).collect();
    plan.stale.extend(
        rows.iter()
            .filter(|r| r.key.is_some() && !built.contains(&r.id))
            .map(|r| r.id),
    );
    for item in &prepared {
        let keys = S::keys(item);
        let Some(row) = by_id.get(&keys.chunk_id) else {
            continue;
        };
        plan.eligible += 1;
        if row.key.as_deref() == Some(keys.key) {
            plan.done += 1;
            continue;
        }
        plan.todo.push(Todo {
            chunk_id: row.id,
            cache_key: keys.cache_key.to_string(),
            chars: keys.request.chars(),
            resend: row.key.is_some(),
            order: row.order,
        });
    }
    Ok(())
}

async fn plan<S: Stage>(stage: &S, sample: Option<usize>, seed: &str) -> Result<Plan> {
    let base = stage.base();
    let config = base.config;
    let extract = &config.llm_extract;
    let source_order: Vec<&String> = if base.sources.is_empty() {
        config.sources.iter().map(|s| &s.name).collect()
    } else {
        base.sources.iter().collect()
    };
    let paths = path_filter(base.paths)?;
    let mut plan = Plan::default();
    let mut candidates = Vec::new();
    let mut rows: HashMap<i64, Row> = HashMap::new();
    let mut after = 0;
    while let Some((last, chunks)) = stage
        .read(after, paths.as_ref(), sample.is_some(), &mut plan)
        .await?
    {
        after = last;
        let mut batch = Vec::new();
        for chunk in chunks {
            let source = source_order
                .iter()
                .position(|s| **s == chunk.source)
                .unwrap_or(source_order.len());
            let row = Row {
                id: chunk.id,
                key: chunk.key.clone(),
                order: (source, chunk.document_id, chunk.id),
            };
            if sample.is_some() {
                candidates.push(candidate(&chunk));
                rows.insert(chunk.id, row);
            } else {
                batch.push(row);
            }
        }
        compare(stage, &batch, &mut plan).await?;
    }
    if let Some(size) = sample {
        let settings = Settings {
            size,
            seed,
            max_per_document: extract.max_per_document,
            unmatched_share: extract.unmatched_share,
            min_letter_share: extract.min_letter_share,
            min_ocr_confidence: extract.min_ocr_confidence,
        };
        let selected = select(&candidates, &settings);
        println!(
            "{}: sample of {} from {} {} ({} repeated texts and {} unreadable or mostly numbers \
             left out)",
            base.task,
            selected.ids.len(),
            candidates.len(),
            S::SAMPLED,
            selected.duplicates,
            selected.junk
        );
        let mut picked = Vec::new();
        for (rank, id) in selected.ids.iter().enumerate() {
            if let Some(mut row) = rows.remove(id) {
                row.order = (rank, 0, 0);
                picked.push(row);
            }
        }
        for part in picked.chunks(base.read_batch) {
            compare(stage, part, &mut plan).await?;
        }
    }
    plan.todo.sort_by_key(|t| t.order);
    Ok(plan)
}

async fn split_known(pool: &PgPool, todo: &[Todo]) -> Result<(HashSet<String>, HashSet<String>)> {
    let mut cached = HashSet::new();
    let mut invalid = HashSet::new();
    for part in todo.chunks(KEY_BATCH) {
        let keys: Vec<String> = part.iter().map(|t| t.cache_key.clone()).collect();
        let (hit, bad) = db::llm::known_keys(pool, &keys).await?;
        cached.extend(hit);
        invalid.extend(bad);
    }
    Ok((cached, invalid))
}

async fn write_cached<S: Stage>(
    stage: &S,
    ids: &[i64],
    method: &str,
    confidence: &Confidence,
    version_id: i64,
) -> Result<usize> {
    let base = stage.base();
    let mut written = 0;
    for part in ids.chunks(base.read_batch) {
        let prepared = stage.prepare(part).await?;
        let keys: Vec<String> = prepared
            .iter()
            .map(|p| S::keys(p).cache_key.to_string())
            .collect();
        let cache = db::llm::cached(base.pool, &keys).await?;
        let mut checked = Vec::new();
        for item in prepared {
            let Some(response) = cache.get(S::keys(&item).cache_key) else {
                continue;
            };
            if let Ok(answer) = serde_json::from_str::<S::Answer>(response) {
                checked.push(stage.check(item, answer));
            }
        }
        written += checked.len();
        stage
            .write(&checked, method, confidence, version_id)
            .await?;
    }
    Ok(written)
}

pub(crate) async fn run<S: Stage>(stage: &S, options: &Options) -> Result<Outcome> {
    let base = stage.base();
    let pool = base.pool;
    let config = base.config;
    let task = base.task;
    let Some(version_id) = db::fit::ontology_version(pool, base.ontology_version).await? else {
        bail!("the ontology changed since the last scan; run zontologize scan first");
    };
    let seed = options.seed.as_deref().unwrap_or(base.seed);
    let started = Instant::now();
    let plan = plan(stage, options.sample, seed).await?;
    let (cached, invalid) = split_known(pool, &plan.todo).await?;
    let resend = plan
        .todo
        .iter()
        .filter(|t| t.resend && !cached.contains(&t.cache_key))
        .count();
    let hits: Vec<i64> = plan
        .todo
        .iter()
        .filter(|t| cached.contains(&t.cache_key))
        .map(|t| t.chunk_id)
        .collect();
    let to_send: Vec<&Todo> = plan
        .todo
        .iter()
        .filter(|t| !cached.contains(&t.cache_key) && !invalid.contains(&t.cache_key))
        .collect();
    let counts = Counts {
        hits: hits.len(),
        send: to_send.len(),
        invalid: plan.todo.len() - hits.len() - to_send.len(),
        resend,
    };
    stage.summary(&plan, &counts, started);
    let share = base.resend_share;
    if plan.eligible > 0 && resend as f64 > share * plan.eligible as f64 && !options.yes {
        println!(
            "{task}: {resend} of {} chunks would be sent again, more than resend_share {:.0}%; \
             run with --yes to go on",
            plan.eligible,
            share * 100.0
        );
        if !options.dry_run {
            bail!("{task} stopped before sending {resend} chunks again; run with --yes");
        }
    }
    let method = base.name.method();
    let precision = db::review::precision(pool).await?;
    let confidence = Confidence {
        base: base.confidence,
        measured: crate::review::measured(&precision, &method, config.review.min_verdicts),
    };
    if !confidence.measured.is_empty() {
        println!(
            "{task}: confidence from reviews for {} classes and properties",
            confidence.measured.len()
        );
    }
    if !plan.stale.is_empty() {
        stage.clear(&plan.stale, options.dry_run).await?;
    }
    let params = JobParams::default();
    let mut job = job::reopen(pool, task, base.name, version_id, &params, options, true).await?;
    if !options.dry_run && !hits.is_empty() {
        let written = write_cached(stage, &hits, &method, &confidence, version_id).await?;
        println!(
            "{task}: {} written from the cache for {written} chunks",
            S::WRITES
        );
    }
    if to_send.is_empty() && !options.dry_run {
        if let Some(open) = &job {
            db::llm::set_job(pool, open.id, "finished", None, None).await?;
        }
        stage.tidy().await?;
        stage.report(None).await?;
        println!("{task}: nothing to send");
        return Ok(Outcome::Finished);
    }
    if job.is_none() && !options.dry_run {
        let created = db::llm::create_job(
            pool,
            task,
            base.name.provider.as_str(),
            &base.name.model,
            version_id,
            &params,
            &[],
        )
        .await?;
        println!("{task} job {}: new", created.id);
        job = Some(created);
    }
    let job_id = job.as_ref().map_or(0, |j| j.id);
    let mut lock = pool.acquire().await?;
    if job_id > 0 && !db::llm::lock(&mut lock, job_id).await? {
        bail!("{task} job {job_id} is running in another process");
    }
    let (queue, chunk_of) = if job_id > 0 {
        let ids: Vec<i64> = to_send.iter().map(|t| t.chunk_id).collect();
        let keys: Vec<String> = to_send.iter().map(|t| t.cache_key.clone()).collect();
        let ords = db::llm::replace_items(pool, job_id, S::OWNER, &ids, &keys).await?;
        let queue: VecDeque<Work> = ords
            .iter()
            .zip(&to_send)
            .map(|(ord, t)| Work {
                ord: *ord,
                chars: t.chars,
            })
            .collect();
        let chunk_of = ords.iter().copied().zip(ids).collect();
        (queue, chunk_of)
    } else {
        let queue = to_send
            .iter()
            .enumerate()
            .map(|(i, t)| Work {
                ord: i as i32 + 1,
                chars: t.chars,
            })
            .collect();
        (queue, HashMap::new())
    };
    let handler = Model {
        stage,
        job: job_id,
        version_id,
        method,
        confidence,
        chunk_of,
        pending: Mutex::new(HashMap::new()),
    };
    let runner = Runner {
        pool,
        config,
        name: base.name,
        job_id,
        resume_after: job.as_ref().and_then(|j| j.resume_after),
    };
    let outcome = job::run(&runner, &handler, queue, options).await;
    if !options.dry_run {
        stage.tidy().await?;
    }
    outcome
}

struct Model<'a, S: Stage> {
    stage: &'a S,
    job: i64,
    version_id: i64,
    method: String,
    confidence: Confidence,
    chunk_of: HashMap<i32, i64>,
    pending: Mutex<HashMap<i32, S::Item>>,
}

impl<S: Stage> Handler for Model<'_, S> {
    fn task(&self) -> &'static str {
        self.stage.base().task
    }

    fn label(&self) -> String {
        format!("{} job {}", self.task(), self.job)
    }

    async fn request(&self, ord: i32) -> Result<Option<(String, Request)>> {
        let Some(chunk_id) = self.chunk_of.get(&ord) else {
            return Ok(None);
        };
        let Some(item) = self.stage.prepare(&[*chunk_id]).await?.pop() else {
            return Ok(None);
        };
        let keys = S::keys(&item);
        let pair = (keys.cache_key.to_string(), keys.request.clone());
        self.pending
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(ord, item);
        Ok(Some(pair))
    }

    fn accepts(&self, _ord: i32, response: &str) -> Result<(), String> {
        serde_json::from_str::<S::Answer>(response)
            .map(|_| ())
            .map_err(|e| e.to_string())
    }

    async fn answered(&self, ord: i32, response: &str) -> Result<()> {
        let taken = self
            .pending
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(&ord);
        let Some(item) = taken else {
            return Ok(());
        };
        let Ok(answer) = serde_json::from_str::<S::Answer>(response) else {
            return Ok(());
        };
        let checked = self.stage.check(item, answer);
        let drops = S::drops(&checked).clone();
        self.stage
            .write(&[checked], &self.method, &self.confidence, self.version_id)
            .await?;
        db::llm::add_stats(self.stage.base().pool, self.job, &drops).await
    }

    async fn report(&self) -> Result<()> {
        if let Ok(mut pending) = self.pending.lock() {
            pending.clear();
        }
        self.stage.report(Some(self.job)).await
    }
}
