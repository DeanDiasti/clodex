//! Session-scoped HTTPS proxy. Only api.anthropic.com is decrypted; only the
//! two model endpoints enter the existing bridge. Everything else retains its
//! Anthropic destination, including Remote Control HTTP and WebSocket traffic.
use std::{
    convert::Infallible, io::Write, net::TcpListener, path::Path, process::Command, sync::Arc,
    thread, time::Duration,
};

use anyhow::{Context, Result, bail};
use axum::{
    body::Body,
    http::{Method, Request, Response, StatusCode, header},
};
use base64::Engine;
use hyper::{body::Incoming, server::conn::http1, service::service_fn};
use hyper_util::rt::TokioIo;
use rcgen::{BasicConstraints, CertificateParams, DnType, IsCa, Issuer, KeyPair, KeyUsagePurpose};
use rustls::pki_types::PrivatePkcs8KeyDer;
use sha2::{Digest, Sha256};
use tokio::{net::TcpStream, sync::oneshot, time::timeout};
use tokio_rustls::TlsAcceptor;

const API_HOST: &str = "api.anthropic.com";
const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);

pub struct RemoteProxy {
    port: u16,
    password: String,
    ca: tempfile::NamedTempFile,
    tool_env: crate::tool_env::ToolEnvironment,
    shutdown: Option<oneshot::Sender<()>>,
    thread: Option<thread::JoinHandle<()>>,
}

struct State {
    bridge_port: u16,
    external_base: String,
    authorization: String,
    tls: TlsAcceptor,
    local: reqwest::Client,
    external: reqwest::Client,
}

impl RemoteProxy {
    pub fn start(bridge_port: u16) -> Result<Self> {
        // Do not silently bypass an organization's outbound proxy. Chaining
        // CONNECT through arbitrary authenticated proxies is not supported yet.
        for key in [
            "https_proxy",
            "HTTPS_PROXY",
            "http_proxy",
            "HTTP_PROXY",
            "all_proxy",
            "ALL_PROXY",
        ] {
            if std::env::var_os(key).is_some_and(|v| !v.is_empty()) {
                bail!(
                    "Remote Control transport cannot chain the existing {key}; use `clodex config remote-control off` to keep your existing proxy configuration"
                );
            }
        }
        Self::start_inner(bridge_port, &format!("https://{API_HOST}"))
    }

    fn start_inner(bridge_port: u16, external_base: &str) -> Result<Self> {
        let (tls, pem, password) = certificate()?;
        let mut ca = tempfile::Builder::new()
            .prefix("clodex-remote-ca-")
            .suffix(".pem")
            .tempfile()?;
        // Preserve extra corporate roots without changing the system store.
        if let Some(path) = std::env::var_os("NODE_EXTRA_CA_CERTS") {
            ca.write_all(&std::fs::read(path).context("could not read NODE_EXTRA_CA_CERTS")?)?;
            ca.write_all(b"\n")?;
        }
        ca.write_all(pem.as_bytes())?;
        ca.flush()?;
        let listener = TcpListener::bind(("127.0.0.1", 0))?;
        listener.set_nonblocking(true)?;
        let port = listener.local_addr()?.port();
        let tool_env = crate::tool_env::ToolEnvironment::new(
            &format!("http://clodex:{password}@127.0.0.1:{port}"),
            ca.path(),
        )?;
        let state = Arc::new(State {
            bridge_port,
            external_base: external_base.to_owned(),
            authorization: format!(
                "Basic {}",
                base64::engine::general_purpose::STANDARD.encode(format!("clodex:{password}"))
            ),
            tls,
            local: reqwest::Client::builder()
                .no_proxy()
                .http1_only()
                .redirect(reqwest::redirect::Policy::none())
                .build()?,
            external: reqwest::Client::builder()
                .no_proxy()
                .http1_only()
                .connect_timeout(CONNECT_TIMEOUT)
                .redirect(reqwest::redirect::Policy::none())
                .build()?,
        });
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()?;
        let (shutdown, stop) = oneshot::channel();
        let thread = thread::spawn(move || runtime.block_on(serve(listener, state, stop)));
        Ok(Self {
            port,
            password,
            ca,
            tool_env,
            shutdown: Some(shutdown),
            thread: Some(thread),
        })
    }

