use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::time::Instant;

use anyhow::{Result, bail};
use globset::GlobSet;
use rayon::ThreadPool;

use super::check::{Limits, check};
use super::{
    Answer, Built, ChunkInput, Context, DOC, Hits, Matcher, Subject, TASK, USER_VARIABLES, build,
};
use crate::config;
use crate::db;
use crate::db::attributes::{ChunkValues, Eligible, Filter};
use crate::db::facts::{ChunkRow, Confidence};
use crate::llm::ModelName;
use crate::llm::prompt::{self, Prompt};
use crate::llm::stage::{self, Base, Chunk, Counts, Keys, Plan, Stage};
use crate::ontology::{self, Ontology};
use crate::units::UnitIndex;

pub use crate::llm::job::{Options, Outcome};

struct Setup<'a> {
    base: Base<'a>,
    ontology: &'a Ontology,
    units: &'a UnitIndex<'a>,
    matcher: &'a Matcher,
    system: &'a Prompt,
    user: &'a Prompt,
    threads: ThreadPool,
    check_key: String,
    extracted: bool,
    out: PathBuf,
}

struct Prepared {
    built: Built,
    chunk: ChunkRow,
    context: String,
}

pub async fn run(
    config_dir: Option<&Path>,
    url: Option<&str>,
    options: &Options,
) -> Result<Outcome> {
    let config = config::load(config_dir)?;
    if config.models.attributes.is_empty() {
        bail!(
            "[models] attributes is empty; set it to provider:model, for example claude-code:sonnet"
        );
    }
    let name = ModelName::configured(&config.models.attributes, &config.providers)
        .map_err(|e| anyhow::anyhow!("models.attributes: {e}"))?;
    if config.ontology.files.is_empty() {
        bail!("[ontology] files is empty");
    }
    let loaded = ontology::load_checked(config_dir, &config.ontology)?;
    let ontology = loaded.ontology;
    if !ontology.properties.values().any(|p| p.is_attribute()) {
        bail!("the ontology selects no datatype property with a number or text range");
    }
    let rules = crate::rules::load(config_dir, &ontology, &config.ontology.document_class)?;
    for doc_type in &config.attributes.doc_types {
        if !rules.doc_types.contains_key(doc_type) {
            bail!("attributes.doc_types: {doc_type:?} is not a key of [doc_types] in rules.toml");
        }
    }
    let system = prompt::load(config_dir, "attributes-system")?;
    system.check(&[])?;
    let user = prompt::load(config_dir, "attributes-user")?;
    user.check(&USER_VARIABLES)?;
    user.require("chunk")?;
    let units = UnitIndex::new(&ontology);
    let matcher = Matcher::new(&ontology, &units)?;
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
    let attributes = &config.attributes;
    let setup = Setup {
        base: Base {
            pool: &pool,
            config: &config,
            name: &name,
            task: TASK,
            ontology_version: &ontology.version,
            sources: &attributes.sources,
            paths: &attributes.paths,
            read_batch: attributes.read_batch as usize,
            seed: &attributes.seed,
            resend_share: attributes
                .resend_share
                .unwrap_or(config.llm_extract.resend_share),
            confidence: attributes.confidence,
        },
        ontology: &ontology,
        units: &units,
        matcher: &matcher,
        system: &system,
        user: &user,
        threads: crate::cpu::pool(attributes.threads)?,
        check_key: super::check_key(&units, attributes.max_text_chars),
        extracted: options.extracted,
        out: PathBuf::from(&attributes.out),
    };
    let result = stage::run(&setup, options).await;
    pool.close().await;
    db::runs::release(lock).await;
    result
}

fn hits(matcher: &Matcher, text: &str, context: &str) -> Hits {
    let mut hits = matcher.hits(text);
    if !context.is_empty() {
        hits.properties.extend(matcher.hits(context).properties);
    }
    hits
}

async fn prepare(setup: &Setup<'_>, ids: &[i64]) -> Result<Vec<Prepared>> {
    let pool = setup.base.pool;
    let rows = db::facts::chunks(pool, ids).await?;
    let mut contexts =
        db::attributes::contexts(pool, ids, setup.base.config.attributes.context_chars as i32)
            .await?;
    let mut known: HashMap<i64, Vec<Subject>> = HashMap::new();
    for row in db::attributes::known(pool, ids).await? {
        let list = known.entry(row.chunk_id).or_default();
        list.push(Subject {
            reference: format!("r{}", list.len() + 1),
            entity_id: row.entity_id,
            class: row.class_iri,
            name: row.name,
        });
    }
    let mut docs: HashMap<i64, Subject> = HashMap::new();
    let mut conn = pool.acquire().await?;
    for row in db::facts::doc_entities(&mut conn, ids).await? {
        docs.entry(row.chunk_id).or_insert(Subject {
            reference: DOC.to_string(),
            entity_id: row.entity_id,
            class: row.class_iri,
            name: String::new(),
        });
    }
    drop(conn);
    let inputs: Vec<(ChunkInput, ChunkRow)> = rows
        .into_iter()
        .map(|row| {
            let input = ChunkInput {
                id: row.id,
                text: row.text.clone(),
                context: contexts.remove(&row.id).unwrap_or_default(),
                path: row.path.clone(),
                pages: row.pages(),
                known: known.remove(&row.id).unwrap_or_default(),
                doc: docs.remove(&row.id),
            };
            (input, row)
        })
        .collect();
    let config = setup.base.config;
    let context = Context {
        ontology: setup.ontology,
        units: setup.units,
        languages: &config.ontology.languages,
        max_properties: config.attributes.max_properties,
        max_subjects: config.attributes.max_subjects,
        unit_symbols: config.attributes.unit_symbols,
        check_key: &setup.check_key,
        system: setup.system,
        user: setup.user,
        model: setup.base.name,
    };
    let matcher = setup.matcher;
    let built = crate::cpu::blocking(|| {
        crate::cpu::map(&setup.threads, &inputs, |(input, _)| {
            build(&context, input, &hits(matcher, &input.text, &input.context))
        })
    });
    Ok(built
        .into_iter()
        .zip(inputs)
        .filter_map(|(built, (input, chunk))| {
            built.map(|built| Prepared {
                built,
                chunk,
                context: input.context,
            })
        })
        .collect())
}

