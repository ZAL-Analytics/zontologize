use std::collections::{HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use anyhow::{Result, bail};
use globset::{Glob, GlobSet, GlobSetBuilder};
use sqlx::PgPool;

use super::check::check;
use super::sample::{Candidate, Settings, first_folder, letter_share, select, text_hash};
use super::{Answer, Built, ChunkInput, Context, Known, TASK, USER_VARIABLES, build};
use crate::config::{self, Config};
use crate::db;
use crate::db::facts::{ChunkFacts, ChunkRow, Confidence};
use crate::db::llm::{Job, JobParams};
use crate::llm::job::{self, Handler, Runner, Work};
use crate::llm::prompt::{self, Prompt};
use crate::llm::shortlist::counts_by_chunk;
use crate::llm::{ModelName, Request};
use crate::ontology::{self, Ontology};
use crate::scan::parallel_map;

pub use crate::llm::job::{Options, Outcome};

const BATCH: i64 = 2000;
const KEY_BATCH: usize = 10_000;

struct Setup<'a> {
    pool: &'a PgPool,
    config: &'a Config,
    ontology: Arc<Ontology>,
    exclude: Arc<Vec<String>>,
    system: Arc<Prompt>,
    user: Arc<Prompt>,
    name: &'a ModelName,
    threads: usize,
}

struct Prepared {
    built: Built,
    chunk: ChunkRow,
}

struct Todo {
    chunk_id: i64,
    key: String,
    chars: usize,
    resend: bool,
    order: (usize, i64, i64),
}

#[derive(Default)]
struct Plan {
    eligible: usize,
    done: usize,
    todo: Vec<Todo>,
}

struct Extract<'a> {
    setup: &'a Setup<'a>,
    job: i64,
    version_id: i64,
    method: String,
    confidence: Confidence,
    out: PathBuf,
    chunk_of: HashMap<i32, i64>,
    pending: Mutex<HashMap<i32, Prepared>>,
}

pub async fn run(
    config_dir: Option<&Path>,
    url: Option<&str>,
    options: &Options,
) -> Result<Outcome> {
    let config = config::load(config_dir)?;
    if config.models.extract.is_empty() {
        bail!("[models] extract is empty; set it to provider:model, for example claude-code:haiku");
    }
    let name = ModelName::parse(&config.models.extract)
        .map_err(|e| anyhow::anyhow!("models.extract: {e}"))?;
    if config.ontology.files.is_empty() {
        bail!("[ontology] files is empty");
    }
    let loaded = ontology::load(config_dir, &config.ontology)?;
    if let Some(error) = loaded.first_error() {
        bail!("ontology: {error}; run zontologize ontology check");
    }
    let rules = crate::rules::load(
        config_dir,
        &loaded.ontology,
        &config.ontology.document_class,
    )?;
    for doc_type in &config.llm_extract.doc_types {
        if !rules.doc_types.contains_key(doc_type) {
            bail!("llm_extract.doc_types: {doc_type:?} is not a key of [doc_types] in rules.toml");
        }
    }
    let mut exclude = Vec::new();
    for name in &config.llm_extract.exclude {
        let class = loaded
            .ontology
            .class(name)
            .map_err(|e| anyhow::anyhow!("llm_extract.exclude: {e}"))?;
        exclude.push(class.iri.clone());
    }
    let system = prompt::load(config_dir, "extract-system")?;
    system.check(&[])?;
    let user = prompt::load(config_dir, "extract-user")?;
    user.check(&USER_VARIABLES)?;
    let _lock = match options.dry_run {
        true => None,
        false => Some(db::runs::lock(url, &config.database.schema).await?),
    };
    let pool = db::pool::connect(
        url,
        &config.database.schema,
        config.database.max_connections,
    )
    .await?;
    let setup = Setup {
        pool: &pool,
        config: &config,
        ontology: Arc::new(loaded.ontology),
        exclude: Arc::new(exclude),
        system: Arc::new(system),
        user: Arc::new(user),
        name: &name,
        threads: config.scan.hash_threads,
    };
    let result = run_with(&setup, options).await;
    pool.close().await;
    result
}

async fn inputs(pool: &PgPool, ids: &[i64]) -> Result<Vec<(ChunkInput, ChunkRow)>> {
    let rows = db::facts::chunks(pool, ids).await?;
    let mut counts = counts_by_chunk(db::fit::class_counts(pool, ids).await?);
    let mut known: HashMap<i64, Vec<Known>> = HashMap::new();
    for row in db::facts::known(pool, ids).await? {
        known.entry(row.chunk_id).or_default().push(Known {
            entity_id: row.entity_id,
            class: row.class_iri,
            name: row.name,
        });
    }
    let mut docs = db::facts::doc_classes(pool, ids).await?;
    Ok(rows
        .into_iter()
        .map(|row| {
            let input = ChunkInput {
                id: row.id,
                text: row.text.clone(),
                path: row.path.clone(),
                pages: row.pages(),
                counts: counts.remove(&row.id).unwrap_or_default(),
                known: known.remove(&row.id).unwrap_or_default(),
                doc_class: docs.remove(&row.id),
            };
            (input, row)
        })
        .collect())
}

