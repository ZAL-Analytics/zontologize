use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};

use anyhow::{Result, bail};
use sqlx::PgPool;

use super::report::{self, Entry, Summary};
use super::{Answer, TASK, check, split_sample};
use crate::config::{self, Config};
use crate::db;
use crate::db::fit::Chunk;
use crate::db::llm::{Item, Job, JobParams};
use crate::llm::job::{self, Handler, Runner, Work};
use crate::llm::prompt::{self, Prompt};
use crate::llm::shortlist::{counts_by_chunk, shortlist, shortlist_lines};
use crate::llm::{ModelName, Request};
use crate::ontology::{self, Ontology};

const USER_VARIABLES: [&str; 5] = ["path", "pages", "languages", "shortlist", "chunk"];

pub use crate::llm::job::{Options, Outcome};

struct Prepared {
    chunk: Chunk,
    shortlist: Vec<String>,
    request: Request,
    key: String,
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
    let name = ModelName::configured(&config.models.fit, &config.providers)
        .map_err(|e| anyhow::anyhow!("models.fit: {e}"))?;
    if config.ontology.files.is_empty() {
        bail!("[ontology] files is empty");
    }
    let loaded = ontology::load_checked(config_dir, &config.ontology)?;
    let system = prompt::load(config_dir, "fit-system")?;
    system.check(&[])?;
    let user = prompt::load(config_dir, "fit-user")?;
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
    db::runs::release(lock).await;
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
    let job = job::reopen(pool, TASK, name, version_id, &params, options, false).await?;
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
                chars: p.request.chars(),
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
    let out = PathBuf::from(&config.fit.out);
    let handler = Fit {
        pool,
        config,
        name,
        ontology,
        job: &job,
        prepared: &prepared,
        out: &out,
    };
    let runner = Runner {
        pool,
        config,
        name,
        job_id: job.id,
        resume_after: job.resume_after,
    };
    job::run(&runner, &handler, queue, options).await
}

impl Handler for Fit<'_> {
    fn task(&self) -> &'static str {
        TASK
    }

    fn label(&self) -> String {
        format!("fit job {}", self.job.id)
    }

    async fn request(&self, ord: i32) -> Result<Option<(String, Request)>> {
        Ok(self
            .prepared
            .get(&ord)
            .map(|p| (p.key.clone(), p.request.clone())))
    }

    fn accepts(&self, _ord: i32, response: &str) -> Result<(), String> {
        serde_json::from_str::<Answer>(response)
            .map(|_| ())
            .map_err(|e| e.to_string())
    }

    async fn answered(&self, _ord: i32, _response: &str) -> Result<()> {
        Ok(())
    }

    async fn report(&self) -> Result<()> {
        write_report(self).await
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
            &[],
            config.fit.max_shortlist,
        );
        let lines = shortlist_lines(ontology, &list, languages, config.fit.definition_chars);
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
