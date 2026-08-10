use std::collections::HashMap;
use std::net::TcpListener;
use std::sync::Mutex;
use std::thread;

use anyhow::{Context, Result, bail};
use axum::Router;
use axum::body::{Body, to_bytes};
use axum::extract::{Request, State};
use axum::http::{HeaderMap, HeaderName, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use futures_util::TryStreamExt;
use serde_json::Value;
use tokio::sync::oneshot;

const BRIDGE_HEADER: &str = "x-clodex-fast-bridge";
const BRIDGE_HEADER_VALUE: &str = "1";
const INITIAL_MODEL_HEADER: &str = "x-clodex-initial-model";
const SESSION_HEADER: &str = "x-claude-code-session-id";
const AGENT_HEADER: &str = "x-claude-code-agent-id";
const MAX_REQUEST_BYTES: usize = 128 * 1024 * 1024;
const MAX_TRACKED_ROUTES: usize = 16_384;

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct ConversationKey {
    session: String,
    agent: Option<String>,
}

#[derive(Debug, Clone, Default)]
struct RouteState {
    selected_model: Option<String>,
    claude_fast_model: Option<String>,
    fast_was_enabled: bool,
}

#[derive(Clone)]
struct BridgeState {
    upstream_port: u16,
    client: reqwest::Client,
}

pub struct FastBridge {
    port: u16,
    shutdown: Option<oneshot::Sender<()>>,
    thread: Option<thread::JoinHandle<Result<()>>>,
}

impl FastBridge {
    pub fn start(upstream_port: u16) -> Result<Self> {
        let listener =
            TcpListener::bind(("127.0.0.1", 0)).context("could not bind the Clodex fast bridge")?;
        listener.set_nonblocking(true)?;
        let port = listener.local_addr()?.port();
        let (shutdown_tx, shutdown_rx) = oneshot::channel();
        let (ready_tx, ready_rx) = std::sync::mpsc::sync_channel(1);

        let thread = thread::spawn(move || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .context("could not create the Clodex fast bridge runtime")?;
            runtime.block_on(async move {
                let listener = tokio::net::TcpListener::from_std(listener)
                    .context("could not adopt the Clodex fast bridge listener")?;
                let state = std::sync::Arc::new(BridgeState {
                    upstream_port,
                    client: reqwest::Client::builder()
                        .build()
                        .context("could not create the Clodex fast bridge client")?,
                });
                let app = Router::new()
                    .route("/__clodex/health", get(health))
                    .fallback(proxy)
                    .with_state(state);
                let _ = ready_tx.send(Ok::<(), String>(()));
                axum::serve(listener, app)
                    .with_graceful_shutdown(async {
                        let _ = shutdown_rx.await;
                    })
                    .await
                    .context("Clodex fast bridge stopped unexpectedly")
            })
        });

        ready_rx
            .recv()
            .context("Clodex fast bridge did not report readiness")?
            .map_err(anyhow::Error::msg)?;
        Ok(Self {
            port,
            shutdown: Some(shutdown_tx),
            thread: Some(thread),
        })
    }

    pub fn port(&self) -> u16 {
        self.port
    }
}

