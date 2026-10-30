pub mod chunk;
pub mod extract;
pub mod table;
pub mod walk;

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Instant;

use anyhow::{Context, Result, bail};
use serde::Serialize;
use sha2::{Digest, Sha256};
use sqlx::PgPool;
use sqlx::types::chrono::{DateTime, Utc};
use tokio::sync::{Semaphore, mpsc};
use tokio::task::JoinSet;

use crate::config::{self, Config, SourceConfig, SourceKind};
use crate::db::documents::{self, Row, Stored};
use crate::db::{self, contents, entities};
use crate::ontology::{self, Ontology, hex};
use crate::rules::{self, Rules};
use chunk::ChunkSettings;
use extract::ExtractSettings;
use walk::{Decision, FileEntry, Kinds, WalkItem};

const EXTRACT_BATCH: i64 = 200;
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
    pub chunk: StageStats,
    pub rules: StageStats,
}

pub async fn run(config_dir: Option<&Path>, only: &[String], url: Option<&str>) -> Result<()> {
    let prepared = Arc::new(prepare(config_dir, only)?);
    if prepared.warnings > 0 {
        println!(
            "ontology: {} warnings (run zontologize ontology check)",
            prepared.warnings
        );
    }
    let config = &prepared.config;
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
    let result = stages(&pool, prepared.clone(), run_id, &mut stats).await;
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
    stats.extract = extract_stage(pool, &prepared, &source_ids, &roots).await?;
    stats.chunk = chunk_stage(pool, &prepared, &source_ids).await?;
    stats.rules = rules_stage(pool, &prepared, &source_ids, version_id).await?;
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
) -> Result<StageStats> {
    let started = Instant::now();
    let key = prepared.extract.key.clone();
    let todo = contents::count_to_extract(pool, &key, source_ids).await?;
    println!("extract: {todo} to extract");
    let mut stats = StageStats::default();
    if todo == 0 {
        return Ok(stats);
    }
    let semaphore = Arc::new(Semaphore::new(prepared.config.extract.threads));
    let mut tasks: JoinSet<Result<bool>> = JoinSet::new();
    let mut after = 0;
    loop {
        let batch = contents::next_to_extract(pool, &key, source_ids, after, EXTRACT_BATCH).await?;
        let Some(last) = batch.last() else {
            break;
        };
        after = last.id;
        for item in batch {
            let permit = semaphore.clone().acquire_owned().await?;
            let root = roots.get(&item.source_id).cloned().unwrap_or_default();
            let path = file_path(&root, &item.path, item.file.as_deref());
            let pool = pool.clone();
            let prepared = prepared.clone();
            let key = key.clone();
            tasks.spawn(async move {
                let _permit = permit;
                let started = Instant::now();
                let result = extract::extract_file(&path, &prepared.extract).await;
                let seconds = started.elapsed().as_secs_f64();
                match result {
                    Ok(pages) => {
                        contents::write_pages(&pool, item.id, &key, &pages, seconds).await?;
                        Ok(true)
                    }
                    Err(error) => {
                        let message = format!("{error:#}");
                        eprintln!("error: {message}");
                        contents::write_error(&pool, item.id, &key, &message, seconds).await?;
                        Ok(false)
                    }
                }
            });
            while let Some(done) = tasks.try_join_next() {
                tally(&mut stats, done??);
            }
        }
    }
    while let Some(done) = tasks.join_next().await {
        tally(&mut stats, done??);
    }
    stats.seconds = started.elapsed().as_secs_f64();
    println!(
        "extract: {} done, {} errors, {:.1} s",
        stats.items, stats.errors, stats.seconds
    );
    Ok(stats)
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

async fn rules_stage(
    pool: &PgPool,
    prepared: &Arc<Prepared>,
    source_ids: &[i64],
    version_id: i64,
) -> Result<StageStats> {
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
            .map(|doc| {
                let facts = (doc.status != "deleted").then(|| rules.document_facts(&doc.path));
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
                    facts,
                }
            })
            .collect();
        stats.items += outputs.len() as u64;
        entities::write_document_rules(pool, &outputs, &rules.key, version_id).await?;
    }
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
        let worker = prepared.clone();
        let outputs = tokio::task::spawn_blocking(move || {
            parallel_map(&chunks, threads, |chunk| entities::ChunkOutput {
                id: chunk.id,
                dataset: chunk.dataset.clone(),
                facts: worker.rules.text_facts(&chunk.text),
            })
        })
        .await?;
        stats.items += ids.len() as u64;
        entities::write_text_rules(pool, &ids, &outputs, &rules.key, version_id).await?;
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
    Ok(stats)
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
