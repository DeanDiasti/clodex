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
use axum::routing::{get, post};
use futures_util::TryStreamExt;
use serde_json::Value;
use tokio::sync::oneshot;

use crate::mapping::ANTHROPIC_PREFIX;

const BRIDGE_HEADER: &str = "x-clodex-fast-bridge";
const BRIDGE_HEADER_VALUE: &str = "1";
const SESSION_FAST_HEADER: &str = "x-clodex-session-fast";
const INITIAL_MODEL_HEADER: &str = "x-clodex-initial-model";
const SESSION_HEADER: &str = "x-claude-code-session-id";
const AGENT_HEADER: &str = "x-claude-code-agent-id";
const MAX_REQUEST_BYTES: usize = 128 * 1024 * 1024;
const MAX_TRACKED_ROUTES: usize = 16_384;
/// How long a `PreCompact` hook keeps a session armed. Long enough for Claude
/// Code to assemble and send the compaction it just announced, short enough
/// that a cancelled compaction does not leave a session armed indefinitely.
const ARM_TTL: std::time::Duration = std::time::Duration::from_secs(120);
/// Concurrent token-count requests while planning a fold.
const COUNT_CONCURRENCY: usize = 8;
/// How often to ping while a round is in flight. A round can run for minutes,
/// and Claude Code drops a stream that goes idle.
const PING_INTERVAL: std::time::Duration = std::time::Duration::from_secs(15);
/// Extra attempts for an upstream request that failed before any of its
/// response reached Claude Code.
const UPSTREAM_RETRIES: u32 = 2;
/// Base backoff between those attempts, scaled by attempt number.
const RETRY_BACKOFF: std::time::Duration = std::time::Duration::from_millis(150);
/// Where Claude-routed requests go. Claude Code's own subscription credential
/// travels with them unchanged.
const ANTHROPIC_API: &str = "https://api.anthropic.com";
/// The subscription beta Claude Code attaches to OAuth requests. It means
/// nothing to Codex and is removed from the Codex route with the credential.
const OAUTH_BETA_PREFIX: &str = "oauth-";
const AUTO_REVIEW_SYSTEM_PREFIX: &str =
    "You are a security monitor for autonomous AI coding agents.";
pub const AUTO_REVIEW_MODEL: &str = "anthropic/claude-sonnet-5";

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
    anthropic_base: String,
    client: reqwest::Client,
    hierarchical: bool,
    ceiling: u64,
}

pub struct FastBridge {
    port: u16,
    shutdown: Option<oneshot::Sender<()>>,
    thread: Option<thread::JoinHandle<Result<()>>>,
}

impl FastBridge {
    /// `ceiling` is the capacity a fold round must fit inside; zero, or
    /// `hierarchical` unset, leaves every request forwarded untouched.
    pub fn start(upstream_port: u16, hierarchical: bool, ceiling: u64) -> Result<Self> {
        Self::start_with(
            upstream_port,
            ANTHROPIC_API.to_string(),
            hierarchical,
            ceiling,
        )
    }

    fn start_with(
        upstream_port: u16,
        anthropic_base: String,
        hierarchical: bool,
        ceiling: u64,
    ) -> Result<Self> {
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
                    anthropic_base,
                    client: reqwest::Client::builder()
                        .build()
                        .context("could not create the Clodex fast bridge client")?,
                    hierarchical,
                    ceiling,
                });
                let app = Router::new()
                    .route("/__clodex/health", get(health))
                    .route("/__clodex/compaction/arm", post(arm_compaction))
                    .route("/v1/models", get(decline_model_discovery))
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
        "version": 2,
        "capabilities": ["session-fast"]
    }))
}

/// Declines Claude Code's gateway model discovery. The launcher writes the
/// full model list into Claude Code's discovery cache; a successful fetch
/// would replace it with a list filtered to Claude-looking IDs, which drops
/// every Codex model. A non-OK status leaves the cache untouched.
async fn decline_model_discovery() -> impl IntoResponse {
    StatusCode::NOT_FOUND
}

/// Records that Claude Code is about to compact this session.
///
/// Claude Code's `PreCompact` hook posts its payload here before it builds the
/// compaction request, which is what lets the bridge recognise that request
/// without inferring it from the prompt body.
async fn arm_compaction(body: axum::body::Bytes) -> impl IntoResponse {
    let session = serde_json::from_slice::<Value>(&body)
        .ok()
        .and_then(|payload| {
            payload
                .get("session_id")
                .and_then(Value::as_str)
                .map(str::to_string)
        });
    let Some(session) = session else {
        return (StatusCode::BAD_REQUEST, "missing session_id");
    };

    let mut armed = armed_sessions().lock().expect("Clodex arm lock");
    let now = std::time::Instant::now();
    armed.retain(|_, at| now.duration_since(*at) < ARM_TTL);
    armed.insert(session, now);
    (StatusCode::OK, "armed")
}

fn armed_sessions() -> &'static Mutex<HashMap<String, std::time::Instant>> {
    static ARMED: std::sync::OnceLock<Mutex<HashMap<String, std::time::Instant>>> =
        std::sync::OnceLock::new();
    ARMED.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Whether this session has a live arming, without consuming it.
fn is_armed(session: &str) -> bool {
    let armed = armed_sessions().lock().expect("Clodex arm lock");
    armed
        .get(session)
        .is_some_and(|at| std::time::Instant::now().duration_since(*at) < ARM_TTL)
}

/// Consumes the arming record, so one `PreCompact` arms exactly one fold.
fn take_armed(session: &str) -> bool {
    let mut armed = armed_sessions().lock().expect("Clodex arm lock");
    match armed.remove(session) {
        Some(at) => std::time::Instant::now().duration_since(at) < ARM_TTL,
        None => false,
    }
}

/// Default output bound for a fold round when the request does not set one.
const DEFAULT_MAX_OUTPUT: u64 = 16_000;

