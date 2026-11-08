use std::path::Path;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow};
use serde::Serialize;

use std::collections::HashSet;
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};

use super::gliner::{self, Gliner, Span, Word};
use super::{METHOD, candidates, windows};
use crate::config::{Config, PersonsConfig};
use crate::db;
use crate::db::entities::ChunkWork;
use crate::db::persons::Hit;
use crate::embed::local::{Files, Wanted, hub_files};
use crate::facts::sample::letter_share;
use crate::rules::persons::{name_key, person_name};

const STAGE_VERSION: u32 = 3;
const PROGRESS_SECS: u64 = 60;

#[derive(Debug, Clone, Default)]
pub struct Options {
    pub minutes: Option<u64>,
    pub paths: Vec<String>,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct Stats {
    pub contents: u64,
    pub chunks: u64,
    pub skipped: u64,
    pub windows: u64,
    pub people: u64,
    pub left: i64,
    pub seconds: f64,
}

pub fn files(settings: &PersonsConfig, cache_dir: &str) -> Result<Files> {
    let wanted = Wanted {
        repo: &settings.repo,
        revision: &settings.revision,
        file: &settings.file,
        data_files: &[],
        tokenizer: &settings.tokenizer,
    };
    hub_files(&wanted, cache_dir, false)
}

pub fn key(settings: &PersonsConfig, files: &Files) -> String {
    let threshold = settings.threshold.to_string();
    let width = settings.max_width.to_string();
    let words = settings.max_words.to_string();
    let context = settings.context_words.to_string();
    let labels = settings.labels.join(",");
    crate::hash::stage_key(
        "persons",
        STAGE_VERSION,
        &[
            &settings.repo,
            &files.revision,
            &settings.file,
            &labels,
            &threshold,
            &width,
            &words,
            &context,
        ],
    )
}

pub async fn run(config_dir: Option<&Path>, url: Option<&str>, options: &Options) -> Result<Stats> {
    let config = crate::config::load(config_dir)?;
    let ontology = crate::ontology::load_or_empty(config_dir, &config.ontology)?;
    let rules = crate::rules::load(config_dir, &ontology, &config.ontology.document_class)?;
    let persons = rules
        .persons
        .as_ref()
        .context("rules.toml has no [persons] section, so there is no class for people")?;
    let class = persons.class.clone();
    let range = ontology
        .properties
        .get(persons.works_for())
        .map(|p| p.range.clone())
        .unwrap_or_default();
    let organisations: Vec<String> = ontology
        .classes
        .keys()
        .filter(|iri| range.iter().any(|r| ontology.is_a(iri, r)))
        .cloned()
        .collect();
    let files = files(&config.persons, &config.embed.cache_dir)?;
    let lock = db::runs::lock(url, &config.database.schema).await?;
    let pool = db::pool::connect(url, &config.database.schema, 2).await?;
    let hash = crate::config::hash(&config)?;
    let work = run_with(&pool, &config, &class, &organisations, &files, options);
    let result = db::runs::track(&pool, &hash, "persons", work).await;
    pool.close().await;
    lock.release().await;
    result
}

async fn run_with(
    pool: &sqlx::PgPool,
    config: &Config,
    class: &str,
    organisations: &[String],
    files: &Files,
    options: &Options,
) -> Result<Stats> {
    let started = Instant::now();
    let settings = &config.persons;
    let key = key(settings, files);
    let version_id = db::ontology::current(pool)
        .await?
        .context("no finished run has stored the ontology yet; run zontologize scan first")?;
    let threads = if settings.threads == 0 {
        std::thread::available_parallelism().map_or(1, |n| n.get())
    } else {
        settings.threads
    };
    let workers = settings.workers.max(1);
    let mut loaded_models = Vec::with_capacity(workers);
    for _ in 0..workers {
        loaded_models.push(Gliner::load(
            files,
            &settings.labels,
            settings.max_width,
            settings.threshold,
            (threads / workers).max(1),
        )?);
    }
    let mut model = Some(loaded_models);
    let deadline = options
        .minutes
        .map(|m| started + Duration::from_secs(m * 60));
    let known: HashSet<String> = db::persons::organisation_names(pool, organisations)
        .await?
        .into_iter()
        .collect();
    let known = std::sync::Arc::new(known);
    let total = db::persons::count_contents(pool, &key, &options.paths).await?;
    println!("persons: {total} contents to read");
    let mut stats = Stats::default();
    let mut after = 0;
    let mut shown = Instant::now();
    loop {
        if deadline.is_some_and(|d| Instant::now() >= d) {
            break;
        }
        let ids =
            db::persons::next_contents(pool, &key, &options.paths, after, settings.read_batch)
                .await?;
        let Some(&last) = ids.last() else {
            break;
        };
        after = last;
        let chunks = db::entities::chunks_of(pool, &ids).await?;
        let mut loaded = model
            .take()
            .ok_or_else(|| anyhow!("the model did not load"))?;
        let job_settings = settings.clone();
        let embed = config.embed.clone();
        let known = known.clone();
        let (loaded, found) = tokio::task::spawn_blocking(move || {
            let found = read_chunks(&mut loaded, &chunks, &job_settings, &embed, &known);
            (loaded, found)
        })
        .await?;
        model = Some(loaded);
        let (hits, counts) = found?;
        stats.contents += ids.len() as u64;
        stats.chunks += counts.chunks;
        stats.skipped += counts.skipped;
        stats.windows += counts.windows;
        stats.people += hits.len() as u64;
        db::persons::write(pool, &ids, hits, class, METHOD, &key, version_id).await?;
        if shown.elapsed() >= Duration::from_secs(PROGRESS_SECS) {
            shown = Instant::now();
            let rate = stats.contents as f64 / started.elapsed().as_secs_f64().max(1.0);
            println!(
                "persons: {} of {total} contents, {} windows, {} names, {rate:.2} contents per second",
                stats.contents, stats.windows, stats.people
            );
        }
    }
    stats.left = db::persons::count_contents(pool, &key, &options.paths).await?;
    stats.seconds = started.elapsed().as_secs_f64();
    Ok(stats)
}

#[derive(Default)]
struct Counts {
    chunks: u64,
    skipped: u64,
    windows: u64,
}

fn readable(chunk: &ChunkWork, embed: &crate::config::EmbedConfig) -> bool {
    let low_ocr = chunk
        .ocr_confidence
        .is_some_and(|c| c < embed.min_ocr_confidence);
    !low_ocr && letter_share(&chunk.text) >= embed.min_letter_share
}

struct Window {
    chunk: usize,
    from: usize,
    to: usize,
}

fn read_chunks(
    models: &mut [Gliner],
    chunks: &[ChunkWork],
    settings: &PersonsConfig,
    embed: &crate::config::EmbedConfig,
    known: &HashSet<String>,
) -> Result<(Vec<Hit>, Counts)> {
    let mut counts = Counts::default();
    let mut words = Vec::with_capacity(chunks.len());
    let mut work = Vec::new();
    for (index, chunk) in chunks.iter().enumerate() {
        counts.chunks += 1;
        if !readable(chunk, embed) {
            counts.skipped += 1;
            words.push(Vec::new());
            continue;
        }
        let found = gliner::words(&chunk.text);
        let starts = candidates(&chunk.text, &found);
        let spans = windows(
            &starts,
            found.len(),
            settings.context_words,
            settings.max_words,
        );
        work.extend(spans.into_iter().map(|(from, to)| Window {
            chunk: index,
            from,
            to,
        }));
        words.push(found);
    }
    counts.windows = work.len() as u64;
    work.sort_by_key(|w| w.to - w.from);
    let batches: Vec<&[Window]> = work.chunks(settings.batch_size.max(1)).collect();
    let next = AtomicUsize::new(0);
    let results: Mutex<Vec<(usize, Vec<Vec<Span>>)>> = Mutex::new(Vec::new());
    let failed: Mutex<Option<anyhow::Error>> = Mutex::new(None);
    std::thread::scope(|scope| {
        for model in models.iter_mut() {
            scope.spawn(|| {
                loop {
                    let at = next.fetch_add(1, Ordering::Relaxed);
                    let Some(batch) = batches.get(at) else {
                        return;
                    };
                    let items: Vec<(&str, &[Word])> = batch
                        .iter()
                        .map(|w| (chunks[w.chunk].text.as_str(), &words[w.chunk][w.from..w.to]))
                        .collect();
                    match model.predict(&items) {
                        Ok(spans) => lock(&results).push((at, spans)),
                        Err(error) => {
                            *lock(&failed) = Some(error);
                            next.store(batches.len(), Ordering::Relaxed);
                            return;
                        }
                    }
                }
            });
        }
    });
    if let Some(error) = lock(&failed).take() {
        return Err(error);
    }
    let mut hits = Vec::new();
    for (at, predicted) in lock(&results).drain(..) {
        for (window, spans) in batches[at].iter().zip(predicted) {
            let chunk = &chunks[window.chunk];
            let found = &words[window.chunk];
            for span in spans {
                let first = found[window.from + span.first];
                let last = found[window.from + span.last];
                if let Some(hit) = hit(chunk, first.start, last.end, span.score)
                    && !is_organisation(&hit.name, known)
                {
                    hits.push(hit);
                }
            }
        }
    }
    hits.sort_by_key(|h| (h.chunk_id, h.start, h.end));
    hits.dedup_by(|a, b| (a.chunk_id, a.start, a.end) == (b.chunk_id, b.start, b.end));
    Ok((hits, counts))
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

pub fn is_organisation(name: &str, known: &HashSet<String>) -> bool {
    let lower = name.to_lowercase();
    let first = lower.split_whitespace().next().unwrap_or("");
    known.contains(&lower)
        || known.contains(first)
        || known
            .iter()
            .any(|k| k.starts_with(&lower) || (k.contains(' ') && lower.starts_with(k.as_str())))
}

fn hit(chunk: &ChunkWork, start: usize, end: usize, score: f32) -> Option<Hit> {
    let name = person_name(&chunk.text[start..end])?;
    let chars = |byte: usize| chunk.text[..byte].chars().count();
    Some(Hit {
        chunk_id: chunk.id,
        key: name_key(&name),
        name,
        dataset: chunk.dataset.clone(),
        start: chars(start),
        end: chars(end),
        confidence: f64::from(score),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_of_known_organisations_are_not_people() {
        let known: HashSet<String> = [
            "northwind pumps",
            "contoso",
            "fabrikam b.v.",
            "tom hale engineering",
        ]
        .map(String::from)
        .into();
        assert!(is_organisation("Northwind Pumps", &known));
        assert!(is_organisation("Contoso Eastport", &known));
        assert!(!is_organisation("Jan Contoso", &known));
        assert!(!is_organisation("Wim Jansen", &known));
        assert!(is_organisation("Tom Hale", &known));
        assert!(is_organisation("Tom Hale EngineeringB.V.", &known));
    }
}