async fn prepare(setup: &Setup<'_>, ids: &[i64]) -> Result<Vec<Prepared>> {
    let pairs = inputs(setup.pool, ids).await?;
    let config = setup.config;
    let ontology = setup.ontology.clone();
    let system = setup.system.clone();
    let user = setup.user.clone();
    let model = setup.name.clone();
    let languages = config.ontology.languages.clone();
    let max_shortlist = config.llm_extract.max_shortlist;
    let exclude = setup.exclude.clone();
    let threads = setup.threads;
    let (built, pairs) = tokio::task::spawn_blocking(move || {
        let context = Context {
            ontology: &ontology,
            languages: &languages,
            max_shortlist,
            exclude: &exclude,
            system: &system,
            user: &user,
            model: &model,
        };
        let built = parallel_map(&pairs, threads, |(input, _)| build(&context, input));
        (built, pairs)
    })
    .await?;
    Ok(built
        .into_iter()
        .zip(pairs)
        .map(|(built, (_, chunk))| Prepared { built, chunk })
        .collect())
}

struct Row {
    id: i64,
    llm_key: Option<String>,
    order: (usize, i64, i64),
}

fn path_filter(patterns: &[String]) -> Result<Option<GlobSet>> {
    if patterns.is_empty() {
        return Ok(None);
    }
    let mut builder = GlobSetBuilder::new();
    for pattern in patterns {
        builder.add(Glob::new(pattern)?);
    }
    Ok(Some(builder.build()?))
}

async fn compare(setup: &Setup<'_>, rows: &[Row], plan: &mut Plan) -> Result<()> {
    let ids: Vec<i64> = rows.iter().map(|r| r.id).collect();
    let by_id: HashMap<i64, &Row> = rows.iter().map(|r| (r.id, r)).collect();
    for item in prepare(setup, &ids).await? {
        let Some(row) = by_id.get(&item.chunk.id) else {
            continue;
        };
        if row.llm_key.as_deref() == Some(item.built.key.as_str()) {
            plan.done += 1;
            continue;
        }
        plan.todo.push(Todo {
            chunk_id: row.id,
            key: item.built.key,
            chars: item.built.request.chars(),
            resend: row.llm_key.is_some(),
            order: row.order,
        });
    }
    Ok(())
}

async fn plan(setup: &Setup<'_>, sample: Option<usize>, seed: &str) -> Result<Plan> {
    let config = setup.config;
    let extract = &config.llm_extract;
    let source_order: Vec<&String> = if extract.sources.is_empty() {
        config.sources.iter().map(|s| &s.name).collect()
    } else {
        extract.sources.iter().collect()
    };
    let paths = path_filter(&extract.paths)?;
    let mut plan = Plan::default();
    let mut candidates = Vec::new();
    let mut rows: HashMap<i64, Row> = HashMap::new();
    let mut after = 0;
    loop {
        let eligible = db::facts::eligible(
            setup.pool,
            &extract.sources,
            &extract.doc_types,
            extract.min_chars as i32,
            after,
            BATCH,
        )
        .await?;
        let Some(last) = eligible.last() else {
            break;
        };
        after = last.id;
        let mut batch = Vec::new();
        for chunk in eligible {
            if paths.as_ref().is_some_and(|set| !set.is_match(&chunk.path)) {
                continue;
            }
            let source = source_order
                .iter()
                .position(|s| **s == chunk.source)
                .unwrap_or(source_order.len());
            let row = Row {
                id: chunk.id,
                llm_key: chunk.llm_key.clone(),
                order: (source, chunk.document_id, chunk.id),
            };
            if sample.is_some() {
                candidates.push(Candidate {
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
                });
                rows.insert(chunk.id, row);
            } else {
                batch.push(row);
            }
        }
        plan.eligible += batch.len();
        compare(setup, &batch, &mut plan).await?;
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
            "extract: sample of {} from {} eligible chunks ({} repeated texts and {} unreadable or \
             mostly numbers left out)",
            selected.ids.len(),
            candidates.len(),
            selected.duplicates,
            selected.junk
        );
        plan.eligible = selected.ids.len();
        let mut picked = Vec::new();
        for (rank, id) in selected.ids.iter().enumerate() {
            if let Some(mut row) = rows.remove(id) {
                row.order = (rank, 0, 0);
                picked.push(row);
            }
        }
        for part in picked.chunks(BATCH as usize) {
            compare(setup, part, &mut plan).await?;
        }
    }
    plan.todo.sort_by_key(|t| t.order);
    Ok(plan)
}

