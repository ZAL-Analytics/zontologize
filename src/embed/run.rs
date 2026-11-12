use std::collections::{BTreeMap, HashMap};
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};
use sqlx::PgPool;

use super::model::{Model, text_key};
use super::{Embed, STAGE_VERSION, embed_sorted, passage};
use crate::config::Config;
use crate::db;
use crate::db::embed::{CHUNK, ENTITY, Vector};
use crate::facts::sample::letter_share;
use crate::ns::local_name;
use crate::ontology::{Class, Ontology};

#[derive(Debug, Clone, Default)]
pub struct Options {
    pub model: Option<String>,
    pub minutes: Option<u64>,
    pub no_index: bool,
    pub prune: bool,
}

#[derive(Debug, Clone, Default, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct Counts {
    pub eligible: u64,
    pub embedded: u64,
    pub copied: u64,
    pub kept: u64,
    pub deleted: u64,
    pub truncated: u64,
}

#[derive(Debug, Clone, Default)]
pub struct Stats {
    pub model_id: i64,
    pub chunks: Counts,
    pub entities: Counts,
    pub classes: Counts,
    pub junk: u64,
    pub seconds: f64,
    pub embed_seconds: f64,
    pub index_seconds: f64,
    pub stopped: bool,
    pub pruned: Vec<String>,
}

pub type Shared = Arc<Mutex<Box<dyn Embed>>>;

struct Stage<'a> {
    pool: &'a PgPool,
    config: &'a Config,
    model: &'a Model,
    embedder: Shared,
    model_id: i64,
    fingerprint: String,
    deadline: Option<Instant>,
    started: Instant,
    last_progress: Instant,
    stats: Stats,
}

pub async fn run(config_dir: Option<&Path>, url: Option<&str>, options: &Options) -> Result<Stats> {
    let config = crate::config::load(config_dir)?;
    let name = options.model.as_deref().unwrap_or(&config.models.embed);
    let model =
        super::model::parse(name, &config.embed.models).map_err(|e| anyhow!("--model: {e}"))?;
    let ontology = crate::ontology::load_or_empty(config_dir, &config.ontology)?;
    let lock = db::runs::lock(url, &config.database.schema).await?;
    let pool = db::pool::connect(url, &config.database.schema, 2).await?;
    let result = async {
        let fetched = model.clone();
        let cache_dir = config.embed.cache_dir.clone();
        println!("embed: model {}", model.full_name());
        let files = tokio::task::spawn_blocking(move || super::local::fetch(&fetched, &cache_dir))
            .await??;
        let local =
            super::local::Local::new(model.clone(), files, config.embed.threads, config.gpu);
        let (run_id, _) = db::runs::start(&pool, &crate::config::hash(&config)?).await?;
        let outcome = run_with(&pool, &config, &ontology, &model, Box::new(local), options).await;
        let (status, stats) = match &outcome {
            Ok(stats) => ("finished", stats_json(stats)),
            Err(error) => ("failed", serde_json::json!({"error": format!("{error:#}")})),
        };
        db::runs::finish(&pool, run_id, status, &stats).await?;
        outcome
    }
    .await;
    pool.close().await;
    lock.release().await;
    result
}

fn stats_json(stats: &Stats) -> serde_json::Value {
    let items = stats.chunks.embedded + stats.entities.embedded + stats.classes.embedded;
    serde_json::json!({
        "embed": {
            "seconds": stats.seconds,
            "items": items,
            "model_id": stats.model_id,
            "chunks": stats.chunks,
            "entities": stats.entities,
            "classes": stats.classes,
            "junk": stats.junk,
            "embed_seconds": stats.embed_seconds,
            "index_seconds": stats.index_seconds,
            "stopped": stats.stopped,
        }
    })
}

pub async fn model_id(
    pool: &PgPool,
    model: &Model,
    embedder: &Shared,
    current: bool,
) -> Result<(i64, String)> {
    let revision = lock(embedder)?.revision().to_string();
    let fingerprint = model.fingerprint(&revision);
    let id = match db::embed::find_model(pool, &fingerprint).await? {
        Some(stored) => stored.id,
        None => {
            let shared = embedder.clone();
            let (sha256, dimensions) = tokio::task::spawn_blocking(move || {
                let mut embedder = lock(&shared)?;
                let sha256 = embedder.sha256()?;
                let dimensions = embedder.dimensions()?;
                Ok::<_, anyhow::Error>((sha256, dimensions))
            })
            .await??;
            let settings = &model.settings;
            let record = db::embed::Record {
                name: &model.full_name(),
                fingerprint: &fingerprint,
                repo: &settings.repo,
                file: &settings.file,
                data_files: &settings.data_files,
                tokenizer: &settings.tokenizer,
                revision: &revision,
                sha256: &sha256,
                dimensions: i32::try_from(dimensions).context("too many dimensions")?,
                pooling: settings.pooling.as_str(),
                output: &settings.output,
                passage_prefix: &settings.passage_prefix,
                query_prefix: &settings.query_prefix,
                max_tokens: i32::try_from(settings.max_tokens).context("max_tokens too large")?,
                normalize: settings.normalize,
            };
            db::embed::insert_model(pool, &record).await?
        }
    };
    if current {
        db::embed::set_current(pool, id).await?;
    }
    Ok((id, fingerprint))
}

