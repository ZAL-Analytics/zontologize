use std::path::PathBuf;
use std::process::Stdio;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use serde::Deserialize;
use sqlx::types::chrono::{DateTime, Utc};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::process::Command;

use super::{CallError, Reply, Request, Resume, Usage, Window, Windows};

pub const BIN_VARIABLE: &str = "CLAUDE_CODE_BIN";
pub const DEFAULT_BIN: &str = "claude";
const SUBSCRIPTION_KEY_SOURCE: &str = "none";
const ALLOWED: &str = "allowed";
const STDERR_TAIL: usize = 500;

static COUNTER: AtomicUsize = AtomicUsize::new(0);

pub struct ClaudeCode {
    program: PathBuf,
    args: Option<Vec<String>>,
    model: String,
    timeout: Duration,
}

impl ClaudeCode {
    pub fn new(program: PathBuf, model: &str, timeout: Duration) -> ClaudeCode {
        ClaudeCode {
            program,
            args: None,
            model: model.to_string(),
            timeout,
        }
    }

    pub fn with_args(program: PathBuf, args: Vec<String>, timeout: Duration) -> ClaudeCode {
        ClaudeCode {
            program,
            args: Some(args),
            model: String::new(),
            timeout,
        }
    }

    pub fn args(&self, request: &Request) -> Vec<String> {
        if let Some(args) = &self.args {
            return args.clone();
        }
        [
            "-p",
            "--model",
            &self.model,
            "--output-format",
            "stream-json",
            "--verbose",
            "--tools",
            "",
            "--no-session-persistence",
            "--setting-sources",
            "",
            "--strict-mcp-config",
            "--system-prompt",
            &request.system,
            "--json-schema",
            &request.schema.to_string(),
        ]
        .iter()
        .map(|arg| arg.to_string())
        .collect()
    }

    pub async fn send(&self, request: &Request) -> Result<Reply, CallError> {
        let dir = WorkDir::new().map_err(|error| CallError::Fatal(error.to_string()))?;
        let mut child = Command::new(&self.program)
            .args(self.args(request))
            .current_dir(&dir.path)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .map_err(|error| {
                CallError::Fatal(format!(
                    "cannot run {}: {error}; install Claude Code or set {BIN_VARIABLE}",
                    self.program.display()
                ))
            })?;
        let mut stdin = child.stdin.take();
        let mut stdout = child.stdout.take();
        let mut stderr = child.stderr.take();
        let input = request.user.clone();
        let run = async {
            if let Some(mut stdin) = stdin.take() {
                let _ = stdin.write_all(input.as_bytes()).await;
                let _ = stdin.shutdown().await;
            }
            let mut out = String::new();
            let mut err = String::new();
            if let Some(stdout) = stdout.as_mut() {
                let _ = stdout.read_to_string(&mut out).await;
            }
            if let Some(stderr) = stderr.as_mut() {
                let _ = stderr.read_to_string(&mut err).await;
            }
            let status = child.wait().await;
            (out, err, status)
        };
        let Ok((out, err, status)) = tokio::time::timeout(self.timeout, run).await else {
            return Err(CallError::Retry {
                after: None,
                message: format!("claude timed out after {} s", self.timeout.as_secs()),
            });
        };
        let parsed = parse(&out);
        if parsed.result.is_none() {
            let code = status.map(|s| s.to_string()).unwrap_or_default();
            return Err(CallError::Retry {
                after: None,
                message: format!("claude exited without a result ({code}): {}", tail(&err)),
            });
        }
        parsed.into_reply()
    }
}

struct WorkDir {
    path: PathBuf,
}

impl WorkDir {
    fn new() -> std::io::Result<WorkDir> {
        let n = COUNTER.fetch_add(1, Ordering::SeqCst);
        let path =
            std::env::temp_dir().join(format!("zontologize-claude-{}-{n}", std::process::id()));
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir_all(&path)?;
        Ok(WorkDir { path })
    }
}

impl Drop for WorkDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

fn tail(text: &str) -> String {
    let text = text.trim();
    let start = text
        .char_indices()
        .rev()
        .nth(STDERR_TAIL)
        .map_or(0, |(i, _)| i);
    text[start..].to_string()
}

#[derive(Debug, Deserialize)]
struct Line {
    #[serde(rename = "type")]
    kind: String,
    subtype: Option<String>,
    #[serde(rename = "apiKeySource")]
    api_key_source: Option<String>,
    rate_limit_info: Option<RateLimitInfo>,
    is_error: Option<bool>,
    result: Option<String>,
    structured_output: Option<Box<serde_json::value::RawValue>>,
    usage: Option<ClaudeUsage>,
    total_cost_usd: Option<f64>,
}

