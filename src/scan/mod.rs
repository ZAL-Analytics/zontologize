pub mod chunk;
pub mod extract;
pub mod memory;
pub mod pool;
pub mod table;
pub mod walk;
pub mod worker;

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use serde::Serialize;
use sha2::{Digest, Sha256};
use sqlx::PgPool;
use sqlx::types::chrono::{DateTime, Utc};
use tokio::sync::mpsc;

use crate::config::{self, Config, SourceConfig, SourceKind};
use crate::db::documents::{self, Row, Stored};
use crate::db::{self, contents, entities};
use crate::ontology::{self, Ontology, hex};
use crate::rules::{self, Rules};
use chunk::ChunkSettings;
use extract::ExtractSettings;
use pool::{Done, Finished, Job, Limits, Pool, Restarts, WorkerCommand};
use walk::{Decision, FileEntry, Kinds, WalkItem};

const EXTRACT_BATCH: i64 = 200;
const MAX_EXTRACT_ATTEMPTS: i32 = 2;
const UNFINISHED: &str = "extraction did not finish in two attempts: the worker reading this \
                          file was stopped over extract.worker_memory_mb or crashed, or the \
                          scan itself was stopped";
const CHUNK_BATCH: i64 = 200;
const TEXT_RULES_BATCH: i64 = 100;

pub struct Prepared {
    pub config: Config,
    pub ontology: Ontology,
    pub rules: Rules,
    pub extract: ExtractSettings,
    pub chunk: ChunkSettings,
    pub sources: Vec<SourceConfig>,
    pub warnings: usize,
}

pub fn prepare(config_dir: Option<&Path>, only: &[String]) -> Result<Prepared> {
    let config = config::load(config_dir)?;
    let loaded = ontology::load(config_dir, &config.ontology)?;
    if let Some(error) = loaded.first_error() {
        bail!("ontology: {error} (run zontologize ontology check for all findings)");
    }
    let warnings = loaded.findings.len();
    let ontology = loaded.ontology;
    let rules = rules::load(config_dir, &ontology, &config.ontology.document_class)?;
    for name in only {
        if !config.sources.iter().any(|s| &s.name == name) {
            bail!("--source {name}: no [[sources]] entry has that name");
        }
    }
    let sources: Vec<SourceConfig> = config
        .sources
        .iter()
        .filter(|s| only.is_empty() || only.contains(&s.name))
        .cloned()
        .collect();
    if sources.is_empty() {
        bail!("no [[sources]] in zontologize.toml");
    }
    let mut checked = Vec::new();
    for source in sources {
        let root = resolve_root(config_dir, &source.root);
        if source.kind == SourceKind::Folder && !root.is_dir() {
            bail!(
                "sources.{}.root: {} is not a directory",
                source.name,
                root.display()
            );
        }
        checked.push(SourceConfig {
            root: root.display().to_string(),
            ..source
        });
    }
    let extract = ExtractSettings::new(&config.extract, &config.ocr);
    let chunk = ChunkSettings::new(&config.chunk, config.extract.format);
    Ok(Prepared {
        config,
        ontology,
        rules,
        extract,
        chunk,
        sources: checked,
        warnings,
    })
}

fn resolve_root(config_dir: Option<&Path>, root: &str) -> PathBuf {
    let path = PathBuf::from(root);
    if path.is_absolute() {
        return path;
    }
    match config_dir {
        Some(dir) => dir.join(path),
        None => path,
    }
}

#[derive(Debug, Default, Serialize)]
pub struct SourceStats {
    pub files: u64,
    pub new: u64,
    pub changed: u64,
    pub unchanged: u64,
    pub skipped: u64,
    pub errors: u64,
    pub hashed: u64,
    pub deleted: u64,
    pub moved: u64,
    pub complete: bool,
    pub seconds: f64,
}

#[derive(Debug, Default, Serialize)]
pub struct StageStats {
    pub items: u64,
    pub errors: u64,
    pub seconds: f64,
}

#[derive(Debug, Default, Serialize)]
pub struct RunStats {
    pub sources: HashMap<String, SourceStats>,
    pub walk: StageStats,
    pub extract: StageStats,
    pub workers: Restarts,
    pub extract_left: i64,
    pub chunk: StageStats,
    pub rules: StageStats,
    pub ocr_ids: OcrIdStats,
    pub segments: SegmentStats,
}

#[derive(Debug, Default, Serialize)]
pub struct SegmentStats {
    pub segments: u64,
    pub split: u64,
}

#[derive(Debug, Default, Serialize)]
pub struct OcrIdStats {
    pub accepted: u64,
    pub rejected: u64,
}

pub struct Options {
    pub worker: WorkerCommand,
    pub extract_minutes: Option<u64>,
}