    pub fn configure(&self, command: &mut Command) {
        configure(command, self.port, &self.password, self.ca.path());
        self.tool_env.configure(command);
    }
}

async fn serve(listener: TcpListener, state: Arc<State>, mut stop: oneshot::Receiver<()>) {
    let Ok(listener) = tokio::net::TcpListener::from_std(listener) else {
        return;
    };
    loop {
        tokio::select! {
            _ = &mut stop => break,
            accepted = listener.accept() => {
                let Ok((socket, _)) = accepted else { break };
                let state = state.clone();
                tokio::spawn(async move {
                    let service = service_fn(move |request| connect(request, state.clone()));
                    let _ = http1::Builder::new()
                        .serve_connection(TokioIo::new(socket), service)
                        .with_upgrades().await;
                });
            }
        }
    }
    // Dropping the runtime cancels active tunnels as well as listeners.
}

fn configure(command: &mut Command, port: u16, password: &str, ca: &Path) {
    command
        .env_remove("ANTHROPIC_BASE_URL")
        .env_remove("CLAUDE_CODE_ENABLE_GATEWAY_MODEL_DISCOVERY")
        // Native endpoint requests can otherwise be compressed before the
        // existing model bridge parses their JSON.
        .env("CLAUDE_CODE_GZIP_REQUEST_BODIES", "0")
        .env("NODE_EXTRA_CA_CERTS", ca)
        .env(
            "HTTPS_PROXY",
            format!("http://clodex:{password}@127.0.0.1:{port}"),
        )
        .env_remove("https_proxy")
        .env_remove("http_proxy")
        .env_remove("HTTP_PROXY")
        // An inherited wildcard or Anthropic bypass would send GPT models
        // directly to Anthropic. Keep only the bridge and local MCPs exempt.
        .env("NO_PROXY", "localhost,127.0.0.1,::1")
        .env_remove("no_proxy");
}

