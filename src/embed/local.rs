use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow, bail};
use ort::session::builder::GraphOptimizationLevel;
use ort::session::{Session, SessionInputValue};
use ort::value::Tensor;
use sha2::{Digest, Sha256};
use tokenizers::{Tokenizer, TruncationParams};

use super::model::Model;
use super::{Embed, Embedded};
use crate::config::Pooling;
use crate::hash::hex;

const INPUT_IDS: &str = "input_ids";
const ATTENTION_MASK: &str = "attention_mask";
const TOKEN_TYPE_IDS: &str = "token_type_ids";
const POSITION_IDS: &str = "position_ids";
const CACHE_PREFIX: &str = "past_key_values";
const NAMES_SHOWN: usize = 6;
const SNAPSHOTS: &str = "snapshots";
const PROBE: &str = "dimensions";

pub struct Files {
    pub model: PathBuf,
    pub data: Vec<PathBuf>,
    pub tokenizer: PathBuf,
    pub revision: String,
}

pub fn fetch(model: &Model, cache_dir: &str) -> Result<Files> {
    files(model, cache_dir, false)
}

pub fn cached(model: &Model, cache_dir: &str) -> Result<Files> {
    files(model, cache_dir, true)
}

fn files(model: &Model, cache_dir: &str, local_only: bool) -> Result<Files> {
    let settings = &model.settings;
    let wanted = Wanted {
        repo: &settings.repo,
        revision: &settings.revision,
        file: &settings.file,
        data_files: &settings.data_files,
        tokenizer: &settings.tokenizer,
    };
    hub_files(&wanted, cache_dir, local_only)
}

pub struct Wanted<'a> {
    pub repo: &'a str,
    pub revision: &'a str,
    pub file: &'a str,
    pub data_files: &'a [String],
    pub tokenizer: &'a str,
}

pub fn hub_files(wanted: &Wanted, cache_dir: &str, local_only: bool) -> Result<Files> {
    let mut builder = hf_hub::HFClientBuilder::new();
    if !cache_dir.is_empty() {
        builder = builder.cache_dir(cache_dir);
    }
    let client = builder
        .build_sync()
        .map_err(|e| anyhow!("cannot start the Hugging Face client: {e}"))?;
    let (owner, name) = hf_hub::split_id(wanted.repo);
    let repository = client.model(owner, name);
    let revision = (!wanted.revision.is_empty()).then(|| wanted.revision.to_string());
    let get = |file: &str| -> Result<PathBuf> {
        repository
            .download_file()
            .filename(file)
            .maybe_revision(revision.clone())
            .local_files_only(local_only)
            .send()
            .map_err(|e| anyhow!("cannot get {file} of {}: {e}", wanted.repo))
    };
    let model_path = get(wanted.file)?;
    let mut data = Vec::new();
    for file in wanted.data_files {
        data.push(get(file)?);
    }
    let tokenizer = get(wanted.tokenizer)?;
    let revision = snapshot_commit(&model_path)
        .or(revision)
        .with_context(|| format!("cannot tell the commit of {}", model_path.display()))?;
    Ok(Files {
        model: model_path,
        data,
        tokenizer,
        revision,
    })
}

fn snapshot_commit(path: &Path) -> Option<String> {
    let mut parts = path.components().map(|c| c.as_os_str().to_string_lossy());
    parts.find(|part| part == SNAPSHOTS)?;
    parts.next().map(|commit| commit.to_string())
}

pub fn sha256(files: &Files) -> Result<String> {
    let mut hashes = Vec::new();
    for path in std::iter::once(&files.model).chain(&files.data) {
        hashes.push(crate::scan::walk::hash_file(path)?);
    }
    if hashes.len() == 1 {
        return Ok(hashes.remove(0));
    }
    Ok(hex(&Sha256::digest(hashes.join("\n").as_bytes())))
}

#[derive(Debug, Clone, PartialEq)]
enum Input {
    Ids,
    Mask,
    TokenTypes,
    Positions,
    EmptyCache(i64, i64),
}

struct Loaded {
    session: Session,
    tokenizer: Tokenizer,
    inputs: Vec<(String, Input)>,
    output: String,
    pad_id: i64,
}

pub struct Local {
    model: Model,
    files: Files,
    threads: usize,
    loaded: Option<Loaded>,
    dimensions: Option<usize>,
}

