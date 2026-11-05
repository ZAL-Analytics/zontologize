use std::collections::{HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use anyhow::{Result, bail};
use globset::{Glob, GlobSet, GlobSetBuilder};
use sqlx::PgPool;

use super::check::{Limits, check};
use super::{
    Answer, Built, ChunkInput, Context, DOC, Hits, Matcher, Subject, TASK, USER_VARIABLES, build,
};
use crate::config::{self, Config};
use crate::db;
use crate::db::attributes::{ChunkValues, Eligible, Filter};
use crate::db::facts::{ChunkRow, Confidence};
use crate::db::llm::{Job, JobParams};
use crate::facts::sample::{Candidate, Settings, first_folder, letter_share, select, text_hash};
use crate::llm::job::{self, Handler, Runner, Work};
use crate::llm::prompt::{self, Prompt};
use crate::llm::{ModelName, Request};
use crate::ontology::{self, Ontology};
use crate::scan::parallel_map;
use crate::units::UnitIndex;

pub use crate::llm::job::{Options, Outcome};

const BATCH: i64 = 2000;
const KEY_BATCH: usize = 10_000;

struct Setup<'a> {
    pool: &'a PgPool,
    config: &'a Config,
    ontology: &'a Ontology,
    units: &'a UnitIndex<'a>,
    matcher: &'a Matcher,
    system: &'a Prompt,
    user: &'a Prompt,
    name: &'a ModelName,
    threads: usize,
    check_key: String,
}

struct Prepared {
    built: Built,
    chunk: ChunkRow,
    context: String,
}

struct Todo {
    chunk_id: i64,
    cache_key: String,
    chars: usize,
    resend: bool,
    order: (usize, i64, i64),
}

#[derive(Default)]
struct Plan {
    scanned: usize,
    eligible: usize,
    done: usize,
    todo: Vec<Todo>,
}

struct Row {
    id: i64,
    attr_key: Option<String>,
    order: (usize, i64, i64),
}

struct Attributes<'a> {
    setup: &'a Setup<'a>,
    job: i64,
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
    if config.models.attributes.is_empty() {
        bail!(
            "[models] attributes is empty; set it to provider:model, for example claude-code:sonnet"
        );
    }
    let name = ModelName::parse(&config.models.attributes)
        .map_err(|e| anyhow::anyhow!("models.attributes: {e}"))?;
    if config.ontology.files.is_empty() {
        bail!("[ontology] files is empty");
    }
    let loaded = ontology::load(config_dir, &config.ontology)?;
    if let Some(error) = loaded.first_error() {
        bail!("ontology: {error}; run zontologize ontology check");
    }
    let ontology = loaded.ontology;
    if !ontology.properties.values().any(|p| p.is_attribute()) {
        bail!("the ontology selects no datatype property with a number or text range");
    }
    let rules = crate::rules::load(config_dir, &ontology, &config.ontology.document_class)?;
    for doc_type in &config.llm_extract.doc_types {
        if !rules.doc_types.contains_key(doc_type) {
            bail!("llm_extract.doc_types: {doc_type:?} is not a key of [doc_types] in rules.toml");
        }
    }
    let system = prompt::load(config_dir, "attributes-system")?;
    system.check(&[])?;
    let user = prompt::load(config_dir, "attributes-user")?;
    user.check(&USER_VARIABLES)?;
    let units = UnitIndex::new(&ontology);
    let matcher = Matcher::new(&ontology, &units)?;
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
        ontology: &ontology,
        units: &units,
        matcher: &matcher,
        system: &system,
        user: &user,
        name: &name,
        threads: config.scan.hash_threads,
        check_key: super::check_key(&units, config.attributes.max_text_chars),
    };
    let result = run_with(&setup, options).await;
    pool.close().await;
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
    let pool = setup.pool;
    let rows = db::facts::chunks(pool, ids).await?;
    let mut contexts =
        db::attributes::contexts(pool, ids, setup.config.attributes.context_chars as i32).await?;
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
    let config = setup.config;
    let context = Context {
        ontology: setup.ontology,
        units: setup.units,
        languages: &config.ontology.languages,
        max_properties: config.attributes.max_properties,
        check_key: &setup.check_key,
        system: setup.system,
        user: setup.user,
        model: setup.name,
    };
    let matcher = setup.matcher;
    let built = parallel_map(&inputs, setup.threads, |(input, _)| {
        build(&context, input, &hits(matcher, &input.text, &input.context))
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
        plan.eligible += 1;
        if row.attr_key.as_deref() == Some(item.built.key.as_str()) {
            plan.done += 1;
            continue;
        }
        plan.todo.push(Todo {
            chunk_id: row.id,
            cache_key: item.built.cache_key,
            chars: item.built.request.chars(),
            resend: row.attr_key.is_some(),
            order: row.order,
        });
    }
    Ok(())
}