pub fn lock(embedder: &Shared) -> Result<std::sync::MutexGuard<'_, Box<dyn Embed>>> {
    embedder
        .lock()
        .map_err(|_| anyhow!("the embedding model failed in another batch"))
}

pub async fn run_with(
    pool: &PgPool,
    config: &Config,
    ontology: &Ontology,
    model: &Model,
    embedder: Box<dyn Embed>,
    options: &Options,
) -> Result<Stats> {
    let started = Instant::now();
    let embedder: Shared = Arc::new(Mutex::new(embedder));
    let current = model.full_name() == config.models.embed;
    let (model_id, fingerprint) = model_id(pool, model, &embedder, current).await?;
    let mut stage = Stage {
        pool,
        config,
        model,
        embedder,
        model_id,
        fingerprint,
        deadline: options
            .minutes
            .map(|m| started + Duration::from_secs(m * 60)),
        started,
        last_progress: started,
        stats: Stats {
            model_id,
            ..Stats::default()
        },
    };
    stage.chunks().await?;
    if !stage.stats.stopped {
        stage.entities(ontology).await?;
    }
    if !stage.stats.stopped {
        stage.classes(ontology).await?;
    }
    if !stage.stats.stopped && !options.no_index {
        stage.index().await?;
    }
    if options.prune {
        if !current {
            bail!("--prune keeps only [models] embed; run it without --model");
        }
        stage.stats.pruned = db::embed::prune(pool, model_id).await?;
    }
    stage.stats.seconds = started.elapsed().as_secs_f64();
    Ok(stage.stats)
}

struct Work {
    owner_id: i64,
    key: String,
    text: String,
}

