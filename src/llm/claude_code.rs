use std::ffi::OsString;
use std::os::unix::fs::DirBuilderExt;
use std::path::{Path, PathBuf};
use std::process::{ExitStatus, Stdio};
use std::time::Duration;

use serde::Deserialize;
use sqlx::types::chrono::{DateTime, Utc};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::process::Command;

use super::{CallError, Reply, Request, Resume, Usage, Window, Windows};

pub const BIN_VARIABLE: &str = "CLAUDE_CODE_BIN";
pub const DEFAULT_BIN: &str = "claude";
const SUBSCRIPTION_KEY_SOURCE: &str = "none";
const ALLOWED: [&str; 2] = ["allowed", "allowed_warning"];
const STDERR_TAIL: usize = 500;
const SYSTEM_FILE: &str = "system-prompt.txt";
const MAX_ARG_BYTES: usize = 128 * 1024 - 1;
const WORK_DIR_MODE: u32 = 0o700;
const WORK_DIR_ATTEMPTS: u32 = 16;
const ENV_ALLOWED: [&str; 22] = [
    "PATH",
    "HOME",
    "USER",
    "LOGNAME",
    "LANG",
    "TZ",
    "TMPDIR",
    "CLAUDE_CONFIG_DIR",
    "XDG_CONFIG_HOME",
    "XDG_CACHE_HOME",
    "XDG_DATA_HOME",
    "XDG_STATE_HOME",
    "XDG_RUNTIME_DIR",
    "HTTPS_PROXY",
    "https_proxy",
    "HTTP_PROXY",
    "http_proxy",
    "NO_PROXY",
    "no_proxy",
    "NODE_EXTRA_CA_CERTS",
    "SSL_CERT_FILE",
    "SSL_CERT_DIR",
];
const ENV_ALLOWED_PREFIX: &str = "LC_";
const AUTH_STATUSES: [u16; 2] = [401, 403];
const REJECTED_STATUSES: [u16; 2] = [400, 413];
const RATE_LIMIT_STATUS: u16 = 429;

pub struct ClaudeCode {
    program: PathBuf,
    #[cfg(test)]
    args: Option<Vec<String>>,
    model: String,
    timeout: Duration,
}

impl ClaudeCode {
    pub fn new(program: PathBuf, model: &str, timeout: Duration) -> ClaudeCode {
        ClaudeCode {
            program,
            #[cfg(test)]
            args: None,
            model: model.to_string(),
            timeout,
        }
    }

    #[cfg(test)]
    pub fn with_args(program: PathBuf, args: Vec<String>, timeout: Duration) -> ClaudeCode {
        ClaudeCode {
            program,
            args: Some(args),
            model: String::new(),
            timeout,
        }
    }

    pub fn args(&self, request: &Request, system_file: &Path) -> Vec<String> {
        #[cfg(test)]
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
            "--system-prompt-file",
            &system_file.to_string_lossy(),
            "--json-schema",
            &request.schema.to_string(),
        ]
        .iter()
        .map(|arg| arg.to_string())
        .collect()
    }

    pub async fn send(&self, request: &Request) -> Result<Reply, CallError> {
        let schema = request.schema.to_string().len();
        if schema > MAX_ARG_BYTES {
            return Err(CallError::invalid(format!(
                "the JSON schema is {schema} bytes, above the {MAX_ARG_BYTES} byte limit for one \
                 claude argument; select fewer classes or properties"
            )));
        }
        let first = self.send_once(request).await?;
        if !first.missing_output() {
            return first.finish();
        }
        self.send_once(request).await?.finish()
    }

    async fn send_once(&self, request: &Request) -> Result<Outcome, CallError> {
        let dir = WorkDir::new().map_err(|error| CallError::Fatal(error.to_string()))?;
        let system_file = dir.path.join(SYSTEM_FILE);
        std::fs::write(&system_file, &request.system).map_err(|error| {
            CallError::Fatal(format!("cannot write {}: {error}", system_file.display()))
        })?;
        let mut child = Command::new(&self.program)
            .args(self.args(request, &system_file))
            .env_clear()
            .envs(allowed_env(std::env::vars_os()))
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
        let stdin = child.stdin.take();
        let stdout = child.stdout.take();
        let stderr = child.stderr.take();
        let input = request.user.as_bytes();
        let run = async {
            let write = async {
                if let Some(mut stdin) = stdin {
                    let _ = stdin.write_all(input).await;
                    let _ = stdin.shutdown().await;
                }
            };
            let ((), out, err) = tokio::join!(write, read_all(stdout), read_all(stderr));
            let status = child.wait().await;
            (out, err, status)
        };
        let Ok((out, err, status)) = tokio::time::timeout(self.timeout, run).await else {
            return Err(CallError::Retry {
                after: None,
                message: format!("claude timed out after {} s", self.timeout.as_secs()),
            });
        };
        Ok(Outcome {
            parsed: parse(&out),
            stderr: err,
            status: status.ok(),
        })
    }
}

