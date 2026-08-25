use std::ffi::OsString;
use std::fs;
use std::io::{self, IsTerminal};
use std::path::Path;
use std::process::Command;

use anyhow::{Context, Result, bail};

use crate::catalog::Catalog;
use crate::config::AppConfig;
use crate::mapping::ModelMapping;
use crate::supervisor;

const CLODEX_THEME: &str = r##"{
  "name": "Clodex",
  "base": "dark",
  "overrides": {
    "claude": "#a78bfa",
    "claudeShimmer": "#c4b5fd",
    "clawd_body": "#a78bfa",
    "promptBorder": "#a78bfa",
    "promptBorderShimmer": "#c4b5fd",
    "permission": "#a78bfa",
    "permissionShimmer": "#c4b5fd"
  }
}
"##;

pub fn run(claude_args: Vec<OsString>) -> Result<()> {
    let catalog = Catalog::load_from_codex()?;
    let mapping = ModelMapping::from_catalog(&catalog)?;
    supervisor::proxy_models_support(&[
        &mapping.fable.model,
        &mapping.opus.model,
        &mapping.sonnet.model,
    ])?;

    let config = AppConfig::load()?;
    if !crate::config::config_path()?.exists() {
        config.save()?;
    }
    let context_capacity = config.effective_context_capacity(&catalog, &mapping)?;
    warn_if_context_was_clamped(config.context.max_tokens, context_capacity);
    let lease = supervisor::acquire()?;
    let proxy_port = lease.proxy_port();
    let supports_fast_bridge = lease.supports_fast_bridge();
    ensure_clodex_theme()?;

    print_banner(
        &mapping,
        context_capacity,
        config.context.compact_at_percent,
    );

    let mut command = build_claude_command(
        claude_args,
        &mapping,
        &config,
        context_capacity,
        proxy_port,
        supports_fast_bridge,
    )?;

    let status = command
        .status()
        .context("could not start Claude Code; is `claude` installed?")?;
    lease.close();
    restore_terminal_title();

    if !status.success() {
        bail!("Claude Code exited with {status}");
    }
    Ok(())
}

fn build_claude_command(
    claude_args: Vec<OsString>,
    mapping: &ModelMapping,
    config: &AppConfig,
    context_capacity: u64,
    proxy_port: u16,
    supports_fast_bridge: bool,
) -> Result<Command> {
    let mut command = Command::new("claude");
    command
        .args(["--settings", &launch_settings(config, Some(proxy_port))?])
        .args(claude_args)
        .env(
            "ANTHROPIC_BASE_URL",
            format!("http://127.0.0.1:{proxy_port}"),
        )
        .env("ANTHROPIC_AUTH_TOKEN", "clodex-local-proxy")
        .env_remove("ANTHROPIC_API_KEY");
    configure_fast_bridge(&mut command, supports_fast_bridge, &mapping.opus.model);
    configure_model_context(
        &mut command,
        mapping,
        context_capacity,
        config.context.compact_at_percent,
    );
    command
        .env("ANTHROPIC_DEFAULT_FABLE_MODEL", &mapping.fable.model)
        .env(
            "ANTHROPIC_DEFAULT_FABLE_MODEL_NAME",
            format!("Fable · {}", mapping.fable.display_name),
        )
        .env(
            "ANTHROPIC_DEFAULT_FABLE_MODEL_DESCRIPTION",
            "Top available Codex model",
        )
        .env("ANTHROPIC_DEFAULT_OPUS_MODEL", &mapping.opus.model)
        .env(
            "ANTHROPIC_DEFAULT_OPUS_MODEL_NAME",
            format!("Opus · {}", mapping.opus.display_name),
        )
        .env(
            "ANTHROPIC_DEFAULT_OPUS_MODEL_DESCRIPTION",
            "Second available Codex model",
        )
        .env("ANTHROPIC_DEFAULT_SONNET_MODEL", &mapping.sonnet.model)
        .env(
            "ANTHROPIC_DEFAULT_SONNET_MODEL_NAME",
            format!("Sonnet · {}", mapping.sonnet.display_name),
        )
        .env(
            "ANTHROPIC_DEFAULT_SONNET_MODEL_DESCRIPTION",
            "Third available Codex model",
        )
        .env(
            "ANTHROPIC_DEFAULT_HAIKU_MODEL",
            &mapping.haiku_compatibility.model,
        )
        .env("CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC", "1")
        .env("CLAUDE_CODE_DISABLE_NONSTREAMING_FALLBACK", "1");
    Ok(command)
}