impl Drop for FastBridge {
    fn drop(&mut self) {
        if let Some(shutdown) = self.shutdown.take() {
            let _ = shutdown.send(());
        }
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

async fn health() -> impl IntoResponse {
    axum::Json(serde_json::json!({
        "ok": true,
        "service": "clodex-fast-bridge",
        "version": 1
    }))
}

async fn proxy(State(state): State<std::sync::Arc<BridgeState>>, request: Request) -> Response {
    match proxy_inner(&state, request).await {
        Ok(response) => response,
        Err(error) => (
            StatusCode::BAD_GATEWAY,
            axum::Json(serde_json::json!({
                "type": "error",
                "error": {
                    "type": "api_error",
                    "message": format!("Clodex fast bridge error: {error:#}")
                }
            })),
        )
            .into_response(),
    }
}

async fn proxy_inner(state: &BridgeState, request: Request) -> Result<Response> {
    let (parts, body) = request.into_parts();
    let body = to_bytes(body, MAX_REQUEST_BYTES)
        .await
        .context("could not read Claude request body")?;
    let mut bytes = body.to_vec();

    let marked = parts
        .headers
        .get(BRIDGE_HEADER)
        .and_then(|value| value.to_str().ok())
        == Some(BRIDGE_HEADER_VALUE);
    if marked && parts.method == axum::http::Method::POST && parts.uri.path() == "/v1/messages" {
        bytes = rewrite_request(&parts.headers, &bytes)?;
    }

    let query = parts
        .uri
        .path_and_query()
        .map_or("/", axum::http::uri::PathAndQuery::as_str);
    let url = format!("http://127.0.0.1:{}{query}", state.upstream_port);
    let mut upstream = state.client.request(parts.method, url);
    for (name, value) in &parts.headers {
        if should_forward_request_header(name) {
            upstream = upstream.header(name, value);
        }
    }
    let upstream = upstream
        .body(bytes)
        .send()
        .await
        .context("could not reach claude-code-proxy")?;

    let status = upstream.status();
    let response_headers = upstream.headers().clone();
    let stream = upstream.bytes_stream().map_err(std::io::Error::other);
    let mut response = Response::new(Body::from_stream(stream));
    *response.status_mut() = status;
    for (name, value) in &response_headers {
        if should_forward_response_header(name) {
            response.headers_mut().append(name, value.clone());
        }
    }
    Ok(response)
}

fn should_forward_request_header(name: &HeaderName) -> bool {
    name != header::HOST
        && name != header::CONTENT_LENGTH
        && name != header::CONNECTION
        && name.as_str() != BRIDGE_HEADER
        && name.as_str() != INITIAL_MODEL_HEADER
}

fn should_forward_response_header(name: &HeaderName) -> bool {
    name != header::CONTENT_LENGTH
        && name != header::CONNECTION
        && name != header::TRANSFER_ENCODING
}

fn rewrite_request(headers: &HeaderMap, body: &[u8]) -> Result<Vec<u8>> {
    let mut value: Value = serde_json::from_slice(body).context("invalid Claude request JSON")?;
    let Some(object) = value.as_object_mut() else {
        bail!("Claude request body was not an object");
    };
    let Some(incoming_model) = object
        .get("model")
        .and_then(Value::as_str)
        .map(str::to_string)
    else {
        return Ok(body.to_vec());
    };
    let fast = object.get("speed").and_then(Value::as_str) == Some("fast");
    let has_tools = object
        .get("tools")
        .and_then(Value::as_array)
        .is_some_and(|tools| !tools.is_empty());
    let Some(key) = conversation_key(headers) else {
        if fast && is_codex_model(&incoming_model) {
            object.insert("model".into(), Value::String(fast_model(&incoming_model)));
        }
        return serde_json::to_vec(&value).context("could not serialize Claude request");
    };

    let mut routes = bridge_routes().lock().expect("Clodex fast route lock");
    if !routes.contains_key(&key) && routes.len() >= MAX_TRACKED_ROUTES {
        routes.clear();
    }
    let route = routes.entry(key).or_insert_with(|| RouteState {
        selected_model: read_header(headers, INITIAL_MODEL_HEADER)
            .filter(|model| is_codex_model(strip_fast_suffix(model))),
        ..RouteState::default()
    });
    let normalized = strip_fast_suffix(&incoming_model);

    if fast {
        if route.selected_model.is_none() && is_codex_model(normalized) {
            route.selected_model = Some(normalized.to_string());
        }
        route.claude_fast_model = Some(incoming_model);
        route.fast_was_enabled = true;
        if let Some(selected) = route.selected_model.as_deref() {
            object.insert("model".into(), Value::String(fast_model(selected)));
        }
    } else if has_tools {
        let is_claude_fast_shadow = route.fast_was_enabled
            && route.claude_fast_model.as_deref() == Some(incoming_model.as_str());
        if is_claude_fast_shadow {
            if let Some(selected) = route.selected_model.as_deref() {
                object.insert("model".into(), Value::String(selected.to_string()));
            }
        } else if is_codex_model(normalized) {
            route.selected_model = Some(normalized.to_string());
            route.claude_fast_model = None;
            route.fast_was_enabled = false;
        }
    }

    serde_json::to_vec(&value).context("could not serialize Claude request")
}

fn bridge_routes() -> &'static Mutex<HashMap<ConversationKey, RouteState>> {
    static ROUTES: std::sync::OnceLock<Mutex<HashMap<ConversationKey, RouteState>>> =
        std::sync::OnceLock::new();
    ROUTES.get_or_init(|| Mutex::new(HashMap::new()))
}

fn conversation_key(headers: &HeaderMap) -> Option<ConversationKey> {
    let session = read_header(headers, SESSION_HEADER)?;
    Some(ConversationKey {
        session,
        agent: read_header(headers, AGENT_HEADER),
    })
}

fn read_header(headers: &HeaderMap, name: &str) -> Option<String> {
    headers
        .get(name)
        .and_then(|value| value.to_str().ok())
        .map(str::trim)
        .filter(|value| !value.is_empty() && value.len() <= 512)
        .map(str::to_string)
}

fn is_codex_model(model: &str) -> bool {
    model.starts_with("gpt-")
}

fn strip_fast_suffix(model: &str) -> &str {
    model.strip_suffix("-fast").unwrap_or(model)
}

fn fast_model(model: &str) -> String {
    format!("{}-fast", strip_fast_suffix(model))
}

pub fn custom_headers(initial_model: &str) -> String {
    format!("X-Clodex-Fast-Bridge: 1\nX-Clodex-Initial-Model: {initial_model}")
}

pub fn healthcheck(port: u16) -> bool {
    let Ok(response) = reqwest::blocking::Client::new()
        .get(format!("http://127.0.0.1:{port}/__clodex/health"))
        .timeout(std::time::Duration::from_millis(500))
        .send()
    else {
        return false;
    };
    response.status().is_success()
        && response
            .json::<Value>()
            .ok()
            .and_then(|body| {
                body.get("service")
                    .and_then(Value::as_str)
                    .map(str::to_string)
            })
            .as_deref()
            == Some("clodex-fast-bridge")
}

#[cfg(test)]
mod tests {
    use std::io::{Read, Write};
    use std::sync::mpsc;