#[derive(Debug, Clone, Deserialize)]
struct RateLimitInfo {
    status: String,
    #[serde(rename = "resetsAt")]
    resets_at: Option<i64>,
    #[serde(rename = "unifiedWindows")]
    unified_windows: Option<UnifiedWindows>,
}

#[derive(Debug, Clone, Deserialize)]
struct UnifiedWindows {
    five_hour: Option<RawWindow>,
    seven_day: Option<RawWindow>,
}

#[derive(Debug, Clone, Deserialize)]
struct RawWindow {
    utilization: f64,
    #[serde(rename = "resetsAt")]
    resets_at: i64,
}

#[derive(Debug, Clone, Default, Deserialize)]
struct ClaudeUsage {
    #[serde(default)]
    input_tokens: i64,
    #[serde(default)]
    cache_creation_input_tokens: i64,
    #[serde(default)]
    cache_read_input_tokens: i64,
    #[serde(default)]
    output_tokens: i64,
}

#[derive(Debug, Default)]
struct Parsed {
    key_source: Option<String>,
    limit: Option<RateLimitInfo>,
    result: Option<Line>,
}

fn parse(out: &str) -> Parsed {
    let mut parsed = Parsed::default();
    for line in out.lines() {
        let Ok(line) = serde_json::from_str::<Line>(line) else {
            continue;
        };
        match line.kind.as_str() {
            "system" if line.subtype.as_deref() == Some("init") => {
                parsed.key_source = line.api_key_source;
            }
            "rate_limit_event" => parsed.limit = line.rate_limit_info,
            "result" => parsed.result = Some(line),
            _ => {}
        }
    }
    parsed
}

fn time(seconds: i64) -> Option<DateTime<Utc>> {
    DateTime::from_timestamp(seconds, 0)
}

fn window(raw: &Option<RawWindow>) -> Option<Window> {
    let raw = raw.as_ref()?;
    Some(Window {
        used: raw.utilization,
        resets_at: time(raw.resets_at)?,
    })
}

impl Parsed {
    fn windows(&self) -> Windows {
        let Some(windows) = self.limit.as_ref().and_then(|l| l.unified_windows.as_ref()) else {
            return Windows::default();
        };
        Windows {
            five_hour: window(&windows.five_hour),
            seven_day: window(&windows.seven_day),
        }
    }

    fn into_reply(self) -> Result<Reply, CallError> {
        let windows = self.windows();
        if let Some(limit) = &self.limit
            && limit.status != ALLOWED
        {
            let resume = limit
                .resets_at
                .and_then(time)
                .or_else(|| latest_reset(&windows))
                .map_or(Resume::Funded, Resume::At);
            return Err(CallError::Pause {
                resume,
                reason: format!("Claude Code usage limit ({})", limit.status),
            });
        }
        let subscription = self.key_source.as_deref() == Some(SUBSCRIPTION_KEY_SOURCE);
        let Some(result) = self.result else {
            return Err(CallError::Retry {
                after: None,
                message: "claude gave no result".to_string(),
            });
        };
        let text = result.result.clone().unwrap_or_default();
        if result.is_error == Some(true) {
            let lower = text.to_lowercase();
            if lower.contains("login") || lower.contains("log in") || lower.contains("api key") {
                return Err(CallError::Fatal(format!("claude: {text}")));
            }
            if lower.contains("limit") {
                return Err(CallError::Pause {
                    resume: latest_reset(&windows).map_or(Resume::Funded, Resume::At),
                    reason: format!("Claude Code usage limit: {text}"),
                });
            }
            return Err(CallError::Retry {
                after: None,
                message: format!("claude: {text}"),
            });
        }
        let Some(output) = result.structured_output else {
            return Err(CallError::Invalid(format!(
                "claude returned no structured output: {}",
                tail(&text)
            )));
        };
        let usage = result.usage.unwrap_or_default();
        Ok(Reply {
            response: output.get().to_string(),
            usage: Usage {
                input_tokens: usage.input_tokens
                    + usage.cache_creation_input_tokens
                    + usage.cache_read_input_tokens,
                cached_input_tokens: usage.cache_read_input_tokens,
                output_tokens: usage.output_tokens,
                cost_usd: result.total_cost_usd.unwrap_or(0.0),
            },
            windows,
            subscription,
        })
    }
}

