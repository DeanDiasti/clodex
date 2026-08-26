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

const BRIDGE_HEADER: &str = "x-clodex-fast-bridge";
const BRIDGE_HEADER_VALUE: &str = "1";
const INITIAL_MODEL_HEADER: &str = "x-clodex-initial-model";
const SESSION_HEADER: &str = "x-claude-code-session-id";
const AGENT_HEADER: &str = "x-claude-code-agent-id";
const MAX_REQUEST_BYTES: usize = 128 * 1024 * 1024;
const MAX_TRACKED_ROUTES: usize = 16_384;
/// How long a `PreCompact` hook keeps a session armed. Long enough for Claude
/// Code to assemble and send the compaction it just announced, short enough
/// that a cancelled compaction does not leave a session armed indefinitely.
const ARM_TTL: std::time::Duration = std::time::Duration::from_secs(120);
/// How long a usage reading is reused before refetching. Claude Code redraws
/// its status line far more often than a subscription window moves.
const USAGE_TTL: std::time::Duration = std::time::Duration::from_secs(60);
/// Concurrent token-count requests while planning a fold.
const COUNT_CONCURRENCY: usize = 8;
/// How often to ping while a round is in flight. A round can run for minutes,
/// and Claude Code drops a stream that goes idle.
const PING_INTERVAL: std::time::Duration = std::time::Duration::from_secs(15);

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
    hierarchical: bool,
    ceiling: u64,
    report_usage: bool,
}

pub struct FastBridge {
    port: u16,
    shutdown: Option<oneshot::Sender<()>>,
    thread: Option<thread::JoinHandle<Result<()>>>,
}

impl FastBridge {
    /// `ceiling` is the capacity a fold round must fit inside; zero, or
    /// `hierarchical` unset, leaves every request forwarded untouched.
    pub fn start(
        upstream_port: u16,
        hierarchical: bool,
        ceiling: u64,
        report_usage: bool,
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
                    client: reqwest::Client::builder()
                        .build()
                        .context("could not create the Clodex fast bridge client")?,
                    hierarchical,
                    ceiling,
                    report_usage,
                });
                let app = Router::new()
                    .route("/__clodex/health", get(health))
                    .route("/__clodex/compaction/arm", post(arm_compaction))
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

/// Current Codex usage, refetched at most once per `USAGE_TTL`.
async fn cached_rate_limits(state: &BridgeState) -> Option<crate::usage::RateLimits> {
    {
        let cache = usage_cache().lock().expect("Clodex usage lock");
        if let Some((limits, at)) = cache.as_ref()
            && std::time::Instant::now().duration_since(*at) < USAGE_TTL
        {
            return *limits;
        }
    }

    let fetched = crate::usage::fetch(&state.client).await;
    let mut cache = usage_cache().lock().expect("Clodex usage lock");
    // Cache misses too, so a failing endpoint is not retried on every request.
    *cache = Some((fetched, std::time::Instant::now()));
    fetched
}

type UsageCache = Option<(Option<crate::usage::RateLimits>, std::time::Instant)>;

fn usage_cache() -> &'static Mutex<UsageCache> {
    static CACHE: std::sync::OnceLock<Mutex<UsageCache>> = std::sync::OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(None))
}

/// Adds the rate-limit headers Claude Code renders its status bars from.
///
/// Behind a custom base URL these never arrive, so the bars vanish even though
/// the session is spending a real Codex quota. Supplying them in the shape
/// Claude Code already parses means an existing status line keeps working.
fn apply_rate_limit_headers(response: &mut Response, limits: &crate::usage::RateLimits) {
    for (name, value) in limits.headers() {
        if let (Ok(name), Ok(value)) = (
            HeaderName::from_bytes(name.as_bytes()),
            axum::http::HeaderValue::from_str(&value),
        ) {
            response.headers_mut().insert(name, value);
        }
    }
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
    if is_messages
        && let Some(response) = hierarchical_compaction(state, &parts.headers, &bytes).await
    {
        return Ok(response);
    }
    if marked && is_messages {
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

    // Fetched before the response is built so the status line reflects the
    // quota this very request is spending.
    let rate_limits = if state.report_usage && is_messages {
        cached_rate_limits(state).await
    } else {
        None
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
    if let Some(limits) = &rate_limits {
        apply_rate_limit_headers(&mut response, limits);
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
            client: reqwest::Client::new(),
            hierarchical: true,
            ceiling: 828_400,
            report_usage: false,
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
            client: reqwest::Client::new(),
            hierarchical: false,
            ceiling: 828_400,
            report_usage: false,
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
            client: reqwest::Client::new(),
            hierarchical: true,
            ceiling: 828_400,
            report_usage: false,
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

        let bridge = FastBridge::start(upstream_port, false, 0, false).unwrap();
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