fn text_hit(setup: &Setup<'_>, chunk: &Eligible) -> bool {
    !hits(setup.matcher, &chunk.text, &chunk.context).is_empty()
}

async fn plan(setup: &Setup<'_>, options: &Options, seed: &str) -> Result<Plan> {
    let config = setup.config;
    let extract = &config.llm_extract;
    let source_order: Vec<&String> = if extract.sources.is_empty() {
        config.sources.iter().map(|s| &s.name).collect()
    } else {
        extract.sources.iter().collect()
    };
    let paths = path_filter(&extract.paths)?;
    let filter = Filter {
        sources: &extract.sources,
        doc_types: &extract.doc_types,
        min_chars: extract.min_chars as i32,
        extracted: options.extracted,
        context_chars: config.attributes.context_chars as i32,
    };
    let mut plan = Plan::default();
    let mut candidates = Vec::new();
    let mut rows: HashMap<i64, Row> = HashMap::new();
    let mut after = 0;
    loop {
        let eligible = db::attributes::eligible(setup.pool, &filter, after, BATCH).await?;
        let Some(last) = eligible.last() else {
            break;
        };
        after = last.id;
        plan.scanned += eligible.len();
        let eligible: Vec<Eligible> = eligible
            .into_iter()
            .filter(|c| paths.as_ref().is_none_or(|set| set.is_match(&c.path)))
            .collect();
        let hit = parallel_map(&eligible, setup.threads, |c| text_hit(setup, c));
        let eligible: Vec<Eligible> = eligible
            .into_iter()
            .zip(hit)
            .filter_map(|(chunk, hit)| hit.then_some(chunk))
            .collect();
        let with_subject: HashSet<i64> = if options.sample.is_some() {
            let ids: Vec<i64> = eligible.iter().map(|c| c.id).collect();
            prepare(setup, &ids)
                .await?
                .into_iter()
                .map(|p| p.chunk.id)
                .collect()
        } else {
            HashSet::new()
        };
        let mut batch = Vec::new();
        for chunk in eligible {
            let source = source_order
                .iter()
                .position(|s| **s == chunk.source)
                .unwrap_or(source_order.len());
            let row = Row {
                id: chunk.id,
                attr_key: chunk.attr_key.clone(),
                order: (source, chunk.document_id, chunk.id),
            };
            if options.sample.is_some() {
                if !with_subject.contains(&chunk.id) {
                    continue;
                }
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
        compare(setup, &batch, &mut plan).await?;
    }
    if let Some(size) = options.sample {
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
            "attributes: sample of {} from {} chunks with a value and a subject ({} repeated \
             texts and {} unreadable or mostly numbers left out)",
            selected.ids.len(),
            candidates.len(),
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
        let keys: Vec<String> = part.iter().map(|t| t.cache_key.clone()).collect();
        let (hit, bad) = db::llm::known_keys(pool, &keys).await?;
        cached.extend(hit);
        invalid.extend(bad);
    }
    Ok((cached, invalid))
}

fn chunk_values(setup: &Setup<'_>, prepared: Prepared, response: &str) -> Option<ChunkValues> {
    let answer: Answer = serde_json::from_str(response).ok()?;
    let checked = check(
        answer,
        &prepared.built,
        &prepared.chunk.text,
        &prepared.context,
        setup.ontology,
        setup.units,
        &Limits {
            max_text_chars: setup.config.attributes.max_text_chars,
        },
    );
    Some(ChunkValues {
        chunk_id: prepared.chunk.id,
        key: prepared.built.key,
        scale: prepared.chunk.ocr_scale(),
        checked,
    })
}

async fn write_cached(
    setup: &Setup<'_>,
    ids: &[i64],
    method: &str,
    confidence: &Confidence,
) -> Result<usize> {
    let mut written = 0;
    for part in ids.chunks(BATCH as usize) {
        let prepared = prepare(setup, part).await?;
        let keys: Vec<String> = prepared.iter().map(|p| p.built.cache_key.clone()).collect();
        let cache = db::llm::cached(setup.pool, &keys).await?;
        let mut values = Vec::new();
        for item in prepared {
            let Some(response) = cache.get(&item.built.cache_key) else {
                continue;
            };
            if let Some(chunk) = chunk_values(setup, item, response) {
                values.push(chunk);
            }
        }
        written += values.len();
        db::attributes::write(setup.pool, &values, method, confidence).await?;
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
        println!("attributes job {}: abandoned, {reason}", open.id);
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
    let started = std::time::Instant::now();
    let plan = plan(setup, options, &seed).await?;
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
    println!(
        "attributes: {} chunks read, {} with a value and a subject, {} done before, {} from the \
         cache, {} to send, {} invalid before, {resend} to send again after a change ({:.1} s)",
        plan.scanned,
        plan.eligible,
        plan.done,
        hits.len(),
        to_send.len(),
        plan.todo.len() - hits.len() - to_send.len(),
        started.elapsed().as_secs_f64()
    );
    let share = config.llm_extract.resend_share;
    if plan.eligible > 0 && resend as f64 > share * plan.eligible as f64 && !options.yes {
        println!(
            "attributes: {resend} of {} chunks would be sent again, more than resend_share \
             {:.0}%; run with --yes to go on",
            plan.eligible,
            share * 100.0
        );
        if !options.dry_run {
            bail!("attributes stopped before sending {resend} chunks again; run with --yes");
        }
    }
    let method = setup.name.method();
    let precision = db::review::precision(pool).await?;
    let confidence = Confidence {
        base: config.attributes.confidence,
        measured: crate::review::measured(&precision, &method, config.review.min_verdicts),
    };
    let mut job = open_job(setup, options, version_id).await?;
    if !options.dry_run && !hits.is_empty() {
        let written = write_cached(setup, &hits, &method, &confidence).await?;
        println!("attributes: values written from the cache for {written} chunks");
    }
    let out = PathBuf::from(&config.attributes.out);
    if to_send.is_empty() && !options.dry_run {
        if let Some(open) = &job {
            db::llm::set_job(pool, open.id, "finished", None, None).await?;
        }
        super::report::write_report(pool, setup.ontology, config, &out, None).await?;
        println!("attributes: nothing to send");
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
        println!("attributes job {}: new", created.id);
        job = Some(created);
    }
    let job_id = job.as_ref().map_or(0, |j| j.id);
    let mut lock = pool.acquire().await?;
    if job_id > 0 && !db::llm::lock(&mut lock, job_id).await? {
        bail!("attributes job {job_id} is running in another process");
    }
    let (queue, chunk_of) = if job_id > 0 {
        let ids: Vec<i64> = to_send.iter().map(|t| t.chunk_id).collect();
        let keys: Vec<String> = to_send.iter().map(|t| t.cache_key.clone()).collect();
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
    let handler = Attributes {
        setup,
        job: job_id,
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
    job::run(&runner, &handler, queue, &limits).await
}

impl Attributes<'_> {
    fn take(&self, ord: i32) -> Option<Prepared> {
        self.pending.lock().ok()?.remove(&ord)
    }
}

impl Handler for Attributes<'_> {
    fn task(&self) -> &'static str {
        TASK
    }

    fn label(&self) -> String {
        format!("attributes job {}", self.job)
    }

    async fn request(&self, ord: i32) -> Result<Option<(String, Request)>> {
        let Some(chunk_id) = self.chunk_of.get(&ord) else {
            return Ok(None);
        };
        let Some(prepared) = prepare(self.setup, &[*chunk_id]).await?.pop() else {
            return Ok(None);
        };
        let pair = (
            prepared.built.cache_key.clone(),
            prepared.built.request.clone(),
        );
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
        let Some(values) = chunk_values(self.setup, prepared, response) else {
            return Ok(());
        };
        let drops = values.checked.drops.clone();
        db::attributes::write(self.setup.pool, &[values], &self.method, &self.confidence).await?;
        db::llm::add_stats(self.setup.pool, self.job, &drops).await
    }

    async fn report(&self) -> Result<()> {
        if let Ok(mut pending) = self.pending.lock() {
            pending.clear();
        }
        super::report::write_report(
            self.setup.pool,
            self.setup.ontology,
            self.setup.config,
            &self.out,
            Some(self.job),
        )
        .await
    }
}
