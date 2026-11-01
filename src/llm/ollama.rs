use std::time::Duration;

use anyhow::{Context, Result};
use serde::Deserialize;

use super::{CallError, Reply, Request, Usage, Windows};

pub const HOST_VARIABLE: &str = "OLLAMA_HOST";
const DEFAULT_HOST: &str = "http://localhost:11434";

pub struct Ollama {
    client: reqwest::Client,
    host: String,
    model: String,
    think: bool,
}

#[derive(Deserialize)]
struct ChatResponse {
    message: Option<Message>,
    #[serde(default)]
    prompt_eval_count: i64,
    #[serde(default)]
    eval_count: i64,
}

#[derive(Deserialize)]
struct Message {
    content: String,
}

#[derive(Deserialize)]
struct ErrorBody {
    error: String,
}

#[derive(Deserialize)]
struct Tags {
    models: Vec<Tag>,
}

#[derive(Deserialize)]
struct Tag {
    name: String,
}

pub fn host() -> String {
    let host = std::env::var(HOST_VARIABLE).unwrap_or_default();
    normalise_host(&host)
}

fn normalise_host(host: &str) -> String {
    let host = host.trim().trim_end_matches('/');
    if host.is_empty() {
        return DEFAULT_HOST.to_string();
    }
    if host.starts_with("http://") || host.starts_with("https://") {
        return host.to_string();
    }
    format!("http://{host}")
}

impl Ollama {
    pub fn new(host: &str, model: &str, think: bool, timeout: Duration) -> Result<Ollama> {
        let client = reqwest::Client::builder()
            .timeout(timeout)
            .build()
            .context("cannot build the HTTP client")?;
        Ok(Ollama {
            client,
            host: normalise_host(host),
            model: model.to_string(),
            think,
        })
    }

    pub fn body(&self, request: &Request) -> serde_json::Value {
        serde_json::json!({
            "model": self.model,
            "messages": [
                {"role": "system", "content": request.system},
                {"role": "user", "content": request.user},
            ],
            "stream": false,
            "format": request.schema,
            "think": self.think,
        })
    }

    pub async fn send(&self, request: &Request) -> Result<Reply, CallError> {
        let response = self
            .client
            .post(format!("{}/api/chat", self.host))
            .json(&self.body(request))
            .send()
            .await
            .map_err(|error| CallError::Retry {
                after: None,
                message: format!("ollama at {}: {error}", self.host),
            })?;
        let status = response.status().as_u16();
        let text = response.text().await.map_err(|error| CallError::Retry {
            after: None,
            message: format!("ollama: {error}"),
        })?;
        if status != 200 {
            let message = serde_json::from_str::<ErrorBody>(&text)
                .map(|b| b.error)
                .unwrap_or_else(|_| text.chars().take(200).collect());
            let message = format!("ollama HTTP {status}: {message}");
            return Err(match status {
                404 | 400 => CallError::Fatal(message),
                _ => CallError::Retry {
                    after: None,
                    message,
                },
            });
        }
        let chat: ChatResponse = serde_json::from_str(&text)
            .map_err(|error| CallError::Invalid(format!("ollama response: {error}")))?;
        let content = chat.message.map(|m| m.content).unwrap_or_default();
        if serde_json::from_str::<serde::de::IgnoredAny>(&content).is_err() {
            return Err(CallError::Invalid(format!(
                "ollama content is not JSON: {}",
                content.chars().take(200).collect::<String>()
            )));
        }
        Ok(Reply {
            response: content,
            usage: Usage {
                input_tokens: chat.prompt_eval_count,
                cached_input_tokens: 0,
                output_tokens: chat.eval_count,
                cost_usd: 0.0,
            },
            windows: Windows::default(),
            subscription: false,
        })
    }

    pub async fn has_model(&self) -> Result<bool> {
        let text = self
            .client
            .get(format!("{}/api/tags", self.host))
            .send()
            .await
            .with_context(|| format!("ollama at {} does not answer", self.host))?
            .text()
            .await?;
        let tags: Tags = serde_json::from_str(&text).context("ollama /api/tags is not JSON")?;
        let wanted = with_tag(&self.model);
        Ok(tags.models.iter().any(|t| with_tag(&t.name) == wanted))
    }
}

fn with_tag(name: &str) -> String {
    if name.contains(':') {
        name.to_string()
    } else {
        format!("{name}:latest")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn host_gets_a_scheme_and_a_default() {
        assert_eq!(normalise_host(""), DEFAULT_HOST);
        assert_eq!(normalise_host("box:11434/"), "http://box:11434");
        assert_eq!(normalise_host("https://box"), "https://box");
    }

    #[test]
    fn model_without_tag_means_latest() {
        assert_eq!(with_tag("qwen3.5"), "qwen3.5:latest");
        assert_eq!(with_tag("qwen3.5:4b"), "qwen3.5:4b");
    }

    #[test]
    fn body_sends_the_schema_as_format() {
        let provider = Ollama::new("", "m", false, Duration::from_secs(1)).unwrap();
        let request = Request {
            task: "fit".to_string(),
            system: "s".to_string(),
            user: "u".to_string(),
            schema: serde_json::json!({"type": "object"}),
        };
        let body = provider.body(&request);
        assert_eq!(body["format"]["type"], "object");
        assert_eq!(body["stream"], false);
        assert_eq!(body["think"], false);
    }
}
