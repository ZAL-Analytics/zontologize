use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use anyhow::{Context, Result, anyhow, bail};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use sqlx::PgPool;

use super::model::{Model, text_key};
use super::run::{Shared, class_text, lock};
use super::{Embed, STAGE_VERSION, embed_sorted, passage, query};
use crate::config::Config;
use crate::db;
use crate::db::embed::{CHUNK, EvalChunk, Vector};
use crate::facts::sample::letter_share;
use crate::ontology::Ontology;

const QUESTIONS_FILE: &str = "questions.toml";
const REPORT_FILE: &str = "embed-report.md";
const TOP: usize = 10;
const HALFVEC_HEADER_BYTES: usize = 8;

#[derive(Debug, Clone, Default)]
pub struct Options {
    pub models: Vec<String>,
    pub sample: Option<usize>,
    pub seed: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct QuestionsFile {
    #[serde(default)]
    questions: Vec<Question>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Question {
    pub text: String,
    pub expect: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct Query {
    pub label: String,
    pub text: String,
    pub expected: HashSet<usize>,
    pub own: Option<usize>,
}

#[derive(Debug, Default)]
pub struct Sample {
    pub chunks: Vec<EvalChunk>,
    pub neighbours: Vec<Query>,
    pub classes: Vec<Query>,
    pub questions: Vec<Query>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Score {
    pub queries: usize,
    pub recall_1: f64,
    pub recall_10: f64,
    pub mrr: f64,
}

#[derive(Debug, Clone)]
pub struct ModelReport {
    pub name: String,
    pub dimensions: usize,
    pub rate: Option<f64>,
    pub truncated: f64,
    pub neighbours: Score,
    pub classes: Score,
    pub questions: Score,
    pub missed: Vec<String>,
}

pub fn first_rank(
    scores: &[f32],
    expected: &HashSet<usize>,
    skip: impl Fn(usize) -> bool,
) -> Option<usize> {
    let mut order: Vec<usize> = (0..scores.len()).filter(|&i| !skip(i)).collect();
    order.sort_by(|&a, &b| scores[b].total_cmp(&scores[a]).then(a.cmp(&b)));
    order
        .iter()
        .position(|i| expected.contains(i))
        .map(|position| position + 1)
}

pub fn score(ranks: &[Option<usize>]) -> Score {
    if ranks.is_empty() {
        return Score::default();
    }
    let n = ranks.len() as f64;
    let within = |k: usize| ranks.iter().filter(|r| r.is_some_and(|r| r <= k)).count() as f64 / n;
    Score {
        queries: ranks.len(),
        recall_1: within(1),
        recall_10: within(TOP),
        mrr: ranks
            .iter()
            .map(|r| r.map_or(0.0, |r| 1.0 / r as f64))
            .sum::<f64>()
            / n,
    }
}

fn dot(a: &[f32], b: &[f32]) -> f32 {
    a.iter().zip(b).map(|(x, y)| x * y).sum()
}

pub fn rank_queries(
    queries: &[Query],
    query_vectors: &[Vec<f32>],
    chunk_vectors: &[Vec<f32>],
    contents: &[i64],
) -> Vec<Option<usize>> {
    queries
        .iter()
        .zip(query_vectors)
        .map(|(q, vector)| {
            let scores: Vec<f32> = chunk_vectors.iter().map(|c| dot(vector, c)).collect();
            let own = q.own.map(|i| contents[i]);
            first_rank(&scores, &q.expected, |i| Some(contents[i]) == own)
        })
        .collect()
}

fn seeded(seed: &str, id: i64) -> [u8; 8] {
    let digest = Sha256::digest(format!("{seed}:{id}").as_bytes());
    let mut out = [0u8; 8];
    out.copy_from_slice(&digest[..8]);
    out
}

pub fn read_questions(config_dir: Option<&Path>) -> Result<Vec<Question>> {
    let Some(path) = config_dir.map(|d| d.join(QUESTIONS_FILE)) else {
        return Ok(Vec::new());
    };
    if !path.exists() {
        return Ok(Vec::new());
    }
    let text = std::fs::read_to_string(&path)
        .with_context(|| format!("cannot read {}", path.display()))?;
    let file: QuestionsFile = toml::from_str(&text)
        .map_err(|e| anyhow!("{}: {}", path.display(), e.to_string().trim_end()))?;
    Ok(file.questions)
}

pub fn match_questions(
    questions: &[Question],
    paths: &[(String, i64)],
) -> Result<Vec<BTreeSet<i64>>> {
    let mut out = Vec::new();
    for question in questions {
        let mut contents = BTreeSet::new();
        for pattern in &question.expect {
            let glob = globset::Glob::new(pattern)
                .map_err(|e| anyhow!("question {:?}: {pattern:?}: {e}", question.text))?
                .compile_matcher();
            let mut matched = false;
            for (path, content) in paths {
                if glob.is_match(path) {
                    matched = true;
                    contents.insert(*content);
                }
            }
            if !matched {
                bail!(
                    "question {:?}: {pattern:?} matches no present document",
                    question.text
                );
            }
        }
        out.push(contents);
    }
    Ok(out)
}

fn readable(chunk: &EvalChunk, config: &Config) -> bool {
    let settings = &config.embed;
    let low_ocr = chunk
        .ocr_confidence
        .is_some_and(|c| c < settings.min_ocr_confidence);
    !low_ocr && letter_share(&chunk.text) >= settings.min_letter_share
}

pub async fn sample(
    pool: &PgPool,
    config: &Config,
    ontology: &Ontology,
    questions: &[Question],
    size: usize,
    seed: &str,
) -> Result<Sample> {
    let min_chars = config.embed.min_chars as i32;
    let linked = db::embed::linked_mentions(pool, min_chars, db::entities::SEGMENT_METHOD).await?;
    let mut per_entity: BTreeMap<i64, Vec<(i64, i64)>> = BTreeMap::new();
    for row in &linked {
        per_entity
            .entry(row.entity_id)
            .or_default()
            .push((row.chunk_id, row.content_id));
    }
    let mut per_chunk: HashMap<i64, Vec<i64>> = HashMap::new();
    for (entity, chunks) in per_entity.iter_mut() {
        chunks.sort_by_key(|(chunk, _)| seeded(seed, *chunk));
        chunks.truncate(config.embed.eval_max_expected.max(2));
        for (chunk, _) in chunks.iter() {
            per_chunk.entry(*chunk).or_default().push(*entity);
        }
    }
    let mut anchors: Vec<i64> = per_chunk.keys().copied().collect();
    anchors.sort_by_key(|id| seeded(seed, *id));
    anchors.truncate(size);
    let mut ids: BTreeSet<i64> = anchors.iter().copied().collect();
    for anchor in &anchors {
        for entity in &per_chunk[anchor] {
            ids.extend(per_entity[entity].iter().map(|(chunk, _)| *chunk));
        }
    }
    ids.extend(db::embed::distractors(pool, min_chars, seed, size as i64).await?);
    let paths = db::embed::present_paths(pool).await?;
    let question_contents = match_questions(questions, &paths)?;
    let all_contents: Vec<i64> = question_contents.iter().flatten().copied().collect();
    ids.extend(db::embed::content_chunks(pool, &all_contents, min_chars).await?);
    let ids: Vec<i64> = ids.into_iter().collect();
    let chunks: Vec<EvalChunk> = db::embed::eval_chunks(pool, &ids)
        .await?
        .into_iter()
        .filter(|c| readable(c, config))
        .collect();
    let index: HashMap<i64, usize> = chunks.iter().enumerate().map(|(i, c)| (c.id, i)).collect();
    let mut neighbours = Vec::new();
    for anchor in &anchors {
        let Some(&own) = index.get(anchor) else {
            continue;
        };
        let content = chunks[own].content_id;
        let expected: HashSet<usize> = per_chunk[anchor]
            .iter()
            .flat_map(|entity| per_entity[entity].iter())
            .filter(|(_, other)| *other != content)
            .filter_map(|(chunk, _)| index.get(chunk).copied())
            .collect();
        if expected.is_empty() {
            continue;
        }
        neighbours.push(Query {
            label: format!("chunk {anchor}"),
            text: chunks[own].text.clone(),
            expected,
            own: Some(own),
        });
    }
    let corpus: Vec<i64> = chunks.iter().map(|c| c.id).collect();
    let mut by_class: BTreeMap<String, HashSet<usize>> = BTreeMap::new();
    for (chunk, class) in db::embed::class_mentions(pool, &corpus).await? {
        if let Some(&i) = index.get(&chunk) {
            by_class.entry(class).or_default().insert(i);
        }
    }
    let classes = by_class
        .into_iter()
        .filter_map(|(iri, expected)| {
            let class = ontology.classes.get(&iri).filter(|c| c.selected)?;
            Some(Query {
                label: iri.clone(),
                text: class_text(class, &config.ontology.languages),
                expected,
                own: None,
            })
        })
        .collect();
    let questions = questions
        .iter()
        .zip(&question_contents)
        .map(|(question, contents)| Query {
            label: question.text.clone(),
            text: question.text.clone(),
            expected: chunks
                .iter()
                .enumerate()
                .filter(|(_, c)| contents.contains(&c.content_id))
                .map(|(i, _)| i)
                .collect(),
            own: None,
        })
        .collect();
    Ok(Sample {
        chunks,
        neighbours,
        classes,
        questions,
    })
}

pub async fn evaluate(
    pool: &PgPool,
    config: &Config,
    model: &Model,
    embedder: Box<dyn Embed>,
    sample: &Sample,
) -> Result<ModelReport> {
    let shared: Shared = Arc::new(Mutex::new(embedder));
    let current = model.full_name() == config.models.embed;
    let (model_id, fingerprint) = super::run::model_id(pool, model, &shared, current).await?;
    let texts: Vec<String> = sample
        .chunks
        .iter()
        .map(|c| passage(model, &c.text))
        .collect();
    let keys: Vec<String> = texts
        .iter()
        .map(|t| text_key(&fingerprint, STAGE_VERSION, t))
        .collect();
    let known = db::embed::known_vectors(pool, model_id, &keys).await?;
    let mut vectors: Vec<Vec<f32>> = vec![Vec::new(); texts.len()];
    let mut truncated = vec![false; texts.len()];
    let mut missing = Vec::new();
    for (i, key) in keys.iter().enumerate() {
        match known.get(key) {
            Some((cut, text)) => {
                vectors[i] = db::embed::parse_vector(text)?;
                truncated[i] = *cut;
            }
            None => missing.push(i),
        }
    }
    let batch_size = config.embed.batch_size;
    let started = Instant::now();
    let fresh = embed_all(
        &shared,
        missing.iter().map(|&i| texts[i].clone()).collect(),
        batch_size,
    )
    .await?;
    let seconds = started.elapsed().as_secs_f64();
    let rate = (!missing.is_empty()).then(|| missing.len() as f64 / seconds.max(1e-9));
    let mut rows = Vec::new();
    let embedded: Vec<String> = fresh
        .vectors
        .iter()
        .map(|v| db::embed::vector_text(v))
        .collect();
    for ((&i, vector), (cut, text)) in missing
        .iter()
        .zip(fresh.vectors)
        .zip(fresh.truncated.iter().zip(&embedded))
    {
        vectors[i] = vector;
        truncated[i] = *cut;
        rows.push(Vector {
            owner_id: sample.chunks[i].id,
            text_key: &keys[i],
            truncated: *cut,
            embedding: text,
        });
    }
    for batch in rows.chunks(batch_size.max(1)) {
        db::embed::write(pool, model_id, CHUNK, batch).await?;
    }
    let contents: Vec<i64> = sample.chunks.iter().map(|c| c.content_id).collect();
    let neighbour_vectors: Vec<Vec<f32>> = sample
        .neighbours
        .iter()
        .map(|q| q.own.map(|i| vectors[i].clone()).unwrap_or_default())
        .collect();
    let class_texts: Vec<String> = sample
        .classes
        .iter()
        .map(|q| query(model, &q.text))
        .collect();
    let question_texts: Vec<String> = sample
        .questions
        .iter()
        .map(|q| query(model, &q.text))
        .collect();
    let class_vectors = embed_all(&shared, class_texts, batch_size).await?.vectors;
    let question_vectors = embed_all(&shared, question_texts, batch_size)
        .await?
        .vectors;
    let question_ranks = rank_queries(&sample.questions, &question_vectors, &vectors, &contents);
    let missed = sample
        .questions
        .iter()
        .zip(&question_ranks)
        .filter(|(_, rank)| rank.is_none_or(|r| r > TOP))
        .map(|(q, _)| q.label.clone())
        .collect();
    let dimensions = lock(&shared)?.dimensions()?;
    Ok(ModelReport {
        name: model.full_name(),
        dimensions,
        rate,
        truncated: truncated.iter().filter(|t| **t).count() as f64 / truncated.len().max(1) as f64,
        neighbours: score(&rank_queries(
            &sample.neighbours,
            &neighbour_vectors,
            &vectors,
            &contents,
        )),
        classes: score(&rank_queries(
            &sample.classes,
            &class_vectors,
            &vectors,
            &contents,
        )),
        questions: score(&question_ranks),
        missed,
    })
}

async fn embed_all(
    shared: &Shared,
    texts: Vec<String>,
    batch_size: usize,
) -> Result<super::Embedded> {
    if texts.is_empty() {
        return Ok(super::Embedded::default());
    }
    let shared = shared.clone();
    tokio::task::spawn_blocking(move || {
        let mut embedder = lock(&shared)?;
        embed_sorted(embedder.as_mut(), &texts, batch_size)
    })
    .await?
}

fn cell(score: &Score) -> String {
    if score.queries == 0 {
        return "- | - | -".to_string();
    }
    format!(
        "{:.3} | {:.3} | {:.3}",
        score.recall_1, score.recall_10, score.mrr
    )
}

pub fn report(sample: &Sample, seed: &str, models: &[ModelReport]) -> String {
    let mut out = String::new();
    let _ = writeln!(out, "# Embedding model comparison\n");
    let _ = writeln!(
        out,
        "Seed `{seed}`: {} chunks in the sample, {} chunk queries (neighbours share an entity found by rules, in another document), {} class queries, {} questions.\n",
        sample.chunks.len(),
        sample.neighbours.len(),
        sample.classes.len(),
        sample.questions.len()
    );
    let _ = writeln!(
        out,
        "| Model | Dimensions | Chunks/s | Truncated | Bytes per vector | Chunk R@1 | Chunk R@10 | Chunk MRR | Class R@1 | Class R@10 | Class MRR | Question R@1 | Question R@10 | Question MRR |"
    );
    let _ = writeln!(
        out,
        "|---|---|---|---|---|---|---|---|---|---|---|---|---|---|"
    );
    for model in models {
        let rate = model
            .rate
            .map_or("reused".to_string(), |r| format!("{r:.1}"));
        let _ = writeln!(
            out,
            "| {} | {} | {rate} | {:.1}% | {} | {} | {} | {} |",
            model.name,
            model.dimensions,
            model.truncated * 100.0,
            model.dimensions * 2 + HALFVEC_HEADER_BYTES,
            cell(&model.neighbours),
            cell(&model.classes),
            cell(&model.questions)
        );
    }
    for model in models.iter().filter(|m| !m.missed.is_empty()) {
        let _ = writeln!(
            out,
            "\n## Questions {} does not find in the top {TOP}\n",
            model.name
        );
        for question in &model.missed {
            let _ = writeln!(out, "- {question}");
        }
    }
    out
}

pub async fn run(
    config_dir: Option<&Path>,
    url: Option<&str>,
    options: &Options,
) -> Result<PathBuf> {
    let config = crate::config::load(config_dir)?;
    let names: Vec<String> = if options.models.is_empty() {
        vec![config.models.embed.clone()]
    } else {
        options.models.clone()
    };
    let mut models = Vec::new();
    for name in &names {
        models.push(
            super::model::parse(name, &config.embed.models)
                .map_err(|e| anyhow!("--models: {e}"))?,
        );
    }
    let ontology = super::run::load_ontology(config_dir, &config)?;
    let questions = read_questions(config_dir)?;
    let seed = options
        .seed
        .clone()
        .unwrap_or_else(|| config.llm_extract.seed.clone());
    let size = options.sample.unwrap_or(config.embed.eval_sample);
    let pool = db::pool::connect(url, &config.database.schema, 2).await?;
    let result = async {
        let sample = sample(&pool, &config, &ontology, &questions, size, &seed).await?;
        println!(
            "eval embed: {} chunks, {} chunk queries, {} class queries, {} questions",
            sample.chunks.len(),
            sample.neighbours.len(),
            sample.classes.len(),
            sample.questions.len()
        );
        let mut reports = Vec::new();
        for model in &models {
            println!("eval embed: {}", model.full_name());
            let fetched = model.clone();
            let cache_dir = config.embed.cache_dir.clone();
            let files =
                tokio::task::spawn_blocking(move || super::local::fetch(&fetched, &cache_dir))
                    .await??;
            let local = super::local::Local::new(model.clone(), files, config.embed.threads);
            reports.push(evaluate(&pool, &config, model, Box::new(local), &sample).await?);
        }
        write_report(&config, &sample, &seed, &reports)
    }
    .await;
    pool.close().await;
    result
}

pub fn write_report(
    config: &Config,
    sample: &Sample,
    seed: &str,
    reports: &[ModelReport],
) -> Result<PathBuf> {
    let dir = PathBuf::from(&config.embed.out);
    std::fs::create_dir_all(&dir).with_context(|| format!("cannot create {}", dir.display()))?;
    let path = dir.join(REPORT_FILE);
    std::fs::write(&path, report(sample, seed, reports))
        .with_context(|| format!("cannot write {}", path.display()))?;
    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn set(items: &[usize]) -> HashSet<usize> {
        items.iter().copied().collect()
    }

    #[test]
    fn recall_and_mrr_on_a_hand_made_list() {
        let ranks = [Some(1), Some(3), None, Some(12)];
        let score = score(&ranks);
        assert_eq!(score.queries, 4);
        assert_eq!(score.recall_1, 0.25);
        assert_eq!(score.recall_10, 0.5);
        let mrr = (1.0 + 1.0 / 3.0 + 0.0 + 1.0 / 12.0) / 4.0;
        assert!((score.mrr - mrr).abs() < 1e-12);
        assert_eq!(super::score(&[]), Score::default());
    }

    #[test]
    fn chunks_of_the_same_document_do_not_count() {
        let chunks = vec![
            vec![1.0, 0.0],
            vec![0.99, 0.1],
            vec![0.9, 0.4],
            vec![0.0, 1.0],
        ];
        let contents = [7, 7, 8, 9];
        let queries = vec![Query {
            label: "q".to_string(),
            text: String::new(),
            expected: set(&[1, 2]),
            own: Some(0),
        }];
        let ranks = rank_queries(&queries, &[vec![1.0, 0.0]], &chunks, &contents);
        assert_eq!(ranks, [Some(1)]);
        let queries = vec![Query {
            expected: set(&[1]),
            ..queries[0].clone()
        }];
        let ranks = rank_queries(&queries, &[vec![1.0, 0.0]], &chunks, &contents);
        assert_eq!(ranks, [None]);
    }

    #[test]
    fn a_question_is_found_by_any_chunk_of_an_expected_document() {
        let chunks = vec![vec![1.0, 0.0], vec![0.0, 1.0], vec![0.7, 0.7]];
        let questions = vec![Query {
            label: "q".to_string(),
            text: String::new(),
            expected: set(&[1, 2]),
            own: None,
        }];
        let ranks = rank_queries(&questions, &[vec![1.0, 0.0]], &chunks, &[1, 2, 2]);
        assert_eq!(ranks, [Some(2)]);
    }

    #[test]
    fn an_unknown_path_names_the_question() {
        let questions = vec![Question {
            text: "Which pump was replaced?".to_string(),
            expect: vec!["site/*.txt".to_string(), "missing/**".to_string()],
        }];
        let paths = vec![("site/report.txt".to_string(), 3)];
        let error = match_questions(&questions, &paths).unwrap_err().to_string();
        assert!(error.contains("Which pump was replaced?"), "{error}");
        assert!(error.contains("missing/**"), "{error}");
        let questions = vec![Question {
            expect: vec!["site/*.txt".to_string()],
            ..questions[0].clone()
        }];
        assert_eq!(
            match_questions(&questions, &paths).unwrap(),
            [BTreeSet::from([3])]
        );
    }
}
