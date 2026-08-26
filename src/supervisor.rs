use std::fs::{self, File, OpenOptions};
use std::io::{BufRead, BufReader, ErrorKind, Read, Write};
use std::net::{Shutdown, TcpListener, TcpStream};
#[cfg(unix)]
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
#[cfg(unix)]
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use fs2::FileExt;
use serde::Serialize;
use signal_hook::consts::{SIGINT, SIGTERM};
use signal_hook::flag;

use crate::auth::{self, CodexCredentials};
use crate::config;
use crate::fast_bridge::FastBridge;

const STARTUP_TIMEOUT: Duration = Duration::from_secs(10);
const STARTUP_IDLE_TIMEOUT: Duration = Duration::from_secs(15);
const SHUTDOWN_GRACE: Duration = Duration::from_secs(1);
const AUTH_REFRESH_MARGIN: Duration = Duration::from_secs(5 * 60);
const CREDENTIAL_SYNC_INTERVAL: Duration = Duration::from_secs(2);
const CONTROL_PROTOCOL: &str = "CLODEX/1";

#[cfg(not(unix))]
compile_error!("the clodex supervisor currently requires Unix domain sockets");

pub struct Lease {
    stream: UnixStream,
    proxy_port: u16,
    fast_bridge: bool,
}

impl Lease {
    pub fn proxy_port(&self) -> u16 {
        self.proxy_port
    }

    pub fn supports_fast_bridge(&self) -> bool {
        self.fast_bridge
    }

    pub fn close(self) {
        let _ = self.stream.shutdown(Shutdown::Both);
    }
}

#[derive(Serialize)]
struct ProxyAuth<'a> {
    access: &'a str,
    refresh: &'a str,
    expires: u64,
    #[serde(rename = "accountId", skip_serializing_if = "Option::is_none")]
    account_id: Option<&'a str>,
}

pub fn acquire() -> Result<Lease> {
    config::ensure_home_layout()?;
    let socket = socket_path()?;
    let deadline = Instant::now() + STARTUP_TIMEOUT;
    let mut last_spawn = None;

    loop {
        let error = match connect_and_acquire(&socket, deadline) {
            Ok(lease) => {
                sync_proxy_credentials(false)?;
                return Ok(lease);
            }
            Err(error) => error,
        };

        let should_spawn = last_spawn
            .map(|instant: Instant| instant.elapsed() >= Duration::from_millis(500))
            .unwrap_or(true);
        if should_spawn {
            spawn_supervisor()?;
            last_spawn = Some(Instant::now());
        }

        if Instant::now() >= deadline {
            bail!(
                "clodex supervisor did not become ready at {}: {error:#}",
                socket.display()
            );
        }
        thread::sleep(Duration::from_millis(50));
    }
}

fn connect_and_acquire(socket: &Path, deadline: Instant) -> Result<Lease> {
    let mut stream = UnixStream::connect(socket)
        .with_context(|| format!("could not connect to {}", socket.display()))?;
    let remaining = deadline.saturating_duration_since(Instant::now());
    if remaining.is_zero() {
        bail!("timed out waiting for the clodex supervisor");
    }
    stream.set_read_timeout(Some(remaining))?;
    stream.write_all(format!("{CONTROL_PROTOCOL} LEASE\n").as_bytes())?;

    let mut response = String::new();
    BufReader::new(&stream).read_line(&mut response)?;
    let mut fields = response.split_whitespace();
    match (fields.next(), fields.next(), fields.next(), fields.next()) {
        (Some(protocol), Some("READY"), Some(port), None) if protocol == CONTROL_PROTOCOL => {
            let proxy_port = port
                .parse::<u16>()
                .with_context(|| format!("supervisor returned invalid proxy port {port:?}"))?;
            if proxy_port == 0 {
                bail!("supervisor returned invalid proxy port 0");
            }
            stream.set_read_timeout(None)?;
            Ok(Lease {
                stream,
                proxy_port,
                // Existing supervisors may still speak CLODEX/1 while
                // returning the translator port directly. Probe the Clodex
                // bridge before enabling Claude's /fast command so an update
                // cannot silently expose a non-functional toggle.
                fast_bridge: crate::fast_bridge::healthcheck(proxy_port),
            })
        }
        _ => bail!("clodex supervisor returned an invalid lease response"),
    }
}