impl Local {
    pub fn new(model: Model, files: Files, threads: usize) -> Local {
        Local {
            model,
            files,
            threads,
            loaded: None,
            dimensions: None,
        }
    }

    fn load(&mut self) -> Result<&mut Loaded> {
        if self.loaded.is_none() {
            self.loaded = Some(load(&self.model, &self.files, self.threads)?);
        }
        self.loaded
            .as_mut()
            .ok_or_else(|| anyhow!("the model did not load"))
    }
}

fn load(model: &Model, files: &Files, threads: usize) -> Result<Loaded> {
    let settings = &model.settings;
    let path = files.model.display().to_string();
    let session = Session::builder()
        .map_err(|e| anyhow!("{path}: {e}"))?
        .with_optimization_level(GraphOptimizationLevel::All)
        .map_err(|e| anyhow!("{path}: {e}"))?
        .with_intra_threads(threads)
        .map_err(|e| anyhow!("{path}: {e}"))?
        .commit_from_file(&files.model)
        .map_err(|e| anyhow!("cannot load {path}: {e}"))?;
    let mut inputs = Vec::new();
    for outlet in session.inputs() {
        let kind = match outlet.name() {
            INPUT_IDS => Input::Ids,
            ATTENTION_MASK => Input::Mask,
            TOKEN_TYPE_IDS => Input::TokenTypes,
            POSITION_IDS => Input::Positions,
            name if name.starts_with(CACHE_PREFIX) => {
                match outlet.dtype().tensor_shape().map(|s| s.to_vec()).as_deref() {
                    Some(&[_, heads, _, width]) if heads > 0 && width > 0 => {
                        Input::EmptyCache(heads, width)
                    }
                    _ => bail!(
                        "{}: cache input {name} has no fixed heads and width",
                        model.full_name()
                    ),
                }
            }
            other => {
                let names: Vec<&str> = session.inputs().iter().map(|o| o.name()).collect();
                let shown = names[..names.len().min(NAMES_SHOWN)].join(", ");
                let more = names.len().saturating_sub(NAMES_SHOWN);
                bail!(
                    "{}: input {other} is not supported (inputs: {shown}{}); only {INPUT_IDS}, {ATTENTION_MASK}, {TOKEN_TYPE_IDS}, {POSITION_IDS} and an empty {CACHE_PREFIX} cache are",
                    model.full_name(),
                    if more > 0 {
                        format!(" and {more} more")
                    } else {
                        String::new()
                    }
                )
            }
        };
        inputs.push((outlet.name().to_string(), kind));
    }
    if !inputs.iter().any(|(_, kind)| *kind == Input::Ids) {
        bail!("{}: the model has no {INPUT_IDS} input", model.full_name());
    }
    let outputs: Vec<String> = session
        .outputs()
        .iter()
        .map(|o| o.name().to_string())
        .collect();
    let output = if settings.output.is_empty() {
        outputs
            .first()
            .cloned()
            .with_context(|| format!("{path} has no outputs"))?
    } else if outputs.contains(&settings.output) {
        settings.output.clone()
    } else {
        bail!(
            "{}: output {:?} is not one of {}",
            model.full_name(),
            settings.output,
            outputs.join(", ")
        );
    };
    let tokenizer_path = files.tokenizer.display().to_string();
    let mut tokenizer = Tokenizer::from_file(&files.tokenizer)
        .map_err(|e| anyhow!("cannot load {tokenizer_path}: {e}"))?;
    let pad_id = tokenizer.get_padding().map_or(0, |p| p.pad_id as i64);
    tokenizer
        .with_truncation(Some(TruncationParams {
            max_length: settings.max_tokens,
            ..Default::default()
        }))
        .map_err(|e| anyhow!("{tokenizer_path}: {e}"))?;
    tokenizer.with_padding(None);
    Ok(Loaded {
        session,
        tokenizer,
        inputs,
        output,
        pad_id,
    })
}

impl Embed for Local {
    fn revision(&self) -> &str {
        &self.files.revision
    }

    fn sha256(&mut self) -> Result<String> {
        sha256(&self.files)
    }