impl Options {
    pub fn new(worker_program: PathBuf) -> Options {
        Options {
            worker: WorkerCommand {
                program: worker_program,
                args: vec![worker::SUBCOMMAND.to_string()],
            },
            extract_minutes: None,
        }
    }
}

pub async fn run(
    config_dir: Option<&Path>,
    only: &[String],
    url: Option<&str>,
    options: &Options,
) -> Result<()> {
    let prepared = Arc::new(prepare(config_dir, only)?);
    if prepared.warnings > 0 {
        println!(
            "ontology: {} warnings (run zontologize ontology check)",
            prepared.warnings
        );
    }
    let config = &prepared.config;
    let _lock = db::runs::lock(url, &config.database.schema).await?;
    let pool = db::pool::connect(
        url,
        &config.database.schema,
        config.database.max_connections,
    )
    .await?;
    check_database(&pool, config).await?;
    for source in prepared
        .sources
        .iter()
        .filter(|s| s.kind == SourceKind::Table)
    {
        table::check_view(&pool, &source.view)
            .await
            .map_err(|e| anyhow::anyhow!("sources.{}: {e:#}", source.name))?;
    }
    let config_hash = hex(&Sha256::digest(format!("{config:?}").as_bytes()));
    let (run_id, aborted) = db::runs::start(&pool, &config_hash).await?;
    if aborted > 0 {
        println!("marked {aborted} earlier run(s) that did not finish as aborted");
    }
    println!("run {run_id}");
    let mut stats = RunStats::default();
    let result = stages(&pool, prepared.clone(), run_id, options, &mut stats).await;
    let status = if result.is_ok() { "finished" } else { "failed" };
    let json = serde_json::to_value(&stats)?;
    db::runs::finish(&pool, run_id, status, &json).await?;
    pool.close().await;
    result
}

async fn check_database(pool: &PgPool, config: &Config) -> Result<()> {
    let mut conn = pool.acquire().await?;
    let applied = db::migrate::applied_versions(&mut conn, &config.database.schema).await?;
    let missing = db::migrate::MIGRATOR
        .iter()
        .any(|m| !applied.contains(&m.version));
    if missing {
        bail!(
            "schema {} is not up to date, run zontologize db migrate",
            config.database.schema
        );
    }
    if !contents::check_text_search(pool, &config.chunk.text_search).await? {
        bail!(
            "chunk.text_search: {:?} is not a text search configuration in this database",
            config.chunk.text_search
        );
    }
    Ok(())
}

async fn stages(
    pool: &PgPool,
    prepared: Arc<Prepared>,
    run_id: i64,
    options: &Options,
    stats: &mut RunStats,
) -> Result<()> {
    let (version_id, stored) = db::ontology::store(pool, &prepared.ontology, run_id).await?;
    println!(
        "ontology: version {} ({})",
        &prepared.ontology.version[..12],
        if stored { "new" } else { "unchanged" }
    );
    let kinds = Kinds::new(&prepared.config.scan.kinds);
    let mut roots = HashMap::new();
    let mut source_ids = Vec::new();
    let started = Instant::now();
    for source in &prepared.sources {
        let view = (source.kind == SourceKind::Table).then_some(source.view.as_str());
        let id = documents::upsert_source(
            pool,
            &source.name,
            source.kind.as_str(),
            &source.root,
            &source.dataset,
            view,
        )
        .await?;
        source_ids.push(id);
        roots.insert(id, PathBuf::from(&source.root));
        let source_started = Instant::now();
        let mut source_stats = match source.kind {
            SourceKind::Folder => scan_folder(pool, id, run_id, source, &prepared, &kinds).await?,
            SourceKind::Table => {
                table::scan_table(pool, id, run_id, source, &prepared, &kinds).await?
            }
        };
        source_stats.seconds = source_started.elapsed().as_secs_f64();
        println!(
            "source {}: {} files, {} new, {} changed, {} unchanged, {} skipped, {} errors, \
             {} deleted, {} moved, {:.1} s",
            source.name,
            source_stats.files,
            source_stats.new,
            source_stats.changed,
            source_stats.unchanged,
            source_stats.skipped,
            source_stats.errors,
            source_stats.deleted,
            source_stats.moved,
            source_stats.seconds
        );
        if !source_stats.complete {
            println!(
                "source {}: the walk did not complete, nothing was marked deleted",
                source.name
            );
        }
        stats.walk.items += source_stats.files;
        stats.walk.errors += source_stats.errors;
        stats.sources.insert(source.name.clone(), source_stats);
    }
    stats.walk.seconds = started.elapsed().as_secs_f64();
    extract_stage(pool, &prepared, &source_ids, &roots, options, stats).await?;
    stats.chunk = chunk_stage(pool, &prepared, &source_ids).await?;
    rules_stage(pool, &prepared, &source_ids, version_id, stats).await?;
    Ok(())
}