/// Runs the hierarchical fold, or returns `None` to leave the request alone.
///
/// Every uncertain path returns `None`. Folding a conversation that would have
/// compacted normally is a regression, so this engages only once the request
/// genuinely exceeds what the routed model accepts.
async fn hierarchical_compaction(
    state: &BridgeState,
    headers: &HeaderMap,
    bytes: &[u8],
) -> Option<Response> {
    if !state.hierarchical || state.ceiling == 0 {
        return None;
    }
    let session = read_header(headers, SESSION_HEADER)?;
    let value: Value = serde_json::from_slice(bytes).ok()?;
    let object = value.as_object()?;
    let messages = object.get("messages")?.as_array()?;
    if !crate::compaction::carries_summary_prompt(messages) {
        return None;
    }
    // Only peek here. Consuming the arming before the plan is known would
    // spend it on an attempt that may forward the request untouched, leaving a
    // genuine compaction unarmed.
    if !is_armed(&session) {
        return None;
    }

    // Everything before the summary prompt is conversation; the prompt and
    // anything after it is the instruction tail every round repeats.
    // The last marker, not the first: an earlier compaction summary quoted in
    // history also contains it, and cutting there would treat live
    // conversation as instruction tail.
    let marker_at = messages.iter().rposition(|message| {
        crate::compaction::message_text(message)
            .is_some_and(|text| text.contains(crate::compaction::COMPACTION_MARKER))
    })?;
    let conversation = &messages[..marker_at];
    let tail: Vec<Value> = messages[marker_at..].to_vec();
    if conversation.is_empty() {
        return None;
    }

    let model = object.get("model").and_then(Value::as_str)?.to_string();
    // Folding runs against the Codex proxy. A Claude-routed compaction is
    // forwarded to Anthropic untouched.
    if model.starts_with(ANTHROPIC_PREFIX) {
        return None;
    }
    let system = object.get("system").cloned();
    let max_output = object
        .get("max_tokens")
        .and_then(Value::as_u64)
        .unwrap_or(DEFAULT_MAX_OUTPUT);

    // Fixed overhead is the system prompt and the instruction tail, which
    // every round repeats.
    let fixed_overhead = count_tokens(state, &model, system.as_ref(), &tail).await?;
    // Counted concurrently: the client is still waiting on response headers at
    // this point, so a long conversation counted one request at a time delays
    // the stream that keeps Claude Code from timing out.
    let owned: Vec<Value> = conversation.to_vec();
    let mut counts: Vec<u64> = Vec::with_capacity(owned.len());
    for group in owned.chunks(COUNT_CONCURRENCY) {
        let counted = futures_util::future::join_all(
            group
                .iter()
                .map(|message| count_tokens(state, &model, None, std::slice::from_ref(message))),
        )
        .await;
        for count in counted {
            counts.push(count?);
        }
    }

    let total: u64 = counts.iter().sum::<u64>() + fixed_overhead;
    if total <= state.ceiling {
        // It fits. Claude Code's own compaction will succeed, so stay out of
        // the way rather than spending extra rounds.
        return None;
    }

    let can_open: Vec<bool> = conversation
        .iter()
        .map(crate::compaction::is_safe_boundary)
        .collect();
    let budget = crate::compaction::Budget {
        ceiling: state.ceiling,
        fixed_overhead,
        max_output,
    };
    let plan = crate::compaction::plan(&counts, &can_open, budget)?;
    if plan.round_count() < 2 {
        return None;
    }

    let rounds: Vec<Vec<Value>> = plan
        .rounds
        .iter()
        .map(|round| {
            round
                .messages
                .iter()
                .map(|index| conversation[*index].clone())
                .collect()
        })
        .collect();

    // Committed: from here the fold owns the response, so the arming is spent.
    take_armed(&session);
    Some(fold_response(
        state.clone(),
        model,
        system,
        tail,
        rounds,
        max_output,
    ))
}

/// Asks the upstream what a set of messages costs. Local and fast, so the fold
/// is sized against real counts rather than a character heuristic.
async fn count_tokens(
    state: &BridgeState,
    model: &str,
    system: Option<&Value>,
    messages: &[Value],
) -> Option<u64> {
    let mut body = serde_json::json!({ "model": model, "messages": messages });
    if let Some(system) = system {
        body["system"] = system.clone();
    }
    let response = state
        .client
        .post(format!(
            "http://127.0.0.1:{}/v1/messages/count_tokens",
            state.upstream_port
        ))
        .header(header::CONTENT_TYPE, "application/json")
        .json(&body)
        .send()
        .await
        .ok()?;
    if !response.status().is_success() {
        return None;
    }
    response
        .json::<Value>()
        .await
        .ok()?
        .get("input_tokens")
        .and_then(Value::as_u64)
}

/// Streams the fold back as one ordinary compaction response.
///
/// Claude Code enforces a stream idle timeout, and a deep fold runs for
/// minutes, so the stream opens immediately and pings between rounds.
fn fold_response(
    state: BridgeState,
    model: String,
    system: Option<Value>,
    tail: Vec<Value>,
    rounds: Vec<Vec<Value>>,
    max_output: u64,
) -> Response {
    let (sender, receiver) = tokio::sync::mpsc::channel::<std::io::Result<Vec<u8>>>(8);

    tokio::spawn(async move {
        let _ = sender
            .send(Ok(sse(
                "message_start",
                &serde_json::json!({
                    "type": "message_start",
                    "message": {
                        "id": "msg_clodex_fold",
                        "type": "message",
                        "role": "assistant",
                        "model": model,
                        "content": [],
                        "stop_reason": null,
                        "stop_sequence": null,
                        "usage": {"input_tokens": 0, "output_tokens": 0}
                    }
                }),
            )))
            .await;
        let _ = sender
            .send(Ok(sse(
                "content_block_start",
                &serde_json::json!({
                    "type": "content_block_start",
                    "index": 0,
                    "content_block": {"type": "text", "text": ""}
                }),
            )))
            .await;

        let mut carry: Option<String> = None;
        for (index, round) in rounds.iter().enumerate() {
            let _ = sender
                .send(Ok(sse("ping", &serde_json::json!({"type": "ping"}))))
                .await;

            let mut messages: Vec<Value> = Vec::new();
            if let Some(previous) = &carry {
                messages.push(serde_json::json!({
                    "role": "user",
                    "content": format!(
                        "Summary of the earlier conversation, to be carried forward \
                         and merged with what follows:\n\n{previous}"
                    )
                }));
            }
            messages.extend(round.iter().cloned());
            messages.extend(tail.iter().cloned());

            // Ping while the round is in flight, not merely before it. A round
            // can run for minutes, and a stream that goes quiet that long is
            // dropped by the client as idle.
            let round = round_summary(&state, &model, system.as_ref(), &messages, max_output);
            tokio::pin!(round);
            let mut ping = tokio::time::interval(PING_INTERVAL);
            // The first tick resolves immediately; the round has only just
            // started, so spend it rather than emitting a redundant ping.
            ping.tick().await;
            let outcome = loop {
                tokio::select! {
                    outcome = &mut round => break outcome,
                    _ = ping.tick() => {
                        let _ = sender
                            .send(Ok(sse("ping", &serde_json::json!({"type": "ping"}))))
                            .await;
                    }
                }
            };

            match outcome {
                Some(summary) => carry = Some(summary),
                None => {
                    let _ = sender
                        .send(Ok(sse(
                            "error",
                            &serde_json::json!({
                                "type": "error",
                                "error": {
                                    "type": "api_error",
                                    "message": format!(
                                        "Clodex hierarchical compaction failed on round {} of {}",
                                        index + 1,
                                        rounds.len()
                                    )
                                }
                            }),
                        )))
                        .await;
                    return;
                }
            }
        }

        let summary = carry.unwrap_or_default();
        let _ = sender
            .send(Ok(sse(
                "content_block_delta",
                &serde_json::json!({
                    "type": "content_block_delta",
                    "index": 0,
                    "delta": {"type": "text_delta", "text": summary}
                }),
            )))
            .await;
        let _ = sender
            .send(Ok(sse(
                "content_block_stop",
                &serde_json::json!({"type": "content_block_stop", "index": 0}),
            )))
            .await;
        let _ = sender
            .send(Ok(sse(
                "message_delta",
                &serde_json::json!({
                    "type": "message_delta",
                    "delta": {"stop_reason": "end_turn", "stop_sequence": null},
                    "usage": {"output_tokens": 0}
                }),
            )))
            .await;
        let _ = sender
            .send(Ok(sse(
                "message_stop",
                &serde_json::json!({"type": "message_stop"}),
            )))
            .await;
    });

    let stream = futures_util::stream::unfold(receiver, |mut receiver| async move {
        receiver.recv().await.map(|item| (item, receiver))
    });
    let mut response = Response::new(Body::from_stream(stream));
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        axum::http::HeaderValue::from_static("text/event-stream"),
    );
    response
}

