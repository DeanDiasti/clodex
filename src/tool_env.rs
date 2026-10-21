//! Keep Clodex's model transport out of shell tools, hooks, and stdio MCPs.
use std::{fs, os::unix::fs::PermissionsExt, path::Path, process::Command};

use anyhow::Result;

pub struct ToolEnvironment {
    directory: tempfile::TempDir,
}

impl ToolEnvironment {
    pub fn new(proxy: &str, ca: &Path) -> Result<Self> {
        let directory = tempfile::Builder::new()
            .prefix("clodex-tool-env-")
            .tempdir()?;
        let shell = ["CLAUDE_CODE_SHELL", "SHELL"]
            .into_iter()
            .filter_map(|key| std::env::var(key).ok())
            .find(|shell| {
                matches!(
                    Path::new(shell).file_name().and_then(|n| n.to_str()),
                    Some("bash" | "zsh")
                ) && Path::new(shell).is_file()
            })
            .unwrap_or_else(|| "/bin/bash".into());
        let original = |key| std::env::var(key).ok();
        let script = script(
            proxy,
            &ca.to_string_lossy(),
            &shell,
            original("CLAUDE_CODE_SHELL_PREFIX").as_deref(),
            original("NO_PROXY").as_deref(),
            original("no_proxy").as_deref(),
            original("NODE_EXTRA_CA_CERTS").as_deref(),
        );
        let path = directory.path().join("shell");
        fs::write(&path, script)?;
        fs::set_permissions(&path, fs::Permissions::from_mode(0o700))?;
        Ok(Self { directory })
    }

    pub fn configure(&self, command: &mut Command) {
        command.env(
            "CLAUDE_CODE_SHELL_PREFIX",
            self.directory.path().join("shell"),
        );
    }
}

fn quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

fn restore(key: &str, value: Option<&str>) -> String {
    match value {
        Some(value) => format!("export {key}={}\n", quote(value)),
        None => format!("unset {key}\n"),
    }
}