fn latest_reset(windows: &Windows) -> Option<DateTime<Utc>> {
    [windows.five_hour, windows.seven_day]
        .into_iter()
        .flatten()
        .map(|w| w.resets_at)
        .max()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    fn fixture(name: &str) -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/llm/claude-code")
            .join(name)
    }

    fn read(name: &str) -> String {
        std::fs::read_to_string(fixture(name)).unwrap()
    }

    #[test]
    fn parses_a_recorded_answer() {
        let reply = parse(&read("answer.jsonl")).into_reply().unwrap();
        let value: serde_json::Value = serde_json::from_str(&reply.response).unwrap();
        assert!(value["classes"].is_array(), "{value}");
        assert!(reply.subscription);
        assert!(reply.usage.input_tokens > 0);
        assert!(reply.usage.output_tokens > 0);
        assert!(reply.usage.cost_usd > 0.0);
        let five = reply.windows.five_hour.unwrap();
        assert!(five.used >= 0.0 && five.used <= 1.0);
        assert!(reply.windows.seven_day.is_some());
    }

    #[test]
    fn rejected_window_pauses_until_reset() {
        let error = parse(&read("rejected.jsonl")).into_reply().unwrap_err();
        let CallError::Pause { resume, .. } = error else {
            panic!("{error:?}");
        };
        assert_eq!(resume, Resume::At(time(1893456000).unwrap()));
    }

    #[test]
    fn missing_output_is_invalid_and_errors_map() {
        let no_output = r#"{"type":"result","is_error":false,"result":"plain text"}"#;
        assert!(matches!(
            parse(no_output).into_reply(),
            Err(CallError::Invalid(_))
        ));
        let login =
            r#"{"type":"result","is_error":true,"result":"Not logged in. Please run /login"}"#;
        assert!(matches!(
            parse(login).into_reply(),
            Err(CallError::Fatal(_))
        ));
        let busy = r#"{"type":"result","is_error":true,"result":"API Error: 529 overloaded"}"#;
        assert!(matches!(
            parse(busy).into_reply(),
            Err(CallError::Retry { .. })
        ));
        assert!(parse("not json\n").result.is_none());
    }

    #[test]
    fn api_key_billing_is_not_a_subscription() {
        let lines = "{\"type\":\"system\",\"subtype\":\"init\",\"apiKeySource\":\"ANTHROPIC_API_KEY\"}\n\
            {\"type\":\"result\",\"is_error\":false,\"structured_output\":{\"a\":1},\"total_cost_usd\":0.5}\n";
        let reply = parse(lines).into_reply().unwrap();
        assert!(!reply.subscription);
        assert_eq!(reply.response, "{\"a\":1}");
        assert_eq!(reply.windows, Windows::default());
    }

    #[test]
    fn isolation_flags_are_passed() {
        let provider = ClaudeCode::new(PathBuf::from("claude"), "haiku", Duration::from_secs(1));
        let request = Request {
            task: "fit".to_string(),
            system: "sys".to_string(),
            user: "user text".to_string(),
            schema: serde_json::json!({"type": "object"}),
        };
        let args = provider.args(&request);
        for flag in [
            "--no-session-persistence",
            "--strict-mcp-config",
            "--setting-sources",
            "--system-prompt",
            "--json-schema",
        ] {
            assert!(args.iter().any(|a| a == flag), "{flag}");
        }
        assert!(!args.iter().any(|a| a.contains("user text")));
    }

    #[tokio::test]
    async fn fake_claude_replays_and_slow_one_is_killed() {
        let request = Request {
            task: "fit".to_string(),
            system: "s".to_string(),
            user: "u".to_string(),
            schema: serde_json::json!({}),
        };
        let fake = ClaudeCode::with_args(
            PathBuf::from("sh"),
            vec![
                "-c".to_string(),
                format!(
                    "cat >/dev/null; cat '{}'",
                    fixture("answer.jsonl").display()
                ),
            ],
            Duration::from_secs(10),
        );
        assert!(fake.send(&request).await.is_ok());
        let slow = ClaudeCode::with_args(
            PathBuf::from("sh"),
            vec!["-c".to_string(), "sleep 30".to_string()],
            Duration::from_millis(200),
        );
        let started = std::time::Instant::now();
        let error = slow.send(&request).await.unwrap_err();
        assert!(matches!(error, CallError::Retry { .. }), "{error:?}");
        assert!(started.elapsed() < Duration::from_secs(5));
        let missing = ClaudeCode::new(
            PathBuf::from("/nonexistent/claude"),
            "haiku",
            Duration::from_secs(1),
        );
        let error = missing.send(&request).await.unwrap_err();
        assert!(
            matches!(&error, CallError::Fatal(m) if m.contains("/nonexistent/claude")),
            "{error:?}"
        );
    }
}
