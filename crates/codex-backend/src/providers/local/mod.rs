//! A provider for any local OpenAI-compatible chat server: ollama, the
//! mlx_lm server, llama.cpp's `llama-server`, vLLM. It needs no sign-in, lists
//! the models the configuration names, and translates Claude Code's Anthropic
//! Messages requests to `POST {baseUrl}/chat/completions` and the streamed
//! reply back.

pub mod translate;

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
    api_key: Option<String>,
    models: Vec<LocalModel>,
    client: reqwest::Client,
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
        Self {
            base_url: config.base_url.trim_end_matches('/').to_string(),
            api_key: config.api_key,
            models: config.models,
            client,
        }
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

    async fn send(
        &self,
        base_url: &str,
        chat: &Value,
        traffic: Option<&TrafficCapture>,
    ) -> Result<reqwest::Response, Box<Response>> {
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
        let response = request.send().await.map_err(|error| {
            Box::new(json_error(
                StatusCode::BAD_GATEWAY,
                "api_error",
                format!("local server at {base_url} unreachable: {error}"),
            ))
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
            return Err(Box::new(json_error(
                status,
                kind,
                format!(
                    "local server at {base_url} answered {status}: {}",
                    message.trim()
                ),
            )));
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
        let upstream = match self.send(&base_url, &chat, ctx.traffic.as_deref()).await {
            Ok(upstream) => upstream,
            Err(response) => return *response,
        };

        if !body.stream {
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
            return (StatusCode::OK, axum::Json(message)).into_response();
        }

        let translator = StreamTranslator::new(message_id, model, estimated_input);
        let traffic: Option<Arc<TrafficCapture>> = ctx.traffic.clone();
        let state = (upstream.bytes_stream(), translator, Vec::<u8>::new(), false);
        let stream = futures_util::stream::unfold(
            state,
            move |(mut upstream, mut translator, mut buffer, mut done)| {
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
                                (upstream, translator, buffer, done),
                            ));
                        }
                        match upstream.next().await {
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
                                    continue;
                                }
                                return Some((
                                    Ok(Bytes::from(out)),
                                    (upstream, translator, buffer, done),
                                ));
                            }
                            Some(Err(error)) => {
                                let out = translator
                                    .fail(&format!("local server stream failed: {error}"));
                                return Some((
                                    Ok(Bytes::from(out)),
                                    (upstream, translator, buffer, true),
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
        );
        (sse_headers(), Body::from_stream(stream)).into_response()
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