fn text_hit(setup: &Setup<'_>, chunk: &Eligible) -> bool {
    !hits(setup.matcher, &chunk.text, &chunk.context).is_empty()
}

impl Stage for Setup<'_> {
    type Item = Prepared;
    type Answer = Answer;
    type Checked = ChunkValues;
    const SAMPLED: &'static str = "chunks with a value and a subject";
    const WRITES: &'static str = "values";

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

    fn drops(checked: &ChunkValues) -> &BTreeMap<String, i64> {
        &checked.checked.drops
    }

    async fn read(
        &self,
        after: i64,
        paths: Option<&GlobSet>,
        sampling: bool,
        plan: &mut Plan,
    ) -> Result<Option<(i64, Vec<Chunk>)>> {
        let config = self.base.config;
        let attributes = &config.attributes;
        let filter = Filter {
            sources: &attributes.sources,
            doc_types: &attributes.doc_types,
            min_chars: config.llm_extract.min_chars as i32,
            extracted: self.extracted,
            context_chars: attributes.context_chars as i32,
        };
        let eligible =
            db::attributes::eligible(self.base.pool, &filter, after, attributes.read_batch).await?;
        let Some(last) = eligible.last().map(|c| c.id) else {
            return Ok(None);
        };
        plan.scanned += eligible.len();
        let eligible: Vec<Eligible> = eligible
            .into_iter()
            .filter(|c| paths.is_none_or(|set| set.is_match(&c.path)))
            .collect();
        let hit = crate::cpu::blocking(|| {
            crate::cpu::map(&self.threads, &eligible, |c| text_hit(self, c))
        });
        let mut stale = Vec::new();
        let eligible: Vec<Eligible> = eligible
            .into_iter()
            .zip(hit)
            .filter_map(|(chunk, hit)| {
                if !hit && chunk.attr_key.is_some() {
                    stale.push(chunk.id);
                }
                hit.then_some(chunk)
            })
            .collect();
        plan.stale.extend(stale);
        let with_subject: HashSet<i64> = if sampling {
            let ids: Vec<i64> = eligible.iter().map(|c| c.id).collect();
            prepare(self, &ids)
                .await?
                .into_iter()
                .map(|p| p.chunk.id)
                .collect()
        } else {
            HashSet::new()
        };
        let mut chunks = Vec::new();
        for chunk in eligible {
            if sampling && !with_subject.contains(&chunk.id) {
                if chunk.attr_key.is_some() {
                    plan.stale.push(chunk.id);
                }
                continue;
            }
            chunks.push(Chunk {
                id: chunk.id,
                key: chunk.attr_key,
                source: chunk.source,
                document_id: chunk.document_id,
                doc_type: chunk.doc_type,
                path: chunk.path,
                text: chunk.text,
                ocr_confidence: chunk.ocr_confidence,
                matched: chunk.matched,
            });
        }
        Ok(Some((last, chunks)))
    }

    async fn prepare(&self, ids: &[i64]) -> Result<Vec<Prepared>> {
        prepare(self, ids).await
    }

    fn check(&self, item: Prepared, answer: Answer) -> ChunkValues {
        let checked = check(
            answer,
            &item.built,
            &item.chunk.text,
            &item.context,
            self.ontology,
            self.units,
            &Limits {
                max_text_chars: self.base.config.attributes.max_text_chars,
            },
        );
        ChunkValues {
            chunk_id: item.chunk.id,
            key: item.built.key,
            scale: item.chunk.ocr_scale(),
            checked,
        }
    }

    async fn write(
        &self,
        checked: &[ChunkValues],
        method: &str,
        confidence: &Confidence,
        _version_id: i64,
    ) -> Result<()> {
        db::attributes::write(self.base.pool, checked, method, confidence).await
    }

    fn summary(&self, plan: &Plan, counts: &Counts, started: Instant) {
        println!(
            "attributes: {} chunks read, {} with a value and a subject, {} done before, {} from \
             the cache, {} to send, {} invalid before, {} to send again after a change ({:.1} s)",
            plan.scanned,
            plan.eligible,
            plan.done,
            counts.hits,
            counts.send,
            counts.invalid,
            counts.resend,
            started.elapsed().as_secs_f64()
        );
    }

    async fn clear(&self, stale: &[i64], dry_run: bool) -> Result<()> {
        if !dry_run {
            db::attributes::clear(self.base.pool, stale).await?;
        }
        println!(
            "attributes: values of {} chunks that no longer have a value and a subject {}",
            stale.len(),
            if dry_run {
                "would be removed"
            } else {
                "removed"
            }
        );
        Ok(())
    }

    async fn tidy(&self) -> Result<()> {
        Ok(())
    }

    async fn report(&self, job: Option<i64>) -> Result<()> {
        super::report::write_report(
            self.base.pool,
            self.ontology,
            self.base.config,
            &self.out,
            job,
        )
        .await
    }
}
