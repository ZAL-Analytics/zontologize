use std::time::Duration;

use anyhow::{Context, Result};
use serde::Deserialize;
use sqlx::types::chrono::{DateTime, Utc};

use super::{CallError, Reply, Request, Resume, Usage, Windows, retry_after};

pub const KEY_VARIABLE: &str = "OPENROUTER_API_KEY";
const SCHEMA_NAME: &str = "answer";

pub struct OpenRouter {
    client: reqwest::Client,
    base_url: String,
    key: String,
    model: String,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Prices {
    pub input: f64,
    pub output: f64,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Credit {
    pub remaining_usd: Option<f64>,
    pub resets_at: Option<DateTime<Utc>>,
}

#[derive(Deserialize)]
struct ChatResponse {
    choices: Vec<Choice>,
    usage: Option<ChatUsage>,
}

#[derive(Deserialize)]
struct Choice {
    message: Message,
}

#[derive(Deserialize)]
struct Message {
    content: Option<String>,
}

#[derive(Deserialize)]
struct ChatUsage {
    #[serde(default)]
    prompt_tokens: i64,
    #[serde(default)]
    completion_tokens: i64,
    prompt_tokens_details: Option<PromptDetails>,
    cost: Option<f64>,
}

#[derive(Deserialize)]
struct PromptDetails {
    #[serde(default)]
    cached_tokens: i64,
}

#[derive(Deserialize)]
struct ErrorBody {
    error: ErrorDetail,
}

#[derive(Deserialize)]
struct ErrorDetail {
    message: String,
    metadata: Option<ErrorMetadata>,
}

#[derive(Deserialize)]
struct ErrorMetadata {
    limit_source: Option<String>,
    reason: Option<String>,
    resets_at: Option<DateTime<Utc>>,
}

#[derive(Deserialize)]
struct ModelList {
    data: Vec<ModelEntry>,
}

#[derive(Deserialize)]
struct ModelEntry {
    id: String,
    pricing: Pricing,
}

#[derive(Deserialize)]
struct Pricing {
    prompt: String,
    completion: String,
}

#[derive(Deserialize)]
struct KeyResponse {
    data: KeyData,
}

#[derive(Deserialize)]
struct KeyData {
    limit_remaining: Option<f64>,
    effective_budget: Option<Budget>,
}

#[derive(Deserialize)]
struct Budget {
    remaining_usd: f64,
    resets_at: Option<DateTime<Utc>>,
}

impl OpenRouter {
    pub fn new(base_url: &str, key: &str, model: &str, timeout: Duration) -> Result<OpenRouter> {
        let client = reqwest::Client::builder()
            .timeout(timeout)
            .build()
            .context("cannot build the HTTP client")?;
        Ok(OpenRouter {
            client,
            base_url: base_url.trim_end_matches('/').to_string(),
            key: key.to_string(),
            model: model.to_string(),
        })
    }

    pub fn body(&self, request: &Request) -> serde_json::Value {
        serde_json::json!({
            "model": self.model,
            "messages": [
                {"role": "system", "content": request.system},
                {"role": "user", "content": request.user},
            ],
            "response_format": {
                "type": "json_schema",
                "json_schema": {"name": SCHEMA_NAME, "strict": true, "schema": request.schema},
            },
            "provider": {"require_parameters": true},
        })
    }

    pub async fn send(&self, request: &Request) -> Result<Reply, CallError> {
        let response = self
            .client
            .post(format!("{}/chat/completions", self.base_url))
            .bearer_auth(&self.key)
            .json(&self.body(request))
            .send()
            .await
            .map_err(|error| CallError::Retry {
                after: None,
                message: format!("openrouter: {}", without_url(&error)),
            })?;
        let status = response.status().as_u16();
        let after = retry_after(
            response
                .headers()
                .get("retry-after")
                .and_then(|v| v.to_str().ok()),
        );
        let text = response.text().await.map_err(|error| CallError::Retry {
            after: None,
            message: format!("openrouter: {}", without_url(&error)),
        })?;
        if status != 200 {
            return Err(error_for(status, &text, after));
        }
        let chat: ChatResponse = serde_json::from_str(&text)
            .map_err(|error| CallError::Invalid(format!("openrouter response: {error}")))?;
        let Some(content) = chat
            .choices
            .into_iter()
            .next()
            .and_then(|c| c.message.content)
        else {
            return Err(CallError::Invalid(
                "openrouter returned no content".to_string(),
            ));
        };
        if serde_json::from_str::<serde::de::IgnoredAny>(&content).is_err() {
            return Err(CallError::Invalid(format!(
                "openrouter content is not JSON: {}",
                content.chars().take(200).collect::<String>()
            )));
        }
        let usage = chat.usage;
        Ok(Reply {
            response: content,
            usage: Usage {
                input_tokens: usage.as_ref().map_or(0, |u| u.prompt_tokens),
                cached_input_tokens: usage
                    .as_ref()
                    .and_then(|u| u.prompt_tokens_details.as_ref())
                    .map_or(0, |d| d.cached_tokens),
                output_tokens: usage.as_ref().map_or(0, |u| u.completion_tokens),
                cost_usd: usage.and_then(|u| u.cost).unwrap_or(0.0),
            },
            windows: Windows::default(),
            subscription: false,
        })
    }

    pub async fn prices(&self) -> Result<Prices> {
        let text = self.get("models").await?;
        let list: ModelList =
            serde_json::from_str(&text).context("openrouter /models is not the expected JSON")?;
        let Some(entry) = list.data.into_iter().find(|m| m.id == self.model) else {
            anyhow::bail!("openrouter has no model {}", self.model);
        };
        let input: f64 = entry.pricing.prompt.parse().unwrap_or(0.0);
        let output: f64 = entry.pricing.completion.parse().unwrap_or(0.0);
        Ok(Prices { input, output })
    }

    pub async fn credit(&self) -> Result<Credit> {
        let text = self.get("key").await?;
        let key: KeyResponse =
            serde_json::from_str(&text).context("openrouter /key is not the expected JSON")?;
        Ok(credit_of(key.data))
    }

    async fn get(&self, path: &str) -> Result<String> {
        let response = self
            .client
            .get(format!("{}/{path}", self.base_url))
            .bearer_auth(&self.key)
            .send()
            .await
            .map_err(|error| anyhow::anyhow!("openrouter /{path}: {}", without_url(&error)))?;
        let status = response.status();
        let text = response.text().await?;
        if !status.is_success() {
            anyhow::bail!("openrouter /{path}: HTTP {status}: {}", message_of(&text));
        }
        Ok(text)
    }
}

fn credit_of(data: KeyData) -> Credit {
    let mut remaining = data.limit_remaining;
    let mut resets_at = None;
    if let Some(budget) = data.effective_budget
        && remaining.is_none_or(|r| budget.remaining_usd < r)
    {
        remaining = Some(budget.remaining_usd);
        resets_at = budget.resets_at;
    }
    Credit {
        remaining_usd: remaining,
        resets_at,
    }
}

fn without_url(error: &reqwest::Error) -> String {
    let mut text = error.to_string();
    if let Some(url) = error.url() {
        text = text.replace(url.as_str(), "");
    }
    text
}

fn message_of(text: &str) -> String {
    serde_json::from_str::<ErrorBody>(text)
        .map(|body| body.error.message)
        .unwrap_or_else(|_| text.chars().take(200).collect())
}

pub fn error_for(status: u16, text: &str, after: Option<Duration>) -> CallError {
    let body = serde_json::from_str::<ErrorBody>(text).ok();
    let message = body.as_ref().map_or_else(
        || text.chars().take(200).collect(),
        |b| b.error.message.clone(),
    );
    let metadata = body.and_then(|b| b.error.metadata);
    let source = metadata
        .as_ref()
        .and_then(|m| m.limit_source.clone())
        .unwrap_or_default();
    let reason = metadata
        .as_ref()
        .and_then(|m| m.reason.clone())
        .unwrap_or_default();
    let resets_at = metadata.and_then(|m| m.resets_at);
    let message = format!("openrouter HTTP {status}: {message}");
    match status {
        402 if reason == "in_flight_budget_exhausted" => CallError::Retry { after, message },
        402 => CallError::Pause {
            resume: Resume::Funded,
            reason: message,
        },
        403 if !source.is_empty() => CallError::Pause {
            resume: resets_at.map_or(Resume::Funded, Resume::At),
            reason: message,
        },
        408 | 429 | 500..=599 => CallError::Retry { after, message },
        _ => CallError::Fatal(message),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn errors_map_to_retry_pause_or_fatal() {
        let key_limit = r#"{"error":{"code":402,"message":"Key limit exceeded","metadata":{"limit_source":"openrouter_key_limit"}}}"#;
        assert!(matches!(
            error_for(402, key_limit, None),
            CallError::Pause {
                resume: Resume::Funded,
                ..
            }
        ));
        let in_flight = r#"{"error":{"code":402,"message":"wait","metadata":{"reason":"in_flight_budget_exhausted","limit_source":"openrouter_in_flight_budget"}}}"#;
        assert!(matches!(
            error_for(402, in_flight, Some(Duration::from_secs(3))),
            CallError::Retry { after: Some(_), .. }
        ));
        let budget = r#"{"error":{"code":403,"message":"budget","metadata":{"limit_source":"openrouter_guardrail_budget","resets_at":"2030-01-06T00:00:00.000Z"}}}"#;
        let CallError::Pause {
            resume: Resume::At(at),
            ..
        } = error_for(403, budget, None)
        else {
            panic!("budget should pause until its reset");
        };
        assert_eq!(at.to_rfc3339(), "2030-01-06T00:00:00+00:00");
        assert!(matches!(
            error_for(429, "{}", None),
            CallError::Retry { .. }
        ));
        assert!(matches!(
            error_for(503, "bad gateway", None),
            CallError::Retry { .. }
        ));
        assert!(matches!(
            error_for(401, r#"{"error":{"message":"No auth"}}"#, None),
            CallError::Fatal(_)
        ));
    }

    #[test]
    fn credit_takes_the_tighter_limit() {
        let data: KeyResponse = serde_json::from_str(
            r#"{"data":{"limit":50,"limit_remaining":41.5,"limit_reset":null,
            "effective_budget":{"remaining_usd":10,"resets_at":"2030-01-06T00:00:00Z"}}}"#,
        )
        .unwrap();
        let credit = credit_of(data.data);
        assert_eq!(credit.remaining_usd, Some(10.0));
        assert!(credit.resets_at.is_some());
        let data: KeyResponse =
            serde_json::from_str(r#"{"data":{"limit_remaining":null}}"#).unwrap();
        assert_eq!(credit_of(data.data).remaining_usd, None);
    }

    #[test]
    fn body_asks_for_a_strict_schema() {
        let provider = OpenRouter::new(
            "http://localhost/api/v1/",
            "k",
            "acme/model",
            Duration::from_secs(1),
        )
        .unwrap();
        let request = Request {
            task: "fit".to_string(),
            system: "s".to_string(),
            user: "u".to_string(),
            schema: serde_json::json!({"type": "object"}),
        };
        let body = provider.body(&request);
        assert_eq!(body["response_format"]["json_schema"]["strict"], true);
        assert_eq!(body["provider"]["require_parameters"], true);
        assert_eq!(body["messages"][1]["content"], "u");
        assert_eq!(provider.base_url, "http://localhost/api/v1");
    }
}
