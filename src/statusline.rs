//! Session status-line wrapper. Rendering never contacts the network.

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::fs;
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::{Context, Result, ensure};
use serde_json::Value;

/// Install the wrapper for this launch only. Explicit --settings are folded
/// into the launch JSON so a second --settings cannot discard the wrapper.
pub fn configure(settings: &mut Value, arguments: &mut Vec<OsString>) -> Result<()> {
    let user_directory = std::env::var_os("CLAUDE_CONFIG_DIR")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".claude")));
    let cwd = std::env::current_dir()?;
    let project = cwd
        .ancestors()
        .find(|directory| directory.join(".git").exists())
        .unwrap_or(&cwd);
    let mut original = inherited_statusline(user_directory.as_deref(), project, arguments);
    let mut remaining = Vec::new();
    let mut incoming = std::mem::take(arguments).into_iter();
    while let Some(argument) = incoming.next() {
        if argument == "--" {
            remaining.push(argument);
            remaining.extend(incoming);
            break;
        }
        let value = if argument == "--settings" {
            Some(
                incoming
                    .next()
                    .context("--settings requires a file or JSON")?,
            )
        } else {
            argument
                .to_str()
                .and_then(|argument| argument.strip_prefix("--settings="))
                .map(OsString::from)
        };
        if let Some(value) = value {
            let value = value.to_str().context("--settings is not UTF-8")?;
            let extra: Value = if value.trim_start().starts_with('{') {
                serde_json::from_str(value).context("invalid Claude --settings JSON")?
            } else {
                serde_json::from_slice(
                    &fs::read(value)
                        .with_context(|| format!("could not read --settings {value}"))?,
                )?
            };
            ensure!(extra.is_object(), "Claude --settings must be a JSON object");
            if let Some(statusline) = extra.get("statusLine") {
                merge(&mut original, statusline.clone());
            }
            merge(settings, extra);
        } else {
            remaining.push(argument);
        }
    }
    let executable = std::env::current_exe()?;
    let command = wrapper_command(
        executable
            .to_str()
            .context("Clodex executable path is not UTF-8")?,
        &original,
    );
    let mut wrapped = original.as_object().cloned().unwrap_or_default();
    wrapped.insert("type".into(), Value::String("command".into()));
    wrapped.insert("command".into(), Value::String(command));
    // Refresh even when idle, so the result of an hourly check becomes visible.
    let interval = wrapped
        .get("refreshInterval")
        .and_then(Value::as_u64)
        .unwrap_or(15)
        .clamp(1, 15);
    wrapped.insert("refreshInterval".into(), interval.into());
    settings["statusLine"] = wrapped.into();
    *arguments = remaining;
    Ok(())
}

fn inherited_statusline(user: Option<&Path>, project: &Path, arguments: &[OsString]) -> Value {
    let sources = option_value(arguments, "--setting-sources");
    let enabled = |scope: &str| {
        sources.is_none_or(|sources| sources.split(',').any(|source| source == scope))
    };
    let mut original = Value::Null;
    let mut paths = Vec::new();
    if enabled("user")
        && let Some(user) = user
    {
        paths.push(user.join("settings.json"));
    }
    if enabled("project") {
        paths.push(project.join(".claude/settings.json"));
    }
    if enabled("local") {
        paths.push(project.join(".claude/settings.local.json"));
    }
    for path in paths {
        if let Some(statusline) = fs::read(path)
            .ok()
            .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok())
            .and_then(|settings| settings.get("statusLine").cloned())
        {
            merge(&mut original, statusline);
        }
    }
    // This command is executed by Claude only after its workspace trust gate.
    original
}

fn option_value<'a>(arguments: &'a [OsString], option: &str) -> Option<&'a str> {
    arguments
        .iter()
        .enumerate()
        .take_while(|(_, argument)| *argument != "--")
        .filter_map(|(index, argument)| {
            if argument == option {
                arguments.get(index + 1)?.to_str()
            } else {
                argument.to_str()?.strip_prefix(option)?.strip_prefix('=')
            }
        })
        .last()
}