impl Stage<'_> {
    fn out_of_time(&mut self) -> bool {
        if self.deadline.is_some_and(|d| Instant::now() >= d) {
            self.stats.stopped = true;
        }
        self.stats.stopped
    }

    fn progress(&mut self, kind: &str, counts: &Counts) {
        if self.last_progress.elapsed() < Duration::from_secs(self.config.embed.progress_secs) {
            return;
        }
        self.last_progress = Instant::now();
        let rate = counts.embedded as f64 / self.stats.embed_seconds.max(1e-9);
        println!(
            "embed: {kind}: {} embedded ({rate:.1}/s), {} copied, {} unchanged, {:.0} s",
            counts.embedded,
            counts.copied,
            counts.kept,
            self.started.elapsed().as_secs_f64()
        );
    }

    fn key(&self, text: &str) -> String {
        text_key(&self.fingerprint, STAGE_VERSION, text)
    }

    async fn chunks(&mut self) -> Result<()> {
        let settings = &self.config.embed;
        let mut counts = Counts::default();
        let mut after = 0;
        loop {
            let page = db::embed::chunk_candidates(
                self.pool,
                self.model_id,
                settings.min_chars as i32,
                after,
                settings.read_batch,
            )
            .await?;
            let Some(last) = page.last().map(|c| c.id) else {
                counts.deleted +=
                    db::embed::delete_stale(self.pool, self.model_id, CHUNK, after, None, &[])
                        .await?;
                break;
            };
            let mut keep = Vec::new();
            let mut work = Vec::new();
            for candidate in page {
                let low_ocr = candidate
                    .ocr_confidence
                    .is_some_and(|c| c < settings.min_ocr_confidence);
                if low_ocr || letter_share(&candidate.text) < settings.min_letter_share {
                    self.stats.junk += 1;
                    continue;
                }
                keep.push(candidate.id);
                counts.eligible += 1;
                let text = passage(self.model, &candidate.text);
                let key = self.key(&text);
                if candidate.text_key.as_deref() == Some(key.as_str()) {
                    counts.kept += 1;
                    continue;
                }
                work.push(Work {
                    owner_id: candidate.id,
                    key,
                    text,
                });
            }
            counts.deleted +=
                db::embed::delete_stale(self.pool, self.model_id, CHUNK, after, Some(last), &keep)
                    .await?;
            self.process(CHUNK, work, &mut counts).await?;
            if self.stats.stopped {
                break;
            }
            after = last;
        }
        self.stats.chunks = counts;
        Ok(())
    }

    async fn entities(&mut self, ontology: &Ontology) -> Result<()> {
        let settings = &self.config.embed;
        let language = self
            .config
            .ontology
            .languages
            .first()
            .cloned()
            .unwrap_or_default();
        let mut counts = Counts::default();
        let mut after = 0;
        loop {
            let page = db::embed::entity_candidates(
                self.pool,
                self.model_id,
                settings.max_aliases as i64,
                after,
                settings.read_batch,
            )
            .await?;
            let Some(last) = page.last().map(|c| c.id) else {
                counts.deleted +=
                    db::embed::delete_stale(self.pool, self.model_id, ENTITY, after, None, &[])
                        .await?;
                break;
            };
            let mut work = Vec::new();
            for candidate in &page {
                counts.eligible += 1;
                let label = class_label(ontology, &candidate.class_iri, &language);
                let text = passage(
                    self.model,
                    &entity_text(&candidate.name, &label, &candidate.aliases),
                );
                let key = self.key(&text);
                if candidate.text_key.as_deref() == Some(key.as_str()) {
                    counts.kept += 1;
                    continue;
                }
                work.push(Work {
                    owner_id: candidate.id,
                    key,
                    text,
                });
            }
            let keep: Vec<i64> = page.iter().map(|c| c.id).collect();
            counts.deleted +=
                db::embed::delete_stale(self.pool, self.model_id, ENTITY, after, Some(last), &keep)
                    .await?;
            self.process(ENTITY, work, &mut counts).await?;
            if self.stats.stopped {
                break;
            }
            after = last;
        }
        self.stats.entities = counts;
        Ok(())
    }

    async fn classes(&mut self, ontology: &Ontology) -> Result<()> {
        let languages = &self.config.ontology.languages;
        let stored = db::embed::class_keys(self.pool, self.model_id).await?;
        let mut counts = Counts::default();
        let mut keep = Vec::new();
        let mut texts = Vec::new();
        let mut owners = Vec::new();
        for class in ontology.classes.values().filter(|c| c.selected && !c.meta) {
            counts.eligible += 1;
            keep.push(class.iri.clone());
            let text = passage(self.model, &class_text(class, languages));
            let key = self.key(&text);
            if stored.get(&class.iri) == Some(&key) {
                counts.kept += 1;
                continue;
            }
            texts.push(text);
            owners.push((class.iri.clone(), key));
        }
        counts.deleted = db::embed::delete_classes_except(self.pool, self.model_id, &keep).await?;
        for (texts, owners) in texts
            .chunks(self.config.embed.batch_size)
            .zip(owners.chunks(self.config.embed.batch_size))
        {
            if self.out_of_time() {
                break;
            }
            let out = self.embed(texts.to_vec()).await?;
            let rows: Vec<(String, String, bool, String)> = owners
                .iter()
                .zip(out.vectors.iter().zip(&out.truncated))
                .map(|((iri, key), (vector, cut))| {
                    (
                        iri.clone(),
                        key.clone(),
                        *cut,
                        db::embed::vector_text(vector),
                    )
                })
                .collect();
            counts.embedded += rows.len() as u64;
            counts.truncated += out.truncated.iter().filter(|t| **t).count() as u64;
            db::embed::write_classes(self.pool, self.model_id, &rows).await?;
        }
        self.stats.classes = counts;
        Ok(())
    }

    async fn embed(&mut self, texts: Vec<String>) -> Result<super::Embedded> {
        let shared = self.embedder.clone();
        let batch_size = self.config.embed.batch_size;
        let started = Instant::now();
        let out = tokio::task::spawn_blocking(move || {
            let mut embedder = lock(&shared)?;
            embed_sorted(embedder.as_mut(), &texts, batch_size)
        })
        .await??;
        self.stats.embed_seconds += started.elapsed().as_secs_f64();
        Ok(out)
    }

    async fn process(&mut self, kind: &str, work: Vec<Work>, counts: &mut Counts) -> Result<()> {
        if work.is_empty() {
            return Ok(());
        }
        let keys: Vec<String> = work.iter().map(|w| w.key.clone()).collect();
        let known = db::embed::known_vectors(self.pool, self.model_id, &keys).await?;
        let copies: Vec<Vector> = work
            .iter()
            .filter_map(|w| {
                known.get(&w.key).map(|(truncated, embedding)| Vector {
                    owner_id: w.owner_id,
                    text_key: &w.key,
                    truncated: *truncated,
                    embedding,
                })
            })
            .collect();
        counts.copied += copies.len() as u64;
        db::embed::write(self.pool, self.model_id, kind, &copies).await?;
        let mut unique: BTreeMap<&str, (&str, Vec<i64>)> = BTreeMap::new();
        for w in work.iter().filter(|w| !known.contains_key(&w.key)) {
            unique
                .entry(&w.key)
                .or_insert((&w.text, Vec::new()))
                .1
                .push(w.owner_id);
        }
        let mut pending: Vec<(&str, &str, Vec<i64>)> = unique
            .into_iter()
            .map(|(key, (text, owners))| (key, text, owners))
            .collect();
        pending.sort_by_key(|(_, text, _)| text.chars().count());
        let batch_size = self.config.embed.batch_size;
        for batch in pending.chunks(batch_size) {
            if self.out_of_time() {
                return Ok(());
            }
            let texts: Vec<String> = batch.iter().map(|(_, text, _)| text.to_string()).collect();
            let out = self.embed(texts).await?;
            let vectors: Vec<String> = out
                .vectors
                .iter()
                .map(|v| db::embed::vector_text(v))
                .collect();
            let mut rows = Vec::new();
            for (((key, _, owners), embedding), truncated) in
                batch.iter().zip(&vectors).zip(&out.truncated)
            {
                for (i, owner_id) in owners.iter().enumerate() {
                    rows.push(Vector {
                        owner_id: *owner_id,
                        text_key: key,
                        truncated: *truncated,
                        embedding,
                    });
                    if i == 0 {
                        counts.embedded += 1;
                        counts.truncated += u64::from(*truncated);
                    } else {
                        counts.copied += 1;
                    }
                }
            }
            db::embed::write(self.pool, self.model_id, kind, &rows).await?;
            let snapshot = counts.clone();
            self.progress(kind, &snapshot);
        }
        Ok(())
    }

    async fn index(&mut self) -> Result<()> {
        let started = Instant::now();
        let dimensions = db::embed::models(self.pool)
            .await?
            .into_iter()
            .find(|m| m.id == self.model_id)
            .map_or(0, |m| m.dimensions);
        let counts: HashMap<&str, u64> = HashMap::from([
            (CHUNK, self.stats.chunks.eligible),
            (ENTITY, self.stats.entities.eligible),
        ]);
        for kind in [CHUNK, ENTITY] {
            if counts[kind] == 0 || db::embed::index_exists(self.pool, self.model_id, kind).await? {
                continue;
            }
            println!("embed: building the {kind} index");
            db::embed::create_index(self.pool, self.model_id, kind, dimensions).await?;
        }
        self.stats.index_seconds = started.elapsed().as_secs_f64();
        Ok(())
    }
}

