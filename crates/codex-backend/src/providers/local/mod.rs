//! A provider for any local OpenAI-compatible chat server: ollama, the
//! mlx_lm server, llama.cpp's `llama-server`, vLLM. It needs no sign-in, lists
//! the models the configuration names, and translates Claude Code's Anthropic
//! Messages requests to `POST {baseUrl}/chat/completions` and the streamed
//! reply back.

pub mod translate;

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use axum::{
    body::Body,
    http::StatusCode,
    response::{IntoResponse, Response},
};
use bytes::Bytes;
use futures_util::StreamExt;
use serde_json::Value;
use tokio::sync::{OwnedSemaphorePermit, Semaphore, mpsc};

use crate::anthropic::error::json_error;
use crate::anthropic::schema::{CountTokensResponse, MessagesRequest};
use crate::config::{LocalModel, ResolvedLocalConfig};
use crate::provider::{CliHandlers, Provider, RequestContext};
use crate::traffic::TrafficCapture;

use translate::{
    StreamTranslator, drain_sse_payloads, estimate_tokens, to_anthropic_message, to_chat_request,
};

/// The provider name the registry and the monitor show.
pub const PROVIDER_NAME: &str = "local";

pub struct LocalProvider {
    base_url: String,
    models: Vec<LocalModel>,
    upstream: Upstream,
    gates: HashMap<String, Arc<Semaphore>>,
    ping_every: Duration,
}

/// What a request needs to reach a server; cheap to clone into a queued task.
#[derive(Clone)]
struct Upstream {
    client: reqwest::Client,
    api_key: Option<String>,
}

/// A request a local server refused or never received, before any reply
/// byte reached the client.
struct LocalError {
    status: StatusCode,
    kind: &'static str,
    message: String,
}

impl LocalError {
    fn into_response(self) -> Response {
        json_error(self.status, self.kind, self.message)
    }
}

impl LocalProvider {
    pub fn new(config: ResolvedLocalConfig) -> Self {
        Self::with_client(
            config,
            reqwest::Client::builder()
                // Loopback servers; an inherited HTTPS_PROXY must not capture them.
                .no_proxy()
                .connect_timeout(Duration::from_secs(10))
                .build()
                .unwrap_or_else(|_| reqwest::Client::new()),
        )
    }

    pub fn with_client(config: ResolvedLocalConfig, client: reqwest::Client) -> Self {
        let mut provider = Self {
            base_url: config.base_url.trim_end_matches('/').to_string(),
            models: config.models,
            upstream: Upstream {
                client,
                api_key: config.api_key,
            },
            gates: HashMap::new(),
            ping_every: PING_EVERY,
        };
        // One request at a time per server. A local server runs one model on
        // one machine; two long prompts prefilled at once can exhaust its
        // memory (the mlx_lm server dies on a Metal out-of-memory error), and
        // Claude Code sends side requests, such as a background agent's
        // progress summary, on the agent's own model.
        provider.gates = provider
            .servers()
            .into_iter()
            .map(|server| (server, Arc::new(Semaphore::new(1))))
            .collect();
        provider
    }

    /// The same provider with another quiet interval before a `ping`.
    pub fn with_ping_every(mut self, ping_every: Duration) -> Self {
        self.ping_every = ping_every;
        self
    }

    pub fn base_url(&self) -> &str {
        &self.base_url
    }

    /// The name the server receives for a listed model id and the server it
    /// lives on (its own, or the shared base URL); an unlisted id (the
    /// registry never routes one here) passes through unchanged.
    fn target(&self, id: &str) -> (String, String) {
        match self.models.iter().find(|model| model.id == id) {
            Some(model) => (
                model.upstream.clone(),
                model
                    .base_url
                    .clone()
                    .unwrap_or_else(|| self.base_url.clone()),
            ),
            None => (id.to_string(), self.base_url.clone()),
        }
    }

    /// The servers the listed models reach, the shared one first, each once.
    pub fn servers(&self) -> Vec<String> {
        let mut servers = vec![self.base_url.clone()];
        for model in &self.models {
            if let Some(url) = model.base_url.as_ref()
                && !servers.contains(url)
            {
                servers.push(url.clone());
            }
        }
        servers
    }