fn merge(destination: &mut Value, source: Value) {
    match (destination, source) {
        (Value::Object(destination), Value::Object(source)) => {
            for (key, value) in source {
                if matches!(
                    key.as_str(),
                    "modelPicker" | "fallbackModel" | "modelSettings"
                ) {
                    destination.insert(key, value);
                } else {
                    merge(destination.entry(key).or_insert(Value::Null), value);
                }
            }
        }
        (Value::Array(destination), Value::Array(source)) => {
            for value in source {
                if !destination.contains(&value) {
                    destination.push(value);
                }
            }
        }
        (destination, source) => *destination = source,
    }
}

fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

fn wrapper_command(executable: &str, original: &Value) -> String {
    let wrapper = format!("{} statusline", shell_quote(executable));
    match original.get("command").and_then(Value::as_str) {
        Some(command) => format!(
            "CLODEX_STATUSLINE_COMMAND={} {wrapper}",
            shell_quote(command)
        ),
        None => wrapper,
    }
}

pub fn run_combined() -> Result<()> {
    let mut input = String::new();
    io::stdin().read_to_string(&mut input)?;
    let original = std::env::var("CLODEX_STATUSLINE_COMMAND")
        .ok()
        .filter(|command| !command.is_empty())
        .and_then(|command| delegate(&command, &input).ok())
        .unwrap_or_default();
    let update = crate::update::available_label();
    let rendered = append_notice(&original, update.as_deref());
    if !rendered.is_empty() {
        println!("{rendered}");
    }
    Ok(())
}

fn delegate(command: &str, input: &str) -> Result<String> {
    let mut child = Command::new("/bin/sh")
        .args(["-c", command])
        // Don't carry the delegate into nested Clodex helpers.
        .env_remove("CLODEX_STATUSLINE_COMMAND")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()?;
    if let Some(mut stdin) = child.stdin.take() {
        use std::io::Write;
        let _ = stdin.write_all(input.as_bytes());
    }
    let output = child.wait_with_output()?;
    Ok(String::from_utf8_lossy(&output.stdout)
        .trim_end_matches(['\r', '\n'])
        .to_owned())
}

fn append_notice(original: &str, update: Option<&str>) -> String {
    match (original.is_empty(), update) {
        (_, None) => original.to_string(),
        (true, Some(update)) => update.to_string(),
        (false, Some(update)) => format!("{original} · {update}"),
    }
}

pub fn run() -> Result<()> {
    if std::env::var("CLODEX_FAST").as_deref() != Ok("1") {
        return Ok(());
    }
    let routes: BTreeMap<String, String> = serde_json::from_str(
        &std::env::var("CLODEX_FAST_ROUTES").unwrap_or_else(|_| "{}".to_string()),
    )
    .context("invalid session fast routes")?;
    let mut input = String::new();
    io::stdin().read_to_string(&mut input)?;
    let input: Value = serde_json::from_str(&input).context("invalid status-line JSON")?;
    println!("{}", label(&input, &routes));
    Ok(())
}