#[derive(Debug, Clone)]
pub struct Found {
    pub path: String,
    pub external_id: Option<String>,
    pub file: Option<String>,
    pub size: i64,
    pub mtime: DateTime<Utc>,
    pub sha256: Option<String>,
}

#[derive(Debug, PartialEq)]
pub enum Plan {
    Unchanged(i64),
    Hash(Row),
    Write(Row),
}

pub fn plan(stored: Option<&Stored>, found: &Found, decision: &Decision) -> Plan {
    if let Some(stored) = stored
        && is_unchanged(stored, found, decision)
    {
        return Plan::Unchanged(stored.id);
    }
    let mut row = Row {
        id: stored.map(|s| s.id),
        path: found.path.clone(),
        external_id: found.external_id.clone(),
        file: found.file.clone(),
        sha256: None,
        size: found.size,
        mtime: found.mtime,
        kind: None,
        status: "present",
        reason: None,
    };
    match decision {
        Decision::Read(kind) => {
            row.kind = Some(kind.clone());
            match &found.sha256 {
                Some(sha) => {
                    row.sha256 = Some(sha.clone());
                    Plan::Write(row)
                }
                None => Plan::Hash(row),
            }
        }
        Decision::Skip { kind, reason } => {
            row.kind = kind.clone();
            row.status = "skipped";
            row.reason = Some(reason.clone());
            Plan::Write(row)
        }
    }
}

fn is_unchanged(stored: &Stored, found: &Found, decision: &Decision) -> bool {
    let same_file = stored.path == found.path
        && stored.file == found.file
        && stored.size == found.size
        && stored.mtime == found.mtime
        && (found.sha256.is_none() || stored.sha256 == found.sha256);
    let same_decision = match decision {
        Decision::Read(kind) => stored.status == "present" && stored.kind.as_deref() == Some(kind),
        Decision::Skip { kind, reason } => {
            stored.status == "skipped"
                && stored.kind == *kind
                && stored.reason.as_deref() == Some(reason.as_str())
        }
    };
    same_file && same_decision
}

pub fn file_path(root: &Path, path: &str, file: Option<&str>) -> PathBuf {
    match file {
        Some(file) if Path::new(file).is_absolute() => PathBuf::from(file),
        Some(file) => root.join(file),
        None => root.join(path),
    }
}

pub async fn apply_plans(
    pool: &PgPool,
    source_id: i64,
    run_id: i64,
    root: &Path,
    plans: Vec<Planned>,
    threads: usize,
    stats: &mut SourceStats,
) -> Result<()> {
    let mut unchanged = Vec::new();
    let mut rows = Vec::new();
    let mut to_hash = Vec::new();
    for Planned {
        plan,
        is_new,
        skipped,
    } in plans
    {
        if skipped {
            stats.skipped += 1;
        }
        match plan {
            Plan::Unchanged(id) => {
                stats.unchanged += 1;
                unchanged.push(id);
            }
            Plan::Write(row) => {
                count(stats, is_new);
                rows.push(row);
            }
            Plan::Hash(row) => {
                count(stats, is_new);
                to_hash.push(row);
            }
        }
    }
    if !to_hash.is_empty() {
        let paths: Vec<PathBuf> = to_hash
            .iter()
            .map(|r| file_path(root, &r.path, r.file.as_deref()))
            .collect();
        let hashes = tokio::task::spawn_blocking(move || walk::hash_all(&paths, threads)).await?;
        stats.hashed += to_hash.len() as u64;
        for (mut row, hash) in to_hash.into_iter().zip(hashes) {
            match hash {
                Ok(sha) => row.sha256 = Some(sha),
                Err(error) => {
                    stats.errors += 1;
                    row.status = "error";
                    row.reason = Some(format!("{error:#}"));
                }
            }
            rows.push(row);
        }
    }
    documents::write(pool, source_id, run_id, &rows).await?;
    documents::touch(pool, &unchanged, run_id).await?;
    Ok(())
}

fn count(stats: &mut SourceStats, is_new: bool) {
    if is_new {
        stats.new += 1;
    } else {
        stats.changed += 1;
    }
}

pub struct Planned {
    pub plan: Plan,
    pub is_new: bool,
    pub skipped: bool,
}

pub fn planned(stored: Option<&Stored>, found: &Found, decision: &Decision) -> Planned {
    Planned {
        plan: plan(stored, found, decision),
        is_new: stored.is_none(),
        skipped: matches!(decision, Decision::Skip { .. }),
    }
}