    /// The queue for a server: one request in flight, the rest in order.
    fn gate(&self, base_url: &str) -> Arc<Semaphore> {
        self.gates
            .get(base_url)
            .cloned()
            .unwrap_or_else(|| Arc::new(Semaphore::new(1)))
    }
}

impl Upstream {
    async fn send(
        &self,
        base_url: &str,
        chat: &Value,
        traffic: Option<&TrafficCapture>,
    ) -> Result<reqwest::Response, LocalError> {
        if let Some(traffic) = traffic {
            traffic.write_json("020-local-chat-request", chat);
        }
        let mut request = self
            .client
            .post(format!("{base_url}/chat/completions"))
            .json(chat);
        if let Some(key) = self.api_key.as_deref() {
            request = request.bearer_auth(key);
        }
        let response = request.send().await.map_err(|error| LocalError {
            status: StatusCode::BAD_GATEWAY,
            kind: "api_error",
            message: format!("local server at {base_url} unreachable: {error}"),
        })?;
        let status = response.status();
        if !status.is_success() {
            let text = response.text().await.unwrap_or_default();
            let message = serde_json::from_str::<Value>(&text)
                .ok()
                .and_then(|v| {
                    v.pointer("/error/message")
                        .or_else(|| v.get("error"))
                        .and_then(|m| m.as_str().map(str::to_string))
                })
                .unwrap_or(text);
            let (kind, status) = match status.as_u16() {
                401 | 403 => ("authentication_error", StatusCode::UNAUTHORIZED),
                404 => ("not_found_error", StatusCode::NOT_FOUND),
                400 | 413 | 422 => ("invalid_request_error", StatusCode::BAD_REQUEST),
                429 => ("rate_limit_error", StatusCode::TOO_MANY_REQUESTS),
                _ => ("api_error", StatusCode::BAD_GATEWAY),
            };
            return Err(LocalError {
                status,
                kind,
                message: format!(
                    "local server at {base_url} answered {status}: {}",
                    message.trim()
                ),
            });
        }
        Ok(response)
    }
}

struct LocalCli;

impl CliHandlers for LocalCli {
    fn login(&self) -> anyhow::Result<()> {
        println!(
            "The local provider needs no sign-in; set local.baseUrl and local.models instead."
        );
        Ok(())
    }

    fn device(&self) -> anyhow::Result<()> {
        self.login()
    }

    fn status(&self) -> anyhow::Result<()> {
        println!(
            "local provider: no credential; configured through local.baseUrl and local.models."
        );
        Ok(())
    }

    fn logout(&self) -> anyhow::Result<()> {
        Ok(())
    }
}

static LOCAL_CLI: LocalCli = LocalCli;