    use axum::http::HeaderValue;

    use super::*;

    fn headers(session: &str, agent: Option<&str>) -> HeaderMap {
        let mut headers = HeaderMap::new();
        headers.insert(SESSION_HEADER, HeaderValue::from_str(session).unwrap());
        if let Some(agent) = agent {
            headers.insert(AGENT_HEADER, HeaderValue::from_str(agent).unwrap());
        }
        headers
    }

    fn rewrite(headers: &HeaderMap, model: &str, fast: bool) -> Value {
        let mut body = serde_json::json!({
            "model": model,
            "messages": [{"role":"user","content":"test"}],
            "tools": [{"name":"Bash","input_schema":{"type":"object"}}]
        });
        if fast {
            body["speed"] = Value::String("fast".to_string());
        }
        serde_json::from_slice(
            &rewrite_request(headers, &serde_json::to_vec(&body).unwrap()).unwrap(),
        )
        .unwrap()
    }

    #[test]
    fn fast_toggle_preserves_the_current_model() {
        let headers = headers("session-a", None);
        assert_eq!(
            rewrite(&headers, "gpt-5.6-terra", false)["model"],
            "gpt-5.6-terra"
        );
        assert_eq!(
            rewrite(&headers, "claude-opus-5", true)["model"],
            "gpt-5.6-terra-fast"
        );
        assert_eq!(
            rewrite(&headers, "claude-opus-5", false)["model"],
            "gpt-5.6-terra"
        );
        assert_eq!(
            rewrite(&headers, "claude-opus-5", false)["model"],
            "gpt-5.6-terra"
        );
    }

    #[test]
    fn explicit_model_change_replaces_the_pinned_model() {
        let headers = headers("session-model", None);
        let _ = rewrite(&headers, "gpt-5.6-sol", false);
        let _ = rewrite(&headers, "claude-opus-5", true);
        assert_eq!(
            rewrite(&headers, "gpt-5.6-luna", false)["model"],
            "gpt-5.6-luna"
        );
        assert_eq!(
            rewrite(&headers, "claude-opus-5", true)["model"],
            "gpt-5.6-luna-fast"
        );
    }

    #[test]
    fn sessions_and_agents_have_independent_fast_routes() {
        let main = headers("shared", None);
        let agent = headers("shared", Some("agent-a"));
        let other = headers("other", None);
        let _ = rewrite(&main, "gpt-5.6-sol", false);
        let _ = rewrite(&agent, "gpt-5.6-luna", false);
        let _ = rewrite(&other, "gpt-5.6-terra", false);
        assert_eq!(
            rewrite(&main, "claude-opus-5", true)["model"],
            "gpt-5.6-sol-fast"
        );
        assert_eq!(
            rewrite(&agent, "claude-opus-5", true)["model"],
            "gpt-5.6-luna-fast"
        );
        assert_eq!(
            rewrite(&other, "claude-opus-5", true)["model"],
            "gpt-5.6-terra-fast"
        );
    }

