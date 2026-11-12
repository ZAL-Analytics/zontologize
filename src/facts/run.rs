use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Instant;

use anyhow::{Result, bail};
use globset::GlobSet;
use rayon::ThreadPool;
use sqlx::PgPool;

use super::check::{check, entity_key};
use super::{Answer, Built, ChunkInput, Context, Known, TASK, USER_VARIABLES, build};
use crate::config;
use crate::db;
use crate::db::facts::{ChunkFacts, ChunkRow, Confidence};
use crate::llm::ModelName;
use crate::llm::prompt::{self, Prompt};
use crate::llm::shortlist::counts_by_chunk;
use crate::llm::stage::{self, Base, Chunk, Counts, Keys, Plan, Stage};
use crate::ontology::{self, Ontology};
use crate::resolve::names::Normaliser;

pub use crate::llm::job::{Options, Outcome};

struct Setup<'a> {
    base: Base<'a>,
    ontology: Arc<Ontology>,
    exclude: Arc<Vec<String>>,
    system: Arc<Prompt>,
    user: Arc<Prompt>,
    check_key: Arc<String>,
    normaliser: Arc<Normaliser>,
    threads: Arc<ThreadPool>,
    out: PathBuf,
}

struct Prepared {
    built: Built,
    chunk: ChunkRow,
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
    let name = ModelName::configured(&config.models.extract, &config.providers)
        .map_err(|e| anyhow::anyhow!("models.extract: {e}"))?;
    if config.ontology.files.is_empty() {
        bail!("[ontology] files is empty");
    }
    let loaded = ontology::load_checked(config_dir, &config.ontology)?;
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
    user.require("chunk")?;
    let lock = match options.dry_run {
        true => None,
        false => Some(db::runs::lock(url, &config.database.schema).await?),
    };
    let pool = db::pool::connect(
        url,
        &config.database.schema,
        config.database.max_connections,
    )
    .await?;
    let extract = &config.llm_extract;
    let ontology = Arc::new(loaded.ontology);
    let setup = Setup {
        base: Base {
            pool: &pool,
            config: &config,
            name: &name,
            task: TASK,
            ontology_version: &ontology.version,
            sources: &extract.sources,
            paths: &extract.paths,
            read_batch: extract.read_batch as usize,
            seed: &extract.seed,
            resend_share: extract.resend_share,
            confidence: extract.confidence,
        },
        check_key: Arc::new(super::check_key(&ontology, &config.resolve.strip_words)),
        normaliser: Arc::new(Normaliser::new(&config.resolve.strip_words)),
        ontology: ontology.clone(),
        exclude: Arc::new(exclude),
        system: Arc::new(system),
        user: Arc::new(user),
        threads: Arc::new(crate::cpu::pool(extract.threads)?),
        out: PathBuf::from(&extract.out),
    };
    if !options.dry_run {
        rekey(&pool, &setup.normaliser).await?;
    }
    let result = stage::run(&setup, options).await;
    pool.close().await;
    db::runs::release(lock).await;
    result
}