async fn read_all<R: AsyncReadExt + Unpin>(reader: Option<R>) -> String {
    let mut text = String::new();
    if let Some(mut reader) = reader {
        let _ = reader.read_to_string(&mut text).await;
    }
    text
}

fn allowed_env(vars: impl Iterator<Item = (OsString, OsString)>) -> Vec<(OsString, OsString)> {
    vars.filter(|(name, _)| {
        let name = name.to_string_lossy();
        ENV_ALLOWED.contains(&name.as_ref()) || name.starts_with(ENV_ALLOWED_PREFIX)
    })
    .collect()
}

struct Outcome {
    parsed: Parsed,
    stderr: String,
    status: Option<ExitStatus>,
}

impl Outcome {
    fn missing_output(&self) -> bool {
        self.parsed
            .result
            .as_ref()
            .is_some_and(|r| r.is_error != Some(true) && r.structured_output.is_none())
    }

    fn finish(self) -> Result<Reply, CallError> {
        let no_result = self.parsed.result.is_none();
        let status = self.status.map(|s| s.to_string()).unwrap_or_default();
        let stderr = self.stderr;
        self.parsed.into_reply().map_err(|error| match error {
            CallError::Retry { after, .. } if no_result => CallError::Retry {
                after,
                message: format!(
                    "claude exited without a result ({status}): {}",
                    tail(&stderr)
                ),
            },
            other => other,
        })
    }
}

struct WorkDir {
    path: PathBuf,
}