async fn scan_folder(
    pool: &PgPool,
    source_id: i64,
    run_id: i64,
    source: &SourceConfig,
    prepared: &Prepared,
    kinds: &Kinds,
) -> Result<SourceStats> {
    let scan = &prepared.config.scan;
    let root = PathBuf::from(&source.root);
    let mut patterns = scan.ignore.clone();
    patterns.extend(source.ignore.iter().cloned());
    let ignore = walk::globs(&patterns)?;
    let (sender, mut receiver) = mpsc::channel(2);
    let walk_root = root.clone();
    let batch_size = scan.batch_size;
    let walker =
        tokio::task::spawn_blocking(move || walk::walk(walk_root, ignore, batch_size, sender));
    let mut stats = SourceStats::default();
    while let Some(item) = receiver.recv().await {
        let files: Vec<FileEntry> = match item {
            WalkItem::Files(files) => files,
            WalkItem::Error(message) => {
                stats.errors += 1;
                eprintln!("warning: source {}: {message}", source.name);
                continue;
            }
        };
        stats.files += files.len() as u64;
        let paths: Vec<String> = files.iter().map(|f| f.path.clone()).collect();
        let stored: HashMap<String, Stored> = documents::by_paths(pool, source_id, &paths)
            .await?
            .into_iter()
            .map(|s| (s.path.clone(), s))
            .collect();
        let plans: Vec<Planned> = files
            .into_iter()
            .map(|file| {
                let found = Found {
                    path: file.path,
                    external_id: None,
                    file: None,
                    size: file.size as i64,
                    mtime: file.mtime,
                    sha256: None,
                };
                let decision = kinds.decide(&found.path, file.size);
                planned(stored.get(&found.path), &found, &decision)
            })
            .collect();
        apply_plans(
            pool,
            source_id,
            run_id,
            &root,
            plans,
            scan.hash_threads,
            &mut stats,
        )
        .await?;
    }
    stats.complete = walker.await.context("walk thread failed")??;
    if stats.complete {
        stats.deleted = documents::mark_deleted(pool, source_id, run_id).await?;
        stats.moved = documents::merge_moves(pool, source_id, run_id).await?;
        stats.deleted -= stats.moved.min(stats.deleted);
        stats.new -= stats.moved.min(stats.new);
    }
    Ok(stats)
}

async fn extract_stage(
    pool: &PgPool,
    prepared: &Arc<Prepared>,
    source_ids: &[i64],
    roots: &HashMap<i64, PathBuf>,
    options: &Options,
    run: &mut RunStats,
) -> Result<()> {
    let started = Instant::now();
    let key = prepared.extract.key.clone();
    let (failed, retried) =
        contents::recover_unfinished(pool, &key, MAX_EXTRACT_ATTEMPTS, UNFINISHED).await?;
    if failed + retried > 0 {
        println!(
            "extract: an earlier run stopped while reading files: {retried} retried one at a time, \
             {failed} marked as errors after {MAX_EXTRACT_ATTEMPTS} attempts"
        );
    }
    let todo = contents::count_to_extract(pool, &key, source_ids).await?;
    println!("extract: {todo} to extract");
    if todo == 0 {
        return Ok(());
    }
    let deadline = options
        .extract_minutes
        .map(|minutes| started + Duration::from_secs(minutes * 60));
    let pass = Pass {
        pool,
        prepared,
        source_ids,
        roots,
        worker: &options.worker,
        deadline,
    };
    let mut stats = StageStats::default();
    let workers = prepared.config.extract.workers;
    run.workers.add(pass.run(true, 1, &mut stats).await?);
    run.workers.add(pass.run(false, workers, &mut stats).await?);
    loop {
        let (failed, retried) =
            contents::recover_unfinished(pool, &key, MAX_EXTRACT_ATTEMPTS, UNFINISHED).await?;
        stats.items += failed;
        stats.errors += failed;
        if retried == 0 {
            break;
        }
        println!("extract: {retried} file(s) whose worker was stopped are retried one at a time");
        run.workers.add(pass.run(true, 1, &mut stats).await?);
    }
    stats.seconds = started.elapsed().as_secs_f64();
    println!(
        "extract: {} done, {} errors, {:.1} s",
        stats.items, stats.errors, stats.seconds
    );
    if deadline.is_some_and(|d| Instant::now() >= d) {
        run.extract_left = contents::count_to_extract(pool, &key, source_ids).await?;
        println!(
            "extract: stopped after --extract-minutes, {} left for the next run",
            run.extract_left
        );
    }
    run.extract = stats;
    Ok(())
}

pub fn setup_line(config: &Config, workers: usize) -> Result<String> {
    let mut extract = config.extract.clone();
    extract.threads = (extract.threads / workers.max(1)).max(1);
    let setup = worker::Setup {
        extract,
        ocr: config.ocr.clone(),
    };
    Ok(serde_json::to_string(&setup)?)
}

