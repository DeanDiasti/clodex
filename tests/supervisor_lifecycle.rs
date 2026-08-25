#![cfg(unix)]

use std::fs::{self, OpenOptions};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

struct TestDirectory(PathBuf);

impl TestDirectory {
    fn new() -> Self {
        let path = PathBuf::from("/tmp").join(format!(
            "cdx-life-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&path).unwrap();
        Self(path)
    }
}

impl Drop for TestDirectory {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

#[test]
fn concurrent_supervisors_share_one_proxy_until_the_final_lease_closes() {
    let temporary = TestDirectory::new();
    let clodex_home = temporary.0.join("clodex");
    let codex_home = temporary.0.join("codex");
    let fake_bin = temporary.0.join("bin");
    let starts = temporary.0.join("proxy-starts");
    let test_binary = std::env::current_exe().unwrap();
    fs::create_dir_all(&codex_home).unwrap();
    fs::create_dir_all(&fake_bin).unwrap();

    let auth_path = codex_home.join("auth.json");
    fs::write(
        &auth_path,
        r#"{"auth_mode":"chatgpt","tokens":{"access_token":"header.eyJleHAiOjk5OTk5OTk5OTl9.signature","account_id":"test-account"}}"#,
    )
    .unwrap();
    fs::set_permissions(&auth_path, fs::Permissions::from_mode(0o600)).unwrap();

    let fake_proxy = fake_bin.join("claude-code-proxy");
    fs::write(
        &fake_proxy,
        r#"#!/usr/bin/env bash
set -euo pipefail

port=""
while (($# > 0)); do
  case "$1" in
    --port)
      port="$2"
      shift 2
      ;;
    *)
      shift
      ;;
  esac
done

[[ -n "${port}" ]] || exit 2
export FAKE_PROXY_PORT="${port}"
export FAKE_PROXY_TRANSPORT="${CCP_CODEX_TRANSPORT:-}"
exec "${FAKE_PROXY_TEST_BINARY}" --exact fake_proxy_process --ignored --nocapture
"#,
    )
    .unwrap();
    fs::set_permissions(&fake_proxy, fs::Permissions::from_mode(0o755)).unwrap();

    let path = format!(
        "{}:{}",
        fake_bin.display(),
        std::env::var("PATH").unwrap_or_default()
    );
    let mut supervisors: Vec<Child> = (0..8)
        .map(|_| {
            Command::new(env!("CARGO_BIN_EXE_clodex"))
                .arg("__supervisor")
                .env("CLODEX_HOME", &clodex_home)
                .env("CODEX_HOME", &codex_home)
                .env("FAKE_PROXY_STARTS", &starts)
                .env("FAKE_PROXY_TEST_BINARY", &test_binary)
                .env("PATH", &path)
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
                .unwrap()
        })
        .collect();

    let socket = clodex_home.join("run/control.sock");
    wait_until(Duration::from_secs(5), || socket.exists());

    let (first, first_port) = acquire_lease(&socket);
    let (second, second_port) = acquire_lease(&socket);
    assert_eq!(first_port, second_port);

    drop(first);
    thread::sleep(Duration::from_millis(1_500));
    assert!(
        std::net::TcpStream::connect(("127.0.0.1", second_port)).is_ok(),
        "the proxy stopped while the second session still held a lease"
    );

    drop(second);
    wait_until(Duration::from_secs(5), || {
        supervisors
            .iter_mut()
            .all(|supervisor| supervisor.try_wait().unwrap().is_some())
    });

    let starts_log = fs::read_to_string(&starts).unwrap();
    assert_eq!(
        starts_log.lines().count(),
        1,
        "racing supervisors started more than one proxy: {starts_log}"
    );
    assert!(
        starts_log.lines().all(|line| line.ends_with(" http")),
        "the proxy did not receive the HTTP transport default: {starts_log}"
    );
    assert!(!socket.exists());
    assert!(!clodex_home.join("run/proxy/codex/auth.json").exists());

    fs::write(
        clodex_home.join("config.json"),
        r#"{"version":1,"codex":{"transport":"websocket"}}"#,
    )
    .unwrap();

    let mut signaled_supervisor = Command::new(env!("CARGO_BIN_EXE_clodex"))
        .arg("__supervisor")
        .env("CLODEX_HOME", &clodex_home)
        .env("CODEX_HOME", &codex_home)
        .env("FAKE_PROXY_STARTS", &starts)
        .env("FAKE_PROXY_TEST_BINARY", &test_binary)
        .env("PATH", &path)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    wait_until(Duration::from_secs(5), || socket.exists());
    let (_lease, signaled_port) = acquire_lease(&socket);

    assert!(
        Command::new("kill")
            .args(["-TERM", &signaled_supervisor.id().to_string()])
            .status()
            .unwrap()
            .success()
    );
    wait_until(Duration::from_secs(5), || {
        signaled_supervisor.try_wait().unwrap().is_some()
    });
    assert!(!socket.exists());
    assert!(!clodex_home.join("run/proxy/codex/auth.json").exists());
    assert!(
        std::net::TcpStream::connect(("127.0.0.1", signaled_port)).is_err(),
        "the proxy survived supervisor SIGTERM"
    );
    let starts_log = fs::read_to_string(&starts).unwrap();
    assert!(
        starts_log
            .lines()
            .last()
            .is_some_and(|line| line.ends_with(" websocket")),
        "the restarted proxy did not receive the configured WebSocket transport: {starts_log}"
    );
}

#[test]
#[ignore = "runs only as the lifecycle test's external proxy process"]
fn fake_proxy_process() {
    let port: u16 = std::env::var("FAKE_PROXY_PORT")
        .expect("FAKE_PROXY_PORT is required")
        .parse()
        .expect("FAKE_PROXY_PORT must be a port number");
    let starts = std::env::var("FAKE_PROXY_STARTS").expect("FAKE_PROXY_STARTS is required");
    let transport = std::env::var("FAKE_PROXY_TRANSPORT").unwrap_or_default();

    writeln!(
        OpenOptions::new()
            .create(true)
            .append(true)
            .open(starts)
            .unwrap(),
        "{} {port} {transport}",
        std::process::id()
    )
    .unwrap();

    let listener = TcpListener::bind(("127.0.0.1", port)).unwrap();
    for incoming in listener.incoming() {
        let mut stream = incoming.unwrap();
        let mut request = [0_u8; 1024];
        let bytes = stream.read(&mut request).unwrap_or_default();
        let healthy = request[..bytes].starts_with(b"GET /healthz ");
        let (status, body) = if healthy {
            ("200 OK", "{\"ok\":true}")
        } else {
            ("404 Not Found", "{\"ok\":false}")
        };
        write!(
            stream,
            "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        )
        .unwrap();
    }
}

fn acquire_lease(socket: &Path) -> (UnixStream, u16) {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if let Ok(mut stream) = UnixStream::connect(socket) {
            stream.write_all(b"CLODEX/1 LEASE\n").unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            let mut response = String::new();
            BufReader::new(&stream).read_line(&mut response).unwrap();
            let fields: Vec<_> = response.split_whitespace().collect();
            assert_eq!(fields.first(), Some(&"CLODEX/1"));
            assert_eq!(fields.get(1), Some(&"READY"));
            let port = fields.get(2).unwrap().parse().unwrap();
            stream.set_read_timeout(None).unwrap();
            return (stream, port);
        }
        assert!(
            Instant::now() < deadline,
            "supervisor control socket did not accept a lease"
        );
        thread::sleep(Duration::from_millis(25));
    }
}

fn wait_until(timeout: Duration, mut condition: impl FnMut() -> bool) {
    let deadline = Instant::now() + timeout;
    while !condition() {
        assert!(Instant::now() < deadline, "condition timed out");
        thread::sleep(Duration::from_millis(25));
    }
}
