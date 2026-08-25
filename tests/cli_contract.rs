use std::process::Command;

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
    for command in ["auth", "models", "config", "context", "doctor"] {
        assert!(
            help.contains(command),
            "{command} missing from help:\n{help}"
        );
    }
    assert!(!help.contains("__supervisor"));

    let version = clodex(&["--version"]);
    assert!(version.status.success());
    assert_eq!(
        String::from_utf8(version.stdout).unwrap().trim(),
        concat!("clodex ", env!("CARGO_PKG_VERSION"))
    );
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
        "report-limits",
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