fn label(input: &Value, routes: &BTreeMap<String, String>) -> &'static str {
    let Some(model) = input.pointer("/model/id").and_then(Value::as_str) else {
        return "FAST (session)";
    };
    let model = model.strip_suffix("[1m]").unwrap_or(model);
    let model = model.strip_suffix("-fast").unwrap_or(model);
    if routes.contains_key(model) {
        "FAST"
    } else {
        "FAST unavailable"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wrapper_forwards_stdin_and_safely_quotes_paths_and_commands() {
        let command = "read -r input; printf '%s\\n' \"$input\"";
        let original = serde_json::json!({"command": command});
        // /bin/sh accepts and executes the quoted delegate exactly as configured.
        let wrapped = wrapper_command("/a path/with ' quote/clodex", &original);
        assert!(wrapped.ends_with("'/a path/with '\\'' quote/clodex' statusline"));
        assert_eq!(
            delegate(command, "{\"model\":{}}\n").unwrap(),
            "{\"model\":{}}"
        );
        assert_eq!(
            append_notice("model · 25%", Some("update available")),
            "model · 25% · update available"
        );
        assert_eq!(append_notice("model", None), "model");
        assert_eq!(
            append_notice("", Some("update available")),
            "update available"
        );
    }

    #[test]
    fn settings_sources_keep_statusline_precedence_and_options() {
        let directory = tempfile::tempdir().unwrap();
        let user = directory.path().join("user");
        let project = directory.path().join("project");
        fs::create_dir_all(&user).unwrap();
        fs::create_dir_all(project.join(".claude")).unwrap();
        fs::write(
            user.join("settings.json"),
            r#"{"statusLine":{"type":"command","command":"user","padding":2}}"#,
        )
        .unwrap();
        fs::write(
            project.join(".claude/settings.json"),
            r#"{"statusLine":{"command":"project"}}"#,
        )
        .unwrap();
        fs::write(
            project.join(".claude/settings.local.json"),
            r#"{"statusLine":{"command":"local"}}"#,
        )
        .unwrap();
        let original = inherited_statusline(Some(&user), &project, &[]);
        assert_eq!(original["command"], "local");
        assert_eq!(original["padding"], 2);
        let arguments = vec![OsString::from("--setting-sources=user")];
        assert_eq!(
            inherited_statusline(Some(&user), &project, &arguments)["command"],
            "user"
        );
        let arguments = vec![OsString::from("--setting-sources"), OsString::from("")];
        assert!(inherited_statusline(Some(&user), &project, &arguments).is_null());
    }

    #[test]
    fn explicit_settings_keep_the_custom_command_without_discarding_launch_settings() {
        let directory = tempfile::tempdir().unwrap();
        let extra_path = directory.path().join("extra.json");
        fs::write(&extra_path, r#"{"statusLine":{"command":"printf 'custom'","padding":3,"refreshInterval":2},"permissions":{"allow":["Read"]}}"#).unwrap();
        let mut arguments = vec![
            OsString::from("--setting-sources="),
            OsString::from("--settings"),
            extra_path.into_os_string(),
            OsString::from("--settings={\"statusLine\":{\"hideVimModeIndicator\":true}}"),
            OsString::from("--resume"),
            OsString::from("--"),
            OsString::from("--settings=literal prompt"),
        ];
        let mut settings =
            serde_json::json!({"theme":"custom:clodex","permissions":{"allow":["Write"]}});
        configure(&mut settings, &mut arguments).unwrap();
        assert_eq!(settings["theme"], "custom:clodex");
        assert_eq!(
            settings["permissions"]["allow"],
            serde_json::json!(["Write", "Read"])
        );
        assert_eq!(settings["statusLine"]["padding"], 3);
        assert_eq!(settings["statusLine"]["refreshInterval"], 2);
        assert_eq!(settings["statusLine"]["hideVimModeIndicator"], true);
        let command = settings["statusLine"]["command"].as_str().unwrap();
        assert!(command.contains("CLODEX_STATUSLINE_COMMAND="));
        assert!(command.ends_with(" statusline"));
        assert_eq!(
            arguments,
            [
                "--setting-sources=",
                "--resume",
                "--",
                "--settings=literal prompt"
            ]
            .map(OsString::from)
        );
    }

    #[test]
    fn label_follows_the_current_model_and_its_capability() {
        let routes = BTreeMap::from([
            ("gpt-test".to_string(), "gpt-test-fast".to_string()),
            ("opus".to_string(), "gpt-test-fast".to_string()),
        ]);
        for model in ["gpt-test", "gpt-test-fast", "gpt-test[1m]", "opus"] {
            assert_eq!(
                label(&serde_json::json!({"model":{"id":model}}), &routes),
                "FAST"
            );
        }
        for model in ["unsupported", "anthropic/claude-opus-5-5"] {
            assert_eq!(
                label(&serde_json::json!({"model":{"id":model}}), &routes),
                "FAST unavailable"
            );
        }
        assert_eq!(label(&serde_json::json!({}), &routes), "FAST (session)");
    }
}