pub fn run() -> Result<()> {
    config::ensure_home_layout()?;
    let paths = SupervisorPaths::new()?;
    let lock = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(&paths.lock)
        .with_context(|| format!("could not open {}", paths.lock.display()))?;

    if FileExt::try_lock_exclusive(&lock).is_err() {
        return Ok(());
    }

    let mut cleanup = SupervisorCleanup::new(&paths);
    let shutdown_requested = Arc::new(AtomicBool::new(false));
    flag::register(SIGTERM, Arc::clone(&shutdown_requested))?;
    flag::register(SIGINT, Arc::clone(&shutdown_requested))?;

    remove_stale_socket(&paths.socket)?;
    let listener = UnixListener::bind(&paths.socket)
        .with_context(|| format!("could not bind {}", paths.socket.display()))?;
    listener.set_nonblocking(true)?;

    let mut credentials = auth::load_codex_credentials(false)?;
    write_proxy_auth(&paths.proxy_config, &credentials)?;
    let app_config = config::AppConfig::load()?;
    let upstream_port = available_proxy_port()?;
    cleanup.proxy = Some(spawn_proxy(
        &paths,
        upstream_port,
        app_config.codex.transport,
    )?);
    wait_for_owned_proxy(
        cleanup
            .proxy
            .as_mut()
            .context("translation proxy child was not available")?,
        upstream_port,
        &shutdown_requested,
    )?;
    let ceiling = hierarchical_ceiling(&app_config);
    cleanup.bridge = Some(FastBridge::start(
        upstream_port,
        app_config.compaction.hierarchical,
        ceiling,
    )?);
    let proxy_port = cleanup
        .bridge
        .as_ref()
        .context("Clodex fast bridge was not available")?
        .port();

    let started = Instant::now();
    let mut leases: Vec<UnixStream> = Vec::new();
    let mut had_lease = false;
    let mut empty_since = Instant::now();
    let mut last_auth_check = Instant::now();

    let result = 'supervisor: loop {
        if shutdown_requested.load(Ordering::Relaxed) {
            break Ok(());
        }

        loop {
            match listener.accept() {
                Ok((mut stream, _)) => {
                    if !accept_lease(&mut stream, proxy_port) {
                        continue;
                    }
                    stream.set_nonblocking(true)?;
                    leases.push(stream);
                    had_lease = true;
                }
                Err(error) if error.kind() == ErrorKind::WouldBlock => break,
                Err(error) => break 'supervisor Err(error.into()),
            }
        }

        let was_nonempty = !leases.is_empty();
        leases.retain_mut(lease_is_open);
        if was_nonempty && leases.is_empty() {
            empty_since = Instant::now();
        }

        if let Some(status) = cleanup
            .proxy
            .as_mut()
            .context("translation proxy child was not available")?
            .try_wait()?
        {
            break Err(anyhow::anyhow!(
                "translation proxy exited unexpectedly with {status}"
            ));
        }

        if last_auth_check.elapsed() >= CREDENTIAL_SYNC_INTERVAL {
            match auth::load_codex_credentials(false) {
                Ok(on_disk) if !credentials.has_same_access_token(&on_disk) => {
                    write_proxy_auth(&paths.proxy_config, &on_disk)?;
                    credentials = on_disk;
                }
                Ok(_) if credentials_need_refresh(&credentials) => {
                    match auth::load_codex_credentials(true) {
                        Ok(refreshed) => {
                            write_proxy_auth(&paths.proxy_config, &refreshed)?;
                            credentials = refreshed;
                        }
                        Err(error) => log_line(
                            &paths.supervisor_log,
                            &format!("auth refresh failed: {error:#}"),
                        ),
                    }
                }
                Ok(_) => {}
                Err(error) => log_line(
                    &paths.supervisor_log,
                    &format!("credential sync failed: {error:#}"),
                ),
            }
            last_auth_check = Instant::now();
        }

        if had_lease && leases.is_empty() && empty_since.elapsed() >= SHUTDOWN_GRACE {
            break Ok(());
        }
        if !had_lease && started.elapsed() >= STARTUP_IDLE_TIMEOUT {
            break Ok(());
        }

        thread::sleep(Duration::from_millis(100));
    };

    drop(cleanup);
    let _ = FileExt::unlock(&lock);
    result
}

