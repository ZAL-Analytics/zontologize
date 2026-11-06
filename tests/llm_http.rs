pub mod common;

use std::time::Duration;

use common::{fake_server, fixture};
use zontologize::llm::ollama::Ollama;
use zontologize::llm::openrouter::OpenRouter;
use zontologize::llm::{CallError, Request, Resume};

const KEY: &str = "sk-test-0000";

fn request() -> Request {
    Request {
        task: "fit".to_string(),
        system: "system text".to_string(),
        user: "user text".to_string(),
        schema: serde_json::json!({"type": "object"}),
    }
}

fn openrouter(url: &str) -> OpenRouter {
    OpenRouter::new(
        &format!("{url}/api/v1"),
        KEY,
        "anthropic/claude-haiku-5.5",
        Duration::from_secs(5),
    )
    .unwrap()
}

#[tokio::test]
async fn openrouter_answer_prices_and_credit() {
    let server = fake_server(vec![
        ("/chat/completions", 200, fixture("openrouter/answer.json")),
        ("/models", 200, fixture("openrouter/models.json")),
        ("/key", 200, fixture("openrouter/key.json")),
    ])
    .await;
    let provider = openrouter(&server.url);
    let reply = provider.send(&request()).await.unwrap();
    let answer: serde_json::Value = serde_json::from_str(&reply.response).unwrap();
    assert!(answer["classes"].is_array());
    assert!(reply.usage.input_tokens > 0 && reply.usage.output_tokens > 0);
    assert!(reply.usage.cost_usd > 0.0);
    assert!(!reply.subscription);
    let prices = provider.prices().await.unwrap();
    assert!((prices.input - 1e-7).abs() < 1e-15);
    assert!((prices.output - 5e-7).abs() < 1e-15);
    let credit = provider.credit().await.unwrap();
    assert_eq!(credit.remaining_usd, Some(46.5));
    let requests = server.requests.lock().unwrap();
    let chat = &requests[0];
    assert!(chat.starts_with("POST /api/v1/chat/completions"), "{chat}");
    assert!(
        chat.to_lowercase()
            .contains(&format!("authorization: bearer {KEY}").to_lowercase())
    );
    assert!(chat.contains("\"require_parameters\":true"), "{chat}");
}

#[tokio::test]
async fn openrouter_key_limit_pauses_and_hides_the_key() {
    let server = fake_server(vec![(
        "/chat/completions",
        402,
        fixture("openrouter/key-limit.json"),
    )])
    .await;
    let error = openrouter(&server.url).send(&request()).await.unwrap_err();
    let CallError::Pause { resume, reason } = &error else {
        panic!("{error:?}");
    };
    assert_eq!(resume, &Resume::Funded);
    assert!(!reason.contains(KEY));
    let server = fake_server(vec![(
        "/chat/completions",
        401,
        "{\"error\":{\"message\":\"No auth\"}}".to_string(),
    )])
    .await;
    let error = openrouter(&server.url).send(&request()).await.unwrap_err();
    assert!(
        matches!(&error, CallError::Fatal(m) if !m.contains(KEY)),
        "{error:?}"
    );
    let server = fake_server(vec![("/chat/completions", 503, "busy".to_string())]).await;
    let error = openrouter(&server.url).send(&request()).await.unwrap_err();
    assert!(matches!(error, CallError::Retry { .. }), "{error:?}");
}

#[tokio::test]
async fn ollama_answer_tags_and_errors() {
    let server = fake_server(vec![
        ("/api/chat", 200, fixture("ollama/answer.json")),
        ("/api/tags", 200, fixture("ollama/tags.json")),
    ])
    .await;
    let provider = Ollama::new(&server.url, "qwen3.5:4b", false, Duration::from_secs(5)).unwrap();
    let reply = provider.send(&request()).await.unwrap();
    assert!(reply.usage.input_tokens > 0);
    assert!(reply.usage.output_tokens > 0);
    assert_eq!(reply.usage.cost_usd, 0.0);
    assert!(provider.has_model().await.unwrap());
    let other = Ollama::new(&server.url, "acme-missing", false, Duration::from_secs(5)).unwrap();
    assert!(!other.has_model().await.unwrap());
    let server = fake_server(vec![("/api/chat", 404, fixture("ollama/not-found.json"))]).await;
    let missing = Ollama::new(&server.url, "acme-missing", false, Duration::from_secs(5)).unwrap();
    let error = missing.send(&request()).await.unwrap_err();
    assert!(
        matches!(&error, CallError::Fatal(m) if m.contains("not found")),
        "{error:?}"
    );
    let refused = Ollama::new("http://127.0.0.1:9", "m", false, Duration::from_secs(5)).unwrap();
    let error = refused.send(&request()).await.unwrap_err();
    assert!(matches!(error, CallError::Retry { .. }), "{error:?}");
}

#[tokio::test]
async fn ollama_auth_errors_are_fatal_and_bad_content_keeps_usage() {
    for status in [401, 403] {
        let server = fake_server(vec![(
            "/api/chat",
            status,
            "{\"error\":\"unauthorized\"}".to_string(),
        )])
        .await;
        let provider = Ollama::new(&server.url, "m", false, Duration::from_secs(5)).unwrap();
        let error = provider.send(&request()).await.unwrap_err();
        assert!(matches!(error, CallError::Fatal(_)), "{status}: {error:?}");
    }
    let server = fake_server(vec![(
        "/api/chat",
        200,
        "{\"message\":{\"content\":\"not json\"},\"prompt_eval_count\":12,\"eval_count\":3}"
            .to_string(),
    )])
    .await;
    let provider = Ollama::new(&server.url, "m", false, Duration::from_secs(5)).unwrap();
    let error = provider.send(&request()).await.unwrap_err();
    let CallError::Invalid { usage, .. } = error else {
        panic!("{error:?}");
    };
    assert_eq!(usage.input_tokens, 12);
}

#[tokio::test]
async fn openrouter_no_choices_retries_and_bad_content_keeps_usage() {
    let server = fake_server(vec![(
        "/chat/completions",
        200,
        "{\"error\":{\"message\":\"upstream failed\"}}".to_string(),
    )])
    .await;
    let error = openrouter(&server.url).send(&request()).await.unwrap_err();
    assert!(
        matches!(&error, CallError::Retry { message, .. } if message.contains("upstream failed")),
        "{error:?}"
    );
    let server = fake_server(vec![(
        "/chat/completions",
        200,
        "{\"choices\":[{\"message\":{\"content\":\"plain\"}}],\"usage\":{\"prompt_tokens\":9,\
         \"completion_tokens\":2,\"cost\":0.25}}"
            .to_string(),
    )])
    .await;
    let error = openrouter(&server.url).send(&request()).await.unwrap_err();
    let CallError::Invalid { usage, .. } = error else {
        panic!("{error:?}");
    };
    assert_eq!(usage.cost_usd, 0.25);
}

#[tokio::test]
async fn openrouter_unknown_prices_are_an_error() {
    for (prompt, completion) in [("-1", "-1"), ("n/a", "0.000001")] {
        let server = fake_server(vec![(
            "/models",
            200,
            format!(
                "{{\"data\":[{{\"id\":\"anthropic/claude-haiku-5.5\",\"pricing\":\
                 {{\"prompt\":\"{prompt}\",\"completion\":\"{completion}\"}}}}]}}"
            ),
        )])
        .await;
        let error = openrouter(&server.url).prices().await.unwrap_err();
        assert!(error.to_string().contains("price"), "{error:#}");
    }
}