    fn dimensions(&mut self) -> Result<usize> {
        if let Some(dimensions) = self.dimensions {
            return Ok(dimensions);
        }
        let probe = self.embed(&[PROBE.to_string()])?;
        let dimensions = probe.vectors.first().map_or(0, Vec::len);
        if dimensions == 0 {
            bail!("{} gives empty vectors", self.model.full_name());
        }
        self.dimensions = Some(dimensions);
        Ok(dimensions)
    }

    fn embed(&mut self, texts: &[String]) -> Result<Embedded> {
        let pooling = self.model.settings.pooling;
        let normalize = self.model.settings.normalize;
        let name = self.model.full_name();
        let loaded = self.load()?;
        let encodings = loaded
            .tokenizer
            .encode_batch(texts.to_vec(), true)
            .map_err(|e| anyhow!("{name}: cannot tokenize: {e}"))?;
        let rows = encodings.len();
        let cols = encodings.iter().map(|e| e.len()).max().unwrap_or(0).max(1);
        let mut ids = vec![loaded.pad_id; rows * cols];
        let mut mask = vec![0i64; rows * cols];
        let mut types = vec![0i64; rows * cols];
        let mut positions = vec![0i64; rows * cols];
        let mut truncated = Vec::with_capacity(rows);
        for (row, encoding) in encodings.iter().enumerate() {
            for (col, (&id, &attend)) in encoding
                .get_ids()
                .iter()
                .zip(encoding.get_attention_mask())
                .enumerate()
            {
                ids[row * cols + col] = id as i64;
                mask[row * cols + col] = attend as i64;
                positions[row * cols + col] = col as i64;
            }
            for (col, &kind) in encoding.get_type_ids().iter().enumerate() {
                types[row * cols + col] = kind as i64;
            }
            truncated.push(!encoding.get_overflowing().is_empty());
        }
        let shape = [rows, cols];
        let mut inputs: Vec<(String, SessionInputValue)> = Vec::new();
        for (input_name, kind) in &loaded.inputs {
            let values = match kind {
                Input::Ids => ids.clone(),
                Input::Mask => mask.clone(),
                Input::TokenTypes => types.clone(),
                Input::Positions => positions.clone(),
                Input::EmptyCache(heads, width) => {
                    let empty = [rows, *heads as usize, 0, *width as usize];
                    let tensor = Tensor::from_array((empty, Vec::<f32>::new()))
                        .map_err(|e| anyhow!("{name}: {input_name}: {e}"))?;
                    inputs.push((input_name.clone(), tensor.into()));
                    continue;
                }
            };
            let tensor = Tensor::from_array((shape, values)).map_err(|e| anyhow!("{name}: {e}"))?;
            inputs.push((input_name.clone(), tensor.into()));
        }
        let output_name = loaded.output.clone();
        let outputs = loaded
            .session
            .run(inputs)
            .map_err(|e| anyhow!("{name}: {e}"))?;
        let (dims, values) = outputs[output_name.as_str()]
            .try_extract_tensor::<f32>()
            .map_err(|e| anyhow!("{name}: output {output_name}: {e}"))?;
        let dims: Vec<usize> = dims.iter().map(|&d| d.max(0) as usize).collect();
        let mut vectors = pool(&dims, values, &mask, pooling)
            .map_err(|e| anyhow!("{name}: output {output_name}: {e}"))?;
        if normalize {
            for vector in &mut vectors {
                normalise(vector);
            }
        }
        Ok(Embedded { vectors, truncated })
    }
}

pub fn pool(
    dims: &[usize],
    values: &[f32],
    mask: &[i64],
    pooling: Pooling,
) -> Result<Vec<Vec<f32>>, String> {
    match (dims, pooling) {
        (&[rows, width], Pooling::None) => Ok(values
            .chunks(width.max(1))
            .take(rows)
            .map(<[f32]>::to_vec)
            .collect()),
        (&[_, _], _) => Err(format!(
            "has shape {dims:?}, already pooled; set pooling = \"none\""
        )),
        (&[_, _, _], Pooling::None) => Err(format!(
            "has shape {dims:?}, one vector per token; set pooling to mean, cls or last"
        )),
        (&[rows, cols, width], pooling) => {
            let mut vectors = Vec::with_capacity(rows);
            for row in 0..rows {
                let mask = &mask[row * cols..(row + 1) * cols];
                let token = |col: usize| {
                    let start = (row * cols + col) * width;
                    &values[start..start + width]
                };
                let used: Vec<usize> = (0..cols).filter(|&col| mask[col] != 0).collect();
                let vector = match pooling {
                    Pooling::Cls => token(0).to_vec(),
                    Pooling::Last => token(used.last().copied().unwrap_or(0)).to_vec(),
                    _ => {
                        let mut sum = vec![0f32; width];
                        for &col in &used {
                            for (total, value) in sum.iter_mut().zip(token(col)) {
                                *total += value;
                            }
                        }
                        let count = used.len().max(1) as f32;
                        sum.iter().map(|total| total / count).collect()
                    }
                };
                vectors.push(vector);
            }
            Ok(vectors)
        }
        _ => Err(format!("has shape {dims:?}, expected 2 or 3 dimensions")),
    }
}