pub fn sync_active_credentials() -> Result<()> {
    let lease = acquire()?;
    sync_proxy_credentials(true)?;
    lease.close();
    Ok(())
}

fn sync_proxy_credentials(force_refresh: bool) -> Result<()> {
    let mut credentials = auth::load_codex_credentials(false)?;
    if force_refresh || credentials_need_refresh(&credentials) {
        credentials = auth::load_codex_credentials(true)?;
    }
    let proxy_config = config::clodex_home()?.join("run").join("proxy");
    write_proxy_auth(&proxy_config, &credentials)
}

fn accept_lease(stream: &mut UnixStream, proxy_port: u16) -> bool {
    let _ = stream.set_read_timeout(Some(Duration::from_secs(1)));
    let mut request = String::new();
    let valid = BufReader::new(&*stream)
        .read_line(&mut request)
        .is_ok_and(|bytes| bytes > 0 && request.trim() == format!("{CONTROL_PROTOCOL} LEASE"));
    if !valid {
        return false;
    }

    stream
        .write_all(format!("{CONTROL_PROTOCOL} READY {proxy_port}\n").as_bytes())
        .is_ok()
}

pub fn proxy_models_support(models: &[&str]) -> Result<()> {
    let version = Command::new("claude-code-proxy")
        .arg("--version")
        .output()
        .context("could not inspect claude-code-proxy version")?;
    if !version.status.success()
        || !proxy_version_supports_fast(&String::from_utf8_lossy(&version.stdout))
    {
        bail!(
            "Clodex /fast requires claude-code-proxy 0.1.32 or newer. Upgrade `claude-code-proxy` and try again"
        );
    }

    let output = Command::new("claude-code-proxy")
        .arg("models")
        .output()
        .context("could not inspect claude-code-proxy models")?;
    if !output.status.success() {
        bail!("claude-code-proxy could not list its supported models");
    }

    let listed = String::from_utf8_lossy(&output.stdout);
    let unsupported = unsupported_proxy_models(&listed, models);
    if !unsupported.is_empty() {
        bail!(
            "the installed translation proxy does not support the current Codex model(s): {}. Upgrade `claude-code-proxy` and try again",
            unsupported.join(", ")
        );
    }
    Ok(())
}

fn proxy_version_supports_fast(output: &str) -> bool {
    let Some(version) = output
        .split_whitespace()
        .find(|word| word.chars().next().is_some_and(|ch| ch.is_ascii_digit()))
    else {
        return false;
    };
    let mut parts = version.split('.').map(|part| {
        part.chars()
            .take_while(char::is_ascii_digit)
            .collect::<String>()
            .parse::<u64>()
    });
    let (Some(Ok(major)), Some(Ok(minor)), Some(Ok(patch))) =
        (parts.next(), parts.next(), parts.next())
    else {
        return false;
    };
    (major, minor, patch) >= (0, 1, 32)
}

fn unsupported_proxy_models<'a>(listed: &str, models: &'a [&'a str]) -> Vec<&'a str> {
    models
        .iter()
        .copied()
        .filter(|model| {
            !listed
                .split([',', ':', ';', ' ', '\n', '\r', '\t'])
                .any(|item| item == *model)
        })
        .collect()
}

fn spawn_supervisor() -> Result<()> {
    let log_path = config::clodex_home()?.join("logs").join("supervisor.log");
    let stdout = append_log(&log_path)?;
    let stderr = stdout.try_clone()?;

    let mut command = Command::new(std::env::current_exe()?);
    command
        .arg("__supervisor")
        .stdin(Stdio::null())
        .stdout(Stdio::from(stdout))
        .stderr(Stdio::from(stderr));

    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.process_group(0);
    }

    command
        .spawn()
        .context("could not start the clodex supervisor")?;
    Ok(())
}