fn configure_fast_bridge(command: &mut Command, supported: bool, initial_model: &str) {
    if supported {
        let marker = crate::fast_bridge::custom_headers(initial_model);
        let headers = std::env::var("ANTHROPIC_CUSTOM_HEADERS")
            .ok()
            .filter(|headers| !headers.trim().is_empty())
            .map_or(marker.clone(), |headers| format!("{headers}\n{marker}"));
        command
            .env("ANTHROPIC_CUSTOM_HEADERS", headers)
            // ANTHROPIC_AUTH_TOKEN and disabled nonessential traffic prevent
            // Claude from completing its Anthropic entitlement probe. This
            // child-only override exposes the TUI toggle; the Clodex bridge
            // supplies the actual Codex priority semantics.
            .env("CLAUDE_CODE_SKIP_FAST_MODE_ORG_CHECK", "1")
            .env_remove("CLAUDE_CODE_SKIP_FAST_MODE_NETWORK_ERRORS");
    } else {
        command
            .env_remove("CLAUDE_CODE_SKIP_FAST_MODE_ORG_CHECK")
            .env_remove("CLAUDE_CODE_SKIP_FAST_MODE_NETWORK_ERRORS");
    }
}

fn configure_model_context(
    command: &mut Command,
    mapping: &ModelMapping,
    context_capacity: u64,
    compact_at_percent: u8,
) {
    // A recognized Claude alias behind a custom base URL is assigned Claude
    // Code's conservative 200K window. The actual routed Codex model ID is
    // unrecognized, so MAX_CONTEXT_TOKENS applies directly.
    command
        .env("ANTHROPIC_MODEL", &mapping.opus.model)
        .env(
            "CLAUDE_CODE_MAX_CONTEXT_TOKENS",
            context_capacity.to_string(),
        )
        .env(
            "CLAUDE_CODE_AUTO_COMPACT_WINDOW",
            context_capacity.to_string(),
        )
        .env(
            "CLAUDE_AUTOCOMPACT_PCT_OVERRIDE",
            compact_at_percent.to_string(),
        );
}

fn launch_settings(config: &AppConfig, bridge_port: Option<u16>) -> Result<String> {
    let mut settings = serde_json::json!({
        "theme": "custom:clodex",
    });
    if !config.permissions.trusted_tools.is_empty() {
        settings["permissions"] = serde_json::json!({
            "allow": config.permissions.trusted_tools,
        });
    }
    if let Some(hooks) = precompact_hook(config, bridge_port) {
        settings["hooks"] = hooks;
    }
    Ok(serde_json::to_string(&settings)?)
}

/// Claude Code fires PreCompact before it builds a compaction request. Arming
/// the bridge from that event is what lets it recognise the request that
/// follows, rather than inferring it from the prompt body.
fn precompact_hook(config: &AppConfig, bridge_port: Option<u16>) -> Option<serde_json::Value> {
    if !config.compaction.hierarchical {
        return None;
    }
    let port = bridge_port?;
    // The hook payload arrives on stdin; forwarding it verbatim gives the
    // bridge the session id and the manual/auto trigger.
    let command = format!(
        "exec curl --silent --show-error --max-time 5          --header 'content-type: application/json'          --data @- http://127.0.0.1:{port}/__clodex/compaction/arm >/dev/null 2>&1 || true"
    );
    Some(serde_json::json!({
        "PreCompact": [{
            "hooks": [{ "type": "command", "command": command }]
        }]
    }))
}

fn ensure_clodex_theme() -> Result<()> {
    let home = std::env::var_os("HOME").context("HOME is not set")?;
    write_clodex_theme(&Path::new(&home).join(".claude").join("themes"))
}

fn write_clodex_theme(themes_directory: &Path) -> Result<()> {
    fs::create_dir_all(themes_directory).with_context(|| {
        format!(
            "could not create Claude theme directory {}",
            themes_directory.display()
        )
    })?;
    let path = themes_directory.join("clodex.json");
    if fs::read_to_string(&path).is_ok_and(|contents| contents == CLODEX_THEME) {
        return Ok(());
    }

    let temporary = themes_directory.join(format!("clodex.json.{}.tmp", std::process::id()));
    fs::write(&temporary, CLODEX_THEME)
        .with_context(|| format!("could not write Clodex theme {}", temporary.display()))?;
    fs::rename(&temporary, &path)
        .with_context(|| format!("could not save Clodex theme {}", path.display()))
}

