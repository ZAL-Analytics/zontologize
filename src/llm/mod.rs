pub mod budget;
pub mod claude_code;
pub mod job;
pub mod ollama;
pub mod openrouter;
pub mod prompt;
pub mod shortlist;
pub mod stage;

use std::fmt;
use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Result, bail};
use sha2::{Digest, Sha256};
use sqlx::types::chrono::{DateTime, Utc};

use crate::config::{Config, ProvidersConfig};

pub const RETRIES: u32 = 3;
pub const METHOD_PREFIX: &str = "llm:";

#[derive(Debug, serde::Serialize)]
pub struct ChatMessage<'a> {
    role: &'static str,
    content: &'a str,
}

pub fn messages(request: &Request) -> [ChatMessage<'_>; 2] {
    [
        ChatMessage {
            role: "system",
            content: &request.system,
        },
        ChatMessage {
            role: "user",
            content: &request.user,
        },
    ]
}
const MAX_RETRY_AFTER: Duration = Duration::from_secs(300);
const JITTER_SHARE: u64 = 4;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProviderKind {
    ClaudeCode,
    OpenRouter,
    Ollama,
}

impl ProviderKind {
    pub const ALL: [ProviderKind; 3] = [
        ProviderKind::ClaudeCode,
        ProviderKind::OpenRouter,
        ProviderKind::Ollama,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            ProviderKind::ClaudeCode => "claude-code",
            ProviderKind::OpenRouter => "openrouter",
            ProviderKind::Ollama => "ollama",
        }
    }

    pub fn parse(name: &str) -> Option<ProviderKind> {
        ProviderKind::ALL
            .into_iter()
            .find(|kind| kind.as_str() == name)
    }

    pub fn expected() -> String {
        let names: Vec<&str> = ProviderKind::ALL.iter().map(|k| k.as_str()).collect();
        match names.split_last() {
            Some((last, [])) => last.to_string(),
            Some((last, rest)) => format!("{} or {last}", rest.join(", ")),
            None => String::new(),
        }
    }

    pub fn concurrency(self, providers: &ProvidersConfig) -> usize {
        match self {
            ProviderKind::ClaudeCode => providers.claude_code.concurrency,
            ProviderKind::OpenRouter => providers.openrouter.concurrency,
            ProviderKind::Ollama => providers.ollama.concurrency,
        }
    }

    pub fn has_windows(self) -> bool {
        self == ProviderKind::ClaudeCode
    }

    fn key_parts(self, providers: &ProvidersConfig) -> Vec<String> {
        match self {
            ProviderKind::ClaudeCode => Vec::new(),
            ProviderKind::OpenRouter => {
                let base_url = providers.openrouter.base_url.trim_end_matches('/');
                if base_url == openrouter::DEFAULT_BASE_URL {
                    return Vec::new();
                }
                vec![format!("base_url={base_url}")]
            }
            ProviderKind::Ollama => {
                if providers.ollama.think == ollama::DEFAULT_THINK {
                    return Vec::new();
                }
                vec![format!("think={}", providers.ollama.think)]
            }
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelName {
    pub provider: ProviderKind,
    pub model: String,
    pub key_parts: Vec<String>,
}

impl ModelName {
    pub fn parse(name: &str) -> Result<ModelName, String> {
        let Some((provider, model)) = name.split_once(':') else {
            return Err(format!("{name:?} must be provider:model"));
        };
        let Some(provider) = ProviderKind::parse(provider) else {
            return Err(format!(
                "unknown provider {provider:?} in {name:?}, expected {}",
                ProviderKind::expected()
            ));
        };
        if model.trim().is_empty() {
            return Err(format!("{name:?} has no model after the provider"));
        }
        Ok(ModelName {
            provider,
            model: model.to_string(),
            key_parts: Vec::new(),
        })
    }

    pub fn configured(name: &str, providers: &ProvidersConfig) -> Result<ModelName, String> {
        let mut model = ModelName::parse(name)?;
        model.key_parts = model.provider.key_parts(providers);
        Ok(model)
    }
}

impl ModelName {
    pub fn method(&self) -> String {
        format!("{METHOD_PREFIX}{self}")
    }
}

impl fmt::Display for ModelName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}:{}", self.provider.as_str(), self.model)
    }
}

#[derive(Debug, Clone)]
pub struct Request {
    pub task: String,
    pub system: String,
    pub user: String,
    pub schema: serde_json::Value,
}

impl Request {
    pub fn key(&self, model: &ModelName) -> String {
        let mut hasher = Sha256::new();
        for part in [
            model.provider.as_str(),
            &model.model,
            &self.system,
            &self.user,
            &self.schema.to_string(),
        ] {
            hasher.update(part.as_bytes());
            hasher.update([0]);
        }
        for part in &model.key_parts {
            hasher.update(part.as_bytes());
            hasher.update([0]);
        }
        crate::hash::hex(&hasher.finalize())
    }

    pub fn stage_key(&self, model: &ModelName, version: u32) -> String {
        crate::hash::stage_hash(&[self.task.as_bytes()], version, self.key(model).as_bytes())
    }