impl WorkDir {
    fn new() -> std::io::Result<WorkDir> {
        let mut builder = std::fs::DirBuilder::new();
        builder.mode(WORK_DIR_MODE);
        let mut attempt = 0;
        loop {
            let path = std::env::temp_dir().join(format!(
                "zontologize-claude-{}-{:016x}",
                std::process::id(),
                super::random()
            ));
            match builder.create(&path) {
                Ok(()) => return Ok(WorkDir { path }),
                Err(error)
                    if error.kind() == std::io::ErrorKind::AlreadyExists
                        && attempt < WORK_DIR_ATTEMPTS =>
                {
                    attempt += 1;
                }
                Err(error) => return Err(error),
            }
        }
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
    api_error_status: Option<u16>,
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
            && !ALLOWED.contains(&limit.status.as_str())
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
            let status = result.api_error_status;
            if status.is_some_and(|s| AUTH_STATUSES.contains(&s))
                || lower.contains("login")
                || lower.contains("log in")
                || lower.contains("api key")
            {
                return Err(CallError::Fatal(format!("claude: {text}")));
            }
            if status.is_some_and(|s| REJECTED_STATUSES.contains(&s)) {
                return Err(CallError::invalid(format!("claude: {text}")));
            }
            if status == Some(RATE_LIMIT_STATUS) {
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
        let raw = result.usage.unwrap_or_default();
        let usage = Usage {
            input_tokens: raw.input_tokens
                + raw.cache_creation_input_tokens
                + raw.cache_read_input_tokens,
            cached_input_tokens: raw.cache_read_input_tokens,
            output_tokens: raw.output_tokens,
            cost_usd: result.total_cost_usd.unwrap_or(0.0),
        };
        let Some(output) = result.structured_output else {
            return Err(CallError::Invalid {
                message: format!("claude returned no structured output: {}", tail(&text)),
                usage: Usage {
                    cost_usd: if subscription { 0.0 } else { usage.cost_usd },
                    ..usage
                },
            });
        };
        Ok(Reply {
            response: output.get().to_string(),
            usage,
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
    fn a_warning_status_still_answers() {
        let lines = "{\"type\":\"rate_limit_event\",\"rate_limit_info\":{\"status\":\"allowed_warning\",\
            \"resetsAt\":1893456000}}\n\
            {\"type\":\"result\",\"is_error\":false,\"structured_output\":{\"a\":1}}\n";
        let reply = parse(lines).into_reply().unwrap();
        assert_eq!(reply.response, "{\"a\":1}");
        let rejected = lines.replace("allowed_warning", "rejected");
        assert!(matches!(
            parse(&rejected).into_reply(),
            Err(CallError::Pause { .. })
        ));
    }

    #[test]
    fn missing_output_is_invalid_and_errors_map() {
        let no_output = r#"{"type":"result","is_error":false,"result":"plain text","total_cost_usd":0.5,"usage":{"input_tokens":7}}"#;
        let error = parse(no_output).into_reply().unwrap_err();
        let CallError::Invalid { usage, .. } = error else {
            panic!("{error:?}");
        };
        assert_eq!(usage.input_tokens, 7);
        assert_eq!(usage.cost_usd, 0.5);
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
        let args = provider.args(&request, Path::new("/private/system.txt"));
        for flag in [
            "--no-session-persistence",
            "--strict-mcp-config",
            "--setting-sources",
            "--system-prompt-file",
            "/private/system.txt",
            "--json-schema",
        ] {
            assert!(args.iter().any(|a| a == flag), "{flag}");
        }
        assert!(!args.iter().any(|a| a.contains("user text") || a == "sys"));
    }

    #[test]
    fn error_codes_decide_not_the_text() {
        let line = |status: &str, text: &str| {
            format!(
                "{{\"type\":\"result\",\"is_error\":true,\"api_error_status\":{status},\"result\":\"{text}\"}}"
            )
        };
        let context = line("400", "prompt is too long: context limit reached");
        assert!(matches!(
            parse(&context).into_reply(),
            Err(CallError::Invalid { .. })
        ));
        let other = line("null", "token limit hit while streaming");
        assert!(matches!(
            parse(&other).into_reply(),
            Err(CallError::Retry { .. })
        ));
        let rate = line("429", "slow down");
        assert!(matches!(
            parse(&rate).into_reply(),
            Err(CallError::Pause { .. })
        ));
        let auth = line("401", "bad credentials");
        assert!(matches!(
            parse(&auth).into_reply(),
            Err(CallError::Fatal(_))
        ));
    }

    #[test]
    fn only_allowed_variables_reach_the_child() {
        let vars = [
            ("PATH", "/usr/bin"),
            ("HOME", "/home/acme"),
            ("LC_ALL", "C"),
            ("CLAUDE_CONFIG_DIR", "/home/acme/.claude"),
            ("ANTHROPIC_API_KEY", "sk-test"),
            ("DATABASE_URL", "postgres://x"),
            ("PGPASSWORD", "secret"),
            ("OPENROUTER_API_KEY", "sk-or"),
            ("CLAUDE_CODE_OAUTH_TOKEN", "token"),
        ]
        .map(|(k, v)| (OsString::from(k), OsString::from(v)));
        let kept: Vec<String> = allowed_env(vars.into_iter())
            .into_iter()
            .map(|(k, _)| k.to_string_lossy().to_string())
            .collect();
        assert_eq!(kept, ["PATH", "HOME", "LC_ALL", "CLAUDE_CONFIG_DIR"]);
    }

    #[test]
    fn work_dirs_are_private_and_unique() {
        use std::os::unix::fs::PermissionsExt;
        let a = WorkDir::new().unwrap();
        let b = WorkDir::new().unwrap();
        assert_ne!(a.path, b.path);
        let mode = std::fs::metadata(&a.path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, WORK_DIR_MODE);
        let path = a.path.clone();
        drop(a);
        assert!(!path.exists());
    }

    fn request() -> Request {
        Request {
            task: "fit".to_string(),
            system: "s".to_string(),
            user: "u".to_string(),
            schema: serde_json::json!({}),
        }
    }

    fn script(body: String) -> ClaudeCode {
        ClaudeCode::with_args(
            PathBuf::from("sh"),
            vec!["-c".to_string(), body],
            Duration::from_secs(10),
        )
    }

    #[tokio::test]
    async fn large_stderr_does_not_block() {
        let fake = script(format!(
            "cat >/dev/null; head -c 300000 /dev/zero | tr '\\0' x >&2; cat '{}'",
            fixture("answer.jsonl").display()
        ));
        assert!(fake.send(&request()).await.is_ok());
    }

    #[tokio::test]
    async fn rejected_window_without_a_result_still_pauses() {
        let fake = script(format!(
            "cat >/dev/null; head -n 2 '{}'; exit 1",
            fixture("rejected.jsonl").display()
        ));
        let error = fake.send(&request()).await.unwrap_err();
        assert!(matches!(error, CallError::Pause { .. }), "{error:?}");
    }

    #[tokio::test]
    async fn missing_output_is_tried_once_more() {
        let marker = std::env::temp_dir().join(format!(
            "zontologize-claude-retry-{:016x}",
            crate::llm::random()
        ));
        let counted = script(format!(
            "cat >/dev/null; echo x >> '{}'; \
             printf '{{\"type\":\"result\",\"is_error\":false,\"result\":\"x\"}}\\n'",
            marker.display()
        ));
        assert!(counted.send(&request()).await.is_err());
        let calls = std::fs::read_to_string(&marker).unwrap().lines().count();
        let _ = std::fs::remove_file(&marker);
        assert_eq!(calls, 2);
    }

    #[tokio::test]
    async fn the_system_prompt_goes_in_a_private_file_and_big_schemas_stop() {
        let provider = ClaudeCode::new(PathBuf::from("claude"), "haiku", Duration::from_secs(1));
        let mut big = request();
        big.schema = serde_json::json!({ "description": "x".repeat(MAX_ARG_BYTES) });
        let error = provider.send(&big).await.unwrap_err();
        assert!(
            matches!(&error, CallError::Invalid { message, .. } if message.contains("131071")),
            "{error:?}"
        );
        let fake = script(format!(
            "cat >/dev/null; test \"$(cat {SYSTEM_FILE})\" = s && cat '{}'",
            fixture("answer.jsonl").display()
        ));
        assert!(fake.send(&request()).await.is_ok());
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
