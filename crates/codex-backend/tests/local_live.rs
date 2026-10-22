//! Drives the local provider against a real OpenAI-compatible server. Ignored
//! by default: run with
//! `CLODEX_LOCAL_TEST_URL=http://127.0.0.1:11434/v1 CLODEX_LOCAL_TEST_MODEL=wollf:32k \
//!  cargo test -p codex-backend --test local_live -- --ignored --nocapture`.

use axum::body::Body;
use axum::http::{Method, Request, StatusCode};
use codex_backend::anthropic::sse::parse_sse_events;
use codex_backend::config::{AliasProvider, LocalModel, ResolvedLocalConfig};
use codex_backend::provider::Provider;
use codex_backend::providers::local::LocalProvider;
use codex_backend::registry::Registry;
use codex_backend::server::app;
use http_body_util::BodyExt;
use serde_json::{Value, json};
use std::sync::Arc;
use tower::util::ServiceExt;

fn target() -> Option<(String, String)> {
    let url = std::env::var("CLODEX_LOCAL_TEST_URL").ok()?;
    let model = std::env::var("CLODEX_LOCAL_TEST_MODEL").ok()?;
    Some((url, model))
}

fn live_app(url: &str, model: &str) -> axum::Router {
    let provider = LocalProvider::new(ResolvedLocalConfig {
        base_url: url.to_string(),
        api_key: None,
        models: vec![LocalModel::parse(model).unwrap()],
    });
    app(Arc::new(Registry::from_providers(
        AliasProvider::Codex,
        [Arc::new(provider) as Arc<dyn Provider>],
    )))
}

async fn post_messages(app: &axum::Router, body: Value) -> (StatusCode, Vec<u8>) {
    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri("/v1/messages")
                .header("content-type", "application/json")
                .body(Body::from(body.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = response.status();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    (status, bytes.to_vec())
}

#[tokio::test]
#[ignore = "needs a running local server; see the module docs"]
async fn a_tool_request_streams_anthropic_events_from_a_live_server() {
    let Some((url, model)) = target() else {
        eprintln!("CLODEX_LOCAL_TEST_URL / CLODEX_LOCAL_TEST_MODEL unset; skipping");
        return;
    };
    let app = live_app(&url, &model);
    let (status, bytes) = post_messages(
        &app,
        json!({
            "model": model,
            "max_tokens": 200,
            "stream": true,
            "system": [{"type": "text", "text": "You are a coding agent. Use tools when asked."}],
            "messages": [{"role": "user", "content": "List the current directory using the Bash tool."}],
            "tools": [{"name": "Bash", "description": "Run a shell command", "input_schema": {
                "type": "object", "properties": {"command": {"type": "string"}}, "required": ["command"]
            }}]
        }),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::OK,
        "{}",
        String::from_utf8_lossy(&bytes)
    );
    let events = parse_sse_events(&bytes);
    let names: Vec<&str> = events
        .iter()
        .map(|event| event.event.as_deref().unwrap_or(""))
        .collect();
    eprintln!("events: {names:?}");
    assert_eq!(names.first(), Some(&"message_start"));
    assert_eq!(names.last(), Some(&"message_stop"));
    let starts: Vec<Value> = events
        .iter()
        .filter(|event| event.event.as_deref() == Some("content_block_start"))
        .map(|event| serde_json::from_str(&event.data).unwrap())
        .collect();
    assert!(!starts.is_empty(), "no content blocks");
    let tool = starts
        .iter()
        .find(|start| start["content_block"]["type"] == "tool_use");
    let delta: Value = serde_json::from_str(
        &events
            .iter()
            .find(|event| event.event.as_deref() == Some("message_delta"))
            .expect("message_delta")
            .data,
    )
    .unwrap();
    eprintln!(
        "stop_reason: {} usage: {}",
        delta["delta"]["stop_reason"], delta["usage"]
    );
    if let Some(tool) = tool {
        assert_eq!(tool["content_block"]["name"], "Bash");
        assert_eq!(delta["delta"]["stop_reason"], "tool_use");
        let input: String = events
            .iter()
            .filter(|event| event.event.as_deref() == Some("content_block_delta"))
            .map(|event| serde_json::from_str::<Value>(&event.data).unwrap())
            .filter(|value| value["delta"]["type"] == "input_json_delta")
            .map(|value| value["delta"]["partial_json"].as_str().unwrap().to_string())
            .collect();
        let parsed: Value = serde_json::from_str(&input).expect("tool input is JSON");
        eprintln!("tool input: {parsed}");
        assert!(parsed.get("command").is_some());
    } else {
        eprintln!("the model answered in text; the stream shape still held");
    }
    assert!(delta["usage"]["output_tokens"].as_u64().unwrap_or(0) > 0);
}

#[tokio::test]
#[ignore = "needs a running local server; see the module docs"]
async fn a_non_streaming_request_returns_one_message_and_count_tokens_estimates() {
    let Some((url, model)) = target() else {
        return;
    };
    let app = live_app(&url, &model);
    let (status, bytes) = post_messages(
        &app,
        json!({
            "model": model,
            "max_tokens": 40,
            "messages": [{"role": "user", "content": "Reply with the single word: pong"}]
        }),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::OK,
        "{}",
        String::from_utf8_lossy(&bytes)
    );
    let message: Value = serde_json::from_slice(&bytes).unwrap();
    eprintln!("message: {message}");
    assert_eq!(message["type"], "message");
    assert_eq!(message["role"], "assistant");
    assert!(message["content"].as_array().is_some_and(|c| !c.is_empty()));

    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri("/v1/messages/count_tokens")
                .header("content-type", "application/json")
                .body(Body::from(
                    json!({"model": model, "messages": [{"role": "user", "content": "hello there"}]})
                        .to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let counted: Value =
        serde_json::from_slice(&response.into_body().collect().await.unwrap().to_bytes()).unwrap();
    assert!(counted["input_tokens"].as_u64().unwrap() > 0);
}
