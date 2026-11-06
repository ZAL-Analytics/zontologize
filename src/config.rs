use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use toml::{Table, Value};

const DEFAULTS: &str = include_str!("../defaults/zontologize.toml");
const CONFIG_FILE: &str = "zontologize.toml";
const ENV_PREFIX: &str = "ZONTOLOGIZE_";
const REPLACED_TABLES: [&str; 1] = ["scan.kinds"];
const DEFAULT_CORES_PER_WORKER: usize = 4;

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    pub database: DatabaseConfig,
    pub extract: ExtractConfig,
    pub ocr: OcrConfig,
    pub ontology: OntologyConfig,
    pub scan: ScanConfig,
    pub chunk: ChunkConfig,
    pub models: ModelsConfig,
    pub providers: ProvidersConfig,
    pub budget: BudgetConfig,
    pub llm: LlmConfig,
    pub fit: FitConfig,
    pub llm_extract: LlmExtractConfig,
    pub attributes: AttributesConfig,
    pub classify: ClassifyConfig,
    pub review: ReviewConfig,
    pub embed: EmbedConfig,
    pub resolve: ResolveConfig,
    pub export: ExportConfig,
    #[serde(default)]
    pub sources: Vec<SourceConfig>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ModelsConfig {
    pub fit: String,
    pub extract: String,
    pub classify: String,
    pub attributes: String,
    pub embed: String,
    pub chars_per_token: f64,
}