/// The ceiling a fold round must fit inside. Resolved from the live catalog so
/// the fold tracks the routed models rather than a fixed number; a catalog
/// that cannot be read leaves the fold disabled rather than guessing.
fn hierarchical_ceiling(app_config: &config::AppConfig) -> u64 {
    if !app_config.compaction.hierarchical {
        return 0;
    }
    crate::catalog::Catalog::load_from_codex()
        .and_then(|catalog| {
            let mapping = crate::mapping::ModelMapping::from_catalog(&catalog)?;
            app_config.effective_context_capacity(&catalog, &mapping)
        })
        .unwrap_or(0)
}

fn spawn_proxy(
    paths: &SupervisorPaths,
    proxy_port: u16,
    transport: config::CodexTransport,
) -> Result<Child> {
    let stdout = append_log(&paths.proxy_log)?;
    let stderr = stdout.try_clone()?;

    Command::new("claude-code-proxy")
        .args(["serve", "--no-monitor", "--port", &proxy_port.to_string()])
        .env("CCP_CONFIG_DIR", &paths.proxy_config)
        .env("CCP_CODEX_TRANSPORT", transport.as_str())
        // Lets Codex compact its own context upstream when a prompt approaches
        // the model's limit, instead of rejecting it with a 413. This is not
        // what makes the extended context window available — that is already
        // served without it — but a 413 is expensive to recover from, because
        // Claude Code answers it by compacting and the compaction request
        // carries the same oversized conversation.
        .env("CCP_CODEX_SERVER_COMPACTION", "1")
        .env("XDG_STATE_HOME", config::clodex_home()?.join("logs"))
        .stdin(Stdio::null())
        .stdout(Stdio::from(stdout))
        .stderr(Stdio::from(stderr))
        .spawn()
        .context("could not start claude-code-proxy")
}

fn available_proxy_port() -> Result<u16> {
    let listener = TcpListener::bind(("127.0.0.1", 0))
        .context("could not select an available loopback port")?;
    Ok(listener.local_addr()?.port())
}

fn wait_for_owned_proxy(
    proxy: &mut Child,
    proxy_port: u16,
    shutdown_requested: &AtomicBool,
) -> Result<()> {
    let deadline = Instant::now() + STARTUP_TIMEOUT;
    loop {
        if shutdown_requested.load(Ordering::Relaxed) {
            bail!("supervisor shutdown requested during proxy startup");
        }
        if let Some(status) = proxy.try_wait()? {
            bail!("translation proxy exited during startup with {status}");
        }
        if proxy_healthcheck(proxy_port) {
            // Check the child again so a port collision cannot be mistaken for
            // successful startup after our child has already failed to bind.
            if let Some(status) = proxy.try_wait()? {
                bail!("translation proxy exited during startup with {status}");
            }
            return Ok(());
        }
        if Instant::now() >= deadline {
            bail!("translation proxy did not become healthy on 127.0.0.1:{proxy_port}");
        }
        thread::sleep(Duration::from_millis(50));
    }
}

fn proxy_healthcheck(proxy_port: u16) -> bool {
    let Ok(mut stream) = TcpStream::connect_timeout(
        &std::net::SocketAddr::from(([127, 0, 0, 1], proxy_port)),
        Duration::from_millis(250),
    ) else {
        return false;
    };
    let _ = stream.set_read_timeout(Some(Duration::from_millis(500)));
    let _ = stream.set_write_timeout(Some(Duration::from_millis(500)));
    if stream
        .write_all(b"GET /healthz HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n\r\n")
        .is_err()
    {
        return false;
    }

    let mut response = Vec::new();
    if stream.read_to_end(&mut response).is_err() {
        return false;
    }
    let Some(body_offset) = response.windows(4).position(|window| window == b"\r\n\r\n") else {
        return false;
    };
    let headers = &response[..body_offset];
    if !headers.starts_with(b"HTTP/1.1 200 ") && !headers.starts_with(b"HTTP/1.0 200 ") {
        return false;
    }
    serde_json::from_slice::<serde_json::Value>(&response[body_offset + 4..])
        .ok()
        .and_then(|body| body.get("ok").and_then(serde_json::Value::as_bool))
        == Some(true)
}

