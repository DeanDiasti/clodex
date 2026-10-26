//! Two requests for one local server at once: the provider sends the second
//! only after the first reply has ended, and keeps its client alive meanwhile.
//! The server is a stand-in that counts the requests it is computing.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use axum::Router;
use axum::body::Body;
use axum::http::{Method, Request, StatusCode};
use axum::routing::post;
use codex_backend::anthropic::sse::parse_sse_events;
use codex_backend::config::{AliasProvider, LocalModel, ResolvedLocalConfig};
use codex_backend::provider::Provider;
use codex_backend::providers::local::LocalProvider;
use codex_backend::registry::Registry;
use codex_backend::server::app;
use http_body_util::BodyExt;
use serde_json::json;
use tower::util::ServiceExt;

#[derive(Default)]
struct Busy {
    now: AtomicUsize,
    most: AtomicUsize,
    seen: AtomicUsize,
}

/// A chat server that takes 300 ms over every request, as a prefill would.
async fn slow_server(busy: Arc<Busy>) -> String {
    let route = post(move || {
        let busy = busy.clone();
        async move {
            let now = busy.now.fetch_add(1, Ordering::SeqCst) + 1;
            busy.most.fetch_max(now, Ordering::SeqCst);
            busy.seen.fetch_add(1, Ordering::SeqCst);
            tokio::time::sleep(Duration::from_millis(300)).await;
            busy.now.fetch_sub(1, Ordering::SeqCst);
            (
                [("content-type", "text/event-stream")],
                "data: {\"choices\":[{\"delta\":{\"content\":\"ok\"},\"finish_reason\":\"stop\"}]}\n\ndata: [DONE]\n\n",
            )
        }
    });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/v1", listener.local_addr().unwrap());
    tokio::spawn(async move {
        axum::serve(listener, Router::new().route("/v1/chat/completions", route))
            .await
            .unwrap();
    });
    url
}

async fn ask(app: &Router) -> (StatusCode, Vec<String>) {
    let body = json!({
        "model": "wollf",
        "max_tokens": 50,
        "stream": true,
        "messages": [{"role": "user", "content": "hi"}]
    });
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
    let names = parse_sse_events(&bytes)
        .into_iter()
        .map(|event| event.event.unwrap_or_default())
        .collect();
    (status, names)
}

#[tokio::test]
async fn two_requests_reach_one_local_server_one_at_a_time() {
    let busy = Arc::new(Busy::default());
    let url = slow_server(busy.clone()).await;
    let provider = LocalProvider::new(ResolvedLocalConfig {
        base_url: url,
        api_key: None,
        models: vec![LocalModel::parse("wollf").unwrap()],
    })
    .with_ping_every(Duration::from_millis(100));
    let app = app(Arc::new(Registry::from_providers(
        AliasProvider::Codex,
        [Arc::new(provider) as Arc<dyn Provider>],
    )));

    let (first, second) = tokio::join!(ask(&app), ask(&app));

    for (status, names) in [&first, &second] {
        assert_eq!(*status, StatusCode::OK);
        assert_eq!(names.first().map(String::as_str), Some("message_start"));
        assert_eq!(
            names.last().map(String::as_str),
            Some("message_stop"),
            "{names:?}"
        );
    }
    assert_eq!(busy.seen.load(Ordering::SeqCst), 2);
    assert_eq!(
        busy.most.load(Ordering::SeqCst),
        1,
        "the server computed two requests at once"
    );
    let queued = if first.1.contains(&"ping".to_string()) {
        &first.1
    } else {
        &second.1
    };
    assert!(
        queued.contains(&"ping".to_string()),
        "the queued client heard nothing while it waited: {first:?} {second:?}"
    );
}