struct Pass<'a> {
    pool: &'a PgPool,
    prepared: &'a Arc<Prepared>,
    source_ids: &'a [i64],
    roots: &'a HashMap<i64, PathBuf>,
    worker: &'a WorkerCommand,
    deadline: Option<Instant>,
}

impl Pass<'_> {
    async fn run(&self, retries: bool, workers: usize, stats: &mut StageStats) -> Result<Restarts> {
        let config = &self.prepared.config;
        let key = &self.prepared.extract.key;
        let limits = Limits {
            timeout: Duration::from_secs(config.extract.timeout_secs),
            memory_mb: config.extract.worker_memory_mb,
            files: config.extract.worker_files,
        };
        let setup = setup_line(config, workers)?;
        let mut workers_pool: Option<Pool> = None;
        let mut after = 0;
        'feed: loop {
            let batch = contents::next_to_extract(
                self.pool,
                key,
                self.source_ids,
                retries,
                after,
                EXTRACT_BATCH,
            )
            .await?;
            let Some(last) = batch.last() else {
                break;
            };
            after = last.id;
            for item in batch {
                if self.deadline.is_some_and(|d| Instant::now() >= d) {
                    break 'feed;
                }
                let root = self.roots.get(&item.source_id).cloned().unwrap_or_default();
                let path = file_path(&root, &item.path, item.file.as_deref());
                let running = workers_pool.get_or_insert_with(|| {
                    Pool::start(self.worker, setup.clone(), workers, limits)
                });
                contents::start_attempt(self.pool, item.id).await?;
                if !running.submit(Job { id: item.id, path }).await? {
                    break 'feed;
                }
                while let Some(finished) = running.try_finished() {
                    self.record(finished, &limits, workers == 1, stats).await?;
                }
            }
        }
        let Some(mut running) = workers_pool else {
            return Ok(Restarts::default());
        };
        while let Some(finished) = running.next_finished().await {
            self.record(finished, &limits, workers == 1, stats).await?;
        }
        running.join().await
    }

    async fn record(
        &self,
        finished: Finished,
        limits: &Limits,
        alone: bool,
        stats: &mut StageStats,
    ) -> Result<()> {
        let key = &self.prepared.extract.key;
        let path = finished.path.display();
        let message = match finished.done {
            Done::Pages(pages) => {
                contents::write_pages(self.pool, finished.id, key, &pages, finished.seconds)
                    .await?;
                tally(stats, true);
                return Ok(());
            }
            Done::Error(message) => message,
            Done::Panic(message) => format!("the extractor panicked on {path}: {message}"),
            Done::Timeout if !alone => {
                eprintln!(
                    "warning: {path} timed out after {:?} next to other workers; \
                     it is retried alone with all threads",
                    limits.timeout
                );
                return Ok(());
            }
            Done::Timeout => format!(
                "extraction timed out after {:?}, worker stopped: {path}",
                limits.timeout
            ),
            Done::Lost(reason) => {
                eprintln!(
                    "warning: the worker reading {path} was stopped ({reason:?}); \
                     the file is retried alone"
                );
                return Ok(());
            }
        };
        eprintln!("error: {message}");
        contents::write_error(self.pool, finished.id, key, &message, finished.seconds).await?;
        tally(stats, false);
        Ok(())
    }
}

fn tally(stats: &mut StageStats, ok: bool) {
    stats.items += 1;
    if !ok {
        stats.errors += 1;
    }
}

pub fn parallel_map<T: Sync, R: Send>(
    items: &[T],
    threads: usize,
    f: impl Fn(&T) -> R + Sync,
) -> Vec<R> {
    if items.is_empty() {
        return Vec::new();
    }
    let per_thread = items.len().div_ceil(threads.clamp(1, items.len()));
    let f = &f;
    std::thread::scope(|scope| {
        let handles: Vec<_> = items
            .chunks(per_thread)
            .map(|chunk| scope.spawn(move || chunk.iter().map(f).collect::<Vec<R>>()))
            .collect();
        handles
            .into_iter()
            .flat_map(|h| h.join().unwrap_or_else(|p| std::panic::resume_unwind(p)))
            .collect()
    })
}