async fn inputs(
    pool: &PgPool,
    ids: &[i64],
    max_known: usize,
) -> Result<Vec<(ChunkInput, ChunkRow)>> {
    let rows = db::facts::chunks(pool, ids).await?;
    let mut counts = counts_by_chunk(db::fit::class_counts(pool, ids).await?);
    let mut known: HashMap<i64, Vec<Known>> = HashMap::new();
    for row in db::facts::known(pool, ids, max_known).await? {
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
    let pairs = inputs(
        setup.base.pool,
        ids,
        setup.base.config.llm_extract.max_known,
    )
    .await?;
    let config = setup.base.config;
    let ontology = setup.ontology.clone();
    let system = setup.system.clone();
    let user = setup.user.clone();
    let model = setup.base.name.clone();
    let languages = config.ontology.languages.clone();
    let max_shortlist = config.llm_extract.max_shortlist;
    let max_known = config.llm_extract.max_known;
    let definition_chars = config.llm_extract.definition_chars;
    let exclude = setup.exclude.clone();
    let check_key = setup.check_key.clone();
    let threads = setup.threads.clone();
    let (built, pairs) = tokio::task::spawn_blocking(move || {
        let context = Context {
            ontology: &ontology,
            languages: &languages,
            max_shortlist,
            max_known,
            definition_chars,
            exclude: &exclude,
            check_key: &check_key,
            system: &system,
            user: &user,
            model: &model,
        };
        let built = crate::cpu::map(&threads, &pairs, |(input, _)| build(&context, input));
        (built, pairs)
    })
    .await?;
    Ok(built
        .into_iter()
        .zip(pairs)
        .map(|(built, (_, chunk))| Prepared { built, chunk })
        .collect())
}

impl Stage for Setup<'_> {
    type Item = Prepared;
    type Answer = Answer;
    type Checked = ChunkFacts;
    const SAMPLED: &'static str = "eligible chunks";
    const WRITES: &'static str = "facts";

    fn base(&self) -> &Base<'_> {
        &self.base
    }

    fn keys(item: &Prepared) -> Keys<'_> {
        Keys {
            chunk_id: item.chunk.id,
            key: &item.built.key,
            cache_key: &item.built.cache_key,
            request: &item.built.request,
        }
    }

    fn drops(checked: &ChunkFacts) -> &BTreeMap<String, i64> {
        &checked.checked.drops
    }

    async fn read(
        &self,
        after: i64,
        paths: Option<&GlobSet>,
        _sampling: bool,
        _plan: &mut Plan,
    ) -> Result<Option<(i64, Vec<Chunk>)>> {
        let extract = &self.base.config.llm_extract;
        let eligible = db::facts::eligible(
            self.base.pool,
            &extract.sources,
            &extract.doc_types,
            extract.min_chars as i32,
            after,
            extract.read_batch,
        )
        .await?;
        let Some(last) = eligible.last().map(|c| c.id) else {
            return Ok(None);
        };
        let chunks = eligible
            .into_iter()
            .filter(|c| paths.is_none_or(|set| set.is_match(&c.path)))
            .map(|c| Chunk {
                id: c.id,
                key: c.llm_key,
                source: c.source,
                document_id: c.document_id,
                doc_type: c.doc_type,
                path: c.path,
                text: c.text,
                ocr_confidence: c.ocr_confidence,
                matched: c.matched,
            })
            .collect();
        Ok(Some((last, chunks)))
    }

    async fn prepare(&self, ids: &[i64]) -> Result<Vec<Prepared>> {
        prepare(self, ids).await
    }

    fn check(&self, item: Prepared, answer: Answer) -> ChunkFacts {
        let checked = check(
            answer,
            &item.built,
            &item.chunk.text,
            &self.ontology,
            &self.base.config.ontology.languages,
            &self.normaliser,
        );
        ChunkFacts {
            chunk_id: item.chunk.id,
            key: item.built.key,
            dataset: item.chunk.dataset.clone(),
            scale: item.chunk.ocr_scale(),
            known: item.built.known.iter().map(|k| k.entity_id).collect(),
            checked,
        }
    }

    async fn write(
        &self,
        checked: &[ChunkFacts],
        method: &str,
        confidence: &Confidence,
        version_id: i64,
    ) -> Result<()> {
        db::facts::write(self.base.pool, checked, method, confidence, version_id).await
    }

    fn summary(&self, plan: &Plan, counts: &Counts, _started: Instant) {
        println!(
            "extract: {} eligible chunks, {} done before, {} from the cache, {} to send, \
             {} invalid before, {} to send again after a change",
            plan.eligible, plan.done, counts.hits, counts.send, counts.invalid, counts.resend,
        );
    }

    async fn clear(&self, _stale: &[i64], _dry_run: bool) -> Result<()> {
        Ok(())
    }

    async fn tidy(&self) -> Result<()> {
        db::facts::delete_orphans(self.base.pool).await?;
        Ok(())
    }

    async fn report(&self, job: Option<i64>) -> Result<()> {
        super::report::write_report(
            self.base.pool,
            &self.ontology,
            self.base.config,
            &self.out,
            job,
        )
        .await
    }
}
async fn rekey(pool: &PgPool, normaliser: &Normaliser) -> Result<()> {
    let rows = db::entities::model_keys(pool).await?;
    let (ids, keys) = changed_keys(&rows, normaliser);
    if ids.is_empty() {
        return Ok(());
    }
    let done = db::entities::rekey(pool, &ids, &keys).await?;
    println!(
        "extract: re-keyed {} entities, {} folded",
        ids.len(),
        done.merged
    );
    if done.kept_apart > 0 {
        println!(
            "extract: {} entities keep their key, a rejected same-as keeps them apart",
            done.kept_apart
        );
    }
    Ok(())
}

fn changed_keys(
    rows: &[(i64, String, String)],
    normaliser: &Normaliser,
) -> (Vec<i64>, Vec<String>) {
    rows.iter()
        .filter_map(|(id, key, name)| {
            let new = entity_key(normaliser, name);
            (new != *key).then_some((*id, new))
        })
        .unzip()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_rows_whose_key_changes_are_re_keyed() {
        let normaliser = Normaliser::new(&["B.V.".to_string(), "BV".to_string()]);
        let row = |id: i64, key: &str, name: &str| (id, key.to_string(), name.to_string());
        let rows = [
            row(1, "acme b.v.", "Acme B.V."),
            row(2, "acme", "Acme BV"),
            row(3, "bv", "BV"),
            row(4, "b.v", "B.V."),
        ];
        let (ids, keys) = changed_keys(&rows, &normaliser);
        assert_eq!(ids, [1, 4]);
        assert_eq!(keys, ["acme", "b.v."]);
    }
}