/// Issues one fold round and returns its summary text.
async fn round_summary(
    state: &BridgeState,
    model: &str,
    system: Option<&Value>,
    messages: &[Value],
    max_output: u64,
) -> Option<String> {
    let mut body = serde_json::json!({
        "model": model,
        "messages": messages,
        "max_tokens": max_output,
        "stream": false,
    });
    if let Some(system) = system {
        body["system"] = system.clone();
    }

    // Interrupted upstream responses are common on this path, and a failed
    // round would sink the whole fold, so each round gets a bounded retry.
    for attempt in 0..3 {
        let response = state
            .client
            .post(format!(
                "http://127.0.0.1:{}/v1/messages",
                state.upstream_port
            ))
            .header(header::CONTENT_TYPE, "application/json")
            .json(&body)
            .send()
            .await;
        if let Ok(response) = response
            && response.status().is_success()
            && let Ok(parsed) = response.json::<Value>().await
            && let Some(text) = assistant_text(&parsed)
        {
            return Some(text);
        }
        if attempt < 2 {
            tokio::time::sleep(std::time::Duration::from_secs(2 << attempt)).await;
        }
    }
    None
}

fn assistant_text(response: &Value) -> Option<String> {
    let blocks = response.get("content")?.as_array()?;
    let mut text = String::new();
    for block in blocks {
        if block.get("type").and_then(Value::as_str) == Some("text")
            && let Some(part) = block.get("text").and_then(Value::as_str)
        {
            text.push_str(part);
        }
    }
    if text.trim().is_empty() {
        None
    } else {
        Some(text)
    }
}