/// A configured capacity above what the routed models accept is not a harmless
/// over-request: Claude Code would auto-compact well past the point where Codex
/// rejects every prompt, and the compaction request carries the same oversized
/// conversation, so it is rejected too.
fn warn_if_context_was_clamped(configured: Option<u64>, capacity: u64) {
    let Some(configured) = configured.filter(|configured| *configured > capacity) else {
        return;
    };
    if io::stderr().is_terminal() {
        eprintln!(
            "\x1b[33m!\x1b[0m Configured context {} exceeds what the routed Codex models accept; using {}.",
            format_tokens(configured),
            format_tokens(capacity)
        );
    }
}

fn print_banner(mapping: &ModelMapping, context_capacity: u64, compact_at: u8) {
    if io::stderr().is_terminal() {
        eprint!("\x1b]0;Clodex · Claude Code + Codex\x07");
        eprintln!(
            "\x1b[38;5;141m◆ Clodex\x1b[0m  Opus: {}  ·  context: {}  ·  compact: {}%",
            mapping.opus.display_name,
            format_tokens(context_capacity),
            compact_at
        );
    }
}

fn restore_terminal_title() {
    if io::stderr().is_terminal() {
        eprint!("\x1b]0;\x07");
    }
}

fn format_tokens(tokens: u64) -> String {
    if tokens >= 1_000_000 && tokens % 1_000_000 == 0 {
        format!("{}m", tokens / 1_000_000)
    } else if tokens >= 1_000 && tokens % 1_000 == 0 {
        format!("{}k", tokens / 1_000)
    } else {
        tokens.to_string()
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::ffi::OsStr;
    use std::time::{SystemTime, UNIX_EPOCH};

    use super::*;
    use crate::mapping::Route;

    #[test]
    fn writes_a_purple_clodex_theme_without_touching_other_themes() {
        let directory = std::env::temp_dir().join(format!(
            "clodex-theme-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&directory).unwrap();
        fs::write(directory.join("personal.json"), "{}").unwrap();

        write_clodex_theme(&directory).unwrap();
        let theme: serde_json::Value =
            serde_json::from_slice(&fs::read(directory.join("clodex.json")).unwrap()).unwrap();

        assert_eq!(theme["overrides"]["claude"], "#a78bfa");
        assert_eq!(theme["overrides"]["clawd_body"], "#a78bfa");
        assert_eq!(theme["overrides"]["promptBorder"], "#a78bfa");
        assert!(directory.join("personal.json").exists());
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn launch_settings_grant_trusted_tools_to_every_claude_agent() {
        let mut config = AppConfig::default();
        config
            .permissions
            .trust("mcp__codebase-memory-mcp__search_code")
            .unwrap();

        let settings: serde_json::Value =
            serde_json::from_str(&launch_settings(&config, None).unwrap()).unwrap();

        assert_eq!(settings["theme"], "custom:clodex");
        assert_eq!(
            settings["permissions"]["allow"],
            serde_json::json!(["mcp__codebase-memory-mcp__search_code"])
        );
    }

    #[test]
    fn custom_opus_route_receives_the_configured_context_capacity() {
        let mapping = mapping();
        let mut command = Command::new("claude");

        configure_model_context(&mut command, &mapping, 600_000, 90);

        let environment: HashMap<_, _> = command
            .get_envs()
            .filter_map(|(name, value)| value.map(|value| (name, value)))
            .collect();
        assert_eq!(
            environment.get(OsStr::new("ANTHROPIC_MODEL")),
            Some(&OsStr::new("gpt-opus"))
        );
        assert_eq!(
            environment.get(OsStr::new("CLAUDE_CODE_MAX_CONTEXT_TOKENS")),
            Some(&OsStr::new("600000"))
        );
        assert_eq!(
            environment.get(OsStr::new("CLAUDE_CODE_AUTO_COMPACT_WINDOW")),
            Some(&OsStr::new("600000"))
        );
        assert_eq!(
            environment.get(OsStr::new("CLAUDE_AUTOCOMPACT_PCT_OVERRIDE")),
            Some(&OsStr::new("90"))
        );
    }

    #[test]
    fn launch_command_passes_arguments_models_context_and_proxy_settings() {
        let mut config = AppConfig::default();
        config.permissions.trust("mcp__memory__search").unwrap();
        let command = build_claude_command(
            vec![OsString::from("--resume"), OsString::from("session-id")],
            &mapping(),
            &config,
            600_000,
            41_234,
            true,
        )
        .unwrap();

        assert_eq!(command.get_program(), "claude");
        let arguments: Vec<_> = command.get_args().collect();
        assert_eq!(arguments[0], "--settings");
        let settings: serde_json::Value =
            serde_json::from_str(arguments[1].to_str().unwrap()).unwrap();
        assert_eq!(settings["theme"], "custom:clodex");
        assert_eq!(
            settings["permissions"]["allow"],
            serde_json::json!(["mcp__memory__search"])
        );
        assert_eq!(
            &arguments[2..],
            [OsStr::new("--resume"), OsStr::new("session-id")]
        );

        let environment: HashMap<_, _> = command
            .get_envs()
            .map(|(name, value)| (name.to_owned(), value.map(OsStr::to_owned)))
            .collect();
        let value = |name: &str| {
            environment
                .get(OsStr::new(name))
                .and_then(|value| value.as_deref())
        };
        assert_eq!(
            value("ANTHROPIC_BASE_URL"),
            Some(OsStr::new("http://127.0.0.1:41234"))
        );
        assert_eq!(
            value("ANTHROPIC_AUTH_TOKEN"),
            Some(OsStr::new("clodex-local-proxy"))
        );
        assert_eq!(
            environment.get(OsStr::new("ANTHROPIC_API_KEY")),
            Some(&None)
        );
        assert_eq!(
            value("ANTHROPIC_DEFAULT_FABLE_MODEL"),
            Some(OsStr::new("gpt-fable"))
        );
        assert_eq!(
            value("ANTHROPIC_DEFAULT_OPUS_MODEL"),
            Some(OsStr::new("gpt-opus"))
        );
        assert_eq!(
            value("ANTHROPIC_DEFAULT_SONNET_MODEL"),
            Some(OsStr::new("gpt-sonnet"))
        );
        assert_eq!(
            value("ANTHROPIC_DEFAULT_HAIKU_MODEL"),
            Some(OsStr::new("gpt-sonnet"))
        );
        assert_eq!(
            value("CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC"),
            Some(OsStr::new("1"))
        );
        assert_eq!(
            value("CLAUDE_CODE_DISABLE_NONSTREAMING_FALLBACK"),
            Some(OsStr::new("1"))
        );
        assert_eq!(
            value("CLAUDE_CODE_SKIP_FAST_MODE_ORG_CHECK"),
            Some(OsStr::new("1"))
        );
        assert!(
            value("ANTHROPIC_CUSTOM_HEADERS")
                .and_then(OsStr::to_str)
                .is_some_and(|headers| headers
                    .lines()
                    .any(|line| line == "X-Clodex-Fast-Bridge: 1"))
        );
        assert!(
            value("ANTHROPIC_CUSTOM_HEADERS")
                .and_then(OsStr::to_str)
                .is_some_and(|headers| headers
                    .lines()
                    .any(|line| line == "X-Clodex-Initial-Model: gpt-opus"))
        );
    }

    #[test]
    fn old_supervisors_cannot_enable_a_false_fast_toggle() {
        let mut command = Command::new("claude");
        configure_fast_bridge(&mut command, false, "gpt-5.6-terra");
        let environment: HashMap<_, _> = command
            .get_envs()
            .map(|(name, value)| (name.to_owned(), value.map(OsStr::to_owned)))
            .collect();
        assert_eq!(
            environment.get(OsStr::new("CLAUDE_CODE_SKIP_FAST_MODE_ORG_CHECK")),
            Some(&None)
        );
        assert_eq!(
            environment.get(OsStr::new("CLAUDE_CODE_SKIP_FAST_MODE_NETWORK_ERRORS")),
            Some(&None)
        );
        assert!(!environment.contains_key(OsStr::new("ANTHROPIC_CUSTOM_HEADERS")));
    }

    #[test]
    fn launch_settings_omit_permissions_when_no_tools_are_trusted() {
        let settings: serde_json::Value =
            serde_json::from_str(&launch_settings(&AppConfig::default(), None).unwrap()).unwrap();
        assert_eq!(settings["theme"], "custom:clodex");
        assert!(settings.get("permissions").is_none());
    }

    #[test]
    fn token_counts_use_compact_labels_only_for_exact_units() {
        assert_eq!(format_tokens(1_000_000), "1m");
        assert_eq!(format_tokens(272_000), "272k");
        assert_eq!(format_tokens(272_001), "272001");
        assert_eq!(format_tokens(999), "999");
    }

    fn mapping() -> ModelMapping {
        ModelMapping {
            fable: Route {
                model: "gpt-fable".to_string(),
                display_name: "Fable".to_string(),
            },
            opus: Route {
                model: "gpt-opus".to_string(),
                display_name: "Opus".to_string(),
            },
            sonnet: Route {
                model: "gpt-sonnet".to_string(),
                display_name: "Sonnet".to_string(),
            },
            haiku_compatibility: Route {
                model: "gpt-sonnet".to_string(),
                display_name: "Sonnet".to_string(),
            },
        }
    }
}