impl ModelsConfig {
    pub fn llm_tasks(&self) -> [(&'static str, &str); 4] {
        [
            ("fit", &self.fit),
            ("extract", &self.extract),
            ("classify", &self.classify),
            ("attributes", &self.attributes),
        ]
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct EmbedConfig {
    pub threads: usize,
    pub batch_size: usize,
    pub max_tokens: usize,
    pub min_chars: usize,
    pub min_letter_share: f64,
    pub min_ocr_confidence: f64,
    pub max_aliases: usize,
    pub cache_dir: String,
    pub out: String,
    pub eval_sample: usize,
    pub eval_max_expected: usize,
    pub read_batch: i64,
    pub progress_secs: u64,
    pub models: BTreeMap<String, EmbedModelConfig>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ResolveConfig {
    pub strip_words: Vec<String>,
    pub fuzzy_score: f64,
    pub embed_score: f64,
    pub max_candidates: usize,
    pub max_block: usize,
    pub block_chars: usize,
    pub classes: Vec<String>,
    pub neighbour_batch: usize,
    pub similar: SimilarConfig,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SimilarConfig {
    pub property: String,
    pub tolerance: f64,
    pub min_shared: usize,
    pub min_score: f64,
    pub max_links: usize,
    pub weights: BTreeMap<String, f64>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct AttributesConfig {
    pub confidence: f64,
    pub max_text_chars: usize,
    pub max_properties: usize,
    pub max_subjects: usize,
    pub context_chars: usize,
    pub out: String,
    pub read_batch: i64,
    pub unit_symbols: usize,
    pub sources: Vec<String>,
    pub doc_types: Vec<String>,
    pub paths: Vec<String>,
    pub seed: String,
    #[serde(default)]
    pub resend_share: Option<f64>,
    pub threads: usize,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct EmbedModelConfig {
    pub repo: String,
    #[serde(default)]
    pub revision: String,
    #[serde(default = "default_model_file")]
    pub file: String,
    #[serde(default)]
    pub data_files: Vec<String>,
    #[serde(default = "default_tokenizer_file")]
    pub tokenizer: String,
    pub pooling: Pooling,
    #[serde(default)]
    pub output: String,
    #[serde(default = "default_true")]
    pub normalize: bool,
    #[serde(default)]
    pub passage_prefix: String,
    #[serde(default)]
    pub query_prefix: String,
    #[serde(default)]
    pub max_tokens: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Pooling {
    Mean,
    Cls,
    Last,
    None,
}

impl Pooling {
    pub fn as_str(self) -> &'static str {
        match self {
            Pooling::Mean => "mean",
            Pooling::Cls => "cls",
            Pooling::Last => "last",
            Pooling::None => "none",
        }
    }
}

fn default_model_file() -> String {
    "onnx/model.onnx".to_string()
}

fn default_tokenizer_file() -> String {
    "tokenizer.json".to_string()
}

fn default_true() -> bool {
    true
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ProvidersConfig {
    #[serde(rename = "claude-code")]
    pub claude_code: ClaudeCodeConfig,
    pub openrouter: OpenRouterConfig,
    pub ollama: OllamaConfig,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ClaudeCodeConfig {
    pub concurrency: usize,
    pub timeout_secs: u64,
    pub max_window_use: f64,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct OpenRouterConfig {
    pub base_url: String,
    pub concurrency: usize,
    pub timeout_secs: u64,
    pub poll_minutes: u64,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct OllamaConfig {
    pub concurrency: usize,
    pub timeout_secs: u64,
    pub think: bool,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct BudgetConfig {
    pub max_usd: f64,
    pub max_requests: u64,
    pub calibration_calls: i64,
    pub default_output_tokens: u64,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct LlmConfig {
    pub retry_backoff_secs: u64,
    pub wait_margin_secs: u64,
    pub unknown_reset_wait_minutes: u64,
    pub progress_secs: u64,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ExportConfig {
    pub ontology_iri: String,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct FitConfig {
    pub sample: usize,
    pub seed: String,
    pub unmatched_share: f64,
    pub min_chars: usize,
    pub max_shortlist: usize,
    pub min_evidence: usize,
    pub proposal_namespace: String,
    pub out: String,
    pub definition_chars: usize,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct LlmExtractConfig {
    pub min_chars: usize,
    pub sources: Vec<String>,
    pub doc_types: Vec<String>,
    pub paths: Vec<String>,
    pub exclude: Vec<String>,
    pub seed: String,
    pub max_per_document: usize,
    pub unmatched_share: f64,
    pub min_letter_share: f64,
    pub min_ocr_confidence: f64,
    pub max_shortlist: usize,
    pub confidence: f64,
    pub resend_share: f64,
    pub min_evidence: usize,
    pub out: String,
    pub read_batch: i64,
    pub definition_chars: usize,
    pub max_known: usize,
    pub threads: usize,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ClassifyConfig {
    pub head_chars: usize,
    pub min_chars: usize,
    pub confidence: f64,
    pub read_batch: usize,
    pub definition_chars: usize,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ReviewConfig {
    pub sample: usize,
    pub min_verdicts: usize,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct DatabaseConfig {
    pub schema: String,
    pub graph: String,
    pub api_schema: String,
    pub max_connections: u32,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct OntologyConfig {
    pub files: Vec<String>,
    pub languages: Vec<String>,
    pub include: Vec<String>,
    pub exclude: Vec<String>,
    pub document_class: String,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ScanConfig {
    pub batch_size: usize,
    pub hash_threads: usize,
    pub extract_batch: i64,
    pub chunk_batch: i64,
    pub rules_batch: i64,
    pub rules_threads: usize,
    pub ignore: Vec<String>,
    pub kinds: BTreeMap<String, KindConfig>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct KindConfig {
    pub extensions: Vec<String>,
    pub max_mb: u64,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ChunkConfig {
    pub max_chars: usize,
    pub overlap: usize,
    pub text_search: String,
    pub threads: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum SourceKind {
    Folder,
    Table,
}

impl SourceKind {
    pub fn as_str(self) -> &'static str {
        match self {
            SourceKind::Folder => "folder",
            SourceKind::Table => "table",
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SourceConfig {
    pub name: String,
    #[serde(default = "default_source_kind")]
    pub kind: SourceKind,
    pub root: String,
    #[serde(default)]
    pub view: String,
    #[serde(default)]
    pub dataset: String,
    #[serde(default)]
    pub ignore: Vec<String>,
}

fn default_source_kind() -> SourceKind {
    SourceKind::Folder
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ExtractConfig {
    pub threads: usize,
    pub timeout_secs: u64,
    pub format: TextFormat,
    pub embedded_depth: usize,
    pub max_embedded_mb: u64,
    pub ocr_embedded_images: bool,
    pub workers: usize,
    pub worker_memory_mb: u64,
    pub worker_files: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum TextFormat {
    Markdown,
    Plain,
}

impl TextFormat {
    pub fn as_str(self) -> &'static str {
        match self {
            TextFormat::Markdown => "markdown",
            TextFormat::Plain => "plain",
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct OcrConfig {
    pub languages: Vec<String>,
    pub model_tier: ModelTier,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, clap::ValueEnum, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum ModelTier {
    Small,
    Medium,
}

struct Layer {
    source: String,
    table: Table,
}

pub fn load(config_dir: Option<&Path>) -> Result<Config> {
    load_from(config_dir, std::env::vars())
}

fn load_from(
    config_dir: Option<&Path>,
    vars: impl IntoIterator<Item = (String, String)>,
) -> Result<Config> {
    let mut merged: Table = DEFAULTS
        .parse()
        .context("built-in defaults/zontologize.toml is not valid TOML")?;
    parse("built-in defaults", &merged)?;
    let mut sources = Vec::new();
    if let Some(layer) = file_layer(config_dir)? {
        apply(&mut merged, layer, &mut sources)?;
    }
    for (name, raw) in env_vars(vars, &merged)? {
        let layer = env_layer(&name, &raw, &merged)?;
        apply(&mut merged, layer, &mut sources)?;
    }
    let source = if sources.is_empty() {
        "built-in defaults".to_string()
    } else {
        sources.join(", ")
    };
    let mut config = parse(&source, &merged)?;
    if let Err(message) = check(&config) {
        bail!("{source}: {message}");
    }
    let cores = std::thread::available_parallelism().map_or(1, |n| n.get());
    if config.extract.threads == 0 {
        config.extract.threads = cores;
    }
    if config.extract.workers == 0 {
        config.extract.workers = (cores / DEFAULT_CORES_PER_WORKER).max(1);
    }
    if config.extract.worker_memory_mb == 0 {
        config.extract.worker_memory_mb =
            crate::scan::memory::default_limit_mb() / config.extract.workers as u64;
    }
    if config.scan.hash_threads == 0 {
        config.scan.hash_threads = cores;
    }
    borrow(&mut config);
    if config.embed.threads == 0 {
        config.embed.threads = cores;
    }
    for model in config.embed.models.values_mut() {
        if model.max_tokens == 0 {
            model.max_tokens = config.embed.max_tokens;
        }
    }
    Ok(config)
}

fn borrow(config: &mut Config) {
    let threads = config.scan.hash_threads;
    for value in [
        &mut config.scan.rules_threads,
        &mut config.chunk.threads,
        &mut config.llm_extract.threads,
        &mut config.attributes.threads,
    ] {
        if *value == 0 {
            *value = threads;
        }
    }
    let extract = &mut config.llm_extract;
    if extract.max_known == 0 {
        extract.max_known = extract.max_shortlist;
    }
    let attributes = &mut config.attributes;
    for (own, borrowed) in [
        (&mut attributes.sources, &extract.sources),
        (&mut attributes.doc_types, &extract.doc_types),
        (&mut attributes.paths, &extract.paths),
    ] {
        if own.is_empty() {
            own.clone_from(borrowed);
        }
    }
    if attributes.seed.is_empty() {
        attributes.seed.clone_from(&extract.seed);
    }
}

pub fn hash(config: &Config) -> Result<String> {
    let json = serde_json::to_vec(config).context("cannot serialize the config")?;
    Ok(crate::hash::hex(&<sha2::Sha256 as sha2::Digest>::digest(
        &json,
    )))
}

fn file_layer(config_dir: Option<&Path>) -> Result<Option<Layer>> {
    let Some(dir) = config_dir else {
        return Ok(None);
    };
    if !dir.is_dir() {
        bail!("config directory {} does not exist", dir.display());
    }
    let path = dir.join(CONFIG_FILE);
    if !path.exists() {
        return Ok(None);
    }
    let text = std::fs::read_to_string(&path)
        .with_context(|| format!("cannot read {}", path.display()))?;
    let table = text
        .parse()
        .with_context(|| format!("{} is not valid TOML", path.display()))?;
    Ok(Some(Layer {
        source: path.display().to_string(),
        table,
    }))
}

fn apply(merged: &mut Table, layer: Layer, sources: &mut Vec<String>) -> Result<()> {
    merge(merged, layer.table, "");
    parse(&layer.source, merged)?;
    sources.push(layer.source);
    Ok(())
}

fn env_vars(
    vars: impl IntoIterator<Item = (String, String)>,
    defaults: &Table,
) -> Result<Vec<(String, String)>> {
    let mut env = Vec::new();
    for (name, raw) in vars {
        let Some(rest) = name.strip_prefix(ENV_PREFIX) else {
            continue;
        };
        if rest.contains("__") {
            env.push((name, raw));
            continue;
        }
        let rest = rest.to_lowercase();
        let section = defaults
            .keys()
            .find(|section| rest == **section || rest.starts_with(&format!("{section}_")));
        if let Some(section) = section {
            bail!(
                "{name}: expected the form ZONTOLOGIZE_{}__<KEY>, with two underscores",
                section.to_uppercase()
            );
        }
    }
    env.sort();
    Ok(env)
}

fn env_layer(name: &str, raw: &str, merged: &Table) -> Result<Layer> {
    let parts: Vec<String> = name[ENV_PREFIX.len()..]
        .split("__")
        .map(str::to_lowercase)
        .collect();
    if parts.iter().any(String::is_empty) {
        bail!("{name}: expected the form ZONTOLOGIZE_<SECTION>__<KEY>");
    }
    let mut value = env_value(raw, existing(merged, &parts));
    for part in parts.iter().rev() {
        let mut table = Table::new();
        table.insert(part.clone(), value);
        value = Value::Table(table);
    }
    let Value::Table(table) = value else {
        bail!("{name}: expected the form ZONTOLOGIZE_<SECTION>__<KEY>");
    };
    Ok(Layer {
        source: name.to_string(),
        table,
    })
}

fn existing<'a>(merged: &'a Table, parts: &[String]) -> Option<&'a Value> {
    let (last, path) = parts.split_last()?;
    let mut table = merged;
    for part in path {
        table = table.get(part)?.as_table()?;
    }
    table.get(last)
}

fn env_value(raw: &str, existing: Option<&Value>) -> Value {
    if matches!(existing, Some(Value::String(_))) {
        return Value::String(raw.to_string());
    }
    format!("value = {raw}")
        .parse::<Table>()
        .ok()
        .and_then(|mut table| table.remove("value"))
        .unwrap_or_else(|| Value::String(raw.to_string()))
}

fn merge(base: &mut Table, overlay: Table, path: &str) {
    for (key, value) in overlay {
        let child = if path.is_empty() {
            key.clone()
        } else {
            format!("{path}.{key}")
        };
        match (base.get_mut(&key), value) {
            (Some(Value::Table(base_table)), Value::Table(overlay_table))
                if !REPLACED_TABLES.contains(&child.as_str()) =>
            {
                merge(base_table, overlay_table, &child)
            }
            (_, value) => {
                base.insert(key, value);
            }
        }
    }
}

fn parse(source: &str, merged: &Table) -> Result<Config> {
    merged
        .clone()
        .try_into()
        .map_err(|error| anyhow::anyhow!("{source}: {}", error.to_string().trim_end()))
}

fn check(config: &Config) -> Result<(), String> {
    let database = &config.database;
    let names = [
        ("schema", &database.schema),
        ("graph", &database.graph),
        ("api_schema", &database.api_schema),
    ];
    for (key, name) in names {
        if !is_identifier(name) {
            return Err(format!(
                "database.{key} {name:?} must match ^[a-z_][a-z0-9_]*$"
            ));
        }
    }
    if database.graph == database.schema
        || database.api_schema == database.schema
        || database.api_schema == database.graph
    {
        return Err(
            "database.schema, database.graph and database.api_schema must differ".to_string(),
        );
    }
    if config.extract.timeout_secs == 0 {
        return Err("extract.timeout_secs must be greater than 0".to_string());
    }
    if config.ocr.languages.is_empty() {
        return Err("ocr.languages must list at least one language".to_string());
    }
    if config.database.max_connections == 0 {
        return Err("database.max_connections must be greater than 0".to_string());
    }
    if config.ontology.languages.is_empty() {
        return Err("ontology.languages must list at least one language".to_string());
    }
    if config.scan.batch_size == 0 {
        return Err("scan.batch_size must be greater than 0".to_string());
    }
    if config.chunk.max_chars == 0 || config.chunk.overlap >= config.chunk.max_chars {
        return Err(
            "chunk.max_chars must be greater than 0 and greater than chunk.overlap".to_string(),
        );
    }
    if !is_identifier(&config.chunk.text_search) {
        return Err(format!(
            "chunk.text_search {:?} must match ^[a-z_][a-z0-9_]*$",
            config.chunk.text_search
        ));
    }
    check_models(config)?;
    check_globs("scan.ignore", &config.scan.ignore)?;
    let mut extensions = BTreeMap::new();
    for (kind, settings) in &config.scan.kinds {
        for extension in &settings.extensions {
            let extension = extension.trim_start_matches('.').to_lowercase();
            if let Some(other) = extensions.insert(extension.clone(), kind) {
                return Err(format!(
                    "scan.kinds: extension {extension:?} is in both {other} and {kind}"
                ));
            }
        }
    }
    let mut names = BTreeSet::new();
    for source in &config.sources {
        if !is_identifier(&source.name) {
            return Err(format!(
                "sources.name {:?} must match ^[a-z_][a-z0-9_]*$",
                source.name
            ));
        }
        if !names.insert(&source.name) {
            return Err(format!("sources: name {:?} is used twice", source.name));
        }
        check_globs(&format!("sources.{}.ignore", source.name), &source.ignore)?;
        match source.kind {
            SourceKind::Folder if !source.view.is_empty() => {
                return Err(format!(
                    "sources.{}: view is only allowed with kind = \"table\"",
                    source.name
                ));
            }
            SourceKind::Table if !is_qualified_identifier(&source.view) => {
                return Err(format!(
                    "sources.{}.view {:?} must be schema.view, each part matching ^[a-z_][a-z0-9_]*$",
                    source.name, source.view
                ));
            }
            _ => {}
        }
    }
    Ok(())
}

fn check_models(config: &Config) -> Result<(), String> {
    for (key, name) in config.models.llm_tasks() {
        if !name.is_empty() {
            crate::llm::ModelName::parse(name).map_err(|error| format!("models.{key}: {error}"))?;
        }
    }
    check_embed(config)?;
    if config.models.chars_per_token <= 0.0 {
        return Err("models.chars_per_token must be greater than 0".to_string());
    }
    let providers = &config.providers;
    for kind in crate::llm::ProviderKind::ALL {
        if kind.concurrency(providers) == 0 {
            return Err(format!(
                "providers.{}.concurrency must be greater than 0",
                kind.as_str()
            ));
        }
    }
    let use_limit = providers.claude_code.max_window_use;
    if !(use_limit > 0.0 && use_limit <= 1.0) {
        return Err(
            "providers.claude-code.max_window_use must be greater than 0 and at most 1".to_string(),
        );
    }
    if !(0.0..=1.0).contains(&config.fit.unmatched_share) {
        return Err("fit.unmatched_share must be between 0 and 1".to_string());
    }
    if config.fit.sample == 0 || config.fit.max_shortlist == 0 {
        return Err("fit.sample and fit.max_shortlist must be greater than 0".to_string());
    }
    if config.fit.proposal_namespace.is_empty() {
        return Err("fit.proposal_namespace must be an IRI namespace".to_string());
    }
    let extract = &config.llm_extract;
    let attributes = &config.attributes;
    for (key, names) in [
        ("llm_extract", &extract.sources),
        ("attributes", &attributes.sources),
    ] {
        for name in names {
            if !config.sources.iter().any(|s| &s.name == name) {
                return Err(format!(
                    "{key}.sources: {name:?} is not the name of a [[sources]] entry"
                ));
            }
        }
    }
    check_globs("llm_extract.paths", &extract.paths)?;
    check_globs("attributes.paths", &attributes.paths)?;
    check_sizes(config)?;
    if extract.max_shortlist == 0 || extract.max_per_document == 0 {
        return Err(
            "llm_extract.max_shortlist and llm_extract.max_per_document must be greater than 0"
                .to_string(),
        );
    }
    let shares = [
        ("llm_extract.confidence", extract.confidence),
        ("llm_extract.resend_share", extract.resend_share),
        ("llm_extract.unmatched_share", extract.unmatched_share),
        ("llm_extract.min_letter_share", extract.min_letter_share),
        ("llm_extract.min_ocr_confidence", extract.min_ocr_confidence),
        ("classify.confidence", config.classify.confidence),
        ("attributes.confidence", config.attributes.confidence),
        (
            "attributes.resend_share",
            attributes.resend_share.unwrap_or(extract.resend_share),
        ),
        (
            "resolve.similar.min_score",
            config.resolve.similar.min_score,
        ),
    ];
    for (key, value) in shares {
        if !(0.0..=1.0).contains(&value) {
            return Err(format!("{key} must be between 0 and 1"));
        }
    }
    if attributes.max_text_chars == 0
        || attributes.max_properties == 0
        || attributes.max_subjects == 0
    {
        return Err(
            "attributes.max_text_chars, attributes.max_properties and attributes.max_subjects \
             must be greater than 0"
                .to_string(),
        );
    }
    let similar = &config.resolve.similar;
    if similar.property.is_empty() {
        return Err("resolve.similar.property must be an IRI".to_string());
    }
    if similar.tolerance <= 0.0 || !similar.tolerance.is_finite() {
        return Err("resolve.similar.tolerance must be greater than 0".to_string());
    }
    if similar.min_shared == 0 || similar.max_links == 0 {
        return Err(
            "resolve.similar.min_shared and resolve.similar.max_links must be greater than 0"
                .to_string(),
        );
    }
    if let Some((name, _)) = similar
        .weights
        .iter()
        .find(|(_, w)| !(**w > 0.0 && w.is_finite()))
    {
        return Err(format!(
            "resolve.similar.weights.{name} must be greater than 0"
        ));
    }
    if config.classify.head_chars == 0 {
        return Err("classify.head_chars must be greater than 0".to_string());
    }
    if config.budget.max_usd < 0.0 || config.budget.calibration_calls <= 0 {
        return Err(
            "budget.max_usd must be 0 or more and budget.calibration_calls greater than 0"
                .to_string(),
        );
    }
    Ok(())
}

fn check_sizes(config: &Config) -> Result<(), String> {
    let sizes = [
        ("scan.extract_batch", config.scan.extract_batch),
        ("scan.chunk_batch", config.scan.chunk_batch),
        ("scan.rules_batch", config.scan.rules_batch),
        ("llm_extract.read_batch", config.llm_extract.read_batch),
        ("attributes.read_batch", config.attributes.read_batch),
        ("classify.read_batch", config.classify.read_batch as i64),
        ("embed.read_batch", config.embed.read_batch),
        (
            "resolve.neighbour_batch",
            config.resolve.neighbour_batch as i64,
        ),
        (
            "attributes.unit_symbols",
            config.attributes.unit_symbols as i64,
        ),
        ("llm.progress_secs", config.llm.progress_secs as i64),
        ("embed.progress_secs", config.embed.progress_secs as i64),
    ];
    for (key, value) in sizes {
        if value <= 0 {
            return Err(format!("{key} must be greater than 0"));
        }
    }
    if config.export.ontology_iri.trim().is_empty() {
        return Err("export.ontology_iri must be an IRI".to_string());
    }
    Ok(())
}

fn check_embed(config: &Config) -> Result<(), String> {
    crate::embed::model::parse(&config.models.embed, &config.embed.models)
        .map_err(|error| format!("models.embed: {error}"))?;
    let embed = &config.embed;
    if embed.batch_size == 0 || embed.max_tokens == 0 {
        return Err("embed.batch_size and embed.max_tokens must be greater than 0".to_string());
    }
    for (key, value) in [
        ("embed.min_letter_share", embed.min_letter_share),
        ("embed.min_ocr_confidence", embed.min_ocr_confidence),
        ("resolve.fuzzy_score", config.resolve.fuzzy_score),
        ("resolve.embed_score", config.resolve.embed_score),
    ] {
        if !(0.0..=1.0).contains(&value) {
            return Err(format!("{key} must be between 0 and 1"));
        }
    }
    for (name, model) in &embed.models {
        if model.repo.trim().is_empty() {
            return Err(format!(
                "embed.models.{name}.repo must name a Hugging Face repository"
            ));
        }
        if model.file.trim().is_empty() || model.tokenizer.trim().is_empty() {
            return Err(format!(
                "embed.models.{name}: file and tokenizer must not be empty"
            ));
        }
    }
    Ok(())
}

fn check_globs(key: &str, globs: &[String]) -> Result<(), String> {
    for glob in globs {
        if let Err(error) = globset::Glob::new(glob) {
            return Err(format!("{key}: {glob:?} is not a valid glob: {error}"));
        }
    }
    Ok(())
}

fn is_qualified_identifier(name: &str) -> bool {
    match name.split_once('.') {
        Some((schema, view)) => is_identifier(schema) && is_identifier(view),
        None => false,
    }
}

pub fn is_identifier(name: &str) -> bool {
    let mut chars = name.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    (first.is_ascii_lowercase() || first == '_')
        && chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dir_with(contents: &str) -> tempdir::Dir {
        let dir = tempdir::Dir::new();
        std::fs::write(dir.path().join(CONFIG_FILE), contents).unwrap();
        dir
    }

    fn vars(list: &[(&str, &str)]) -> Vec<(String, String)> {
        list.iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    fn error(result: Result<Config>) -> String {
        format!("{:#}", result.unwrap_err())
    }

    #[test]
    fn stage_settings_borrow_until_set() {
        let dir = dir_with(
            "[[sources]]\nname = \"a\"\nroot = \"/a\"\n\n[[sources]]\nname = \"b\"\nroot = \"/b\"\n\n\
             [scan]\nhash_threads = 3\n\n[llm_extract]\nsources = [\"a\"]\npaths = [\"x/**\"]\n\
             seed = \"s\"\nresend_share = 0.4\nmax_shortlist = 7\n",
        );
        let config = load_from(Some(dir.path()), vars(&[])).unwrap();
        assert_eq!(config.attributes.sources, ["a"]);
        assert_eq!(config.attributes.paths, ["x/**"]);
        assert_eq!(config.attributes.seed, "s");
        assert_eq!(config.attributes.resend_share, None);
        assert_eq!(config.llm_extract.max_known, 7);
        assert_eq!(config.llm_extract.threads, 3);
        assert_eq!(config.attributes.threads, 3);
        assert_eq!(config.chunk.threads, 3);
        assert_eq!(config.scan.rules_threads, 3);
        let env = vars(&[
            ("ZONTOLOGIZE_ATTRIBUTES__SOURCES", "[\"b\"]"),
            ("ZONTOLOGIZE_ATTRIBUTES__SEED", "t"),
            ("ZONTOLOGIZE_ATTRIBUTES__RESEND_SHARE", "0.1"),
            ("ZONTOLOGIZE_ATTRIBUTES__THREADS", "2"),
            ("ZONTOLOGIZE_LLM_EXTRACT__MAX_KNOWN", "4"),
        ]);
        let config = load_from(Some(dir.path()), env).unwrap();
        assert_eq!(config.attributes.sources, ["b"]);
        assert_eq!(config.attributes.seed, "t");
        assert_eq!(config.attributes.resend_share, Some(0.1));
        assert_eq!(config.attributes.threads, 2);
        assert_eq!(config.llm_extract.max_known, 4);
        let env = vars(&[("ZONTOLOGIZE_ATTRIBUTES__SOURCES", "[\"c\"]")]);
        let message = error(load_from(Some(dir.path()), env));
        assert!(message.contains("attributes.sources"), "{message}");
    }

    #[test]
    fn config_hash_is_stable_and_follows_values() {
        let config = load_from(None, vars(&[])).unwrap();
        let again = load_from(None, vars(&[])).unwrap();
        assert_eq!(hash(&config).unwrap(), hash(&again).unwrap());
        let env = vars(&[("ZONTOLOGIZE_CHUNK__MAX_CHARS", "999")]);
        let other = load_from(None, env).unwrap();
        assert_ne!(hash(&config).unwrap(), hash(&other).unwrap());
    }

    #[test]
    fn defaults_load() {
        let config = load_from(None, vars(&[])).unwrap();
        assert_eq!(config.database.schema, "zontologize");
        assert_eq!(config.ocr.model_tier, ModelTier::Small);
        assert!(config.extract.threads >= 1);
    }

    #[test]
    fn graph_and_api_schema_are_checked() {
        let config = load_from(None, vars(&[])).unwrap();
        assert_eq!(config.database.graph, "zontologize_graph");
        assert_eq!(config.database.api_schema, "zontologize_api");
        let dir = dir_with("[database]\ngraph = \"Graph\"\n");
        let message = error(load_from(Some(dir.path()), vars(&[])));
        assert!(message.contains("database.graph"), "{message}");
        let dir = dir_with("[database]\ngraph = \"zontologize\"\n");
        let message = error(load_from(Some(dir.path()), vars(&[])));
        assert!(message.contains("must differ"), "{message}");
    }

    #[test]
    fn workers_and_their_memory_default_from_the_machine() {
        let config = load_from(None, vars(&[])).unwrap();
        assert!(config.extract.workers >= 1);
        assert!(config.extract.worker_memory_mb >= 1);
        let env = vars(&[("ZONTOLOGIZE_EXTRACT__WORKERS", "3")]);
        let config = load_from(None, env).unwrap();
        assert_eq!(config.extract.workers, 3);
    }

    #[test]
    fn model_names_and_provider_keys_are_checked() {
        let dir = dir_with("[models]\nfit = \"openai:gpt\"\n");
        let message = error(load_from(Some(dir.path()), vars(&[])));
        assert!(message.contains("models.fit"), "{message}");
        assert!(message.contains("unknown provider"), "{message}");
        let dir = dir_with("[providers.openrouter]\napi_key = \"x\"\n");
        let message = error(load_from(Some(dir.path()), vars(&[])));
        assert!(message.contains("api_key"), "{message}");
        let dir = dir_with("[providers.claude-code]\nmax_window_use = 1.5\n");
        let message = error(load_from(Some(dir.path()), vars(&[])));
        assert!(message.contains("max_window_use"), "{message}");
        let dir =
            dir_with("[models]\nfit = \"ollama:qwen3.5:4b\"\n[providers.ollama]\nthink = true\n");
        let config = load_from(Some(dir.path()), vars(&[])).unwrap();
        assert_eq!(config.models.fit, "ollama:qwen3.5:4b");
        assert!(config.providers.ollama.think);
        assert_eq!(config.providers.ollama.concurrency, 1);
    }

    #[test]
    fn extract_and_classify_settings_are_checked() {
        let config = load_from(None, vars(&[])).unwrap();
        assert_eq!(config.models.extract, "claude-code:sonnet");
        assert_eq!(config.models.classify, "claude-code:haiku");
        assert_eq!(config.llm_extract.min_chars, 200);
        assert_eq!(config.llm_extract.out, "extract");
        assert_eq!(config.classify.head_chars, 1500);
        assert_eq!(config.models.attributes, "claude-code:sonnet");
        assert_eq!(config.attributes.context_chars, 400);
        assert_eq!(config.resolve.similar.property, "urn:zontologize#similarTo");
        assert_eq!(config.resolve.similar.min_shared, 2);
        let dir = dir_with(
            "[resolve.similar.weights]
\"acme:speed\" = 0\n",
        );
        let message = error(load_from(Some(dir.path()), vars(&[])));
        assert!(
            message.contains("resolve.similar.weights.acme:speed"),
            "{message}"
        );
        let dir = dir_with("[resolve.similar]\nweights = { \"acme:speed\" = 2.0 }\n");
        let config = load_from(Some(dir.path()), vars(&[])).unwrap();
        assert_eq!(config.resolve.similar.weights["acme:speed"], 2.0);
        assert_eq!(config.resolve.similar.max_links, 5);
        let dir = dir_with("[attributes]\nconfidence = 2.0\n");
        let message = error(load_from(Some(dir.path()), vars(&[])));
        assert!(message.contains("attributes.confidence"), "{message}");
        let dir = dir_with("[models]\nclassify = \"local:x\"\n");
        let message = error(load_from(Some(dir.path()), vars(&[])));
        assert!(message.contains("models.classify"), "{message}");
        let dir = dir_with(
            "[llm_extract]\nsources = [\"other\"]\n\
             [[sources]]\nname = \"docs\"\nroot = \"/tmp\"\n",
        );
        let message = error(load_from(Some(dir.path()), vars(&[])));
        assert!(message.contains("llm_extract.sources"), "{message}");
        assert!(message.contains("other"), "{message}");
        let dir = dir_with("[llm_extract]\nconfidence = 1.5\n");
        let message = error(load_from(Some(dir.path()), vars(&[])));
        assert!(message.contains("llm_extract.confidence"), "{message}");
    }

    #[test]
    fn embed_models_are_checked_and_merge_key_by_key() {
        let config = load_from(None, vars(&[])).unwrap();
        assert_eq!(config.models.embed, "local:arctic-embed-m-v2.0");
        assert_eq!(config.embed.models.len(), 5);
        assert!(config.embed.threads >= 1);
        let e5 = &config.embed.models["multilingual-e5-base"];
        assert_eq!(e5.pooling, Pooling::Mean);
        assert_eq!(e5.passage_prefix, "passage: ");
        let dir = dir_with("[models]\nembed = \"ollama:nomic\"\n");
        let message = error(load_from(Some(dir.path()), vars(&[])));
        assert!(
            message.contains("models.embed") && message.contains("locally only"),
            "{message}"
        );
        let dir = dir_with("[models]\nembed = \"local:missing\"\n");
        let message = error(load_from(Some(dir.path()), vars(&[])));
        assert!(message.contains("missing"), "{message}");
        let dir = dir_with("[embed.models.mine]\nrepo = \"\"\npooling = \"mean\"\n");
        let message = error(load_from(Some(dir.path()), vars(&[])));
        assert!(message.contains("embed.models.mine.repo"), "{message}");
        let dir = dir_with("[embed.models.mine]\nrepo = \"a/b\"\npooling = \"max\"\n");
        let message = error(load_from(Some(dir.path()), vars(&[])));
        assert!(
            message.contains("pooling") && message.contains("mean"),
            "{message}"
        );
        let dir = dir_with(
            "[models]\nembed = \"local:mine\"\n[embed]\nmax_tokens = 256\n\
             [embed.models.mine]\nrepo = \"a/b\"\npooling = \"cls\"\n\
             [embed.models.multilingual-e5-small]\nmax_tokens = 128\n",
        );
        let config = load_from(Some(dir.path()), vars(&[])).unwrap();
        let mine = &config.embed.models["mine"];
        assert_eq!(mine.file, "onnx/model.onnx");
        assert_eq!(mine.tokenizer, "tokenizer.json");
        assert!(mine.normalize);
        assert_eq!(mine.max_tokens, 256);
        let small = &config.embed.models["multilingual-e5-small"];
        assert_eq!(small.max_tokens, 128);
        assert_eq!(small.passage_prefix, "passage: ");
        assert_eq!(small.repo, "intfloat/multilingual-e5-small");
    }

    #[test]
    fn every_default_section_is_documented() {
        let docs = include_str!("../docs/configuration.md");
        let rules = include_str!("../defaults/rules.toml");
        let mut missing = Vec::new();
        for text in [DEFAULTS, rules] {
            let table: Table = text.parse().unwrap();
            for section in table.keys() {
                let dotted = format!("`{section}.");
                let header = format!("[{section}");
                if !docs.contains(&dotted) && !docs.contains(&header) {
                    missing.push(section.clone());
                }
            }
        }
        assert!(
            missing.is_empty(),
            "not in docs/configuration.md: {missing:?}"
        );
    }

    #[test]
    fn removed_memory_key_names_the_replacement() {
        let dir = dir_with("[extract]\nmax_memory_mb = 100\n");
        let error = format!("{:#}", load_from(Some(dir.path()), vars(&[])).unwrap_err());
        assert!(error.contains("max_memory_mb"), "{error}");
        assert!(error.contains("worker_memory_mb"), "{error}");
    }

    #[test]
    fn file_overrides_defaults_key_by_key() {
        let dir = dir_with("[database]\nschema = \"acme\"\n[extract]\nthreads = 3\n");
        let config = load_from(Some(dir.path()), vars(&[])).unwrap();
        assert_eq!(config.database.schema, "acme");
        assert_eq!(config.extract.threads, 3);
        assert_eq!(config.extract.timeout_secs, 300);
    }

    #[test]
    fn env_overrides_file() {
        let dir = dir_with("[extract]\nthreads = 3\n");
        let env = vars(&[
            ("ZONTOLOGIZE_EXTRACT__THREADS", "5"),
            ("ZONTOLOGIZE_OCR__MODEL_TIER", "medium"),
            ("ZONTOLOGIZE_OCR__LANGUAGES", "[\"nld\", \"eng\"]"),
            ("ZONTOLOGIZE_CONFIG", "ignored"),
        ]);
        let config = load_from(Some(dir.path()), env).unwrap();
        assert_eq!(config.extract.threads, 5);
        assert_eq!(config.ocr.model_tier, ModelTier::Medium);
        assert_eq!(config.ocr.languages, ["nld", "eng"]);
    }

    #[test]
    fn layers_are_checked_together_after_merging() {
        let env = vars(&[
            ("ZONTOLOGIZE_CHUNK__MAX_CHARS", "150"),
            ("ZONTOLOGIZE_CHUNK__OVERLAP", "50"),
        ]);
        let config = load_from(None, env).unwrap();
        assert_eq!((config.chunk.max_chars, config.chunk.overlap), (150, 50));
    }

    #[test]
    fn env_keeps_strings_as_text_and_sets_nested_keys() {
        let env = vars(&[
            ("ZONTOLOGIZE_FIT__SEED", "42"),
            ("ZONTOLOGIZE_PROVIDERS__OLLAMA__THINK", "true"),
        ]);
        let config = load_from(None, env).unwrap();
        assert_eq!(config.fit.seed, "42");
        assert!(config.providers.ollama.think);
    }

    #[test]
    fn env_var_with_one_underscore_is_an_error() {
        let env = vars(&[("ZONTOLOGIZE_EXTRACT_THREADS", "5")]);
        let message = error(load_from(None, env));
        assert!(message.contains("ZONTOLOGIZE_EXTRACT_THREADS"), "{message}");
        assert!(message.contains("ZONTOLOGIZE_EXTRACT__"), "{message}");
        let env = vars(&[("ZONTOLOGIZE_TEST_DATABASE_URL", "x")]);
        assert!(load_from(None, env).is_ok());
    }

    #[test]
    fn threads_zero_means_all_cores() {
        let env = vars(&[("ZONTOLOGIZE_EXTRACT__THREADS", "0")]);
        let config = load_from(None, env).unwrap();
        let cores = std::thread::available_parallelism().unwrap().get();
        assert_eq!(config.extract.threads, cores);
    }

    #[test]
    fn unknown_key_names_file_and_key() {
        let dir = dir_with("[database]\nschema = \"acme\"\npassword = \"x\"\n");
        let message = error(load_from(Some(dir.path()), vars(&[])));
        assert!(message.contains(CONFIG_FILE), "{message}");
        assert!(message.contains("password"), "{message}");
    }

    #[test]
    fn secret_looking_top_level_key_is_rejected() {
        let dir = dir_with("api_key = \"x\"\n");
        let message = error(load_from(Some(dir.path()), vars(&[])));
        assert!(message.contains(CONFIG_FILE), "{message}");
        assert!(message.contains("api_key"), "{message}");
    }

    #[test]
    fn invalid_schema_from_env_names_variable() {
        let env = vars(&[("ZONTOLOGIZE_DATABASE__SCHEMA", "bad-name")]);
        let message = error(load_from(None, env));
        assert!(
            message.contains("ZONTOLOGIZE_DATABASE__SCHEMA"),
            "{message}"
        );
        assert!(message.contains("^[a-z_][a-z0-9_]*$"), "{message}");
    }

    #[test]
    fn invalid_model_tier_names_file_and_key() {
        let dir = dir_with("[ocr]\nmodel_tier = \"huge\"\n");
        let message = error(load_from(Some(dir.path()), vars(&[])));
        assert!(message.contains(CONFIG_FILE), "{message}");
        assert!(message.contains("ocr.model_tier"), "{message}");
        assert!(message.contains("small"), "{message}");
    }

    #[test]
    fn ocr_backend_key_is_rejected() {
        let dir = dir_with("[ocr]\nbackend = \"paddleocr\"\n");
        let message = error(load_from(Some(dir.path()), vars(&[])));
        assert!(message.contains("backend"), "{message}");
    }

    #[test]
    fn wrong_type_names_key() {
        let env = vars(&[("ZONTOLOGIZE_EXTRACT__THREADS", "many")]);
        let message = error(load_from(None, env));
        assert!(
            message.contains("ZONTOLOGIZE_EXTRACT__THREADS"),
            "{message}"
        );
        assert!(message.contains("extract.threads"), "{message}");
    }

    #[test]
    fn missing_config_dir_is_an_error() {
        let message = error(load_from(
            Some(Path::new("/nonexistent/zontologize-config")),
            vars(&[]),
        ));
        assert!(
            message.contains("/nonexistent/zontologize-config"),
            "{message}"
        );
    }

    #[test]
    fn dir_without_file_uses_defaults() {
        let dir = tempdir::Dir::new();
        let config = load_from(Some(dir.path()), vars(&[])).unwrap();
        assert_eq!(config.database.schema, "zontologize");
    }

    #[test]
    fn identifier_check() {
        assert!(is_identifier("zontologize"));
        assert!(is_identifier("_a1"));
        assert!(!is_identifier(""));
        assert!(!is_identifier("1a"));
        assert!(!is_identifier("bad-name"));
        assert!(!is_identifier("Upper"));
    }

    #[test]
    fn llm_tasks_cover_every_model_task() {
        let models = load(None).unwrap().models;
        let tasks: Vec<&str> = models.llm_tasks().iter().map(|(task, _)| *task).collect();
        assert_eq!(tasks, ["fit", "extract", "classify", "attributes"]);
    }

    mod tempdir {
        use std::path::{Path, PathBuf};
        use std::sync::atomic::{AtomicUsize, Ordering};

        static COUNTER: AtomicUsize = AtomicUsize::new(0);

        pub struct Dir(PathBuf);

        impl Dir {
            pub fn new() -> Self {
                let n = COUNTER.fetch_add(1, Ordering::SeqCst);
                let path = std::env::temp_dir()
                    .join(format!("zontologize-config-{}-{n}", std::process::id()));
                std::fs::create_dir_all(&path).unwrap();
                Dir(path)
            }

            pub fn path(&self) -> &Path {
                &self.0
            }
        }

        impl Drop for Dir {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.0);
            }
        }
    }
}