pub fn normalise(vector: &mut [f32]) {
    let length = vector.iter().map(|v| v * v).sum::<f32>().sqrt();
    if length > 0.0 {
        for value in vector {
            *value /= length;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mean_pooling_ignores_padding() {
        let values = [
            1.0, 2.0, 3.0, 4.0, 100.0, 100.0, 5.0, 6.0, 7.0, 8.0, 9.0, 10.0,
        ];
        let mask = [1, 1, 0, 1, 1, 1];
        let vectors = pool(&[2, 3, 2], &values, &mask, Pooling::Mean).unwrap();
        assert_eq!(vectors, [vec![2.0, 3.0], vec![7.0, 8.0]]);
    }

    #[test]
    fn cls_takes_the_first_and_last_the_last_unpadded_token() {
        let values = [
            1.0, 2.0, 3.0, 4.0, 100.0, 100.0, 5.0, 6.0, 7.0, 8.0, 9.0, 10.0,
        ];
        let mask = [1, 1, 0, 1, 1, 1];
        let cls = pool(&[2, 3, 2], &values, &mask, Pooling::Cls).unwrap();
        assert_eq!(cls, [vec![1.0, 2.0], vec![5.0, 6.0]]);
        let last = pool(&[2, 3, 2], &values, &mask, Pooling::Last).unwrap();
        assert_eq!(last, [vec![3.0, 4.0], vec![9.0, 10.0]]);
    }

    #[test]
    fn none_passes_pooled_output_through_and_shapes_are_checked() {
        let values = [1.0, 2.0, 3.0, 4.0];
        let vectors = pool(&[2, 2], &values, &[1, 1], Pooling::None).unwrap();
        assert_eq!(vectors, [vec![1.0, 2.0], vec![3.0, 4.0]]);
        assert!(
            pool(&[2, 2], &values, &[1, 1], Pooling::Mean)
                .unwrap_err()
                .contains("none")
        );
        assert!(
            pool(&[1, 2, 2], &values, &[1, 1], Pooling::None)
                .unwrap_err()
                .contains("mean")
        );
    }

    #[test]
    fn normalised_vectors_have_length_one() {
        let mut vector = vec![3.0, 4.0];
        normalise(&mut vector);
        assert_eq!(vector, [0.6, 0.8]);
        let mut zero = vec![0.0, 0.0];
        normalise(&mut zero);
        assert_eq!(zero, [0.0, 0.0]);
    }

    #[test]
    fn commit_comes_from_the_snapshot_path() {
        let path = Path::new("/cache/models--a--b/snapshots/abc123/onnx/model.onnx");
        assert_eq!(snapshot_commit(path).as_deref(), Some("abc123"));
        assert_eq!(snapshot_commit(Path::new("/tmp/model.onnx")), None);
    }

    #[test]
    fn ort_is_the_version_xberg_uses() {
        let lock = include_str!("../../Cargo.lock");
        let versions: Vec<&str> = lock
            .split("[[package]]")
            .filter(|p| p.contains("\nname = \"ort\"\n"))
            .collect();
        assert_eq!(versions.len(), 1, "more than one ort in Cargo.lock");
        let manifest = include_str!("../../Cargo.toml");
        let pinned = manifest
            .lines()
            .find(|l| l.starts_with("ort = "))
            .and_then(|l| l.split('"').nth(1))
            .unwrap();
        assert!(versions[0].contains(&format!("version = \"{}\"", &pinned[1..])));
    }
}
