use std::collections::{BTreeMap, HashMap, VecDeque};
use std::path::Path;
use std::sync::{Mutex, PoisonError};

use anyhow::{Result, bail};
use serde::Deserialize;
use sqlx::PgPool;

use crate::config::{self, Config};
use crate::db;
use crate::db::classify::{TypeRow, Unit};
use crate::db::llm::JobParams;
use crate::facts::check::Haystack;
use crate::llm::job::{self, Handler, Options, Outcome, Runner, Work};
use crate::llm::prompt::{self, Prompt};
use crate::llm::shortlist::{label, short_definition};
use crate::llm::{ModelName, Request};
use crate::ontology::{self, Ontology};
use crate::rules::Rules;

pub const TASK: &str = "classify";
pub const STAGE_VERSION: u32 = 1;
pub const USER_VARIABLES: [&str; 4] = ["doc_types", "path", "pages", "text"];
pub const NONE: &str = "none";
const UNKNOWN_TYPE: &str = "type not configured";
const QUOTE_NOT_FOUND: &str = "quote not in the text";

#[derive(Debug, Clone, Deserialize)]
pub struct Answer {
    pub doc_type: String,
    pub evidence: String,
}

struct Setup<'a> {
    pool: &'a PgPool,
    config: &'a Config,
    system: &'a Prompt,
    user: &'a Prompt,
    name: &'a ModelName,
    names: Vec<String>,
    lines: String,
}

struct Built {
    unit: Unit,
    text: String,
    request: Request,
    key: String,
}

struct Classify<'a> {
    setup: &'a Setup<'a>,
    job: i64,
    method: String,
    unit_of: HashMap<i32, Unit>,
    pending: Mutex<HashMap<i32, Built>>,
    written: Mutex<usize>,
}

pub fn doc_type_lines(
    rules: &Rules,
    ontology: &Ontology,
    languages: &[String],
    definition_chars: usize,
) -> String {
    let mut out = String::new();
    for (name, iri) in &rules.doc_types {
        let class = ontology.classes.get(iri);
        let class_label = class.map_or(String::new(), |c| label(c, languages));
        let definition = class.map_or(String::new(), |c| {
            short_definition(c, languages, definition_chars)
        });
        out.push_str(&format!("- {name} | {class_label} | {definition}\n"));
    }
    out
}

pub fn schema(names: &[String]) -> serde_json::Value {
    let mut allowed: Vec<&str> = names.iter().map(String::as_str).collect();
    allowed.push(NONE);
    serde_json::json!({
        "type": "object",
        "additionalProperties": false,
        "required": ["doc_type", "evidence"],
        "properties": {
            "doc_type": {"type": "string", "enum": allowed},
            "evidence": {"type": "string"},
        },
    })
}

pub fn check(
    answer: &Answer,
    names: &[String],
    text: &str,
) -> Result<Option<String>, &'static str> {
    let doc_type = answer.doc_type.trim();
    if doc_type.eq_ignore_ascii_case(NONE) {
        return Ok(None);
    }
    let Some(name) = names.iter().find(|n| n.eq_ignore_ascii_case(doc_type)) else {
        return Err(UNKNOWN_TYPE);
    };
    if Haystack::new(text).find(&answer.evidence).is_none() {
        return Err(QUOTE_NOT_FOUND);
    }
    Ok(Some(name.clone()))
}

fn head(text: &str, chars: usize) -> String {
    text.chars()
        .take(chars)
        .collect::<String>()
        .trim()
        .to_string()
}

pub async fn run(
    config_dir: Option<&Path>,
    url: Option<&str>,
    options: &Options,
) -> Result<Outcome> {
    let config = config::load(config_dir)?;
    if config.models.classify.is_empty() {
        bail!(
            "[models] classify is empty; set it to provider:model, for example claude-code:haiku"
        );
    }
    let name = ModelName::configured(&config.models.classify, &config.providers)
        .map_err(|e| anyhow::anyhow!("models.classify: {e}"))?;
    let loaded = ontology::load_checked(config_dir, &config.ontology)?;
    let rules = crate::rules::load(
        config_dir,
        &loaded.ontology,
        &config.ontology.document_class,
    )?;
    if rules.doc_types.is_empty() {
        bail!("rules.toml has no [doc_types]; classify has nothing to choose from");
    }
    let system = prompt::load(config_dir, "classify-system")?;
    system.check(&[])?;
    let user = prompt::load(config_dir, "classify-user")?;
    user.check(&USER_VARIABLES)?;
    user.require("text")?;
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
    let setup = Setup {
        pool: &pool,
        config: &config,
        system: &system,
        user: &user,
        name: &name,
        names: rules.doc_types.keys().cloned().collect(),
        lines: doc_type_lines(
            &rules,
            &loaded.ontology,
            &config.ontology.languages,
            config.classify.definition_chars,
        ),
    };
    let result = run_with(&setup, options, &loaded.ontology).await;
    let result = match result {
        Ok((outcome, written)) if written > 0 && !options.dry_run => {
            println!("classify: {written} parts typed; running the rules stage for them");
            crate::scan::rules_only(config_dir, &pool)
                .await
                .map(|_| outcome)
        }
        other => other.map(|(outcome, _)| outcome),
    };
    pool.close().await;
    db::runs::release(lock).await;
    result
}