fn sse_headers() -> [(http::HeaderName, &'static str); 3] {
    [
        (http::header::CONTENT_TYPE, "text/event-stream"),
        (http::header::CACHE_CONTROL, "no-cache"),
        (http::header::CONNECTION, "keep-alive"),
    ]
}

/// How long the client may go without an event before a `ping` is sent. A
/// local server can spend minutes prefilling a long prompt before its first
/// token (the mlx_lm server sends only SSE comments meanwhile, llama.cpp sends
/// nothing), and Claude Code abandons a stream that is silent for about a
/// minute and retries it from the start.
pub const PING_EVERY: Duration = Duration::from_secs(10);

/// Translates the upstream chat-completions stream into Anthropic SSE events,
/// sending a `ping` whenever the client has had no event for `ping_every`.
fn relay<S, E>(
    upstream: S,
    translator: StreamTranslator,
    traffic: Option<Arc<TrafficCapture>>,
    ping_every: Duration,
) -> impl futures_util::Stream<Item = Result<Bytes, std::io::Error>> + Send + 'static
where
    S: futures_util::Stream<Item = Result<Bytes, E>> + Unpin + Send + 'static,
    E: std::fmt::Display + Send + 'static,
{
    let state = (
        upstream,
        translator,
        Vec::<u8>::new(),
        false,
        tokio::time::Instant::now(),
    );
    futures_util::stream::unfold(
        state,
        move |(mut upstream, mut translator, mut buffer, mut done, mut quiet_since)| {
            let traffic = traffic.clone();
            async move {
                if translator.is_finished() {
                    return None;
                }
                loop {
                    if done {
                        let out = translator.finish();
                        return Some((
                            Ok::<Bytes, std::io::Error>(Bytes::from(out)),
                            (upstream, translator, buffer, done, quiet_since),
                        ));
                    }
                    let next =
                        match tokio::time::timeout_at(quiet_since + ping_every, upstream.next())
                            .await
                        {
                            Ok(next) => next,
                            Err(_) => {
                                let out = translator.ping();
                                quiet_since = tokio::time::Instant::now();
                                if out.is_empty() {
                                    continue;
                                }
                                return Some((
                                    Ok(Bytes::from(out)),
                                    (upstream, translator, buffer, done, quiet_since),
                                ));
                            }
                        };
                    match next {
                        Some(Ok(chunk)) => {
                            if let Some(traffic) = traffic.as_deref() {
                                traffic.write_bytes("031-local-chat-chunk", &chunk);
                            }
                            buffer.extend_from_slice(&chunk);
                            let (payloads, finished) = drain_sse_payloads(&mut buffer);
                            done = finished;
                            let mut out = Vec::new();
                            for payload in &payloads {
                                out.extend(translator.on_chunk(payload));
                            }
                            if done {
                                out.extend(translator.finish());
                            }
                            if out.is_empty() {
                                // An SSE comment (the mlx_lm server's prefill
                                // keepalive) or a partial event: the client
                                // still has heard nothing, so the clock runs on.
                                continue;
                            }
                            quiet_since = tokio::time::Instant::now();
                            return Some((
                                Ok(Bytes::from(out)),
                                (upstream, translator, buffer, done, quiet_since),
                            ));
                        }
                        Some(Err(error)) => {
                            let out =
                                translator.fail(&format!("local server stream failed: {error}"));
                            return Some((
                                Ok(Bytes::from(out)),
                                (upstream, translator, buffer, true, quiet_since),
                            ));
                        }
                        None => {
                            // The server closed without `[DONE]`; close the message anyway.
                            done = true;
                        }
                    }
                }
            }
        },
    )
}

/// One relayed reply as the client reads it, from a task that owns the relay.
type Relayed = Result<Bytes, std::io::Error>;

fn from_task(
    replies: mpsc::Receiver<Relayed>,
) -> impl futures_util::Stream<Item = Relayed> + Send + 'static {
    futures_util::stream::unfold(replies, |mut replies| async move {
        replies.recv().await.map(|reply| (reply, replies))
    })
}

/// Runs `stream` to its end, holding the server's `turn` until then, and
/// hands each item to `client` while it listens. A client that goes away does
/// not cut the reply short: the server keeps computing it regardless, so the
/// next request in the queue waits until it has finished.
async fn drain<S>(stream: S, turn: OwnedSemaphorePermit, client: mpsc::Sender<Relayed>)
where
    S: futures_util::Stream<Item = Relayed> + Send + 'static,
{
    let mut stream = Box::pin(stream);
    let mut client = Some(client);
    while let Some(reply) = stream.next().await {
        if let Some(sender) = client.as_ref()
            && sender.send(reply).await.is_err()
        {
            client = None;
        }
    }
    drop(turn);
}

/// A reply that already has its server's turn, relayed from its own task.
fn hold_until_done<S>(
    stream: S,
    turn: OwnedSemaphorePermit,
) -> impl futures_util::Stream<Item = Relayed> + Send + 'static
where
    S: futures_util::Stream<Item = Relayed> + Send + 'static,
{
    let (client, replies) = mpsc::channel(16);
    tokio::spawn(drain(stream, turn, client));
    from_task(replies)
}