    pub fn chars(&self) -> usize {
        self.system.chars().count()
            + self.user.chars().count()
            + self.schema.to_string().chars().count()
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Usage {
    pub input_tokens: i64,
    pub cached_input_tokens: i64,
    pub output_tokens: i64,
    pub cost_usd: f64,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Window {
    pub used: f64,
    pub resets_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Windows {
    pub five_hour: Option<Window>,
    pub seven_day: Option<Window>,
}

#[derive(Debug, Clone)]
pub struct Reply {
    pub response: String,
    pub usage: Usage,
    pub windows: Windows,
    pub subscription: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Resume {
    At(DateTime<Utc>),
    Funded,
}

#[derive(Debug, Clone, PartialEq)]
pub enum CallError {
    Retry {
        after: Option<Duration>,
        message: String,
    },
    Pause {
        resume: Resume,
        reason: String,
    },
    Invalid {
        message: String,
        usage: Usage,
    },
    Fatal(String),
}

impl CallError {
    pub fn invalid(message: String) -> CallError {
        CallError::Invalid {
            message,
            usage: Usage::default(),
        }
    }
}

impl fmt::Display for CallError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            CallError::Retry { message, .. } => write!(f, "{message}"),
            CallError::Pause { reason, .. } => write!(f, "{reason}"),
            CallError::Invalid { message, .. } => write!(f, "invalid answer: {message}"),
            CallError::Fatal(message) => write!(f, "{message}"),
        }
    }
}

pub enum Provider {
    ClaudeCode(claude_code::ClaudeCode),
    OpenRouter(openrouter::OpenRouter),
    Ollama(ollama::Ollama),
}

impl Provider {
    pub fn new(name: &ModelName, config: &Config, claude_program: PathBuf) -> Result<Provider> {
        let providers = &config.providers;
        Ok(match name.provider {
            ProviderKind::ClaudeCode => Provider::ClaudeCode(claude_code::ClaudeCode::new(
                claude_program,
                &name.model,
                Duration::from_secs(providers.claude_code.timeout_secs),
            )),
            ProviderKind::OpenRouter => {
                let Ok(key) = std::env::var(openrouter::KEY_VARIABLE) else {
                    bail!(
                        "{} is not set; the openrouter provider needs it",
                        openrouter::KEY_VARIABLE
                    );
                };
                Provider::OpenRouter(openrouter::OpenRouter::new(
                    &providers.openrouter.base_url,
                    &key,
                    &name.model,
                    Duration::from_secs(providers.openrouter.timeout_secs),
                )?)
            }
            ProviderKind::Ollama => Provider::Ollama(ollama::Ollama::new(
                &ollama::host(),
                &name.model,
                providers.ollama.think,
                Duration::from_secs(providers.ollama.timeout_secs),
            )?),
        })
    }

    pub async fn send(&self, request: &Request) -> Result<Reply, CallError> {
        match self {
            Provider::ClaudeCode(provider) => provider.send(request).await,
            Provider::OpenRouter(provider) => provider.send(request).await,
            Provider::Ollama(provider) => provider.send(request).await,
        }
    }

    pub async fn prices(&self) -> Result<Option<openrouter::Prices>> {
        Ok(match self {
            Provider::OpenRouter(provider) => Some(provider.prices().await?),
            Provider::Ollama(_) => Some(openrouter::Prices {
                input: 0.0,
                output: 0.0,
            }),
            Provider::ClaudeCode(_) => None,
        })
    }

    pub async fn credit(&self) -> Result<Option<openrouter::Credit>> {
        match self {
            Provider::OpenRouter(provider) => Ok(Some(provider.credit().await?)),
            Provider::ClaudeCode(_) | Provider::Ollama(_) => Ok(None),
        }
    }

    pub fn has_credit(&self) -> bool {
        matches!(self, Provider::OpenRouter(_))
    }
}

pub fn claude_program() -> PathBuf {
    std::env::var_os(claude_code::BIN_VARIABLE)
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(claude_code::DEFAULT_BIN))
}

pub async fn send_with_retries(
    provider: &Provider,
    request: &Request,
    backoff: Duration,
) -> (Result<Reply, CallError>, u32) {
    let mut attempt = 0;
    loop {
        let result = provider.send(request).await;
        match result {
            Err(CallError::Retry { after, .. }) if attempt < RETRIES => {
                let wait = retry_wait(after, backoff, attempt);
                attempt += 1;
                tokio::time::sleep(with_jitter(wait)).await;
            }
            result => return (result, attempt),
        }
    }
}

pub(crate) fn random() -> u64 {
    use std::hash::{BuildHasher, Hasher};
    std::collections::hash_map::RandomState::new()
        .build_hasher()
        .finish()
}

fn retry_wait(after: Option<Duration>, backoff: Duration, attempt: u32) -> Duration {
    after.map_or(backoff * 2u32.pow(attempt), |after| {
        after.min(MAX_RETRY_AFTER)
    })
}

fn with_jitter(wait: Duration) -> Duration {
    let spread = wait.as_millis() as u64 / JITTER_SHARE;
    if spread == 0 {
        return wait;
    }
    wait + Duration::from_millis(random() % spread)
}

pub fn retry_after(value: Option<&str>) -> Option<Duration> {
    value?.trim().parse::<u64>().ok().map(Duration::from_secs)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_model_names() {
        let name = ModelName::parse("ollama:qwen3.5:4b").unwrap();
        assert_eq!(name.provider, ProviderKind::Ollama);
        assert_eq!(name.model, "qwen3.5:4b");
        assert_eq!(name.to_string(), "ollama:qwen3.5:4b");
        let name = ModelName::parse("openrouter:anthropic/claude-haiku-5.5").unwrap();
        assert_eq!(name.provider, ProviderKind::OpenRouter);
        assert!(
            ModelName::parse("haiku")
                .unwrap_err()
                .contains("provider:model")
        );
        assert_eq!(
            ModelName::parse("openai:gpt").unwrap_err(),
            "unknown provider \"openai\" in \"openai:gpt\", expected claude-code, openrouter or ollama"
        );
        for kind in ProviderKind::ALL {
            assert_eq!(ProviderKind::parse(kind.as_str()), Some(kind));
        }
        assert!(ModelName::parse("claude-code:").is_err());
    }

    fn request(user: &str) -> Request {
        Request {
            task: "fit".to_string(),
            system: "s".to_string(),
            user: user.to_string(),
            schema: serde_json::json!({"type": "object"}),
        }
    }

    #[test]
    fn key_changes_with_every_part() {
        let haiku = ModelName::parse("claude-code:haiku").unwrap();
        let sonnet = ModelName::parse("claude-code:sonnet").unwrap();
        let a = request("a");
        assert_eq!(a.key(&haiku), request("a").key(&haiku));
        assert_ne!(a.key(&haiku), request("b").key(&haiku));
        assert_ne!(a.key(&haiku), a.key(&sonnet));
        let mut other = request("a");
        other.schema = serde_json::json!({"type": "array"});
        assert_ne!(a.key(&haiku), other.key(&haiku));
        assert_eq!(a.chars(), 2 + "{\"type\":\"object\"}".len());
    }

    #[test]
    fn retry_after_reads_seconds() {
        assert_eq!(retry_after(Some("7")), Some(Duration::from_secs(7)));
        assert_eq!(retry_after(Some("soon")), None);
        assert_eq!(retry_after(None), None);
    }

    #[test]
    fn retry_after_is_capped_and_jittered() {
        let backoff = Duration::from_secs(2);
        assert_eq!(retry_wait(None, backoff, 2), Duration::from_secs(8));
        assert_eq!(
            retry_wait(Some(Duration::from_secs(5)), backoff, 0),
            Duration::from_secs(5)
        );
        assert_eq!(
            retry_wait(Some(Duration::from_secs(86_400)), backoff, 0),
            MAX_RETRY_AFTER
        );
        let wait = Duration::from_secs(100);
        let waits: Vec<Duration> = (0..20).map(|_| with_jitter(wait)).collect();
        assert!(waits.iter().all(|w| *w >= wait && *w < wait + wait / 4));
        assert!(waits.iter().any(|w| *w != waits[0]));
    }

    #[test]
    fn key_parts_only_when_not_default() {
        let config = crate::config::load(None).unwrap();
        assert_eq!(
            config.providers.openrouter.base_url,
            openrouter::DEFAULT_BASE_URL
        );
        assert_eq!(config.providers.ollama.think, ollama::DEFAULT_THINK);
        let mut providers = config.providers.clone();
        providers.openrouter.base_url = format!("{}/", openrouter::DEFAULT_BASE_URL);
        providers.ollama.think = ollama::DEFAULT_THINK;
        for name in ["claude-code:sonnet", "ollama:qwen3.5:4b", "openrouter:x/y"] {
            let plain = ModelName::parse(name).unwrap();
            let configured = ModelName::configured(name, &providers).unwrap();
            assert_eq!(request("a").key(&plain), request("a").key(&configured));
        }
        providers.ollama.think = !ollama::DEFAULT_THINK;
        providers.openrouter.base_url = "http://localhost:9999/api/v1".to_string();
        for name in ["ollama:qwen3.5:4b", "openrouter:x/y"] {
            let plain = ModelName::parse(name).unwrap();
            let configured = ModelName::configured(name, &providers).unwrap();
            assert_ne!(request("a").key(&plain), request("a").key(&configured));
        }
    }

    #[tokio::test]
    async fn retries_stop_after_three() {
        let script =
            "printf '{\"type\":\"result\",\"is_error\":true,\"result\":\"overloaded\"}\\n'";
        let provider = Provider::ClaudeCode(claude_code::ClaudeCode::with_args(
            PathBuf::from("sh"),
            vec!["-c".to_string(), script.to_string()],
            Duration::from_secs(10),
        ));
        let (result, attempts) =
            send_with_retries(&provider, &request("a"), Duration::from_millis(1)).await;
        assert!(matches!(result, Err(CallError::Retry { .. })), "{result:?}");
        assert_eq!(attempts, RETRIES);
    }
}