pub fn class_label(ontology: &Ontology, iri: &str, language: &str) -> String {
    ontology
        .classes
        .get(iri)
        .and_then(|c| c.labels.get(language).or_else(|| c.labels.values().next()))
        .cloned()
        .unwrap_or_else(|| local_name(iri).to_string())
}

fn entity_text(name: &str, label: &str, aliases: &[String]) -> String {
    let mut text = format!("{name} ({label})");
    if !aliases.is_empty() {
        text.push_str("; ");
        text.push_str(&aliases.join("; "));
    }
    text
}

pub fn class_text(class: &Class, languages: &[String]) -> String {
    let mut labels: Vec<&str> = Vec::new();
    for language in languages {
        if let Some(label) = class.labels.get(language) {
            labels.push(label);
        }
        for alt in class.alt_labels.get(language).into_iter().flatten() {
            labels.push(alt);
        }
    }
    if labels.is_empty() {
        labels.push(local_name(&class.iri));
    }
    labels.dedup();
    let mut text = labels.join("; ");
    for language in languages {
        if let Some(definition) = class.definition.get(language) {
            text.push_str(". ");
            text.push_str(definition);
        }
    }
    text
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn entity_text_has_name_class_and_aliases() {
        assert_eq!(
            entity_text("Acme B.V.", "Customer", &[]),
            "Acme B.V. (Customer)"
        );
        let aliases = vec!["Acme".to_string(), "ACME Pumps".to_string()];
        assert_eq!(
            entity_text("Acme B.V.", "Customer", &aliases),
            "Acme B.V. (Customer); Acme; ACME Pumps"
        );
    }

    #[test]
    fn class_text_has_labels_in_every_language_and_the_definition() {
        let mut class = Class {
            iri: "http://example.org/onto#Pump".to_string(),
            ..Class::default()
        };
        let languages = vec!["en".to_string(), "nl".to_string()];
        assert_eq!(class_text(&class, &languages), "Pump");
        class.labels.insert("en".to_string(), "pump".to_string());
        class.labels.insert("nl".to_string(), "pomp".to_string());
        class
            .alt_labels
            .insert("nl".to_string(), vec!["pompset".to_string()]);
        class
            .definition
            .insert("en".to_string(), "A machine that moves fluid.".to_string());
        assert_eq!(
            class_text(&class, &languages),
            "pump; pomp; pompset. A machine that moves fluid."
        );
    }
}
