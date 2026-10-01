#![cfg(unix)]

use fs2::FileExt;
use sha2::{Digest, Sha256};
use std::fs::{self, OpenOptions};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

struct TestDirectory(PathBuf);

impl TestDirectory {
    fn new() -> Self {
        // Tests run in parallel, and macOS clocks have microsecond resolution,
        // so the time alone can hand two tests the same directory.
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let path = PathBuf::from("/tmp").join(format!(
            "cdx-life-{}-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed),
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
    // The built-in backend is the default; this test covers the external one.
    fs::create_dir_all(&clodex_home).unwrap();
    fs::write(
        clodex_home.join("config.json"),
        r#"{"version":1,"codex":{"backend":"proxy"}}"#,
    )
    .unwrap();

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
    // Compute the expected namespace before the supervisor's idle timer starts.
    let runtime = runtime_directory(&clodex_home, Path::new(env!("CARGO_BIN_EXE_clodex")));
    let socket = runtime.join("control.sock");
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

    wait_until(Duration::from_secs(20), || socket.exists());

    let (first, first_port) = acquire_lease(&socket);
    let (second, second_port) = acquire_lease(&socket);
    assert_eq!(first_port, second_port);

    // Hold a lease until every contender has attempted the lock. Otherwise a
    // delayed child can legitimately start a second proxy after this one drains.
    wait_until(Duration::from_secs(20), || {
        supervisors
            .iter_mut()
            .map(|supervisor| supervisor.try_wait().unwrap().is_none())
            .filter(|running| *running)
            .count()
            == 1
    });

    drop(first);
    thread::sleep(Duration::from_millis(1_500));
    assert!(
        std::net::TcpStream::connect(("127.0.0.1", second_port)).is_ok(),
        "the proxy stopped while the second session still held a lease"
    );

    drop(second);
    wait_until(Duration::from_secs(20), || {
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
    assert!(!runtime.join("proxy/codex/auth.json").exists());

    fs::write(
        clodex_home.join("config.json"),
        r#"{"version":1,"codex":{"transport":"websocket","backend":"proxy"}}"#,
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
    wait_until(Duration::from_secs(20), || socket.exists());
    let (_lease, signaled_port) = acquire_lease(&socket);

    assert!(
        Command::new("kill")
            .args(["-TERM", &signaled_supervisor.id().to_string()])
            .status()
            .unwrap()
            .success()
    );
    wait_until(Duration::from_secs(20), || {
        signaled_supervisor.try_wait().unwrap().is_some()
    });
    assert!(!socket.exists());
    assert!(!runtime.join("proxy/codex/auth.json").exists());
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
fn the_builtin_backend_serves_requests_without_the_external_proxy() {
    let temporary = TestDirectory::new();
    let clodex_home = temporary.0.join("clodex");
    let codex_home = temporary.0.join("codex");
    let fake_bin = temporary.0.join("bin");
    let proxy_runs = temporary.0.join("proxy-runs");
    fs::create_dir_all(&clodex_home).unwrap();
    fs::create_dir_all(&codex_home).unwrap();
    fs::create_dir_all(&fake_bin).unwrap();
    fs::write(
        clodex_home.join("config.json"),
        r#"{"version":1,"codex":{"backend":"builtin"}}"#,
    )
    .unwrap();

    let auth_path = codex_home.join("auth.json");
    fs::write(
        &auth_path,
        r#"{"auth_mode":"chatgpt","tokens":{"access_token":"header.eyJleHAiOjk5OTk5OTk5OTl9.signature","account_id":"test-account"}}"#,
    )
    .unwrap();
    fs::set_permissions(&auth_path, fs::Permissions::from_mode(0o600)).unwrap();

    // Any run of the external proxy is recorded, and must not happen.
    let fake_proxy = fake_bin.join("claude-code-proxy");
    fs::write(
        &fake_proxy,
        format!(
            "#!/bin/sh\necho ran >> '{}'\nexit 1\n",
            proxy_runs.display()
        ),
    )
    .unwrap();
    fs::set_permissions(&fake_proxy, fs::Permissions::from_mode(0o755)).unwrap();

    let mut supervisor = Command::new(env!("CARGO_BIN_EXE_clodex"))
        .arg("__supervisor")
        .env("CLODEX_HOME", &clodex_home)
        .env("CODEX_HOME", &codex_home)
        .env("XDG_STATE_HOME", clodex_home.join("logs"))
        .env("PATH", format!("{}:/usr/bin:/bin", fake_bin.display()))
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();

    let runtime = runtime_directory(&clodex_home, Path::new(env!("CARGO_BIN_EXE_clodex")));
    let socket = runtime.join("control.sock");
    wait_until(Duration::from_secs(20), || socket.exists());
    let (lease, port) = acquire_lease(&socket);

    // Token counting is answered by the backend itself, so this exercises the
    // bridge and the embedded backend without reaching Codex.
    let body = r#"{"model":"gpt-6-sol","messages":[{"role":"user","content":"hello there"}]}"#;
    let mut stream = std::net::TcpStream::connect(("127.0.0.1", port)).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(10)))
        .unwrap();
    write!(
        stream,
        "POST /v1/messages/count_tokens HTTP/1.1\r\nHost: 127.0.0.1\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    )
    .unwrap();
    let mut response = String::new();
    stream.read_to_string(&mut response).unwrap();
    assert!(response.starts_with("HTTP/1.1 200"), "{response}");
    assert!(response.contains("input_tokens"), "{response}");

    drop(lease);
    wait_until(Duration::from_secs(20), || {
        supervisor.try_wait().unwrap().is_some()
    });
    assert!(!socket.exists());
    assert!(
        std::net::TcpStream::connect(("127.0.0.1", port)).is_err(),
        "the bridge survived the final lease"
    );
    assert!(
        !proxy_runs.exists(),
        "the built-in backend started the external proxy"
    );
}

struct Session(Child);

impl Session {
    fn finish(&mut self) {
        drop(self.0.stdin.take());
        wait_until(Duration::from_secs(10), || {
            self.0.try_wait().unwrap().is_some()
        });
        assert!(self.0.wait().unwrap().success());
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        drop(self.0.stdin.take());
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

#[test]
fn replacing_the_binary_routes_new_sessions_to_a_new_deployment_and_drains_the_old_one() {
    let temporary = TestDirectory::new();
    let home = temporary.0.join("c");
    let codex = temporary.0.join("codex");
    let user = temporary.0.join("user");
    let bin = temporary.0.join("bin");
    for directory in [
        home.join("cache"),
        home.join("run/proxy"),
        codex.clone(),
        user.clone(),
        bin.clone(),
    ] {
        fs::create_dir_all(directory).unwrap();
    }
    fs::write(
        home.join("config.json"),
        r#"{"version":1,"codex":{"backend":"builtin"}}"#,
    )
    .unwrap();
    let auth = codex.join("auth.json");
    fs::write(&auth, r#"{"auth_mode":"chatgpt","tokens":{"access_token":"header.eyJleHAiOjk5OTk5OTk5OTl9.signature"}}"#).unwrap();
    fs::set_permissions(&auth, fs::Permissions::from_mode(0o600)).unwrap();
    let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap();
    fs::write(home.join("cache/codex-models.json"), serde_json::to_vec(&serde_json::json!({
        "fetched_at_ms":now.as_millis() as u64,"client_version":"0.159.0",
        "catalog":{"models":[{"slug":"gpt-6-sol","display_name":"GPT","visibility":"list","supported_in_api":true,"context_window":200000}]}
    })).unwrap()).unwrap();
    fs::write(
        home.join("cache/updates.json"),
        format!(r#"{{"checked_at":{},"release":null}}"#, now.as_secs()),
    )
    .unwrap();

    // A pre-rollout supervisor's global paths must be left completely alone.
    let legacy_lock = fs::File::create(home.join("run/supervisor.lock")).unwrap();
    legacy_lock.lock_exclusive().unwrap();
    fs::write(home.join("run/control.sock"), "legacy socket").unwrap();
    fs::write(home.join("run/proxy/legacy"), "legacy credentials").unwrap();

    let fake_claude = bin.join("claude");
    fs::write(
        &fake_claude,
        r#"#!/bin/sh
if [ "$1" = auth ]; then
  printf '{"loggedIn":false}\n'
  exit 0
fi
[ "$1" = --settings ] || exit 2
export FAKE_CLAUDE_SETTINGS="$2"
exec "$FAKE_CLAUDE_TEST_BINARY" --exact fake_claude_process --ignored --nocapture
"#,
    )
    .unwrap();
    fs::set_permissions(fake_claude, fs::Permissions::from_mode(0o755)).unwrap();

    let installed = bin.join("clodex");
    variant_executable(&installed, 1);
    let old_runtime = runtime_directory(&home, &installed);
    let launch = |record: &Path| {
        let mut command = Command::new(&installed);
        command
            .env("CLODEX_HOME", &home)
            .env("CODEX_HOME", &codex)
            .env("HOME", &user)
            .env("CLAUDE_CONFIG_DIR", user.join(".claude"))
            .env("PATH", format!("{}:/usr/bin:/bin", bin.display()))
            .env("HTTPS_PROXY", "http://127.0.0.1:1")
            .env("HTTP_PROXY", "http://127.0.0.1:1")
            .env("NO_PROXY", "127.0.0.1,localhost")
            .env("FAKE_CLAUDE_RECORD", record)
            .env("FAKE_CLAUDE_TEST_BINARY", std::env::current_exe().unwrap())
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::inherit());
        Session(spawn_variant(&mut command).unwrap())
    };
    let old_record = temporary.0.join("old.json");
    let mut old_session = launch(&old_record);
    let old_port = session_port(&old_record);
    assert_backend_responds(old_port);

    let replacement = bin.join("replacement");
    variant_executable(&replacement, 2);
    fs::rename(replacement, &installed).unwrap();
    let new_runtime = runtime_directory(&home, &installed);
    assert_ne!(
        old_runtime, new_runtime,
        "different builds of the same package version must be isolated"
    );
    let new_record = temporary.0.join("new.json");
    let mut new_session = launch(&new_record);
    let new_port = session_port(&new_record);
    assert_ne!(old_port, new_port);
    assert_backend_responds(old_port);
    assert_backend_responds(new_port);

    let later_record = temporary.0.join("later.json");
    let mut later_session = launch(&later_record);
    assert_eq!(
        session_port(&later_record),
        new_port,
        "new sessions of one build must share its backend"
    );
    for (record, runtime) in [(&old_record, &old_runtime), (&new_record, &new_runtime)] {
        let record: serde_json::Value = serde_json::from_slice(&fs::read(record).unwrap()).unwrap();
        assert_eq!(
            record["status_command"],
            format!("'{}' statusline", runtime.join("clodex").display())
        );
        assert!(runtime.join("proxy/codex/auth.json").exists());
        assert!(
            Command::new(runtime.join("clodex"))
                .arg("--version")
                .output()
                .unwrap()
                .status
                .success()
        );
    }

    old_session.finish();
    // The socket is removed first; wait for the whole cleanup, not just its start.
    wait_until(Duration::from_secs(10), || {
        !old_runtime.join("control.sock").exists()
            && !old_runtime.join("proxy/codex/auth.json").exists()
            && !old_runtime.join("clodex").exists()
    });
    assert!(!old_runtime.join("proxy/codex/auth.json").exists());
    assert!(!old_runtime.join("clodex").exists());
    assert!(std::net::TcpStream::connect(("127.0.0.1", old_port)).is_err());
    assert_backend_responds(new_port);
    new_session.finish();
    assert_backend_responds(new_port);
    later_session.finish();
    wait_until(Duration::from_secs(10), || {
        !new_runtime.join("control.sock").exists()
            && !new_runtime.join("proxy/codex/auth.json").exists()
            && !new_runtime.join("clodex").exists()
    });
    assert!(!new_runtime.join("proxy/codex/auth.json").exists());
    assert!(!new_runtime.join("clodex").exists());
    assert_eq!(
        fs::read_to_string(home.join("run/control.sock")).unwrap(),
        "legacy socket"
    );
    assert_eq!(
        fs::read_to_string(home.join("run/proxy/legacy")).unwrap(),
        "legacy credentials"
    );
}

/// Make two valid, functionally identical binaries with different fingerprints,
/// without recompiling the entire workspace or changing the package version.
fn variant_executable(destination: &Path, marker: u8) {
    let mut bytes = fs::read(env!("CARGO_BIN_EXE_clodex")).unwrap();
    #[cfg(target_os = "macos")]
    {
        // Change the Mach-O LC_UUID, then recreate its ad-hoc code signature.
        assert_eq!(&bytes[..4], &[0xcf, 0xfa, 0xed, 0xfe]);
        let commands = u32::from_le_bytes(bytes[16..20].try_into().unwrap());
        let mut offset = 32;
        let mut changed = false;
        for _ in 0..commands {
            let command = u32::from_le_bytes(bytes[offset..offset + 4].try_into().unwrap());
            let size =
                u32::from_le_bytes(bytes[offset + 4..offset + 8].try_into().unwrap()) as usize;
            if command == 0x1b {
                bytes[offset + 8] ^= marker;
                changed = true;
                break;
            }
            offset += size;
        }
        assert!(changed, "fixture binary has no LC_UUID");
    }
    #[cfg(not(target_os = "macos"))]
    bytes.push(marker);
    fs::write(destination, bytes).unwrap();
    fs::set_permissions(destination, fs::Permissions::from_mode(0o755)).unwrap();
    #[cfg(target_os = "macos")]
    {
        let output = Command::new("/usr/bin/codesign")
            .args(["--force", "--sign", "-"])
            .arg(destination)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
}

fn spawn_variant(command: &mut Command) -> std::io::Result<Child> {
    let mut retries = 5;
    loop {
        match command.spawn() {
            Err(error) if error.kind() == std::io::ErrorKind::ExecutableFileBusy && retries > 0 => {
                retries -= 1;
                thread::sleep(Duration::from_millis(25));
            }
            result => return result,
        }
    }
}

fn session_port(record: &Path) -> u16 {
    wait_until(Duration::from_secs(20), || {
        fs::read(record)
            .ok()
            .and_then(|bytes| serde_json::from_slice::<serde_json::Value>(&bytes).ok())
            .is_some()
    });
    serde_json::from_slice::<serde_json::Value>(&fs::read(record).unwrap()).unwrap()["port"]
        .as_u64()
        .unwrap() as u16
}

fn assert_backend_responds(port: u16) {
    let body = r#"{"model":"gpt-6-sol","messages":[{"role":"user","content":"hello"}]}"#;
    let mut stream = std::net::TcpStream::connect(("127.0.0.1", port)).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    write!(stream, "POST /v1/messages/count_tokens HTTP/1.1\r\nHost: 127.0.0.1\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).unwrap();
    let mut response = String::new();
    stream.read_to_string(&mut response).unwrap();
    assert!(response.starts_with("HTTP/1.1 200"), "{response}");
}

#[test]
#[ignore = "runs only as the rollout test's external Claude session"]
fn fake_claude_process() {
    let port = std::env::var("ANTHROPIC_BASE_URL")
        .unwrap()
        .rsplit(':')
        .next()
        .unwrap()
        .parse::<u16>()
        .unwrap();
    let settings: serde_json::Value =
        serde_json::from_str(&std::env::var("FAKE_CLAUDE_SETTINGS").unwrap()).unwrap();
    fs::write(
        std::env::var("FAKE_CLAUDE_RECORD").unwrap(),
        serde_json::to_vec(&serde_json::json!({
            "port":port,"status_command":settings["statusLine"]["command"]
        }))
        .unwrap(),
    )
    .unwrap();
    let mut input = Vec::new();
    std::io::stdin().read_to_end(&mut input).unwrap();
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

fn runtime_directory(home: &Path, executable: &Path) -> PathBuf {
    let id = format!("{:x}", Sha256::digest(fs::read(executable).unwrap()));
    home.join("run").join(&id[..24])
}

fn acquire_lease(socket: &Path) -> (UnixStream, u16) {
    let deadline = Instant::now() + Duration::from_secs(20);
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