async fn chunk_stage(
    pool: &PgPool,
    prepared: &Arc<Prepared>,
    source_ids: &[i64],
) -> Result<StageStats> {
    let started = Instant::now();
    let key = prepared.chunk.key.clone();
    let todo = contents::count_to_chunk(pool, &key, source_ids).await?;
    println!("chunk: {todo} to chunk");
    let mut stats = StageStats::default();
    let mut chunks_written = 0;
    let mut after = 0;
    let threads = prepared.config.scan.hash_threads;
    loop {
        let ids = contents::next_to_chunk(pool, &key, source_ids, after, CHUNK_BATCH).await?;
        let Some(last) = ids.last() else {
            break;
        };
        after = *last;
        let pages = contents::pages_of(pool, &ids).await?;
        let mut grouped: Vec<(i64, Vec<(i32, String)>)> =
            ids.iter().map(|id| (*id, Vec::new())).collect();
        let index: HashMap<i64, usize> = ids.iter().enumerate().map(|(i, id)| (*id, i)).collect();
        for (content_id, page_no, text) in pages {
            if let Some(i) = index.get(&content_id) {
                grouped[*i].1.push((page_no, text));
            }
        }
        let worker = prepared.clone();
        let results = tokio::task::spawn_blocking(move || {
            parallel_map(&grouped, threads, |(id, pages)| {
                (*id, chunk::chunk_pages(pages, &worker.chunk))
            })
        })
        .await?;
        for (content_id, result) in results {
            stats.items += 1;
            let chunks = match result {
                Ok(chunks) => chunks,
                Err(error) => {
                    stats.errors += 1;
                    eprintln!("error: content {content_id}: {error:#}");
                    Vec::new()
                }
            };
            chunks_written += chunks.len();
            contents::write_chunks(
                pool,
                content_id,
                &key,
                &prepared.config.chunk.text_search,
                &chunks,
            )
            .await?;
        }
    }
    stats.seconds = started.elapsed().as_secs_f64();
    if todo > 0 {
        println!(
            "chunk: {} contents, {chunks_written} chunks, {:.1} s",
            stats.items, stats.seconds
        );
    }
    Ok(stats)
}

pub const PATH_METHOD: &str = "path";

pub async fn rules_only(config_dir: Option<&Path>, pool: &PgPool) -> Result<()> {
    let prepared = Arc::new(prepare(config_dir, &[])?);
    let names: Vec<String> = prepared.sources.iter().map(|s| s.name.clone()).collect();
    let ids = documents::source_ids(pool, &names).await?;
    let Some(version_id) = db::fit::ontology_version(pool, &prepared.ontology.version).await?
    else {
        bail!("the ontology changed since the last scan; run zontologize scan first");
    };
    rules_stage(pool, &prepared, &ids, version_id, &mut RunStats::default()).await
}

async fn rules_stage(
    pool: &PgPool,
    prepared: &Arc<Prepared>,
    source_ids: &[i64],
    version_id: i64,
    run: &mut RunStats,
) -> Result<()> {
    let started = Instant::now();
    let rules = &prepared.rules;
    let batch = prepared.config.scan.batch_size as i64;
    let threads = prepared.config.scan.hash_threads;
    let documents_todo = entities::count_documents_to_rule(pool, &rules.key, source_ids).await?;
    let contents_todo =
        entities::count_contents_to_rule(pool, &rules.key, source_ids, &prepared.chunk.key).await?;
    println!("rules: {documents_todo} documents and {contents_todo} contents to rule");
    let mut stats = StageStats::default();
    let mut after = 0;
    loop {
        let work =
            entities::next_documents_to_rule(pool, &rules.key, source_ids, after, batch).await?;
        let Some(last) = work.last() else {
            break;
        };
        after = last.id;
        let outputs: Vec<entities::DocumentOutput> = work
            .into_iter()
            .map(|doc| document_output(rules, doc))
            .collect();
        stats.items += outputs.len() as u64;
        entities::write_document_rules(pool, &outputs, &rules.key, version_id).await?;
    }
    let known = Arc::new(known_keys(pool, rules).await?);
    let mut after = 0;
    loop {
        let ids = entities::next_contents_to_rule(
            pool,
            &rules.key,
            source_ids,
            &prepared.chunk.key,
            after,
            TEXT_RULES_BATCH,
        )
        .await?;
        let Some(last) = ids.last() else {
            break;
        };
        after = *last;
        let chunks = entities::chunks_of(pool, &ids).await?;
        let pages = if rules.page_types.is_empty() {
            Vec::new()
        } else {
            entities::pages_of(pool, &ids).await?
        };
        let model_types: ModelTypes = if rules.page_types.is_empty() {
            HashMap::new()
        } else {
            entities::model_types(pool, &ids)
                .await?
                .into_iter()
                .map(|(content, page, model)| ((content, page), model))
                .collect()
        };
        let worker = prepared.clone();
        let known = known.clone();
        let content_ids = ids.clone();
        let (outputs, segments) = tokio::task::spawn_blocking(move || {
            let segments = parallel_map(&content_ids, threads, |id| {
                segment_content(&worker.rules, *id, &pages, &chunks, &model_types)
            });
            let outputs = parallel_map(&chunks, threads, |chunk| entities::ChunkOutput {
                id: chunk.id,
                dataset: chunk.dataset.clone(),
                facts: worker
                    .rules
                    .text_facts(&chunk.text, chunk.ocr_scale(), &known),
            });
            (outputs, segments)
        })
        .await?;
        for content in &segments {
            run.segments.segments += content.segments.len() as u64;
            run.segments.split += u64::from(content.segments.len() > 1);
        }
        for output in &outputs {
            run.ocr_ids.rejected += output.facts.rejected as u64;
            run.ocr_ids.accepted += output
                .facts
                .entities
                .iter()
                .filter(|hit| hit.method == rules::ids::OCR_METHOD)
                .count() as u64;
        }
        stats.items += ids.len() as u64;
        entities::write_text_rules(
            pool,
            &ids,
            &outputs,
            &segments,
            rules.part_property.as_deref(),
            &rules.key,
            version_id,
        )
        .await?;
    }
    let unlisted = entities::clear_unlisted_contents(pool).await?;
    if unlisted > 0 {
        println!("rules: facts cleared for {unlisted} contents that no present file uses");
    }
    if documents_todo + contents_todo > 0 {
        let similar = prepared.config.resolve.similar.property.as_str();
        let stale = entities::delete_unselected(pool, version_id, similar).await?;
        if stale > 0 {
            println!(
                "rules: {stale} facts removed whose class or property the ontology no longer selects"
            );
        }
    }
    let orphans = entities::delete_orphans(pool).await?;
    entities::sync_aliases(pool, &rules.lexicon.entries).await?;
    stats.seconds = started.elapsed().as_secs_f64();
    if documents_todo + contents_todo > 0 {
        println!(
            "rules: {} rows, {orphans} entities without mentions removed, {:.1} s",
            stats.items, stats.seconds
        );
    }
    if run.ocr_ids.accepted + run.ocr_ids.rejected > 0 {
        println!(
            "rules: identifiers in OCR text: {} matched a known key, {} rejected",
            run.ocr_ids.accepted, run.ocr_ids.rejected
        );
    }
    run.rules = stats;
    Ok(())
}