fn sse(event: &str, data: &Value) -> Vec<u8> {
    format!("event: {event}\ndata: {data}\n\n").into_bytes()
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
    let is_messages =
        parts.method == axum::http::Method::POST && parts.uri.path() == "/v1/messages";
    // Classifier calls belong to Anthropic, even in a Codex conversation.
    // Route them before session-fast or native /fast can rewrite their model.
    let auto_review = is_messages && route_auto_review_to_anthropic(&mut bytes)?;
    let session_fast = marked && parts.headers.contains_key(SESSION_FAST_HEADER);
    if !auto_review && session_fast && is_messages {
        bytes = rewrite_request(&parts.headers, &bytes)?;
    }
    // Fold rounds use the same session tier as ordinary Codex requests.
    if !auto_review
        && is_messages
        && let Some(response) = hierarchical_compaction(state, &parts.headers, &bytes).await
    {
        return Ok(response);
    }
    if !auto_review && marked && is_messages && !session_fast {
        bytes = rewrite_request(&parts.headers, &bytes)?;
    }
    let is_count =
        parts.method == axum::http::Method::POST && parts.uri.path() == "/v1/messages/count_tokens";
    let to_anthropic =
        auto_review || ((is_messages || is_count) && route_to_anthropic(&mut bytes)?);

    let query = parts
        .uri
        .path_and_query()
        .map_or("/", axum::http::uri::PathAndQuery::as_str);
    let url = if to_anthropic {
        format!("{}{query}", state.anthropic_base)
    } else {
        format!("http://127.0.0.1:{}{query}", state.upstream_port)
    };

    // Codex intermittently drops a pooled connection, which surfaces as a 502
    // that fails in tens of milliseconds -- before the request was ever sent.
    // Nothing has reached Claude Code at this point, so replaying is safe, and
    // without it these surface to the user as hard API errors.
    let mut attempt = 0;
    let upstream = loop {
        let mut request = state.client.request(parts.method.clone(), &url);
        for (name, value) in &parts.headers {
            if !should_forward_request_header(name) {
                continue;
            }
            if to_anthropic {
                request = request.header(name, value);
            } else if let Some(value) = codex_header_value(name, value) {
                request = request.header(name, value);
            }
        }
        let sent = request.body(bytes.clone()).send().await;

        // The replay rule covers the local proxy's dropped Codex connection.
        // Claude Code retries Anthropic errors itself.
        let retryable = !to_anthropic
            && attempt < UPSTREAM_RETRIES
            && is_replayable(&parts.method, parts.uri.path());
        match sent {
            Ok(response) if response.status() == StatusCode::BAD_GATEWAY && retryable => {}
            Ok(response) => break response,
            Err(_) if retryable => {}
            Err(error) if to_anthropic => return Err(error).context("could not reach Anthropic"),
            Err(error) => return Err(error).context("could not reach claude-code-proxy"),
        }

        attempt += 1;
        tokio::time::sleep(RETRY_BACKOFF * attempt).await;
    };

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

/// Whether a failed attempt can be sent again.
///
/// Only the message endpoints, and only before any response has been
/// forwarded: replaying once Claude Code has seen part of a stream would
/// duplicate content it has already rendered.
fn is_replayable(method: &axum::http::Method, path: &str) -> bool {
    method == axum::http::Method::POST
        && (path == "/v1/messages" || path == "/v1/messages/count_tokens")
}

fn should_forward_request_header(name: &HeaderName) -> bool {
    name != header::HOST
        && name != header::CONTENT_LENGTH
        && name != header::CONNECTION
        && name.as_str() != BRIDGE_HEADER
        && name.as_str() != INITIAL_MODEL_HEADER
        && name.as_str() != SESSION_FAST_HEADER
}

/// Returns the value to send to the Codex proxy, or `None` to drop the header.
///
/// Claude Code sends its Claude subscription credential with every request
/// once a Claude route is configured. The Codex proxy authenticates with its
/// own credential, so this one never leaves for the Codex route.
fn codex_header_value(
    name: &HeaderName,
    value: &axum::http::HeaderValue,
) -> Option<axum::http::HeaderValue> {
    if name == header::AUTHORIZATION || name.as_str() == "x-api-key" {
        return None;
    }
    if name.as_str() != "anthropic-beta" {
        return Some(value.clone());
    }
    let betas: Vec<&str> = value
        .to_str()
        .ok()?
        .split(',')
        .map(str::trim)
        .filter(|beta| !beta.is_empty() && !beta.starts_with(OAUTH_BETA_PREFIX))
        .collect();
    if betas.is_empty() {
        return None;
    }
    axum::http::HeaderValue::from_str(&betas.join(",")).ok()
}

/// Whether a message request is bound for Anthropic. If it is, the routing
/// prefix is removed so Anthropic receives the bare Claude model ID.
fn route_to_anthropic(bytes: &mut Vec<u8>) -> Result<bool> {
    #[derive(serde::Deserialize)]
    struct Peek {
        model: Option<String>,
    }
    // Peeking avoids building the whole document for every Codex request.
    let routed = serde_json::from_slice::<Peek>(bytes)
        .ok()
        .and_then(|peek| peek.model)
        .is_some_and(|model| model.starts_with(ANTHROPIC_PREFIX));
    if !routed {
        return Ok(false);
    }

    let mut value: Value = serde_json::from_slice(bytes).context("invalid Claude request JSON")?;
    if let Some(model) = value.get("model").and_then(Value::as_str) {
        let bare = model
            .strip_prefix(ANTHROPIC_PREFIX)
            .unwrap_or(model)
            .to_string();
        value["model"] = Value::String(bare);
    }
    *bytes = serde_json::to_vec(&value).context("could not serialize Claude request")?;
    Ok(true)
}

/// Claude Code's local permission classifier uses a distinct system prompt,
/// no tools, and a non-streaming reply. Keep the requested Claude judge and
/// its prompt intact; if Claude Code chose a mapped GPT model, use Sonnet.
fn route_auto_review_to_anthropic(bytes: &mut Vec<u8>) -> Result<bool> {
    #[derive(serde::Deserialize)]
    struct SystemBlock {
        text: Option<String>,
    }
    #[derive(serde::Deserialize)]
    struct Peek {
        model: Option<String>,
        stream: Option<bool>,
        tools: Option<Vec<serde::de::IgnoredAny>>,
        system: Option<Vec<SystemBlock>>,
    }
    // Ordinary conversations can be large; do not allocate their messages
    // just to decide whether this is a classifier request.
    let Ok(peek) = serde_json::from_slice::<Peek>(bytes) else {
        return Ok(false);
    };
    if peek.stream == Some(true)
        || peek.tools.as_ref().is_some_and(|tools| !tools.is_empty())
        || !peek.system.as_ref().is_some_and(|blocks| {
            blocks.iter().any(|block| {
                block
                    .text
                    .as_deref()
                    .is_some_and(|text| text.starts_with(AUTO_REVIEW_SYSTEM_PREFIX))
            })
        })
    {
        return Ok(false);
    }
    let Some(model) = peek.model else {
        return Ok(false);
    };
    let bare = model.strip_prefix(ANTHROPIC_PREFIX).unwrap_or(&model);
    let model = if bare.starts_with("claude-") {
        bare.to_string()
    } else {
        AUTO_REVIEW_MODEL
            .strip_prefix(ANTHROPIC_PREFIX)
            .expect("Anthropic judge model")
            .to_string()
    };
    let mut value: Value =
        serde_json::from_slice(bytes).context("invalid auto-review request JSON")?;
    value["model"] = Value::String(model);
    *bytes = serde_json::to_vec(&value).context("could not serialize auto-review request")?;
    Ok(true)
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
    if let Some(routes) = headers.get(SESSION_FAST_HEADER) {
        // Claude routes bypass the Codex-only session policy entirely.
        if incoming_model.starts_with(ANTHROPIC_PREFIX) {
            return Ok(body.to_vec());
        }
        let routes: std::collections::BTreeMap<String, String> =
            serde_json::from_str(routes.to_str().context("invalid session-fast header")?)
                .context("invalid session-fast routes")?;
        let model = strip_fast_suffix(&incoming_model);
        let routed = routes.get(model).map(String::as_str).unwrap_or(model);
        object.insert("model".into(), Value::String(routed.to_string()));
        // Unsupported models fall back to standard; Claude's toggle does not
        // control a session launched with --fast.
        object.remove("speed");
        return serde_json::to_vec(&value).context("could not serialize Claude request");
    }
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
            .filter(|model| is_routed_model(strip_fast_suffix(model))),
        ..RouteState::default()
    });
    let normalized = strip_fast_suffix(&incoming_model);

    if fast {
        if route.selected_model.is_none() && is_routed_model(normalized) {
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
        } else if is_routed_model(normalized) {
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

/// A model Clodex routed to a Claude Code role, on either provider.
fn is_routed_model(model: &str) -> bool {
    is_codex_model(model) || model.starts_with(ANTHROPIC_PREFIX)
}

fn strip_fast_suffix(model: &str) -> &str {
    model.strip_suffix("-fast").unwrap_or(model)
}

/// Codex selects its priority tier by model suffix. Anthropic reads the
/// request's own `speed` field, so a Claude route keeps its model ID.
fn fast_model(model: &str) -> String {
    if model.starts_with(ANTHROPIC_PREFIX) {
        return model.to_string();
    }
    format!("{}-fast", strip_fast_suffix(model))
}

pub fn custom_headers(initial_model: &str) -> String {
    format!("X-Clodex-Fast-Bridge: 1\nX-Clodex-Initial-Model: {initial_model}")
}

pub fn supports_session_fast(port: u16) -> bool {
    reqwest::blocking::Client::new()
        .get(format!("http://127.0.0.1:{port}/__clodex/health"))
        .timeout(std::time::Duration::from_millis(500))
        .send()
        .ok()
        .filter(|response| response.status().is_success())
        .and_then(|response| response.json::<Value>().ok())
        .is_some_and(|body| {
            body["service"] == "clodex-fast-bridge"
                && body["capabilities"]
                    .as_array()
                    .is_some_and(|caps| caps.iter().any(|cap| cap == "session-fast"))
        })
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

    #[test]
    fn arming_is_consumed_once_and_expires() {
        let session = "arm-once-session";
        {
            let mut armed = armed_sessions().lock().unwrap();
            armed.insert(session.to_string(), std::time::Instant::now());
        }

        assert!(take_armed(session), "the armed session was not recognized");
        assert!(
            !take_armed(session),
            "one PreCompact must arm exactly one fold"
        );
    }

    #[test]
    fn a_stale_arming_does_not_trigger_a_fold() {
        let session = "stale-arm-session";
        {
            let mut armed = armed_sessions().lock().unwrap();
            armed.insert(
                session.to_string(),
                std::time::Instant::now() - (ARM_TTL + std::time::Duration::from_secs(1)),
            );
        }

        assert!(!take_armed(session));
    }

    #[test]
    fn an_unarmed_session_is_never_folded() {
        assert!(!take_armed("never-armed-session"));
    }

    #[test]
    fn peeking_at_an_arming_does_not_consume_it() {
        let session = "peek-session";
        {
            let mut armed = armed_sessions().lock().unwrap();
            armed.insert(session.to_string(), std::time::Instant::now());
        }

        // Planning can bail after this point, and a genuine compaction must
        // still be foldable on the next attempt.
        assert!(is_armed(session));
        assert!(is_armed(session));
        assert!(take_armed(session));
        assert!(!is_armed(session));
    }

    #[tokio::test]
    async fn a_request_that_cannot_be_planned_keeps_its_arming() {
        let session = "unplannable-session";
        {
            let mut armed = armed_sessions().lock().unwrap();
            armed.insert(session.to_string(), std::time::Instant::now());
        }
        let state = BridgeState {
            upstream_port: 1,
            anthropic_base: String::new(),
            client: reqwest::Client::new(),
            hierarchical: true,
            ceiling: 828_400,
        };
        // Carries the marker but has no conversation before it, so planning
        // bails before any fold is committed.
        let body = serde_json::json!({
            "model": "gpt-5.6-sol",
            "messages": [{
                "role": "user",
                "content": crate::compaction::COMPACTION_MARKER
            }]
        });

        let response = hierarchical_compaction(
            &state,
            &headers(session, None),
            &serde_json::to_vec(&body).unwrap(),
        )
        .await;

        assert!(response.is_none());
        assert!(
            take_armed(session),
            "a failed plan must not spend the arming"
        );
    }

    #[tokio::test]
    async fn a_disabled_bridge_leaves_every_request_alone() {
        let state = BridgeState {
            upstream_port: 1,
            anthropic_base: String::new(),
            client: reqwest::Client::new(),
            hierarchical: false,
            ceiling: 828_400,
        };
        let body = serde_json::json!({
            "model": "gpt-5.6-sol",
            "messages": [{
                "role": "user",
                "content": crate::compaction::COMPACTION_MARKER
            }]
        });

        let response = hierarchical_compaction(
            &state,
            &headers("session", None),
            &serde_json::to_vec(&body).unwrap(),
        )
        .await;

        assert!(response.is_none());
    }

    #[tokio::test]
    async fn an_ordinary_request_is_never_folded_even_when_armed() {
        let session = "ordinary-request-session";
        {
            let mut armed = armed_sessions().lock().unwrap();
            armed.insert(session.to_string(), std::time::Instant::now());
        }
        let state = BridgeState {
            upstream_port: 1,
            anthropic_base: String::new(),
            client: reqwest::Client::new(),
            hierarchical: true,
            ceiling: 828_400,
        };
        let body = serde_json::json!({
            "model": "gpt-5.6-sol",
            "messages": [{"role": "user", "content": "what does this function do?"}]
        });

        let response = hierarchical_compaction(
            &state,
            &headers(session, None),
            &serde_json::to_vec(&body).unwrap(),
        )
        .await;

        assert!(response.is_none(), "an ordinary request was folded");
        // The arming must survive, so the real compaction still folds.
        assert!(
            take_armed(session),
            "arming was consumed by the wrong request"
        );
    }

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
    fn session_fast_follows_models_and_agents_without_native_fast_flags() {
        for agent in [None, Some("codex-agent-a"), Some("codex-agent-b")] {
            let mut forced = headers("forced-session", agent);
            forced.insert(
                SESSION_FAST_HEADER,
                HeaderValue::from_static(
                    r#"{"gpt-astra":"gpt-astra-fast","gpt-luna":"gpt-luna-fast"}"#,
                ),
            );
            // A parent's initial model must not pin a child's chosen model.
            forced.insert(INITIAL_MODEL_HEADER, HeaderValue::from_static("gpt-astra"));
            for model in [
                "gpt-astra",
                "gpt-luna",
                "gpt-astra-fast",
                "gpt-unsupported-fast",
            ] {
                let body = serde_json::json!({"model":model,"speed":"standard","messages":[]});
                let rewritten: Value = serde_json::from_slice(
                    &rewrite_request(&forced, &serde_json::to_vec(&body).unwrap()).unwrap(),
                )
                .unwrap();
                let expected = if model == "gpt-unsupported-fast" {
                    "gpt-unsupported".to_string()
                } else {
                    fast_model(model)
                };
                assert_eq!(rewritten["model"], expected);
                assert!(rewritten.get("speed").is_none());
            }
            let claude = serde_json::to_vec(&serde_json::json!({
                "model":"anthropic/claude-opus-5-5", "speed":"fast", "messages":[]
            }))
            .unwrap();
            assert_eq!(rewrite_request(&forced, &claude).unwrap(), claude);
        }
        let mut forced = HeaderMap::new();
        forced.insert(
            SESSION_FAST_HEADER,
            HeaderValue::from_static(r#"{"gpt-luna":"gpt-luna-fast"}"#),
        );
        assert_eq!(
            rewrite(&forced, "gpt-luna", false)["model"],
            "gpt-luna-fast"
        );
        assert_eq!(
            rewrite(&HeaderMap::new(), "gpt-luna", false)["model"],
            "gpt-luna"
        );
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
    fn fast_mode_on_a_claude_route_stays_on_that_route() {
        let mut headers = headers("session-claude-fast", None);
        headers.insert(
            INITIAL_MODEL_HEADER,
            HeaderValue::from_static("anthropic/claude-opus-5-5"),
        );
        // Claude Code sends the bare Claude ID while fast mode is on; the
        // selected Claude route must not fall through to Codex.
        let fast = rewrite(&headers, "claude-opus-5-5", true);
        assert_eq!(fast["model"], "anthropic/claude-opus-5-5");
        assert_eq!(fast["speed"], "fast");
        assert_eq!(
            rewrite(&headers, "claude-opus-5-5", false)["model"],
            "anthropic/claude-opus-5-5"
        );
        // An explicit switch to Codex still takes effect.
        let _ = rewrite(&headers, "gpt-5.6-sol", false);
        assert_eq!(
            rewrite(&headers, "claude-opus-5-5", true)["model"],
            "gpt-5.6-sol-fast"
        );
    }

    #[test]
    fn the_codex_route_never_receives_the_claude_credential() {
        let value = |value: &'static str| HeaderValue::from_static(value);
        assert_eq!(
            codex_header_value(&header::AUTHORIZATION, &value("Bearer sk-ant-oat01-secret")),
            None
        );
        assert_eq!(
            codex_header_value(&HeaderName::from_static("x-api-key"), &value("sk-ant-api")),
            None
        );
        assert_eq!(
            codex_header_value(
                &HeaderName::from_static("anthropic-beta"),
                &value("claude-code-20250219,oauth-2025-04-20, effort-2025-11-24")
            ),
            Some(value("claude-code-20250219,effort-2025-11-24"))
        );
        assert_eq!(
            codex_header_value(
                &HeaderName::from_static("anthropic-beta"),
                &value("oauth-2025-04-20")
            ),
            None
        );
        assert_eq!(
            codex_header_value(&header::CONTENT_TYPE, &value("application/json")),
            Some(value("application/json"))
        );
    }

    #[test]
    fn only_prefixed_models_are_routed_to_anthropic() {
        let mut claude =
            serde_json::to_vec(&serde_json::json!({"model": "anthropic/claude-opus-5-5"})).unwrap();
        assert!(route_to_anthropic(&mut claude).unwrap());
        assert_eq!(
            serde_json::from_slice::<Value>(&claude).unwrap()["model"],
            "claude-opus-5-5"
        );

        for model in ["gpt-5.6-sol", "claude-opus-5-5"] {
            let original = serde_json::to_vec(&serde_json::json!({"model": model})).unwrap();
            let mut bytes = original.clone();
            assert!(!route_to_anthropic(&mut bytes).unwrap(), "{model}");
            assert_eq!(bytes, original);
        }
    }

    fn auto_review_body(model: &str) -> Value {
        serde_json::json!({
            "model": model,
            "stream": false,
            "system": [{"type":"text", "text":format!("{AUTO_REVIEW_SYSTEM_PREFIX}\nPolicy") }],
            "messages": [{"role":"user", "content":"Review this action"}],
            "max_tokens": 256,
            "stop_sequences": ["</block>"]
        })
    }

    #[test]
    fn auto_review_preserves_claude_models_and_uses_sonnet_for_mapped_models() {
        for (incoming, expected) in [
            ("claude-sonnet-5[1m]", "claude-sonnet-5[1m]"),
            ("anthropic/claude-opus-5-5", "claude-opus-5-5"),
            ("gpt-6-luna", "claude-sonnet-5"),
        ] {
            let mut expected_body = auto_review_body(incoming);
            let mut bytes = serde_json::to_vec(&expected_body).unwrap();
            assert!(route_auto_review_to_anthropic(&mut bytes).unwrap());
            expected_body["model"] = Value::String(expected.to_string());
            assert_eq!(
                serde_json::from_slice::<Value>(&bytes).unwrap(),
                expected_body
            );
        }
    }

    #[test]
    fn ordinary_requests_do_not_become_anthropic_classifier_calls() {
        let classifier = auto_review_body("gpt-6-luna");
        let mut streamed = classifier.clone();
        streamed["stream"] = Value::Bool(true);
        let mut with_tools = classifier.clone();
        with_tools["tools"] = serde_json::json!([{"name":"Bash"}]);
        let mut quoted = classifier.clone();
        quoted["system"][0]["text"] = Value::String(format!("Quoted: {AUTO_REVIEW_SYSTEM_PREFIX}"));
        let mut no_model = classifier;
        no_model.as_object_mut().unwrap().remove("model");
        for body in [streamed, with_tools, quoted, no_model, Value::Null] {
            let original = serde_json::to_vec(&body).unwrap();
            let mut bytes = original.clone();
            assert!(!route_auto_review_to_anthropic(&mut bytes).unwrap());
            assert_eq!(bytes, original);
        }
    }

    /// Accepts `count` requests, capturing each one's headers and JSON body.
    fn capture_upstream(
        count: usize,
    ) -> (u16, mpsc::Receiver<(String, Value)>, thread::JoinHandle<()>) {
        capture_upstream_response(
            count,
            b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 11\r\nConnection: close\r\n\r\n{\"ok\":true}".to_vec(),
        )
    }

    fn capture_upstream_response(
        count: usize,
        response: Vec<u8>,
    ) -> (u16, mpsc::Receiver<(String, Value)>, thread::JoinHandle<()>) {
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let port = listener.local_addr().unwrap().port();
        let (captured_tx, captured_rx) = mpsc::channel();
        let handle = thread::spawn(move || {
            for _ in 0..count {
                let (mut stream, _) = listener.accept().unwrap();
                let mut request = Vec::new();
                let mut chunk = [0_u8; 4096];
                let header_end = loop {
                    let read = stream.read(&mut chunk).unwrap();
                    assert!(read > 0);
                    request.extend_from_slice(&chunk[..read]);
                    if let Some(position) = request.windows(4).position(|part| part == b"\r\n\r\n")
                    {
                        break position + 4;
                    }
                };
                let headers = String::from_utf8_lossy(&request[..header_end]).to_lowercase();
                let length = headers
                    .lines()
                    .find_map(|line| line.strip_prefix("content-length:"))
                    .and_then(|value| value.trim().parse::<usize>().ok())
                    .unwrap_or(0);
                while request.len() < header_end + length {
                    let read = stream.read(&mut chunk).unwrap();
                    assert!(read > 0);
                    request.extend_from_slice(&chunk[..read]);
                }
                let body = serde_json::from_slice(&request[header_end..header_end + length])
                    .unwrap_or(Value::Null);
                captured_tx.send((headers, body)).unwrap();
                stream.write_all(&response).unwrap();
            }
        });
        (port, captured_rx, handle)
    }

    #[test]
    fn http_claude_server_review_fields_and_responses_pass_through() {
        // Treat review fields as opaque: their schema belongs to Anthropic,
        // and future keys and events must survive without interpretation.
        let results = serde_json::json!({"safeguard_results":{"future_field":["opaque"]}});
        let json = results.to_string();
        let sse = format!(
            "event: ping\ndata: {{\"type\":\"ping\"}}\n\nevent: message_delta\ndata: {json}\n\nevent: message_stop\ndata: {{\"type\":\"message_stop\"}}\n\n"
        );
        for (content_type, payload) in [("application/json", json), ("text/event-stream", sse)] {
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{payload}",
                payload.len()
            );
            let (port, captured, upstream) = capture_upstream_response(1, response.into_bytes());
            let bridge =
                FastBridge::start_with(0, format!("http://127.0.0.1:{port}"), false, 0).unwrap();
            let mut expected = serde_json::json!({
                "model":"anthropic/claude-opus-5-5",
                "messages":[{"role":"user","content":"test"}],
                "stream":content_type == "text/event-stream",
                "safeguards":{"future_field":["opaque"]},
                "future_request_field":{"nested":true}
            });
            let actual = reqwest::blocking::Client::new()
                .post(format!(
                    "http://127.0.0.1:{}/v1/messages?beta=true",
                    bridge.port()
                ))
                .header("anthropic-beta", "oauth-2025-04-20,future-safety-beta")
                .header(header::AUTHORIZATION, "Bearer test-claude-credential")
                .json(&expected)
                .send()
                .unwrap();
            assert_eq!(actual.headers()[header::CONTENT_TYPE], content_type);
            assert_eq!(actual.bytes().unwrap().as_ref(), payload.as_bytes());
            expected["model"] = Value::String("claude-opus-5-5".to_string());
            let (headers, body) = captured.recv_timeout(TEST_TIMEOUT).unwrap();
            assert_eq!(body, expected);
            assert!(headers.contains("anthropic-beta: oauth-2025-04-20,future-safety-beta"));
            assert!(headers.contains("authorization: bearer test-claude-credential"));
            drop(bridge);
            upstream.join().unwrap();
        }
    }

    #[test]
    fn http_bridge_splits_traffic_between_codex_and_anthropic() {
        let (codex_port, codex_rx, codex) = capture_upstream(2);
        let (anthropic_port, anthropic_rx, anthropic) = capture_upstream(2);
        let bridge = FastBridge::start_with(
            codex_port,
            format!("http://127.0.0.1:{anthropic_port}"),
            false,
            0,
        )
        .unwrap();
        let client = reqwest::blocking::Client::new();
        let send = |path: &str, model: &str| {
            let response = client
                .post(format!("http://127.0.0.1:{}{path}", bridge.port()))
                .header(SESSION_HEADER, "split-session")
                .header(BRIDGE_HEADER, BRIDGE_HEADER_VALUE)
                .header(INITIAL_MODEL_HEADER, "anthropic/claude-opus-5-5")
                .header(header::AUTHORIZATION, "Bearer sk-ant-oat01-secret")
                .header("anthropic-beta", "oauth-2025-04-20,effort-2025-11-24")
                .json(&serde_json::json!({
                    "model": model,
                    "messages": [{"role":"user","content":"test"}],
                    "tools": [{"name":"Bash","input_schema":{"type":"object"}}]
                }))
                .send()
                .unwrap();
            assert!(response.status().is_success());
        };

        send("/v1/messages?beta=true", "anthropic/claude-opus-5-5");
        send(
            "/v1/messages/count_tokens?beta=true",
            "anthropic/claude-opus-5-5",
        );
        send("/v1/messages?beta=true", "gpt-5.6-sol");
        send("/v1/messages/count_tokens?beta=true", "gpt-5.6-sol");

        for _ in 0..2 {
            let (headers, body) = anthropic_rx.recv_timeout(TEST_TIMEOUT).unwrap();
            assert_eq!(body["model"], "claude-opus-5-5");
            assert!(headers.contains("authorization: bearer sk-ant-oat01-secret"));
            assert!(headers.contains("anthropic-beta: oauth-2025-04-20,effort-2025-11-24"));
            assert!(!headers.contains("x-clodex-"));
        }
        for _ in 0..2 {
            let (headers, body) = codex_rx.recv_timeout(TEST_TIMEOUT).unwrap();
            assert_eq!(body["model"], "gpt-5.6-sol");
            assert!(
                !headers.contains("sk-ant-"),
                "Claude credential reached Codex"
            );
            assert!(!headers.contains("authorization:"));
            assert!(!headers.contains("oauth-2025-04-20"));
            assert!(headers.contains("anthropic-beta: effort-2025-11-24"));
        }

        drop(bridge);
        codex.join().unwrap();
        anthropic.join().unwrap();
    }

    #[test]
    fn http_auto_review_uses_anthropic_during_native_and_session_fast() {
        for session_fast in [false, true] {
            let (codex_port, codex_rx, codex) = capture_upstream(2);
            let (anthropic_port, anthropic_rx, anthropic) = capture_upstream(2);
            let bridge = FastBridge::start_with(
                codex_port,
                format!("http://127.0.0.1:{anthropic_port}"),
                false,
                0,
            )
            .unwrap();
            let client = reqwest::blocking::Client::new();
            let send = |body: &Value| {
                let mut request = client
                    .post(format!(
                        "http://127.0.0.1:{}/v1/messages?beta=true",
                        bridge.port()
                    ))
                    .header(SESSION_HEADER, format!("judge-fast-{session_fast}"))
                    .header(BRIDGE_HEADER, BRIDGE_HEADER_VALUE)
                    .header(INITIAL_MODEL_HEADER, "gpt-6-sol")
                    .header(header::AUTHORIZATION, "Bearer sk-ant-oat01-test")
                    .header("anthropic-beta", "oauth-2025-04-20");
                if session_fast {
                    request = request.header(
                        SESSION_FAST_HEADER,
                        r#"{"gpt-6-sol":"gpt-6-sol-fast","claude-sonnet-5":"gpt-6-sol-fast"}"#,
                    );
                }
                assert!(request.json(body).send().unwrap().status().is_success());
            };
            let conversation = serde_json::json!({
                "model":"gpt-6-sol", "speed":"fast", "stream":true,
                "messages":[{"role":"user","content":"test"}],
                "tools":[{"name":"Bash","input_schema":{"type":"object"}}]
            });
            send(&conversation);
            for model in ["claude-sonnet-5", "gpt-6-luna"] {
                let mut expected = auto_review_body(model);
                send(&expected);
                let (headers, body) = anthropic_rx.recv_timeout(TEST_TIMEOUT).unwrap();
                expected["model"] = Value::String("claude-sonnet-5".to_string());
                assert_eq!(body, expected);
                assert!(headers.contains("authorization: bearer sk-ant-oat01-test"));
                assert!(headers.contains("anthropic-beta: oauth-2025-04-20"));
                assert!(!headers.contains("x-clodex-"));
            }
            // A classifier must not change the conversation's selected model.
            send(&conversation);
            for _ in 0..2 {
                let (headers, body) = codex_rx.recv_timeout(TEST_TIMEOUT).unwrap();
                assert_eq!(body["model"], "gpt-6-sol-fast");
                assert!(!headers.contains("sk-ant-"));
                assert!(!headers.contains("oauth-"));
            }
            drop(bridge);
            codex.join().unwrap();
            anthropic.join().unwrap();
        }
    }

    #[test]
    fn http_session_fast_covers_subagents_and_keeps_other_sessions_isolated() {
        let (codex_port, codex_rx, codex) = capture_upstream(6);
        let (anthropic_port, anthropic_rx, anthropic) = capture_upstream(1);
        let bridge = FastBridge::start_with(
            codex_port,
            format!("http://127.0.0.1:{anthropic_port}"),
            false,
            0,
        )
        .unwrap();
        assert!(supports_session_fast(bridge.port()));
        let url = format!("http://127.0.0.1:{}/v1/messages", bridge.port());
        let client = reqwest::blocking::Client::new();
        for (session, agent, marked, forced, model, expected) in [
            ("fast-http", None, true, true, "gpt-astra", "gpt-astra-fast"),
            (
                "fast-http",
                Some("agent-a"),
                true,
                true,
                "gpt-luna",
                "gpt-luna-fast",
            ),
            (
                "fast-http",
                Some("agent-a"),
                true,
                true,
                "gpt-astra",
                "gpt-astra-fast",
            ),
            (
                "fast-http",
                Some("agent-b"),
                true,
                true,
                "gpt-unsupported",
                "gpt-unsupported",
            ),
            (
                "standard-http",
                Some("agent-a"),
                true,
                false,
                "gpt-luna",
                "gpt-luna",
            ),
            ("unmarked-http", None, false, true, "gpt-astra", "gpt-astra"),
            (
                "fast-http",
                Some("claude-agent"),
                true,
                true,
                "anthropic/claude-opus-5-5",
                "claude-opus-5-5",
            ),
        ] {
            // Claude Code doesn't send speed:fast for an explicit Codex agent.
            let mut request = client
                .post(&url)
                .header(SESSION_HEADER, session)
                .json(&serde_json::json!({"model":model,"messages":[]}));
            if marked {
                request = request
                    .header(BRIDGE_HEADER, BRIDGE_HEADER_VALUE)
                    .header(INITIAL_MODEL_HEADER, "gpt-astra");
            }
            if forced {
                request = request.header(
                    SESSION_FAST_HEADER,
                    r#"{"gpt-astra":"gpt-astra-fast","gpt-luna":"gpt-luna-fast"}"#,
                );
            }
            if let Some(agent) = agent {
                request = request.header(AGENT_HEADER, agent);
            }
            assert!(request.send().unwrap().status().is_success());
            let receiver = if model.starts_with(ANTHROPIC_PREFIX) {
                &anthropic_rx
            } else {
                &codex_rx
            };
            let (headers, body) = receiver.recv_timeout(TEST_TIMEOUT).unwrap();
            assert_eq!(body["model"], expected);
            assert!(body.get("speed").is_none());
            assert!(!headers.contains("x-clodex-"));
        }
        drop(bridge);
        codex.join().unwrap();
        anthropic.join().unwrap();
    }

    #[test]
    fn model_discovery_is_declined_so_the_launch_list_survives() {
        let bridge = FastBridge::start(1, false, 0).unwrap();
        let response = reqwest::blocking::Client::new()
            .get(format!(
                "http://127.0.0.1:{}/v1/models?limit=1000",
                bridge.port()
            ))
            .send()
            .unwrap();
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }

    #[test]
    fn marker_is_clodex_specific() {
        assert_ne!(BRIDGE_HEADER, "authorization");
        assert_eq!(
            custom_headers("gpt-5.6-terra"),
            "X-Clodex-Fast-Bridge: 1\nX-Clodex-Initial-Model: gpt-5.6-terra"
        );
    }

    const TEST_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

    /// Reads a whole request before responding. Answering early closes the
    /// socket while the client is still writing, which surfaces as a client
    /// write error rather than the status under test.
    fn drain_request(stream: &mut std::net::TcpStream) {
        let mut request = Vec::new();
        let mut chunk = [0u8; 8192];
        loop {
            let count = stream.read(&mut chunk).unwrap();
            assert!(count > 0, "upstream connection closed mid-request");
            request.extend_from_slice(&chunk[..count]);
            let Some(header_end) = request
                .windows(4)
                .position(|window| window == b"\r\n\r\n")
                .map(|position| position + 4)
            else {
                continue;
            };
            let headers = String::from_utf8_lossy(&request[..header_end]).to_lowercase();
            let length: usize = headers
                .lines()
                .find_map(|line| line.strip_prefix("content-length:"))
                .and_then(|value| value.trim().parse().ok())
                .unwrap_or(0);
            if request.len() >= header_end + length {
                return;
            }
        }
    }

    #[test]
    fn a_transient_upstream_502_is_retried_before_the_client_sees_it() {
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let upstream_port = listener.local_addr().unwrap().port();
        let (attempts_tx, attempts_rx) = mpsc::channel();

        // Fails the first attempt the way a dropped pooled connection does,
        // then succeeds -- exactly the pattern seen against Codex.
        let upstream = thread::spawn(move || {
            for attempt in 0..2 {
                let mut stream = listener.accept().unwrap().0;
                drain_request(&mut stream);
                let response: &[u8] = if attempt == 0 {
                    b"HTTP/1.1 502 Bad Gateway\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{}"
                } else {
                    b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 11\r\nConnection: close\r\n\r\n{\"ok\":true}"
                };
                stream.write_all(response).unwrap();
                attempts_tx.send(attempt).unwrap();
            }
        });

        let bridge = FastBridge::start(upstream_port, false, 0).unwrap();
        let response = reqwest::blocking::Client::new()
            .post(format!("http://127.0.0.1:{}/v1/messages", bridge.port()))
            .header(SESSION_HEADER, "retry-session")
            .json(&serde_json::json!({"model": "gpt-5.6-sol", "messages": []}))
            .send()
            .unwrap();

        assert_eq!(
            response.status(),
            200,
            "the transient 502 reached the client instead of being retried"
        );
        assert_eq!(attempts_rx.recv_timeout(TEST_TIMEOUT).unwrap(), 0);
        assert_eq!(
            attempts_rx.recv_timeout(TEST_TIMEOUT).unwrap(),
            1,
            "the request was not replayed"
        );

        drop(bridge);
        upstream.join().unwrap();
    }

    #[test]
    fn a_persistent_upstream_502_is_surfaced_rather_than_retried_forever() {
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let upstream_port = listener.local_addr().unwrap().port();
        let (attempts_tx, attempts_rx) = mpsc::channel();

        let upstream = thread::spawn(move || {
            for _ in 0..(UPSTREAM_RETRIES + 1) {
                let mut stream = listener.accept().unwrap().0;
                drain_request(&mut stream);
                stream
                    .write_all(
                        b"HTTP/1.1 502 Bad Gateway\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{}",
                    )
                    .unwrap();
                attempts_tx.send(()).unwrap();
            }
        });

        let bridge = FastBridge::start(upstream_port, false, 0).unwrap();
        let response = reqwest::blocking::Client::new()
            .post(format!("http://127.0.0.1:{}/v1/messages", bridge.port()))
            .header(SESSION_HEADER, "persistent-session")
            .json(&serde_json::json!({"model": "gpt-5.6-sol", "messages": []}))
            .send()
            .unwrap();

        assert_eq!(response.status(), 502);
        for _ in 0..(UPSTREAM_RETRIES + 1) {
            attempts_rx.recv_timeout(TEST_TIMEOUT).unwrap();
        }
        assert!(attempts_rx.recv_timeout(TEST_TIMEOUT).is_err());

        drop(bridge);
        upstream.join().unwrap();
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

        let bridge = FastBridge::start(upstream_port, false, 0).unwrap();
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
