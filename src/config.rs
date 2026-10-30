use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use anyhow::{Context, Result, bail};
use serde::Deserialize;
use toml::{Table, Value};

const DEFAULTS: &str = include_str!("../defaults/zontologize.toml");
const CONFIG_FILE: &str = "zontologize.toml";
const ENV_PREFIX: &str = "ZONTOLOGIZE_";
const REPLACED_TABLES: [&str; 1] = ["scan.kinds"];

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    pub database: DatabaseConfig,
    pub extract: ExtractConfig,
    pub ocr: OcrConfig,
    pub ontology: OntologyConfig,
    pub scan: ScanConfig,
    pub chunk: ChunkConfig,
    #[serde(default)]
    pub sources: Vec<SourceConfig>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DatabaseConfig {
    pub schema: String,
    pub max_connections: u32,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OntologyConfig {
    pub files: Vec<String>,
    pub languages: Vec<String>,
    pub include: Vec<String>,
    pub exclude: Vec<String>,
    pub document_class: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ScanConfig {
    pub batch_size: usize,
    pub hash_threads: usize,
    pub ignore: Vec<String>,
    pub kinds: BTreeMap<String, KindConfig>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct KindConfig {
    pub extensions: Vec<String>,
    pub max_mb: u64,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ChunkConfig {
    pub max_chars: usize,
    pub overlap: usize,
    pub text_search: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
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

#[derive(Debug, Clone, Deserialize)]
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

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExtractConfig {
    pub threads: usize,
    pub timeout_secs: u64,
    pub format: TextFormat,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
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

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OcrConfig {
    pub backend: OcrBackend,
    pub languages: Vec<String>,
    pub model_tier: ModelTier,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, clap::ValueEnum)]
#[serde(rename_all = "lowercase")]
pub enum OcrBackend {
    Paddleocr,
    Tesseract,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, clap::ValueEnum)]
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
    let mut config = build("built-in defaults", &merged)?;
    for layer in layers(config_dir, vars)? {
        merge(&mut merged, layer.table, "");
        config = build(&layer.source, &merged)?;
    }
    let cores = std::thread::available_parallelism().map_or(1, |n| n.get());
    if config.extract.threads == 0 {
        config.extract.threads = cores;
    }
    if config.scan.hash_threads == 0 {
        config.scan.hash_threads = cores;
    }
    Ok(config)
}

fn layers(
    config_dir: Option<&Path>,
    vars: impl IntoIterator<Item = (String, String)>,
) -> Result<Vec<Layer>> {
    let mut layers = Vec::new();
    if let Some(dir) = config_dir {
        if !dir.is_dir() {
            bail!("config directory {} does not exist", dir.display());
        }
        let path = dir.join(CONFIG_FILE);
        if path.exists() {
            let text = std::fs::read_to_string(&path)
                .with_context(|| format!("cannot read {}", path.display()))?;
            let table = text
                .parse()
                .with_context(|| format!("{} is not valid TOML", path.display()))?;
            layers.push(Layer {
                source: path.display().to_string(),
                table,
            });
        }
    }
    let mut env: Vec<(String, String)> = vars
        .into_iter()
        .filter(|(name, _)| name.starts_with(ENV_PREFIX) && name.contains("__"))
        .collect();
    env.sort();
    for (name, raw) in env {
        layers.push(env_layer(&name, &raw)?);
    }
    Ok(layers)
}

fn env_layer(name: &str, raw: &str) -> Result<Layer> {
    let rest = &name[ENV_PREFIX.len()..];
    let Some((section, key)) = rest.split_once("__") else {
        bail!("{name}: expected the form ZONTOLOGIZE_<SECTION>__<KEY>");
    };
    if section.is_empty() || key.is_empty() || key.contains("__") {
        bail!("{name}: expected the form ZONTOLOGIZE_<SECTION>__<KEY>");
    }
    let mut inner = Table::new();
    inner.insert(key.to_lowercase(), env_value(raw));
    let mut table = Table::new();
    table.insert(section.to_lowercase(), Value::Table(inner));
    Ok(Layer {
        source: name.to_string(),
        table,
    })
}

fn env_value(raw: &str) -> Value {
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

fn build(source: &str, merged: &Table) -> Result<Config> {
    let config: Config = merged
        .clone()
        .try_into()
        .map_err(|error| anyhow::anyhow!("{source}: {}", error.to_string().trim_end()))?;
    if let Err(message) = check(&config) {
        bail!("{source}: {message}");
    }
    Ok(config)
}

fn check(config: &Config) -> Result<(), String> {
    if !is_identifier(&config.database.schema) {
        return Err(format!(
            "database.schema {:?} must match ^[a-z_][a-z0-9_]*$",
            config.database.schema
        ));
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

fn check_globs(key: &str, globs: &[String]) -> Result<(), String> {
    for glob in globs {
        if let Err(error) = globset::Glob::new(glob) {
            return Err(format!("{key}: {glob:?} is not a valid glob: {error}"));
        }
    }
    Ok(())
}

pub fn is_qualified_identifier(name: &str) -> bool {
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
    fn defaults_load() {
        let config = load_from(None, vars(&[])).unwrap();
        assert_eq!(config.database.schema, "zontologize");
        assert_eq!(config.ocr.backend, OcrBackend::Paddleocr);
        assert!(config.extract.threads >= 1);
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
            ("ZONTOLOGIZE_OCR__BACKEND", "tesseract"),
            ("ZONTOLOGIZE_OCR__LANGUAGES", "[\"nld\", \"eng\"]"),
            ("ZONTOLOGIZE_CONFIG", "ignored"),
        ]);
        let config = load_from(Some(dir.path()), env).unwrap();
        assert_eq!(config.extract.threads, 5);
        assert_eq!(config.ocr.backend, OcrBackend::Tesseract);
        assert_eq!(config.ocr.languages, ["nld", "eng"]);
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
    fn invalid_backend_names_file_and_key() {
        let dir = dir_with("[ocr]\nbackend = \"easyocr\"\n");
        let message = error(load_from(Some(dir.path()), vars(&[])));
        assert!(message.contains(CONFIG_FILE), "{message}");
        assert!(message.contains("ocr.backend"), "{message}");
        assert!(message.contains("paddleocr"), "{message}");
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