pub fn document_output(rules: &Rules, doc: entities::DocumentWork) -> entities::DocumentOutput {
    let mut facts = (doc.status != "deleted").then(|| rules.document_facts(&doc.path));
    let mut type_method = None;
    let mut type_confidence = 1.0;
    if let Some(facts) = facts.as_mut() {
        if facts.doc_type.is_some() {
            type_method = Some(PATH_METHOD.to_string());
        } else if let Some(doc_type) = doc.llm_type.filter(|t| rules.doc_types.contains_key(t)) {
            facts.doc_type = Some(doc_type);
            type_method = doc.llm_method;
            type_confidence = doc.llm_confidence.unwrap_or(1.0);
        }
    }
    let document_class = facts.as_ref().and_then(|f| {
        rules
            .document_class_for(f.doc_type.as_deref())
            .map(str::to_string)
    });
    entities::DocumentOutput {
        id: doc.id,
        path: doc.path,
        dataset: doc.dataset,
        document_class,
        type_method,
        type_confidence,
        facts,
    }
}

pub type ModelTypes = HashMap<(i64, i32), entities::ModelType>;

pub fn segment_content(
    rules: &Rules,
    content_id: i64,
    pages: &[entities::PageWork],
    chunks: &[entities::ChunkWork],
    model_types: &ModelTypes,
) -> entities::ContentSegments {
    let own: Vec<&entities::PageWork> = pages
        .iter()
        .filter(|p| p.content_id == content_id)
        .collect();
    let inputs: Vec<rules::pages::PageInput> = own
        .iter()
        .map(|page| rules::pages::PageInput {
            page_no: page.page_no,
            text: &page.text,
            keys: rules
                .ids
                .find(&page.text, rules::ids::On::Text)
                .into_iter()
                .map(|hit| hit.key)
                .collect(),
            ocr_confidence: page.ocr_confidence,
        })
        .collect();
    let segments = rules.page_types.segments(&inputs);
    let mut entities = Vec::new();
    if segments.len() > 1 {
        let own_chunks: Vec<&entities::ChunkWork> = chunks
            .iter()
            .filter(|c| c.content_id == content_id)
            .collect();
        for segment in &segments {
            let model = model_types
                .get(&(content_id, segment.page_from))
                .filter(|_| segment.doc_type.is_none())
                .filter(|m| rules.doc_types.contains_key(&m.doc_type));
            let doc_type = segment
                .doc_type
                .as_deref()
                .or(model.map(|m| m.doc_type.as_str()));
            let Some(class) = rules.document_class_for(doc_type) else {
                continue;
            };
            let first = own_chunks
                .iter()
                .find(|c| c.page_to.is_none_or(|to| to >= segment.page_from));
            let Some(chunk) = first else {
                continue;
            };
            entities.push(entities::SegmentEntity {
                class: class.to_string(),
                method: model.map_or(entities::SEGMENT_METHOD.to_string(), |m| m.method.clone()),
                key: format!("{}#{}", own[0].sha256, segment.ord),
                name: format!(
                    "{} pages {}-{}",
                    doc_type.unwrap_or("segment"),
                    segment.page_from,
                    segment.page_to
                ),
                dataset: chunk.dataset.clone(),
                chunk_id: chunk.id,
                span_end: chunk.text.chars().count(),
                confidence: segment.confidence * model.map_or(1.0, |m| m.confidence),
            });
        }
    }
    entities::ContentSegments {
        content_id,
        segments,
        entities,
    }
}