async fn build(setup: &Setup<'_>, units: &[Unit]) -> Result<Vec<Built>> {
    let spans: Vec<(i64, i32, i32)> = units
        .iter()
        .map(|u| (u.content_id, u.page_from, u.page_to))
        .collect();
    let head_chars = setup.config.classify.head_chars;
    let mut texts: HashMap<(i64, i32), String> =
        db::classify::heads(setup.pool, &spans, head_chars as i32)
            .await?
            .into_iter()
            .map(|(content, page, text)| ((content, page), head(&text, head_chars)))
            .collect();
    let mut built = Vec::new();
    for unit in units {
        let Some(text) = texts.remove(&(unit.content_id, unit.page_from)) else {
            continue;
        };
        if text.chars().count() < setup.config.classify.min_chars {
            continue;
        }
        let pages = if unit.page_from == unit.page_to {
            unit.page_from.to_string()
        } else {
            format!("{}-{}", unit.page_from, unit.page_to)
        };
        let request = Request {
            task: TASK.to_string(),
            system: setup.system.text.clone(),
            user: setup.user.fill(&[
                ("doc_types", setup.lines.trim_end()),
                ("path", &unit.path),
                ("pages", &pages),
                ("text", &text),
            ]),
            schema: schema(&setup.names),
        };
        let key = request.stage_key(setup.name, STAGE_VERSION);
        built.push(Built {
            unit: unit.clone(),
            text,
            request,
            key,
        });
    }
    Ok(built)
}

fn type_of(
    setup: &Setup<'_>,
    built: &Built,
    response: &str,
) -> (Option<String>, Option<&'static str>) {
    match serde_json::from_str::<Answer>(response) {
        Ok(answer) => match check(&answer, &setup.names, &built.text) {
            Ok(doc_type) => (doc_type, None),
            Err(reason) => (None, Some(reason)),
        },
        Err(_) => (None, None),
    }
}

async fn write_typed(
    setup: &Setup<'_>,
    typed: &[(&Built, Option<String>)],
    method: &str,
) -> Result<()> {
    let rows: Vec<TypeRow> = typed
        .iter()
        .map(|(built, doc_type)| TypeRow {
            content_id: built.unit.content_id,
            page_from: built.unit.page_from,
            page_to: built.unit.page_to,
            doc_type: doc_type.as_deref(),
            key: &built.key,
        })
        .collect();
    db::classify::write(setup.pool, &rows, method, setup.config.classify.confidence).await
}

async fn run_with(
    setup: &Setup<'_>,
    options: &Options,
    ontology: &Ontology,
) -> Result<(Outcome, usize)> {
    let pool = setup.pool;
    let method = setup.name.method();
    let Some(version_id) = db::fit::ontology_version(pool, &ontology.version).await? else {
        bail!("the ontology changed since the last scan; run zontologize scan first");
    };
    let units = db::classify::units(pool).await?;
    let mut todo: Vec<(Unit, String, usize)> = Vec::new();
    let (mut done, mut cached, mut written) = (0, 0, 0);
    for part in units.chunks(setup.config.classify.read_batch) {
        let built = build(setup, part).await?;
        let changed: Vec<Built> = built
            .into_iter()
            .filter(|b| {
                let same = b.unit.cache_key.as_deref() == Some(b.key.as_str());
                done += usize::from(same);
                !same
            })
            .collect();
        let keys: Vec<String> = changed.iter().map(|b| b.key.clone()).collect();
        let cache = db::llm::cached(pool, &keys).await?;
        let mut typed = Vec::new();
        for item in &changed {
            match cache.get(&item.key) {
                Some(response) => typed.push((item, type_of(setup, item, response).0)),
                None => todo.push((item.unit.clone(), item.key.clone(), item.request.chars())),
            }
        }
        cached += typed.len();
        if !options.dry_run {
            write_typed(setup, &typed, &method).await?;
            written += typed.len();
        }
    }
    println!(
        "classify: {} parts without a type from rules, {done} done before, {cached} from the cache, \
         {} to send",
        units.len(),
        todo.len()
    );
    let params = JobParams::default();
    let mut job = job::reopen(pool, TASK, setup.name, version_id, &params, options, true).await?;
    if todo.is_empty() && !options.dry_run {
        if let Some(open) = &job {
            db::llm::set_job(pool, open.id, "finished", None, None).await?;
        }
        println!("classify: nothing to send");
        return Ok((Outcome::Finished, written));
    }
    if job.is_none() && !options.dry_run {
        let created = db::llm::create_job(
            pool,
            TASK,
            setup.name.provider.as_str(),
            &setup.name.model,
            version_id,
            &params,
            &[],
        )
        .await?;
        println!("classify job {}: new", created.id);
        job = Some(created);
    }
    let job_id = job.as_ref().map_or(0, |j| j.id);
    let mut lock = pool.acquire().await?;
    if job_id > 0 && !db::llm::lock(&mut lock, job_id).await? {
        bail!("classify job {job_id} is running in another process");
    }
    let ords: Vec<i32> = if job_id > 0 {
        let spans: Vec<(i64, i32)> = todo
            .iter()
            .map(|t| (t.0.content_id, t.0.page_from))
            .collect();
        let keys: Vec<String> = todo.iter().map(|t| t.1.clone()).collect();
        db::classify::replace_items(pool, job_id, &spans, &keys).await?
    } else {
        (1..=todo.len() as i32).collect()
    };
    let queue: VecDeque<Work> = ords
        .iter()
        .zip(&todo)
        .map(|(ord, t)| Work {
            ord: *ord,
            chars: t.2,
        })
        .collect();
    let handler = Classify {
        setup,
        job: job_id,
        method,
        unit_of: ords
            .iter()
            .copied()
            .zip(todo.into_iter().map(|t| t.0))
            .collect(),
        pending: Mutex::new(HashMap::new()),
        written: Mutex::new(0),
    };
    let runner = Runner {
        pool,
        config: setup.config,
        name: setup.name,
        job_id,
        resume_after: job.as_ref().and_then(|j| j.resume_after),
    };
    let outcome = job::run(&runner, &handler, queue, options).await?;
    let sent = *handler
        .written
        .lock()
        .unwrap_or_else(PoisonError::into_inner);
    Ok((outcome, written + sent))
}