fn write_proxy_auth(config_dir: &Path, credentials: &CodexCredentials) -> Result<()> {
    let directory = config_dir.join("codex");
    fs::create_dir_all(&directory)
        .with_context(|| format!("could not create {}", directory.display()))?;
    #[cfg(unix)]
    fs::set_permissions(&directory, fs::Permissions::from_mode(0o700))?;

    let path = directory.join("auth.json");
    let temporary = directory.join("auth.json.tmp");
    let auth = ProxyAuth {
        access: credentials.access_token(),
        refresh: "",
        // Codex remains responsible for refreshing. A high value prevents the
        // translator from trying to use the intentionally absent refresh token.
        expires: u64::MAX,
        account_id: credentials.account_id(),
    };
    let bytes = serde_json::to_vec(&auth)?;

    let mut options = OpenOptions::new();
    options.create(true).truncate(true).write(true);
    #[cfg(unix)]
    options.mode(0o600);
    let mut file = options
        .open(&temporary)
        .with_context(|| format!("could not create {}", temporary.display()))?;
    file.write_all(&bytes)?;
    file.sync_all()?;
    fs::rename(&temporary, &path).with_context(|| format!("could not save {}", path.display()))?;
    Ok(())
}

fn credentials_need_refresh(credentials: &CodexCredentials) -> bool {
    let now_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64;
    credentials
        .expires_at_ms()
        .map(|expiry| {
            expiry
                <= now_ms.saturating_add(
                    AUTH_REFRESH_MARGIN
                        .as_millis()
                        .try_into()
                        .unwrap_or(u64::MAX),
                )
        })
        .unwrap_or(true)
}

fn lease_is_open(stream: &mut UnixStream) -> bool {
    let mut byte = [0_u8; 1];
    match stream.read(&mut byte) {
        Ok(0) => false,
        Ok(_) => true,
        Err(error) if error.kind() == ErrorKind::WouldBlock => true,
        Err(_) => false,
    }
}

fn stop_proxy(proxy: &mut Child) {
    if proxy.try_wait().ok().flatten().is_none() {
        let _ = proxy.kill();
        let _ = proxy.wait();
    }
}

struct SupervisorCleanup {
    socket: PathBuf,
    proxy_config: PathBuf,
    bridge: Option<FastBridge>,
    proxy: Option<Child>,
}

impl SupervisorCleanup {
    fn new(paths: &SupervisorPaths) -> Self {
        Self {
            socket: paths.socket.clone(),
            proxy_config: paths.proxy_config.clone(),
            bridge: None,
            proxy: None,
        }
    }
}

impl Drop for SupervisorCleanup {
    fn drop(&mut self) {
        // Stop accepting/forwarding requests before terminating the private
        // translator process behind the bridge.
        drop(self.bridge.take());
        if let Some(proxy) = self.proxy.as_mut() {
            stop_proxy(proxy);
        }
        let _ = fs::remove_file(&self.socket);
        remove_ephemeral_auth(&self.proxy_config);
    }
}

fn remove_stale_socket(path: &Path) -> Result<()> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(()),
        Err(error) => {
            Err(error).with_context(|| format!("could not remove stale socket {}", path.display()))
        }
    }
}

fn remove_ephemeral_auth(config_dir: &Path) {
    let codex_dir = config_dir.join("codex");
    let _ = fs::remove_file(codex_dir.join("auth.json"));
    let _ = fs::remove_file(codex_dir.join("auth.json.tmp"));
    let _ = fs::remove_dir(&codex_dir);
    let _ = fs::remove_dir(config_dir);
}

fn append_log(path: &Path) -> Result<File> {
    OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .with_context(|| format!("could not open log {}", path.display()))
}

fn log_line(path: &Path, line: &str) {
    if let Ok(mut file) = OpenOptions::new().create(true).append(true).open(path) {
        let _ = writeln!(file, "{line}");
    }
}

fn socket_path() -> Result<PathBuf> {
    Ok(config::clodex_home()?.join("run").join("control.sock"))
}