async fn known_keys(pool: &PgPool, rules: &Rules) -> Result<rules::ids::KnownKeys> {
    let mut known = rules::ids::KnownKeys::new();
    let stored = entities::known_keys(pool, rules::ids::OCR_METHOD).await?;
    let listed = rules
        .lexicon
        .entries
        .iter()
        .map(|e| (e.class.clone(), e.key.clone()));
    for (class, key) in stored.into_iter().chain(listed) {
        known.entry(class).or_default().insert(key);
    }
    Ok(known)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stored(status: &str, kind: Option<&str>, reason: Option<&str>) -> Stored {
        Stored {
            id: 7,
            path: "a/b.pdf".to_string(),
            external_id: None,
            file: None,
            sha256: Some("aa".to_string()),
            size: 10,
            mtime: DateTime::from_timestamp(1_700_000_000, 0).unwrap(),
            kind: kind.map(str::to_string),
            status: status.to_string(),
            reason: reason.map(str::to_string),
        }
    }

    fn found(size: i64) -> Found {
        Found {
            path: "a/b.pdf".to_string(),
            external_id: None,
            file: None,
            size,
            mtime: DateTime::from_timestamp(1_700_000_000, 0).unwrap(),
            sha256: None,
        }
    }

    fn read() -> Decision {
        Decision::Read("pdf".to_string())
    }

    #[test]
    fn same_path_size_and_mtime_is_unchanged() {
        let s = stored("present", Some("pdf"), None);
        assert_eq!(plan(Some(&s), &found(10), &read()), Plan::Unchanged(7));
    }

    #[test]
    fn new_or_changed_file_is_hashed() {
        assert!(matches!(plan(None, &found(10), &read()), Plan::Hash(r) if r.id.is_none()));
        let s = stored("present", Some("pdf"), None);
        assert!(matches!(plan(Some(&s), &found(11), &read()), Plan::Hash(r) if r.id == Some(7)));
    }

    #[test]
    fn error_and_deleted_rows_are_retried() {
        for status in ["error", "deleted"] {
            let s = stored(status, Some("pdf"), None);
            assert!(matches!(plan(Some(&s), &found(10), &read()), Plan::Hash(_)));
        }
    }

    #[test]
    fn skip_decision_writes_without_hash() {
        let skip = Decision::Skip {
            kind: None,
            reason: "extension .zip".to_string(),
        };
        let Plan::Write(row) = plan(None, &found(10), &skip) else {
            panic!("expected a write");
        };
        assert_eq!(row.status, "skipped");
        assert_eq!(row.sha256, None);
        let s = stored("skipped", None, Some("extension .zip"));
        assert_eq!(plan(Some(&s), &found(10), &skip), Plan::Unchanged(7));
    }

    #[test]
    fn kind_change_in_config_rewrites() {
        let s = stored("skipped", None, Some("extension .pdf"));
        assert!(matches!(plan(Some(&s), &found(10), &read()), Plan::Hash(_)));
    }

    #[test]
    fn given_sha_skips_hashing_and_detects_change() {
        let mut f = found(10);
        f.sha256 = Some("bb".to_string());
        let s = stored("present", Some("pdf"), None);
        assert!(
            matches!(plan(Some(&s), &f, &read()), Plan::Write(r) if r.sha256.as_deref() == Some("bb"))
        );
        f.sha256 = Some("aa".to_string());
        assert_eq!(plan(Some(&s), &f, &read()), Plan::Unchanged(7));
    }

    #[test]
    fn parallel_map_keeps_order() {
        let items: Vec<u32> = (0..100).collect();
        let out = parallel_map(&items, 7, |x| x * 2);
        assert_eq!(out, (0..100).map(|x| x * 2).collect::<Vec<_>>());
    }
}