async fn split_known(pool: &PgPool, todo: &[Todo]) -> Result<(HashSet<String>, HashSet<String>)> {
    let mut cached = HashSet::new();
    let mut invalid = HashSet::new();
    for part in todo.chunks(KEY_BATCH) {
        let keys: Vec<String> = part.iter().map(|t| t.key.clone()).collect();
        let (hit, bad) = db::llm::known_keys(pool, &keys).await?;
        cached.extend(hit);
        invalid.extend(bad);
    }
    Ok((cached, invalid))
}

fn chunk_facts(setup: &Setup<'_>, prepared: Prepared, response: &str) -> Option<ChunkFacts> {
    let answer: Answer = serde_json::from_str(response).ok()?;
    let checked = check(
        answer,
        &prepared.built,
        &prepared.chunk.text,
        &setup.ontology,
        &setup.config.ontology.languages,
    );
    Some(ChunkFacts {
        chunk_id: prepared.chunk.id,
        key: prepared.built.key,
        dataset: prepared.chunk.dataset.clone(),
        scale: prepared.chunk.ocr_scale(),
        known: prepared.built.known.iter().map(|k| k.entity_id).collect(),
        checked,
    })
}

async fn write_cached(
    setup: &Setup<'_>,
    ids: &[i64],
    method: &str,
    confidence: &Confidence,
    version_id: i64,
) -> Result<usize> {
    let mut written = 0;
    for part in ids.chunks(BATCH as usize) {
        let prepared = prepare(setup, part).await?;
        let keys: Vec<String> = prepared.iter().map(|p| p.built.key.clone()).collect();
        let cache = db::llm::cached(setup.pool, &keys).await?;
        let mut facts = Vec::new();
        for item in prepared {
            let Some(response) = cache.get(&item.built.key) else {
                continue;
            };
            if let Some(chunk) = chunk_facts(setup, item, response) {
                facts.push(chunk);
            }
        }
        written += facts.len();
        db::facts::write(setup.pool, &facts, method, confidence, version_id).await?;
    }
    Ok(written)
}

async fn open_job(setup: &Setup<'_>, options: &Options, version_id: i64) -> Result<Option<Job>> {
    let Some(open) = db::llm::open_job(setup.pool, TASK).await? else {
        return Ok(None);
    };
    let name = setup.name;
    let changed = open.provider != name.provider.as_str()
        || open.model != name.model
        || open.ontology_version_id != Some(version_id);
    if !(options.new || changed) {
        return Ok(Some(open));
    }
    if !options.dry_run {
        let reason = if options.new {
            "abandoned by --new"
        } else {
            "model or ontology changed"
        };
        db::llm::set_job(setup.pool, open.id, "abandoned", Some(reason), None).await?;
        println!("extract job {}: abandoned, {reason}", open.id);
    }
    Ok(None)
}