struct SupervisorPaths {
    socket: PathBuf,
    lock: PathBuf,
    proxy_config: PathBuf,
    supervisor_log: PathBuf,
    proxy_log: PathBuf,
}

impl SupervisorPaths {
    fn new() -> Result<Self> {
        let home = config::clodex_home()?;
        Ok(Self {
            socket: home.join("run").join("control.sock"),
            lock: home.join("run").join("supervisor.lock"),
            proxy_config: home.join("run").join("proxy"),
            supervisor_log: home.join("logs").join("supervisor.log"),
            proxy_log: home.join("logs").join("proxy.log"),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temporary_directory(test_name: &str) -> PathBuf {
        PathBuf::from("/tmp").join(format!(
            "clodex-{test_name}-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ))
    }

    fn serve_http_once(response: &'static [u8]) -> (u16, thread::JoinHandle<()>) {
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut request = [0_u8; 512];
            let _ = stream.read(&mut request);
            stream.write_all(response).unwrap();
        });
        (port, server)
    }

    #[test]
    fn credential_refresh_is_expiry_aware() {
        let future = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_millis() as u64
            + Duration::from_secs(60 * 60).as_millis() as u64;
        let credentials = CodexCredentials::for_test("token", Some("account"), Some(future));
        assert!(!credentials_need_refresh(&credentials));

        let near_expiry = CodexCredentials::for_test(
            "token",
            None,
            Some(
                SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap()
                    .as_millis() as u64
                    + Duration::from_secs(60).as_millis() as u64,
            ),
        );
        assert!(credentials_need_refresh(&near_expiry));
        assert!(credentials_need_refresh(&CodexCredentials::for_test(
            "token", None, None
        )));
    }

    #[test]
    fn lease_is_acknowledged_with_the_supervisor_owned_port() {
        let directory = PathBuf::from("/tmp").join(format!(
            "cdx-ctl-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&directory).unwrap();
        let socket = directory.join("control.sock");
        let listener = UnixListener::bind(&socket).unwrap();

        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            assert!(accept_lease(&mut stream, 42_123));
            thread::sleep(Duration::from_millis(50));
        });

        let lease = connect_and_acquire(&socket, Instant::now() + Duration::from_secs(1)).unwrap();
        assert_eq!(lease.proxy_port(), 42_123);
        lease.close();
        server.join().unwrap();
        fs::remove_file(&socket).unwrap();
        fs::remove_dir(directory).unwrap();
    }

    #[test]
    fn arbitrary_tcp_listener_is_not_considered_a_healthy_proxy() {
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut request = [0_u8; 512];
            let _ = stream.read(&mut request);
            stream
                .write_all(
                    b"HTTP/1.1 200 OK\r\nContent-Length: 17\r\nConnection: close\r\n\r\n{\"service\":\"other\"}",
                )
                .unwrap();
        });

        assert!(!proxy_healthcheck(port));
        server.join().unwrap();
    }

    #[test]
    fn healthcheck_requires_success_status_valid_json_and_true_ok() {
        for response in [
            &b"HTTP/1.1 500 Error\r\nContent-Length: 11\r\n\r\n{\"ok\":true}"[..],
            &b"HTTP/1.1 200 OK\r\nContent-Length: 8\r\n\r\nnot-json"[..],
            &b"HTTP/1.1 200 OK\r\nContent-Length: 12\r\n\r\n{\"ok\":false}"[..],
            &b"HTTP/1.1 200 OK\r\nContent-Length: 11\r\n\r\n{\"ok\":true}"[..],
        ] {
            let (port, server) = serve_http_once(response);
            let expected =
                response.ends_with(b"{\"ok\":true}") && response.starts_with(b"HTTP/1.1 200 ");
            assert_eq!(proxy_healthcheck(port), expected);
            server.join().unwrap();
        }
    }

    #[test]
    fn rejects_invalid_control_requests_and_responses() {
        let (mut client, mut server) = UnixStream::pair().unwrap();
        client.write_all(b"CLODEX/1 WRONG\n").unwrap();
        assert!(!accept_lease(&mut server, 42_123));

        for response in [
            "CLODEX/1 READY 0\n",
            "CLODEX/1 READY not-a-port\n",
            "CLODEX/2 READY 42123\n",
            "CLODEX/1 READY 42123 extra\n",
        ] {
            let directory = temporary_directory("invalid-response");
            fs::create_dir_all(&directory).unwrap();
            let socket = directory.join("control.sock");
            let listener = UnixListener::bind(&socket).unwrap();
            let reply = response.to_string();
            let handle = thread::spawn(move || {
                let (mut stream, _) = listener.accept().unwrap();
                let mut request = String::new();
                BufReader::new(&stream).read_line(&mut request).unwrap();
                stream.write_all(reply.as_bytes()).unwrap();
            });
            assert!(connect_and_acquire(&socket, Instant::now() + Duration::from_secs(1)).is_err());
            handle.join().unwrap();
            fs::remove_file(socket).unwrap();
            fs::remove_dir(directory).unwrap();
        }
    }

    #[test]
    fn writes_access_only_proxy_credentials_with_strict_permissions() {
        let directory = temporary_directory("proxy-auth");
        let credentials = CodexCredentials::for_test("access-secret", Some("account-1"), Some(123));

        write_proxy_auth(&directory, &credentials).unwrap();

        let path = directory.join("codex/auth.json");
        let value: serde_json::Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        assert_eq!(value["access"], "access-secret");
        assert_eq!(value["refresh"], "");
        assert_eq!(value["expires"], u64::MAX);
        assert_eq!(value["accountId"], "account-1");
        assert!(value.get("refresh_token").is_none());
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            assert_eq!(fs::metadata(&path).unwrap().mode() & 0o777, 0o600);
            assert_eq!(
                fs::metadata(directory.join("codex")).unwrap().mode() & 0o777,
                0o700
            );
        }
        remove_ephemeral_auth(&directory);
        assert!(!directory.exists());
    }