impl Drop for RemoteProxy {
    fn drop(&mut self) {
        if let Some(shutdown) = self.shutdown.take() {
            let _ = shutdown.send(());
        }
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

fn certificate() -> Result<(TlsAcceptor, String, String)> {
    let key = KeyPair::generate()?;
    let password = format!("{:x}", Sha256::digest(key.serialize_der()));
    let mut params = CertificateParams::new(Vec::<String>::new())?;
    params
        .distinguished_name
        .push(DnType::CommonName, "Clodex session CA");
    params.is_ca = IsCa::Ca(BasicConstraints::Constrained(0));
    params.key_usages = vec![KeyUsagePurpose::KeyCertSign, KeyUsagePurpose::CrlSign];
    let root = params.self_signed(&key)?;
    let issuer = Issuer::new(params, key);
    let leaf_key = KeyPair::generate()?;
    let mut leaf_params = CertificateParams::new(vec![API_HOST.to_owned()])?;
    leaf_params
        .distinguished_name
        .push(DnType::CommonName, API_HOST);
    let leaf = leaf_params.signed_by(&leaf_key, &issuer)?;
    let mut config = rustls::ServerConfig::builder_with_provider(Arc::new(
        rustls::crypto::ring::default_provider(),
    ))
    .with_safe_default_protocol_versions()?
    .with_no_client_auth()
    .with_single_cert(
        vec![leaf.der().clone(), root.der().clone()],
        PrivatePkcs8KeyDer::from(leaf_key.serialize_der()).into(),
    )?;
    config.alpn_protocols = vec![b"http/1.1".to_vec()];
    // Issuer/private root key is dropped here. Only its public certificate is
    // written to disk; this server can impersonate only API_HOST.
    Ok((TlsAcceptor::from(Arc::new(config)), root.pem(), password))
}

fn response(status: StatusCode) -> Response<Body> {
    Response::builder()
        .status(status)
        .body(Body::empty())
        .expect("static response")
}

async fn connect(
    mut request: Request<Incoming>,
    state: Arc<State>,
) -> Result<Response<Body>, Infallible> {
    if request
        .headers()
        .get(header::PROXY_AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        != Some(&state.authorization)
    {
        let mut reply = response(StatusCode::PROXY_AUTHENTICATION_REQUIRED);
        reply.headers_mut().insert(
            header::PROXY_AUTHENTICATE,
            "Basic realm=\"Clodex\"".parse().unwrap(),
        );
        return Ok(reply);
    }
    if request.method() != Method::CONNECT {
        return Ok(response(StatusCode::METHOD_NOT_ALLOWED));
    }
    let Some(authority) = request.uri().authority() else {
        return Ok(response(StatusCode::BAD_REQUEST));
    };
    let Some(port) = authority.port_u16().filter(|port| *port != 0) else {
        return Ok(response(StatusCode::BAD_REQUEST));
    };
    let host = authority.host().to_owned();
    let upgrade = hyper::upgrade::on(&mut request);
    tokio::spawn(async move {
        let Ok(Ok(upgraded)) = timeout(CONNECT_TIMEOUT, upgrade).await else {
            return;
        };
        let mut client = TokioIo::new(upgraded);
        if host.eq_ignore_ascii_case(API_HOST) && port == 443 {
            let Ok(Ok(tls)) = timeout(CONNECT_TIMEOUT, state.tls.accept(client)).await else {
                return;
            };
            let service = service_fn(move |request| forward(request, state.clone()));
            let _ = http1::Builder::new()
                .serve_connection(TokioIo::new(tls), service)
                .with_upgrades()
                .await;
        } else {
            let Ok(Ok(mut upstream)) =
                timeout(CONNECT_TIMEOUT, TcpStream::connect((host.as_str(), port))).await
            else {
                return;
            };
            let _ = tokio::io::copy_bidirectional(&mut client, &mut upstream).await;
        }
    });
    Ok(response(StatusCode::OK))
}

fn model_endpoint(method: &Method, path: &str) -> bool {
    method == Method::POST && matches!(path, "/v1/messages" | "/v1/messages/count_tokens")
}

async fn forward(
    request: Request<Incoming>,
    state: Arc<State>,
) -> Result<Response<Body>, Infallible> {
    // Do not log request errors: URLs and headers may carry login credentials.
    Ok(forward_inner(request, &state)
        .await
        .unwrap_or_else(|_| response(StatusCode::BAD_GATEWAY)))
}

async fn forward_inner(mut request: Request<Incoming>, state: &State) -> Result<Response<Body>> {
    let upgrade = hyper::upgrade::on(&mut request);
    let (parts, body) = request.into_parts();
    if parts.uri.authority().is_some_and(|a| {
        !a.host().eq_ignore_ascii_case(API_HOST) || a.port_u16().is_some_and(|p| p != 443)
    }) {
        return Ok(response(StatusCode::BAD_REQUEST));
    }
    let is_model = model_endpoint(&parts.method, parts.uri.path());
    let path = parts.uri.path_and_query().map_or("/", |p| p.as_str());
    let (client, url) = if is_model {
        (
            &state.local,
            format!("http://127.0.0.1:{}{path}", state.bridge_port),
        )
    } else {
        (&state.external, format!("{}{path}", state.external_base))
    };
    let mut headers = parts.headers;
    headers.remove(header::HOST);
    headers.remove(header::PROXY_AUTHORIZATION);
    headers.remove("proxy-connection");
    if is_model {
        headers.insert(crate::fast_bridge::NATIVE_API_HEADER, "1".parse().unwrap());
    }
    if !is_model {
        // Clodex routing metadata is local, never sent to account/control APIs.
        let names: Vec<_> = headers
            .keys()
            .filter(|k| k.as_str().starts_with("x-clodex-"))
            .cloned()
            .collect();
        for name in names {
            headers.remove(name);
        }
    }
    let upstream = client
        .request(parts.method, url)
        .headers(headers)
        .body(reqwest::Body::wrap_stream(
            Body::new(body).into_data_stream(),
        ))
        .send()
        .await?;
    let status = upstream.status();
    let mut reply = Response::builder().status(status);
    *reply.headers_mut().expect("response headers") = upstream.headers().clone();
    if status == StatusCode::SWITCHING_PROTOCOLS {
        tokio::spawn(async move {
            if let (Ok(client), Ok(mut server)) = (upgrade.await, upstream.upgrade().await) {
                let _ = tokio::io::copy_bidirectional(&mut TokioIo::new(client), &mut server).await;
            }
        });
        Ok(reply.body(Body::empty())?)
    } else {
        Ok(reply.body(Body::from_stream(upstream.bytes_stream()))?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{Router, body::to_bytes, routing::any};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    async fn fixture(label: &'static str) -> (u16, tokio::task::JoinHandle<()>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let app = Router::new().fallback(any(move |mut request: axum::extract::Request| async move {
            if request.uri().path() == "/control/ws" {
                let upgrade = hyper::upgrade::on(&mut request);
                tokio::spawn(async move {
                    let mut socket = TokioIo::new(upgrade.await.unwrap());
                    let mut bytes = [0; 4];
                    socket.read_exact(&mut bytes).await.unwrap();
                    socket.write_all(&bytes).await.unwrap();
                });
                return Response::builder().status(StatusCode::SWITCHING_PROTOCOLS)
                    .header(header::CONNECTION, "upgrade").header(header::UPGRADE, "websocket")
                    .body(Body::empty()).unwrap();
            }
            let path = request.uri().path_and_query().unwrap().to_string();
            assert!(!request.headers().contains_key(header::PROXY_AUTHORIZATION));
            assert!(!request.headers().contains_key("proxy-connection"));
            if label == "external" { assert!(!request.headers().contains_key("x-clodex-test")); }
            let auth = request.headers().get(header::AUTHORIZATION).and_then(|v| v.to_str().ok()).unwrap_or("").to_string();
            let bytes = to_bytes(request.into_body(), 1024).await.unwrap();
            Response::new(Body::from(serde_json::json!({"destination":label,"path":path,"auth":auth,"body":String::from_utf8_lossy(&bytes)}).to_string()))
        }));
        let task = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        (port, task)
    }

    fn client(proxy: &RemoteProxy, trusted: bool, authenticated: bool) -> reqwest::Client {
        let mut via = reqwest::Proxy::https(format!("http://127.0.0.1:{}", proxy.port)).unwrap();
        if authenticated {
            via = via.basic_auth("clodex", &proxy.password);
        }
        let mut builder = reqwest::Client::builder()
            .no_proxy()
            .proxy(via)
            .http1_only()
            .timeout(Duration::from_secs(5));
        if trusted {
            builder = builder.add_root_certificate(
                reqwest::Certificate::from_pem(&std::fs::read(proxy.ca.path()).unwrap()).unwrap(),
            );
        }
        builder.build().unwrap()
    }

    #[tokio::test]
    async fn tls_proxy_routes_only_model_posts_and_preserves_control_traffic() {
        let (bridge, bridge_task) = fixture("bridge").await;
        let (external, external_task) = fixture("external").await;
        let proxy =
            RemoteProxy::start_inner(bridge, &format!("http://127.0.0.1:{external}")).unwrap();
        let client = client(&proxy, true, true);
        for (method, path, destination) in [
            (Method::POST, "/v1/messages?beta=true", "bridge"),
            (Method::POST, "/v1/messages/count_tokens", "bridge"),
            (Method::GET, "/v1/messages", "external"),
            (Method::POST, "/v1/messages/other", "external"),
            (Method::GET, "/v1/models", "external"),
            (Method::POST, "/v1/sessions?test=1", "external"),
        ] {
            let reply: serde_json::Value = client
                .request(method, format!("https://{API_HOST}{path}"))
                .header(header::AUTHORIZATION, "Bearer test-claude-credential")
                .header("x-clodex-test", "local-only")
                .body("test body")
                .send()
                .await
                .unwrap()
                .json()
                .await
                .unwrap();
            assert_eq!(reply["destination"], destination);
            assert_eq!(reply["path"], path);
            assert_eq!(reply["body"], "test body");
            // The model bridge owns the provider-specific credential stripping.
            assert_eq!(reply["auth"], "Bearer test-claude-credential");
        }
        bridge_task.abort();
        external_task.abort();
    }

    #[tokio::test]
    async fn requires_proxy_auth_and_scoped_ca_and_cleans_up() {
        let (bridge, task) = fixture("bridge").await;
        let proxy = RemoteProxy::start_inner(bridge, "http://127.0.0.1:1").unwrap();
        let path = proxy.ca.path().to_path_buf();
        let port = proxy.port;
        let mut command = Command::new("claude");
        proxy.configure(&mut command);
        let env: std::collections::BTreeMap<_, _> = command.get_envs().collect();
        let wrapper = std::path::PathBuf::from(
            env[std::ffi::OsStr::new("CLAUDE_CODE_SHELL_PREFIX")].unwrap(),
        );
        assert!(wrapper.is_file());
        assert_eq!(
            env[std::ffi::OsStr::new("HTTPS_PROXY")].unwrap(),
            format!("http://clodex:{}@127.0.0.1:{port}", proxy.password).as_str()
        );
        for (trusted, authenticated) in [(false, true), (true, false)] {
            assert!(
                client(&proxy, trusted, authenticated)
                    .post(format!("https://{API_HOST}/v1/messages"))
                    .send()
                    .await
                    .is_err()
            );
        }
        let ok = client(&proxy, true, true)
            .post(format!("https://{API_HOST}/v1/messages"))
            .send()
            .await
            .unwrap();
        assert_eq!(ok.status(), StatusCode::OK);
        drop(proxy);
        assert!(!path.exists());
        assert!(!wrapper.exists());
        assert!(TcpStream::connect(("127.0.0.1", port)).await.is_err());
        task.abort();
    }

    #[tokio::test]
    async fn control_protocol_upgrade_remains_bidirectional() {
        let (external, task) = fixture("external").await;
        let proxy = RemoteProxy::start_inner(1, &format!("http://127.0.0.1:{external}")).unwrap();
        let response = client(&proxy, true, true)
            .get(format!("https://{API_HOST}/control/ws"))
            .header(header::CONNECTION, "upgrade")
            .header(header::UPGRADE, "websocket")
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::SWITCHING_PROTOCOLS);
        let mut socket = response.upgrade().await.unwrap();
        socket.write_all(b"ping").await.unwrap();
        let mut reply = [0; 4];
        timeout(Duration::from_secs(5), socket.read_exact(&mut reply))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(&reply, b"ping");
        task.abort();
    }

    #[tokio::test]
    async fn other_hosts_use_opaque_tunnels_including_custom_https_ports() {
        opaque_tunnel("HTTP/1.1", true).await;
    }

    #[tokio::test]
    async fn gcloud_http10_connect_without_host_header_opens_a_tunnel() {
        opaque_tunnel("HTTP/1.0", false).await;
    }

    async fn opaque_tunnel(version: &str, host_header: bool) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let target = listener.local_addr().unwrap();
        let echo = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut bytes = [0; 4];
            socket.read_exact(&mut bytes).await.unwrap();
            socket.write_all(&bytes).await.unwrap();
        });
        let proxy = RemoteProxy::start_inner(1, "http://127.0.0.1:1").unwrap();
        let mut socket = TcpStream::connect(("127.0.0.1", proxy.port)).await.unwrap();
        let auth =
            base64::engine::general_purpose::STANDARD.encode(format!("clodex:{}", proxy.password));
        let host = if host_header {
            format!("Host: {target}\r\n")
        } else {
            String::new()
        };
        socket
            .write_all(
                format!(
                    "CONNECT {target} {version}\r\n{host}Proxy-Authorization: Basic {auth}\r\n\r\n"
                )
                .as_bytes(),
            )
            .await
            .unwrap();
        let mut headers = Vec::new();
        while !headers.ends_with(b"\r\n\r\n") {
            headers.push(
                timeout(Duration::from_secs(5), socket.read_u8())
                    .await
                    .unwrap()
                    .unwrap(),
            );
        }
        assert!(
            String::from_utf8_lossy(&headers).starts_with(&format!("{version} 200")),
            "{}",
            String::from_utf8_lossy(&headers)
        );
        // These bytes need not even be TLS: the proxy must not inspect them.
        socket.write_all(b"ping").await.unwrap();
        let mut reply = [0; 4];
        timeout(Duration::from_secs(5), socket.read_exact(&mut reply))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(&reply, b"ping");
        echo.await.unwrap();
    }

    #[tokio::test]
    async fn model_events_stream_before_the_upstream_finishes() {
        use futures_util::StreamExt;
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let (release, finish) = oneshot::channel();
        let finish = Arc::new(tokio::sync::Mutex::new(Some(finish)));
        let app = Router::new().fallback(any(move || {
            let finish = finish.clone();
            async move {
                let finish = finish.lock().await.take().unwrap();
                let first = futures_util::stream::once(async {
                    Ok::<_, std::io::Error>("data: first\n\n")
                });
                let last = futures_util::stream::once(async move {
                    let _ = finish.await;
                    Ok::<_, std::io::Error>("data: last\n\n")
                });
                Response::builder()
                    .header(header::CONTENT_TYPE, "text/event-stream")
                    .body(Body::from_stream(first.chain(last)))
                    .unwrap()
            }
        }));
        let task = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        let proxy = RemoteProxy::start_inner(port, "http://127.0.0.1:1").unwrap();
        let mut stream = client(&proxy, true, true)
            .post(format!("https://{API_HOST}/v1/messages"))
            .send()
            .await
            .unwrap()
            .bytes_stream();
        let first = timeout(Duration::from_secs(5), stream.next())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert_eq!(first, "data: first\n\n");
        release.send(()).unwrap();
        assert_eq!(stream.next().await.unwrap().unwrap(), "data: last\n\n");
        assert!(stream.next().await.is_none());
        task.abort();
    }

    #[test]
    fn child_transport_does_not_change_api_identity_or_global_trust() {
        let mut command = Command::new("claude");
        command.env("ANTHROPIC_BASE_URL", "http://old-proxy");
        configure(
            &mut command,
            1234,
            "test-secret",
            Path::new("/tmp/session-ca.pem"),
        );
        let env: std::collections::BTreeMap<_, _> = command.get_envs().collect();
        for key in [
            "ANTHROPIC_BASE_URL",
            "CLAUDE_CODE_ENABLE_GATEWAY_MODEL_DISCOVERY",
            "https_proxy",
            "no_proxy",
        ] {
            assert_eq!(env[std::ffi::OsStr::new(key)], None);
        }
        assert_eq!(
            env[std::ffi::OsStr::new("NODE_EXTRA_CA_CERTS")].unwrap(),
            "/tmp/session-ca.pem"
        );
        assert_eq!(
            env[std::ffi::OsStr::new("HTTPS_PROXY")].unwrap(),
            "http://clodex:test-secret@127.0.0.1:1234"
        );
    }
}