async fn run_with(setup: &Setup<'_>, options: &Options) -> Result<Outcome> {
    let pool = setup.pool;
    let config = setup.config;
    let Some(version_id) = db::fit::ontology_version(pool, &setup.ontology.version).await? else {
        bail!("the ontology changed since the last scan; run zontologize scan first");
    };
    let seed = options
        .seed
        .clone()
        .unwrap_or_else(|| config.llm_extract.seed.clone());
    let plan = plan(setup, options.sample, &seed).await?;
    let (cached, invalid) = split_known(pool, &plan.todo).await?;
    let resend = plan.todo.iter().filter(|t| t.resend).count();
    let hits: Vec<i64> = plan
        .todo
        .iter()
        .filter(|t| cached.contains(&t.key))
        .map(|t| t.chunk_id)
        .collect();
    let to_send: Vec<&Todo> = plan
        .todo
        .iter()
        .filter(|t| !cached.contains(&t.key) && !invalid.contains(&t.key))
        .collect();
    println!(
        "extract: {} eligible chunks, {} done before, {} from the cache, {} to send, \
         {} invalid before, {resend} to send again after a change",
        plan.eligible,
        plan.done,
        hits.len(),
        to_send.len(),
        plan.todo.len() - hits.len() - to_send.len(),
    );
    let share = config.llm_extract.resend_share;
    if plan.eligible > 0 && resend as f64 > share * plan.eligible as f64 && !options.yes {
        println!(
            "extract: {resend} of {} chunks would be sent again, more than resend_share {:.0}%; \
             run with --yes to go on",
            plan.eligible,
            share * 100.0
        );
        if !options.dry_run {
            bail!("extract stopped before sending {resend} chunks again; run with --yes");
        }
    }
    let method = setup.name.method();
    let precision = db::review::precision(pool).await?;
    let confidence = Confidence {
        base: config.llm_extract.confidence,
        measured: crate::review::measured(&precision, &method, config.review.min_verdicts),
    };
    if !confidence.measured.is_empty() {
        println!(
            "extract: confidence from reviews for {} classes and properties",
            confidence.measured.len()
        );
    }
    let mut job = open_job(setup, options, version_id).await?;
    if !options.dry_run && !hits.is_empty() {
        let written = write_cached(setup, &hits, &method, &confidence, version_id).await?;
        println!("extract: facts written from the cache for {written} chunks");
    }
    let out = PathBuf::from(&config.llm_extract.out);
    if to_send.is_empty() && !options.dry_run {
        if let Some(open) = &job {
            db::llm::set_job(pool, open.id, "finished", None, None).await?;
        }
        db::facts::delete_orphans(pool).await?;
        super::report::write_report(pool, &setup.ontology, config, &out, None).await?;
        println!("extract: nothing to send");
        return Ok(Outcome::Finished);
    }
    if job.is_none() && !options.dry_run {
        let created = db::llm::create_job(
            pool,
            TASK,
            setup.name.provider.as_str(),
            &setup.name.model,
            version_id,
            &JobParams::default(),
            &[],
        )
        .await?;
        println!("extract job {}: new", created.id);
        job = Some(created);
    }
    let job_id = job.as_ref().map_or(0, |j| j.id);
    let mut lock = pool.acquire().await?;
    if job_id > 0 && !db::llm::lock(&mut lock, job_id).await? {
        bail!("extract job {job_id} is running in another process");
    }
    let (queue, chunk_of) = if job_id > 0 {
        let ids: Vec<i64> = to_send.iter().map(|t| t.chunk_id).collect();
        let keys: Vec<String> = to_send.iter().map(|t| t.key.clone()).collect();
        let ords = db::llm::replace_items(pool, job_id, &ids, &keys).await?;
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
    let handler = Extract {
        setup,
        job: job_id,
        version_id,
        method,
        confidence,
        out,
        chunk_of,
        pending: Mutex::new(HashMap::new()),
    };
    let runner = Runner {
        pool,
        config,
        name: setup.name,
        job_id,
        resume_after: job.as_ref().and_then(|j| j.resume_after),
    };
    let limits = options.limits();
    let outcome = job::run(&runner, &handler, queue, &limits).await;
    if !options.dry_run {
        db::facts::delete_orphans(pool).await?;
    }
    outcome
}

impl Extract<'_> {
    fn take(&self, ord: i32) -> Option<Prepared> {
        self.pending.lock().ok()?.remove(&ord)
    }
}

impl Handler for Extract<'_> {
    fn task(&self) -> &'static str {
        TASK
    }

    fn label(&self) -> String {
        format!("extract job {}", self.job)
    }

    async fn request(&self, ord: i32) -> Result<Option<(String, Request)>> {
        let Some(chunk_id) = self.chunk_of.get(&ord) else {
            return Ok(None);
        };
        let Some(prepared) = prepare(self.setup, &[*chunk_id]).await?.pop() else {
            return Ok(None);
        };
        let pair = (prepared.built.key.clone(), prepared.built.request.clone());
        if let Ok(mut pending) = self.pending.lock() {
            pending.insert(ord, prepared);
        }
        Ok(Some(pair))
    }

    fn accepts(&self, _ord: i32, response: &str) -> Result<(), String> {
        serde_json::from_str::<Answer>(response)
            .map(|_| ())
            .map_err(|e| e.to_string())
    }

    async fn answered(&self, ord: i32, response: &str) -> Result<()> {
        let Some(prepared) = self.take(ord) else {
            return Ok(());
        };
        let Some(facts) = chunk_facts(self.setup, prepared, response) else {
            return Ok(());
        };
        let drops = facts.checked.drops.clone();
        db::facts::write(
            self.setup.pool,
            &[facts],
            &self.method,
            &self.confidence,
            self.version_id,
        )
        .await?;
        db::llm::add_stats(self.setup.pool, self.job, &drops).await
    }

    async fn report(&self) -> Result<()> {
        if let Ok(mut pending) = self.pending.lock() {
            pending.clear();
        }
        super::report::write_report(
            self.setup.pool,
            &self.setup.ontology,
            self.setup.config,
            &self.out,
            Some(self.job),
        )
        .await
    }
}