/// A request whose server is busy with another: the client gets
/// `message_start` and a `ping` every `ping_every` while it waits its turn,
/// then the reply `start` opens. A client that leaves while queued never
/// reaches the server.
fn queued<F, U, E>(
    gate: Arc<Semaphore>,
    mut translator: StreamTranslator,
    traffic: Option<Arc<TrafficCapture>>,
    ping_every: Duration,
    start: F,
) -> impl futures_util::Stream<Item = Relayed> + Send + 'static
where
    F: Future<Output = Result<U, String>> + Send + 'static,
    U: futures_util::Stream<Item = Result<Bytes, E>> + Unpin + Send + 'static,
    E: std::fmt::Display + Send + 'static,
{
    let (client, replies) = mpsc::channel(16);
    tokio::spawn(async move {
        let turn = gate.acquire_owned();
        tokio::pin!(turn);
        let turn = loop {
            tokio::select! {
                turn = &mut turn => break turn,
                () = client.closed() => return,
                () = tokio::time::sleep(ping_every) => {
                    if client.send(Ok(Bytes::from(translator.ping()))).await.is_err() {
                        return;
                    }
                }
            }
        };
        let Ok(turn) = turn else {
            let failed = translator.fail("local server queue closed");
            let _ = client.send(Ok(Bytes::from(failed))).await;
            return;
        };
        match start.await {
            Ok(upstream) => {
                let relayed = relay(upstream, translator, traffic, ping_every);
                drain(relayed, turn, client).await;
            }
            Err(message) => {
                let _ = client
                    .send(Ok(Bytes::from(translator.fail(&message))))
                    .await;
            }
        }
    });
    from_task(replies)
}