fn script(
    proxy: &str,
    ca: &str,
    shell: &str,
    prefix: Option<&str>,
    no_proxy: Option<&str>,
    lower_no_proxy: Option<&str>,
    extra_ca: Option<&str>,
) -> String {
    // Match exact launch values. In particular, do not strip an SRT/Claude
    // sandbox proxy or a tool's explicit replacement proxy.
    let mut script = format!(
        r#"#!/bin/sh
if [ "${{HTTPS_PROXY-}}" = {proxy} ]; then
  unset HTTPS_PROXY
  if [ "${{NO_PROXY-}}" = 'localhost,127.0.0.1,::1' ]; then
{restore_no}  fi
  if [ "${{no_proxy+x}}" != x ]; then
{restore_lower}  fi
fi
if [ "${{NODE_EXTRA_CA_CERTS-}}" = {ca} ]; then
{restore_ca}fi
"#,
        proxy = quote(proxy),
        ca = quote(ca),
        restore_no = restore("NO_PROXY", no_proxy),
        restore_lower = restore("no_proxy", lower_no_proxy),
        restore_ca = restore("NODE_EXTRA_CA_CERTS", extra_ca)
    );
    // Claude passes the entire assembled command as ONE argument. Preserve
    // that contract, including for existing prefixes with flag arguments.
    script.push_str(&restore("CLAUDE_CODE_SHELL_PREFIX", prefix));
    if let Some(prefix) = prefix.filter(|p| !p.is_empty()) {
        if let Some((executable, flags)) = prefix.rsplit_once(" -") {
            script.push_str(&format!("exec {} -{} \"$1\"\n", quote(executable), flags));
        } else {
            script.push_str(&format!("exec {} \"$1\"\n", quote(prefix)));
        }
    } else {
        script.push_str(&format!("exec {} -c \"$1\"\n", quote(shell)));
    }
    script
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(proxy: &str, prefix: Option<&str>, body: &str) -> std::process::Output {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("shell");
        fs::write(
            &path,
            script(
                "http://clodex:test@127.0.0.1:1",
                "/tmp/clodex-ca",
                "/bin/bash",
                prefix,
                Some("example.org"),
                Some("internal.example"),
                Some("/tmp/user ca's.pem"),
            ),
        )
        .unwrap();
        Command::new("/bin/sh")
            .arg(path)
            .arg(body)
            .env("HTTPS_PROXY", proxy)
            .env("NO_PROXY", "localhost,127.0.0.1,::1")
            .env_remove("no_proxy")
            .env("NODE_EXTRA_CA_CERTS", "/tmp/clodex-ca")
            .env("CLAUDE_CODE_SHELL_PREFIX", "temporary-wrapper")
            .env("BUN_OPTIONS", "--smol")
            .output()
            .unwrap()
    }

    #[test]
    fn tools_get_original_environment_and_commands_keep_their_shell_semantics() {
        let out = run(
            "http://clodex:test@127.0.0.1:1",
            None,
            "test -z \"${HTTPS_PROXY+x}\" && test -z \"${CLAUDE_CODE_SHELL_PREFIX+x}\" && printf '%s\\n' \"$NO_PROXY\" \"$no_proxy\" \"$NODE_EXTRA_CA_CERTS\" \"$BUN_OPTIONS\"; exit 23",
        );
        assert_eq!(out.status.code(), Some(23));
        assert_eq!(
            String::from_utf8(out.stdout).unwrap(),
            "example.org\ninternal.example\n/tmp/user ca's.pem\n--smol\n"
        );
    }

    #[test]
    fn sandbox_proxy_and_bypass_rules_are_preserved() {
        let out = run(
            "http://sandbox:4321",
            None,
            "printf '%s\\n' \"$HTTPS_PROXY\" \"$NO_PROXY\"",
        );
        assert!(out.status.success());
        assert_eq!(
            String::from_utf8(out.stdout).unwrap(),
            "http://sandbox:4321\nlocalhost,127.0.0.1,::1\n"
        );
    }

    #[test]
    fn originally_absent_variables_stay_absent() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("shell");
        fs::write(
            &path,
            script(
                "http://clodex:test@127.0.0.1:1",
                "/tmp/clodex-ca",
                "/bin/bash",
                None,
                None,
                None,
                None,
            ),
        )
        .unwrap();
        let out = Command::new("/bin/sh")
            .arg(path)
            .arg("test -z \"${HTTPS_PROXY+x}${NO_PROXY+x}${no_proxy+x}${NODE_EXTRA_CA_CERTS+x}\"")
            .env("HTTPS_PROXY", "http://clodex:test@127.0.0.1:1")
            .env("NO_PROXY", "localhost,127.0.0.1,::1")
            .env_remove("no_proxy")
            .env("NODE_EXTRA_CA_CERTS", "/tmp/clodex-ca")
            .output()
            .unwrap();
        assert!(out.status.success());
    }

    #[test]
    fn existing_prefix_receives_one_command_argument() {
        let directory = tempfile::tempdir().unwrap();
        let prefix = directory.path().join("prefix with spaces");
        fs::write(&prefix,"#!/bin/sh\n[ \"$1\" = '--audit' ] || exit 90\nshift\n[ \"$#\" = 1 ] || exit 91\n[ -z \"${HTTPS_PROXY+x}\" ] || exit 92\nexec /bin/bash -c \"$1\"\n").unwrap();
        fs::set_permissions(&prefix, fs::Permissions::from_mode(0o700)).unwrap();
        let out = run(
            "http://clodex:test@127.0.0.1:1",
            Some(&format!("{} --audit", prefix.display())),
            "printf '%s' 'argument with spaces' | cat",
        );
        assert!(out.status.success());
        assert_eq!(out.stdout, b"argument with spaces");
    }
}
