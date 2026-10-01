use std::io::Write;
use std::process::{Command, Stdio};

fn clodex(arguments: &[&str]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_clodex"))
        .args(arguments)
        .output()
        .unwrap()
}

#[test]
fn top_level_help_and_version_are_available_without_runtime_dependencies() {
    let help = clodex(&["--help"]);
    assert!(help.status.success());
    let help = String::from_utf8(help.stdout).unwrap();
    for command in ["auth", "models", "config", "context", "doctor", "update"] {
        assert!(
            help.contains(command),
            "{command} missing from help:\n{help}"
        );
    }
    assert!(!help.contains("__supervisor"));
    assert!(help.contains("--fast"));
    assert!(help.contains("subagents"));

    let fast_help = clodex(&["--fast", "--help"]);
    assert!(fast_help.status.success());

    let version = clodex(&["--version"]);
    assert!(version.status.success());
    assert_eq!(
        String::from_utf8(version.stdout).unwrap().trim(),
        concat!("clodex ", env!("CARGO_PKG_VERSION"))
    );
}

#[test]
fn update_help_is_available_without_runtime_dependencies() {
    let output = Command::new(env!("CARGO_BIN_EXE_clodex"))
        .args(["update", "--help"])
        .env("PATH", "")
        .env("CLODEX_HOME", "/dev/null/clodex-update-test")
        .output()
        .unwrap();
    assert!(output.status.success());
    let help = String::from_utf8(output.stdout).unwrap();
    assert!(help.contains("latest stable"));
    assert!(help.contains("clodex update"));
}

#[test]
fn statusline_appends_cached_updates_preserving_custom_stdin_and_output() {
    let directory = tempfile::tempdir().unwrap();
    let cache = directory.path().join("cache");
    std::fs::create_dir(&cache).unwrap();
    let target = match (std::env::consts::OS, std::env::consts::ARCH) {
        ("macos", "aarch64") => "aarch64-apple-darwin",
        ("macos", "x86_64") => "x86_64-apple-darwin",
        ("linux", "aarch64") => "aarch64-unknown-linux-gnu",
        ("linux", "x86_64") => "x86_64-unknown-linux-gnu",
        _ => return,
    };
    for (tag, notice) in [
        ("v999.0.0", true),
        ("v0.0.1", false),
        ("v999.0.0-rc.1", false),
    ] {
        let assets: Vec<_> = [format!("clodex-{tag}-{target}.tar.gz"), "SHA256SUMS".into()]
            .into_iter().map(|name| serde_json::json!({"browser_download_url":format!("https://github.com/DeanDiasti/clodex/releases/download/{tag}/{name}"), "name":name})).collect();
        std::fs::write(
            cache.join("updates.json"),
            serde_json::to_vec(&serde_json::json!({
                "checked_at":0,"release":{"tag_name":tag,"assets":assets}
            }))
            .unwrap(),
        )
        .unwrap();
        let mut child = Command::new(env!("CARGO_BIN_EXE_clodex"))
            .arg("statusline")
            .env("PATH", "")
            .env("CLODEX_HOME", directory.path())
            .env("HTTP_PROXY", "http://127.0.0.1:1")
            .env("HTTPS_PROXY", "http://127.0.0.1:1")
            .env(
                "CLODEX_STATUSLINE_COMMAND",
                "read -r input; printf 'custom %s\\n' \"$input\"",
            )
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        child
            .stdin
            .take()
            .unwrap()
            .write_all(b"{\"model\":{}}\n")
            .unwrap();
        let output = child.wait_with_output().unwrap();
        assert!(output.status.success());
        assert!(output.stderr.is_empty());
        let expected = if notice {
            "custom {\"model\":{}} · Clodex v999.0.0 update available · clodex update\n"
        } else {
            "custom {\"model\":{}}\n"
        };
        assert_eq!(String::from_utf8(output.stdout).unwrap(), expected);
    }
    // Missing/broken cache and a failing custom command remain quiet.
    std::fs::write(cache.join("updates.json"), "broken").unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_clodex"))
        .arg("statusline")
        .env("PATH", "")
        .env("CLODEX_HOME", directory.path())
        .env("CLODEX_STATUSLINE_COMMAND", "exit 1")
        .output()
        .unwrap();
    assert!(output.status.success());
    assert!(output.stdout.is_empty());
    assert!(output.stderr.is_empty());
}

#[test]
fn statusline_fast_runs_locally_without_starting_claude() {
    for (fast, input, expected) in [
        ("0", "", ""),
        ("1", r#"{"model":{"id":"gpt-6.1-sol"}}"#, "FAST\n"),
        (
            "1",
            r#"{"model":{"id":"anthropic/claude-opus-5-5"}}"#,
            "FAST unavailable\n",
        ),
        ("1", "{}", "FAST (session)\n"),
    ] {
        let mut child = Command::new(env!("CARGO_BIN_EXE_clodex"))
            .arg("statusline-fast")
            // A regression into the launcher must fail rather than start a
            // nested Claude session or make an authenticated network request.
            .env("PATH", "")
            .env("CLODEX_HOME", "/dev/null/clodex-statusline-test")
            .env("CLODEX_FAST", fast)
            .env(
                "CLODEX_FAST_ROUTES",
                r#"{"gpt-6.1-sol":"gpt-6.1-sol-fast"}"#,
            )
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        child
            .stdin
            .take()
            .unwrap()
            .write_all(input.as_bytes())
            .unwrap();
        let output = child.wait_with_output().unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(String::from_utf8(output.stdout).unwrap(), expected);
        assert!(output.stderr.is_empty());
    }
}

#[test]
fn nested_command_help_documents_the_public_configuration_contract() {
    let help = clodex(&["config", "--help"]);
    assert!(help.status.success());
    let help = String::from_utf8(help.stdout).unwrap();
    for command in [
        "show",
        "context",
        "compact-at",
        "transport",
        "hierarchical-compaction",
        "allow-tool",
        "forget-tool",
        "path",
    ] {
        assert!(
            help.contains(command),
            "{command} missing from help:\n{help}"
        );
    }

    let models = clodex(&["models", "--help"]);
    assert!(models.status.success());
    let models = String::from_utf8(models.stdout).unwrap();
    assert!(models.contains("list"));
    assert!(models.contains("map"));
    assert!(models.contains("--json"));

    let auth = clodex(&["auth", "--help"]);
    assert!(auth.status.success());
    let auth = String::from_utf8(auth.stdout).unwrap();
    assert!(auth.contains("status"));
    assert!(auth.contains("sync"));
}

#[cfg(unix)]
#[test]
fn installer_is_valid_shell_and_has_standalone_help() {
    let installer = format!("{}/scripts/install.sh", env!("CARGO_MANIFEST_DIR"));

    let syntax = Command::new("bash")
        .args(["-n", &installer])
        .status()
        .unwrap();
    assert!(syntax.success());

    let help = Command::new("bash")
        .args([&installer, "--help"])
        .output()
        .unwrap();
    assert!(help.status.success());
    let help = String::from_utf8(help.stdout).unwrap();
    assert!(help.contains("--root"));
    assert!(help.contains("--install-proxy"));
    assert!(help.contains("--skip-prerequisite-checks"));
    assert!(help.contains("CLODEX_INSTALL_ROOT"));
    assert!(help.contains("update"));
}