    #[test]
    fn auxiliary_requests_do_not_replace_the_selected_model() {
        let headers = headers("session-aux", None);
        let _ = rewrite(&headers, "gpt-5.6-sol", false);
        let auxiliary = serde_json::json!({
            "model": "gpt-5.6-luna",
            "messages": [{"role":"user","content":"title"}]
        });
        let _ = rewrite_request(&headers, &serde_json::to_vec(&auxiliary).unwrap()).unwrap();
        assert_eq!(
            rewrite(&headers, "claude-opus-5", true)["model"],
            "gpt-5.6-sol-fast"
        );
    }

    #[test]
    fn fast_before_first_prompt_uses_the_launch_model() {
        let mut headers = headers("session-initial", None);
        headers.insert(
            INITIAL_MODEL_HEADER,
            HeaderValue::from_static("gpt-5.6-terra"),
        );
        assert_eq!(
            rewrite(&headers, "claude-opus-5", true)["model"],
            "gpt-5.6-terra-fast"
        );
    }

    #[test]
    fn marker_is_clodex_specific() {
        assert_ne!(BRIDGE_HEADER, "authorization");
        assert_eq!(
            custom_headers("gpt-5.6-terra"),
            "X-Clodex-Fast-Bridge: 1\nX-Clodex-Initial-Model: gpt-5.6-terra"
        );
    }

    #[test]
    fn http_bridge_rewrites_only_marked_clodex_messages() {
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let upstream_port = listener.local_addr().unwrap().port();
        let (captured_tx, captured_rx) = mpsc::channel();
        let upstream = thread::spawn(move || {
            for _ in 0..3 {
                let (mut stream, _) = listener.accept().unwrap();
                let mut request = Vec::new();
                let mut chunk = [0_u8; 4096];
                let header_end = loop {
                    let count = stream.read(&mut chunk).unwrap();
                    assert!(count > 0);
                    request.extend_from_slice(&chunk[..count]);
                    if let Some(position) = request.windows(4).position(|part| part == b"\r\n\r\n")
                    {
                        break position + 4;
                    }
                };
                let headers = String::from_utf8_lossy(&request[..header_end]).to_lowercase();
                let content_length = headers
                    .lines()
                    .find_map(|line| line.strip_prefix("content-length:"))
                    .unwrap()
                    .trim()
                    .parse::<usize>()
                    .unwrap();
                while request.len() < header_end + content_length {
                    let count = stream.read(&mut chunk).unwrap();
                    assert!(count > 0);
                    request.extend_from_slice(&chunk[..count]);
                }
                captured_tx
                    .send((
                        headers,
                        serde_json::from_slice::<Value>(
                            &request[header_end..header_end + content_length],
                        )
                        .unwrap(),
                    ))
                    .unwrap();
                stream
                    .write_all(
                        b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 11\r\nConnection: close\r\n\r\n{\"ok\":true}",
                    )
                    .unwrap();
            }
        });

        let bridge = FastBridge::start(upstream_port).unwrap();
        let url = format!("http://127.0.0.1:{}/v1/messages", bridge.port());
        let client = reqwest::blocking::Client::new();
        let send = |marked: bool, model: &str, fast: bool| {
            let mut body = serde_json::json!({
                "model": model,
                "messages": [{"role":"user","content":"test"}],
                "tools": [{"name":"Bash","input_schema":{"type":"object"}}]
            });
            if fast {
                body["speed"] = Value::String("fast".to_string());
            }
            let mut request = client
                .post(&url)
                .header(SESSION_HEADER, "http-bridge-session")
                .json(&body);
            if marked {
                request = request
                    .header(BRIDGE_HEADER, BRIDGE_HEADER_VALUE)
                    .header(INITIAL_MODEL_HEADER, "gpt-5.6-terra");
            }
            assert!(request.send().unwrap().status().is_success());
        };

        send(false, "gpt-5.6-terra", true);
        send(true, "gpt-5.6-terra", false);
        send(true, "claude-opus-5", true);

        let captured: Vec<_> = (0..3)
            .map(|_| {
                captured_rx
                    .recv_timeout(std::time::Duration::from_secs(2))
                    .unwrap()
            })
            .collect();
        assert_eq!(captured[0].1["model"], "gpt-5.6-terra");
        assert_eq!(captured[1].1["model"], "gpt-5.6-terra");
        assert_eq!(captured[2].1["model"], "gpt-5.6-terra-fast");
        assert!(
            captured
                .iter()
                .all(|(headers, _)| !headers.contains("x-clodex-"))
        );

        drop(bridge);
        upstream.join().unwrap();
    }
}