#[async_trait]
impl Provider for LocalProvider {
    fn name(&self) -> &'static str {
        PROVIDER_NAME
    }

    fn supported_models(&self) -> Vec<String> {
        let mut models: Vec<String> = self.models.iter().map(|m| m.id.clone()).collect();
        models.sort_unstable();
        models.dedup();
        models
    }

    fn cli(&self) -> &'static dyn CliHandlers {
        &LOCAL_CLI
    }

    async fn handle_messages(&self, body: MessagesRequest, ctx: RequestContext) -> Response {
        let model = body.model.clone().unwrap_or_default();
        let message_id = format!("msg_local_{}", uuid::Uuid::new_v4().simple());
        let (upstream_model, base_url) = self.target(&model);
        let chat = to_chat_request(&body, &upstream_model, body.stream);
        let estimated_input = estimate_tokens(&chat);
        if let Some(monitor) = ctx.monitor.as_ref() {
            monitor.model_resolved(&ctx.req_id, &model);
        }
        let gate = self.gate(&base_url);
        if body.stream {
            let translator = StreamTranslator::new(message_id, model, estimated_input);
            let stream = match gate.clone().try_acquire_owned() {
                Ok(turn) => {
                    let upstream = match self
                        .upstream
                        .send(&base_url, &chat, ctx.traffic.as_deref())
                        .await
                    {
                        Ok(upstream) => upstream,
                        Err(error) => return error.into_response(),
                    };
                    let relayed = relay(
                        upstream.bytes_stream(),
                        translator,
                        ctx.traffic.clone(),
                        self.ping_every,
                    );
                    hold_until_done(relayed, turn).boxed()
                }
                Err(_) => {
                    let sender = self.upstream.clone();
                    let traffic = ctx.traffic.clone();
                    let start = async move {
                        sender
                            .send(&base_url, &chat, traffic.as_deref())
                            .await
                            .map(|upstream| upstream.bytes_stream())
                            .map_err(|error| error.message)
                    };
                    queued(
                        gate,
                        translator,
                        ctx.traffic.clone(),
                        self.ping_every,
                        start,
                    )
                    .boxed()
                }
            };
            return (sse_headers(), Body::from_stream(stream)).into_response();
        }

        // A reply without a stream has nothing to keep the client waiting
        // with, so it simply waits its turn.
        let _turn = gate.acquire_owned().await;
        let upstream = match self
            .upstream
            .send(&base_url, &chat, ctx.traffic.as_deref())
            .await
        {
            Ok(upstream) => upstream,
            Err(error) => return error.into_response(),
        };
        let text = match upstream.text().await {
            Ok(text) => text,
            Err(error) => {
                return json_error(
                    StatusCode::BAD_GATEWAY,
                    "api_error",
                    format!("local server reply unreadable: {error}"),
                );
            }
        };
        let parsed: Value = match serde_json::from_str(&text) {
            Ok(value) => value,
            Err(error) => {
                return json_error(
                    StatusCode::BAD_GATEWAY,
                    "api_error",
                    format!("local server reply is not JSON: {error}"),
                );
            }
        };
        if let Some(traffic) = ctx.traffic.as_deref() {
            traffic.write_json("030-local-chat-response", &parsed);
        }
        let message = to_anthropic_message(&parsed, &message_id, &model);
        if let Some(monitor) = ctx.monitor.as_ref() {
            monitor.usage_updated(
                &ctx.req_id,
                message["usage"]["input_tokens"].as_u64(),
                message["usage"]["output_tokens"].as_u64(),
            );
        }
        (StatusCode::OK, axum::Json(message)).into_response()
    }

    async fn handle_count_tokens(&self, body: MessagesRequest, ctx: RequestContext) -> Response {
        let model = body.model.clone().unwrap_or_default();
        let (upstream_model, _) = self.target(&model);
        let tokens = estimate_tokens(&to_chat_request(&body, &upstream_model, false));
        if let Some(monitor) = ctx.monitor.as_ref() {
            monitor.usage_updated(&ctx.req_id, Some(tokens), None);
        }
        (
            StatusCode::OK,
            axum::Json(CountTokensResponse {
                input_tokens: tokens,
            }),
        )
            .into_response()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::anthropic::sse::parse_sse_events;

    fn names(bytes: &[u8]) -> Vec<String> {
        parse_sse_events(bytes)
            .into_iter()
            .map(|event| event.event.unwrap_or_default())
            .collect()
    }

    /// An upstream that sends only SSE comments for `quiet` (a prompt being
    /// prefilled, as the mlx_lm server does every four seconds), then a reply.
    fn prefilling_upstream(
        quiet: Duration,
    ) -> impl futures_util::Stream<Item = Result<Bytes, std::io::Error>> + Unpin + Send + 'static
    {
        let comments = (quiet.as_secs() / 4) as usize;
        Box::pin(futures_util::stream::unfold(
            0usize,
            move |step| async move {
                let chunk: &'static [u8] = if step < comments {
                    tokio::time::sleep(Duration::from_secs(4)).await;
                    b": keepalive 512/27000\n\n"
                } else if step == comments {
                    b"data: {\"choices\":[{\"delta\":{\"content\":\"ok\"},\"finish_reason\":\"stop\"}]}\n\ndata: [DONE]\n\n"
                } else {
                    return None;
                };
                Some((Ok(Bytes::from_static(chunk)), step + 1))
            },
        ))
    }

    #[tokio::test(start_paused = true)]
    async fn a_quiet_upstream_still_gets_pings_through_its_comments() {
        let translator = StreamTranslator::new("msg_r".into(), "wollf".into(), 27_000);
        let stream = relay(
            prefilling_upstream(Duration::from_secs(25)),
            translator,
            None,
            Duration::from_secs(10),
        );
        let chunks: Vec<Bytes> = stream.map(|chunk| chunk.unwrap()).collect().await;
        let bytes: Vec<u8> = chunks.concat();
        let names = names(&bytes);
        let pings = names.iter().filter(|name| *name == "ping").count();
        assert_eq!(names[0], "message_start");
        assert_eq!(
            pings, 2,
            "a ping at 10 s and 20 s of a 24 s prefill: {names:?}"
        );
        assert_eq!(names.last().map(String::as_str), Some("message_stop"));
        assert!(names.contains(&"content_block_delta".to_string()));
    }

    use std::sync::Mutex;

    async fn reply_after_turn(
        opened: Arc<Mutex<Option<tokio::time::Instant>>>,
    ) -> Result<
        impl futures_util::Stream<Item = Result<Bytes, std::io::Error>> + Unpin + Send + 'static,
        String,
    > {
        *opened.lock().unwrap() = Some(tokio::time::Instant::now());
        Ok(prefilling_upstream(Duration::ZERO))
    }

    #[tokio::test(start_paused = true)]
    async fn a_queued_request_pings_until_the_server_is_free() {
        let gate = Arc::new(Semaphore::new(1));
        let busy = gate.clone().try_acquire_owned().unwrap();
        let begun = tokio::time::Instant::now();
        let opened = Arc::new(Mutex::new(None));
        let stream = queued(
            gate.clone(),
            StreamTranslator::new("msg_w".into(), "wollf".into(), 27_000),
            None,
            Duration::from_secs(10),
            reply_after_turn(opened.clone()),
        );
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_secs(25)).await;
            drop(busy);
        });
        let chunks: Vec<Bytes> = stream.map(|chunk| chunk.unwrap()).collect().await;
        let names = names(&chunks.concat());
        let opened_at = opened.lock().unwrap().expect("the server was asked");
        assert!(
            opened_at - begun >= Duration::from_secs(25),
            "the server was asked before the first reply ended"
        );
        assert_eq!(names[0], "message_start");
        let pings = names.iter().filter(|name| *name == "ping").count();
        assert_eq!(
            pings, 2,
            "a ping at 10 s and 20 s of a 25 s wait: {names:?}"
        );
        assert!(names.contains(&"content_block_delta".to_string()));
        assert_eq!(names.last().map(String::as_str), Some("message_stop"));
        assert_eq!(gate.available_permits(), 1);
    }

    #[tokio::test(start_paused = true)]
    async fn a_client_gone_while_queued_never_reaches_the_server() {
        let gate = Arc::new(Semaphore::new(1));
        let busy = gate.clone().try_acquire_owned().unwrap();
        let opened = Arc::new(Mutex::new(None));
        let mut stream = Box::pin(queued(
            gate.clone(),
            StreamTranslator::new("msg_g".into(), "wollf".into(), 27_000),
            None,
            Duration::from_secs(10),
            reply_after_turn(opened.clone()),
        ));
        let first = stream.next().await.unwrap().unwrap();
        assert_eq!(names(&first), ["message_start", "ping"]);
        drop(stream);
        tokio::time::sleep(Duration::from_secs(1)).await;
        drop(busy);
        tokio::time::sleep(Duration::from_secs(30)).await;
        assert!(
            opened.lock().unwrap().is_none(),
            "the server was asked after its client left the queue"
        );
        assert_eq!(gate.available_permits(), 1);
    }

    #[tokio::test(start_paused = true)]
    async fn a_client_gone_mid_reply_keeps_the_turn_until_the_reply_ends() {
        let gate = Arc::new(Semaphore::new(1));
        let turn = gate.clone().try_acquire_owned().unwrap();
        let relayed = relay(
            prefilling_upstream(Duration::from_secs(25)),
            StreamTranslator::new("msg_m".into(), "wollf".into(), 27_000),
            None,
            Duration::from_secs(10),
        );
        let mut stream = Box::pin(hold_until_done(relayed, turn));
        let first = stream.next().await.unwrap().unwrap();
        assert!(names(&first).contains(&"ping".to_string()));
        drop(stream);
        tokio::time::sleep(Duration::from_secs(5)).await;
        assert_eq!(
            gate.available_permits(),
            0,
            "the server is still computing the abandoned reply"
        );
        tokio::time::sleep(Duration::from_secs(30)).await;
        assert_eq!(gate.available_permits(), 1);
    }

    #[tokio::test(start_paused = true)]
    async fn a_prompt_upstream_gets_no_ping() {
        let translator = StreamTranslator::new("msg_q".into(), "wollf".into(), 10);
        let stream = relay(
            prefilling_upstream(Duration::ZERO),
            translator,
            None,
            Duration::from_secs(10),
        );
        let chunks: Vec<Bytes> = stream.map(|chunk| chunk.unwrap()).collect().await;
        let names = names(&chunks.concat());
        assert!(!names.contains(&"ping".to_string()), "{names:?}");
        assert_eq!(names.first().map(String::as_str), Some("message_start"));
    }
}