    #[test]
    fn proxy_model_matching_uses_exact_tokens() {
        let listed = "models: gpt-sol, gpt-terra;\ngpt-luna";
        assert!(unsupported_proxy_models(listed, &["gpt-sol", "gpt-luna"]).is_empty());
        assert_eq!(
            unsupported_proxy_models(listed, &["gpt-ter", "gpt-missing"]),
            ["gpt-ter", "gpt-missing"]
        );
    }

    #[test]
    fn fast_bridge_requires_a_proxy_with_priority_suffix_support() {
        assert!(!proxy_version_supports_fast("claude-code-proxy 0.1.31"));
        assert!(proxy_version_supports_fast("claude-code-proxy 0.1.32"));
        assert!(proxy_version_supports_fast("claude-code-proxy 1.0.0\n"));
        assert!(!proxy_version_supports_fast("unknown"));
    }

    #[test]
    fn stale_socket_cleanup_is_idempotent() {
        let path = temporary_directory("stale-socket");
        fs::write(&path, b"stale").unwrap();
        remove_stale_socket(&path).unwrap();
        remove_stale_socket(&path).unwrap();
        assert!(!path.exists());
    }

    #[test]
    fn cleanup_guard_removes_control_socket_and_ephemeral_auth() {
        let directory = std::env::temp_dir().join(format!(
            "clodex-cleanup-test-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let run = directory.join("run");
        let proxy_config = run.join("proxy");
        let codex = proxy_config.join("codex");
        fs::create_dir_all(&codex).unwrap();
        let socket = run.join("control.sock");
        fs::write(&socket, b"socket placeholder").unwrap();
        fs::write(codex.join("auth.json"), b"credential placeholder").unwrap();

        let paths = SupervisorPaths {
            socket: socket.clone(),
            lock: run.join("supervisor.lock"),
            proxy_config: proxy_config.clone(),
            supervisor_log: directory.join("supervisor.log"),
            proxy_log: directory.join("proxy.log"),
        };
        drop(SupervisorCleanup::new(&paths));

        assert!(!socket.exists());
        assert!(!proxy_config.exists());
        fs::remove_dir(run).unwrap();
        fs::remove_dir(directory).unwrap();
    }
}