impl Handler for Classify<'_> {
    fn task(&self) -> &'static str {
        TASK
    }

    fn label(&self) -> String {
        format!("classify job {}", self.job)
    }

    async fn request(&self, ord: i32) -> Result<Option<(String, Request)>> {
        let Some(unit) = self.unit_of.get(&ord) else {
            return Ok(None);
        };
        let Some(built) = build(self.setup, std::slice::from_ref(unit)).await?.pop() else {
            return Ok(None);
        };
        let pair = (built.key.clone(), built.request.clone());
        self.pending
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(ord, built);
        Ok(Some(pair))
    }

    fn accepts(&self, _ord: i32, response: &str) -> Result<(), String> {
        serde_json::from_str::<Answer>(response)
            .map(|_| ())
            .map_err(|e| e.to_string())
    }

    async fn answered(&self, ord: i32, response: &str) -> Result<()> {
        let removed = self
            .pending
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(&ord);
        let Some(built) = removed else {
            return Ok(());
        };
        let (doc_type, drop) = type_of(self.setup, &built, response);
        write_typed(self.setup, &[(&built, doc_type)], &self.method).await?;
        *self.written.lock().unwrap_or_else(PoisonError::into_inner) += 1;
        let Some(reason) = drop else {
            return Ok(());
        };
        let stats = BTreeMap::from([(reason.to_string(), 1)]);
        db::llm::add_stats(self.setup.pool, self.job, &stats).await
    }

    async fn report(&self) -> Result<()> {
        let counts: Vec<String> = db::classify::counts(self.setup.pool)
            .await?
            .into_iter()
            .map(|(doc_type, count)| format!("{count} {}", doc_type.as_deref().unwrap_or(NONE)))
            .collect();
        println!("classify: types so far: {}", counts.join(", "));
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn names() -> Vec<String> {
        vec!["invoice".to_string(), "report".to_string()]
    }

    fn answer(doc_type: &str, evidence: &str) -> Answer {
        Answer {
            doc_type: doc_type.to_string(),
            evidence: evidence.to_string(),
        }
    }

    #[test]
    fn schema_allows_the_configured_types_and_none() {
        let built = schema(&names());
        assert_eq!(
            built["properties"]["doc_type"]["enum"],
            serde_json::json!(["invoice", "report", "none"])
        );
    }

    #[test]
    fn check_needs_a_known_type_and_a_quote_from_the_text() {
        let text = "INVOICE no. 12\nTotal due: 40";
        assert_eq!(
            check(&answer("Invoice", "invoice  no."), &names(), text),
            Ok(Some("invoice".to_string()))
        );
        assert_eq!(check(&answer("none", ""), &names(), text), Ok(None));
        assert_eq!(
            check(&answer("invoice", "receipt"), &names(), text),
            Err(QUOTE_NOT_FOUND)
        );
        assert_eq!(
            check(&answer("letter", "invoice"), &names(), text),
            Err(UNKNOWN_TYPE)
        );
    }

    #[test]
    fn recorded_answer_parses_and_checks() {
        let answer: Answer = crate::facts::tests::recorded("classify-recorded.jsonl");
        let text = "Work order 17. Service report for project P-2024-0001 at the north site.";
        let names = vec!["work_order".to_string()];
        assert_eq!(
            check(&answer, &names, text),
            Ok(Some("work_order".to_string()))
        );
    }

    #[test]
    fn built_in_prompts_use_known_variables() {
        prompt::load(None, "classify-system")
            .unwrap()
            .check(&[])
            .unwrap();
        prompt::load(None, "classify-user")
            .unwrap()
            .check(&USER_VARIABLES)
            .unwrap();
    }
}
