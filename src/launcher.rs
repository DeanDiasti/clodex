use std::collections::BTreeMap;
use std::ffi::OsString;
use std::fs;
use std::io::{self, IsTerminal};
use std::path::Path;
use std::process::Command;

use anyhow::{Context, Result, bail};

use crate::catalog::Catalog;
use crate::config::AppConfig;
use crate::mapping::{ModelMapping, Provider, Route};
use crate::{picker, supervisor};

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

pub fn run(claude_args: Vec<OsString>, fast: bool) -> Result<()> {
    let config = AppConfig::load()?;
    if !crate::config::config_path()?.exists() {
        config.save()?;
    }
    let catalog = Catalog::load()?;
    let mapping = ModelMapping::resolve(&catalog, &config.routes)?;
    let support = supervisor::CodexSupport::detect(config.codex.backend)?;
    support.require(&mapping.codex_models())?;
    let requires_claude_subscription = mapping.uses_anthropic();
    if requires_claude_subscription {
        require_claude_subscription()?;
    }

    let context_capacity = config.effective_context_capacity(&catalog, &mapping)?;
    warn_if_context_was_clamped(config.context.max_tokens, context_capacity);
    let lease = supervisor::acquire()?;
    let proxy_port = lease.proxy_port();
    let supports_fast_bridge = lease.supports_fast_bridge();
    if fast && !crate::fast_bridge::supports_session_fast(proxy_port) {
        lease.close();
        bail!(
            "clodex --fast requires the session-fast bridge. Close every active Clodex session, then start a new one"
        );
    }
    let fast_routes = fast.then(|| session_fast_routes(&catalog, &mapping, &support));
    // Only the bridge separates the two providers; without it, Claude-routed
    // requests and the subscription credential would reach the Codex proxy.
    if mapping.uses_anthropic() && !supports_fast_bridge {
        lease.close();
        bail!(
            "Claude routes need the current Clodex supervisor. Close every active Clodex session, then start a new one"
        );
    }
    // With a subscription login, Claude models are offered even when no role
    // is routed to one, so Claude Code keeps that login for every request.
    // Claude-routed launches already checked the subscription above. Reuse
    // that result instead of starting a second `claude auth status` process.
    let claude =
        supports_fast_bridge && (requires_claude_subscription || has_claude_subscription());
    let models = picker::entries(&catalog, &support, claude);
    if let Err(error) =
        picker::write_gateway_cache(&format!("http://127.0.0.1:{proxy_port}"), &models, &mapping)
    {
        // The picker is a convenience; routing works without it.
        if io::stderr().is_terminal() {
            eprintln!("\x1b[33m!\x1b[0m Could not list every model in /model: {error:#}");
        }
    }
    ensure_clodex_theme()?;

    print_banner(
        &mapping,
        context_capacity,
        config.context.compact_at_percent,
        fast,
    );

    let mut command = build_claude_command(
        claude_args,
        &mapping,
        &config,
        context_capacity,
        proxy_port,
        supports_fast_bridge,
        claude,
        &models,
        fast_routes.as_ref(),
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

#[allow(clippy::too_many_arguments)]
fn build_claude_command(
    claude_args: Vec<OsString>,
    mapping: &ModelMapping,
    config: &AppConfig,
    context_capacity: u64,
    proxy_port: u16,
    supports_fast_bridge: bool,
    claude: bool,
    models: &[picker::Entry],
    fast_routes: Option<&BTreeMap<String, String>>,
) -> Result<Command> {
    let mut command = Command::new("claude");
    command.args([
        "--settings",
        &launch_settings(config, Some(proxy_port), models)?,
    ]);
    // A user's own --agents takes precedence over the per-model agents.
    let user_agents = claude_args.iter().any(|argument| argument == "--agents");
    if !models.is_empty() && !user_agents {
        command.args(["--agents", &picker::agents_json(models)]);
    }
    command
        .args(claude_args)
        .env(
            "ANTHROPIC_BASE_URL",
            format!("http://127.0.0.1:{proxy_port}"),
        )
        .env_remove("ANTHROPIC_API_KEY");
    if !models.is_empty() {
        // Reads the model list Clodex wrote into the discovery cache.
        command.env("CLAUDE_CODE_ENABLE_GATEWAY_MODEL_DISCOVERY", "1");
    }
    if claude {
        // Claude Code then authenticates with its own subscription login. The
        // bridge forwards that credential to Anthropic only, and strips it
        // from every request bound for Codex.
        command.env_remove("ANTHROPIC_AUTH_TOKEN");
        command.env(
            "CLAUDE_CODE_AUTO_MODE_MODEL",
            crate::fast_bridge::AUTO_REVIEW_MODEL,
        );
        // Keep Claude Code's server-review default (and any user override).
        // Claude routes forward the review protocol intact; Codex routes
        // return no review results, so Claude Code uses its local classifier.
    } else {
        command.env("ANTHROPIC_AUTH_TOKEN", "clodex-local-proxy");
    }
    configure_fast_bridge(&mut command, supports_fast_bridge, &mapping.opus.model);
    configure_session_fast(&mut command, fast_routes)?;
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
            describe(&mapping.fable, "Top available Codex model"),
        )
        .env("ANTHROPIC_DEFAULT_OPUS_MODEL", &mapping.opus.model)
        .env(
            "ANTHROPIC_DEFAULT_OPUS_MODEL_NAME",
            format!("Opus · {}", mapping.opus.display_name),
        )
        .env(
            "ANTHROPIC_DEFAULT_OPUS_MODEL_DESCRIPTION",
            describe(&mapping.opus, "Second available Codex model"),
        )
        .env("ANTHROPIC_DEFAULT_SONNET_MODEL", &mapping.sonnet.model)
        .env(
            "ANTHROPIC_DEFAULT_SONNET_MODEL_NAME",
            format!("Sonnet · {}", mapping.sonnet.display_name),
        )
        .env(
            "ANTHROPIC_DEFAULT_SONNET_MODEL_DESCRIPTION",
            describe(&mapping.sonnet, "Third available Codex model"),
        )
        .env(
            "ANTHROPIC_DEFAULT_HAIKU_MODEL",
            &mapping.haiku_compatibility.model,
        )
        .env("CLAUDE_CODE_DISABLE_NONSTREAMING_FALLBACK", "1");
    if !claude {
        // With only the placeholder token, Claude Code's own calls to
        // Anthropic cannot authenticate. With the subscription login they
        // can, and they are what load feature-flagged tools and the
        // organization's managed plugins, so they stay on.
        command.env("CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC", "1");
    }
    Ok(command)
}

fn describe(route: &Route, codex: &'static str) -> &'static str {
    match route.provider {
        Provider::Codex => codex,
        Provider::Anthropic => "Claude model on your Claude subscription",
    }
}

/// Reports Claude Code's own login without reading the credential. Claude
/// routes send Claude Code's subscription token, so Clodex only needs to know
/// that one exists.
pub fn claude_login_status() -> Result<String> {
    let status = read_claude_login()?;
    Ok(match (status.logged_in, status.auth_method.as_deref()) {
        (true, Some("claude.ai")) => "Claude subscription".to_string(),
        (true, Some(method)) => format!("logged in with {method}, not a Claude subscription"),
        (true, None) => "logged in".to_string(),
        (false, _) => "not logged in".to_string(),
    })
}

fn has_claude_subscription() -> bool {
    read_claude_login()
        .is_ok_and(|status| status.logged_in && status.auth_method.as_deref() == Some("claude.ai"))
}

fn require_claude_subscription() -> Result<()> {
    let status = read_claude_login()?;
    if !status.logged_in || status.auth_method.as_deref() != Some("claude.ai") {
        bail!(
            "Claude routes reuse your Claude subscription, but Claude Code is not logged in with one. Run `claude auth login`, or reset the routes with `clodex config route <role> codex`"
        );
    }
    Ok(())
}

#[derive(serde::Deserialize)]
struct ClaudeLogin {
    #[serde(rename = "loggedIn", default)]
    logged_in: bool,
    #[serde(rename = "authMethod", default)]
    auth_method: Option<String>,
}

fn read_claude_login() -> Result<ClaudeLogin> {
    // Match the launched child: an inherited API key or token would otherwise
    // mask the subscription login it actually uses.
    let output = Command::new("claude")
        .args(["auth", "status"])
        .env_remove("ANTHROPIC_API_KEY")
        .env_remove("ANTHROPIC_AUTH_TOKEN")
        .output()
        .context("could not run `claude auth status`; is Claude Code installed?")?;
    // A logged-out Claude Code may still report its status as JSON, which
    // carries a clearer answer than the exit status alone.
    match serde_json::from_slice(&output.stdout) {
        Ok(status) => Ok(status),
        Err(_) if !output.status.success() => bail!(
            "`claude auth status` failed with {}: {}",
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        ),
        Err(error) => Err(error).context("could not read `claude auth status`"),
    }
}

fn configure_fast_bridge(command: &mut Command, supported: bool, initial_model: &str) {
    if supported {
        let marker = crate::fast_bridge::custom_headers(initial_model);
        let headers = std::env::var("ANTHROPIC_CUSTOM_HEADERS")
            .ok()
            .map(|headers| {
                headers
                    .lines()
                    .filter(|line| {
                        !line.split_once(':').is_some_and(|(name, _)| {
                            name.trim().to_ascii_lowercase().starts_with("x-clodex-")
                        })
                    })
                    .collect::<Vec<_>>()
                    .join("\n")
            })
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

fn session_fast_routes(
    catalog: &Catalog,
    mapping: &ModelMapping,
    support: &supervisor::CodexSupport,
) -> BTreeMap<String, String> {
    let mut routes: BTreeMap<_, _> = catalog
        .models
        .iter()
        .filter(|model| {
            model.supported_in_api
                && model.slug.starts_with("gpt-")
                && model
                    .additional_speed_tiers
                    .iter()
                    .any(|tier| tier == "fast")
        })
        .map(|model| (model.slug.clone(), format!("{}-fast", model.slug)))
        .filter(|(_, target)| support.supports(target))
        .collect();
    // Older Claude Code versions may send a bare Claude alias for a role.
    // Only Codex roles get aliases here; real Claude routes keep their IDs.
    for (aliases, route) in [
        (
            &["fable", "claude-fable-5", "claude-fable-5-1"][..],
            &mapping.fable,
        ),
        (
            &[
                "opus",
                "claude-opus-5",
                "claude-opus-5-5",
                "claude-opus-4-8",
                "claude-opus-4-7",
            ][..],
            &mapping.opus,
        ),
        (
            &[
                "sonnet",
                "claude-sonnet-5",
                "claude-sonnet-5-5",
                "claude-sonnet-4-6",
            ][..],
            &mapping.sonnet,
        ),
        (
            &["haiku", "claude-haiku-4-5", "claude-haiku-4-5-20251001"][..],
            &mapping.haiku_compatibility,
        ),
    ] {
        if route.is_codex()
            && let Some(target) = routes.get(&route.model).cloned()
        {
            for alias in aliases {
                routes.insert((*alias).to_string(), target.clone());
            }
        }
    }
    routes
}

fn configure_session_fast(
    command: &mut Command,
    routes: Option<&BTreeMap<String, String>>,
) -> Result<()> {
    command.env("CLODEX_FAST", if routes.is_some() { "1" } else { "0" });
    let Some(routes) = routes else {
        command.env_remove("CLODEX_FAST_ROUTES");
        return Ok(());
    };
    let routes = serde_json::to_string(routes)?;
    let headers = command
        .get_envs()
        .find(|(key, _)| *key == "ANTHROPIC_CUSTOM_HEADERS")
        .and_then(|(_, value)| value)
        .and_then(|value| value.to_str())
        .unwrap_or("");
    let headers = format!("{headers}\nX-Clodex-Session-Fast: {routes}");
    command
        .env("ANTHROPIC_CUSTOM_HEADERS", headers)
        .env("CLODEX_FAST_ROUTES", routes)
        // Clodex owns the tier. Claude's Opus-only toggle must not switch the
        // selected model or enable Anthropic fast mode from saved preferences.
        .env("CLAUDE_CODE_DISABLE_FAST_MODE", "1");
    Ok(())
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

fn launch_settings(
    config: &AppConfig,
    bridge_port: Option<u16>,
    models: &[picker::Entry],
) -> Result<String> {
    let mut settings = serde_json::json!({
        "theme": "custom:clodex",
    });
    if let Some(model_picker) = picker::model_picker(models) {
        settings["modelPicker"] = model_picker;
    }
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
    // No `exec`: it would replace the shell, leaving `|| true` unreachable and
    // surfacing a failed arm as a failed hook.
    let command = format!(
        "curl --silent --show-error --max-time 5          --header 'content-type: application/json'          --data @- http://127.0.0.1:{port}/__clodex/compaction/arm >/dev/null 2>&1 || true"
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
            "\x1b[33m!\x1b[0m Configured context {} exceeds what the routed models accept; using {}.",
            format_tokens(configured),
            format_tokens(capacity)
        );
    }
}

fn print_banner(mapping: &ModelMapping, context_capacity: u64, compact_at: u8, fast: bool) {
    if io::stderr().is_terminal() {
        eprint!("\x1b]0;Clodex · Claude Code + Codex\x07");
        eprintln!(
            "\x1b[38;5;141m◆ Clodex\x1b[0m  Opus: {}  ·  context: {}  ·  compact: {}%{}",
            mapping.opus.display_name,
            format_tokens(context_capacity),
            compact_at,
            if fast {
                "  ·  FAST (Codex session)"
            } else {
                ""
            }
        );
    }
}

fn restore_terminal_title() {
    if io::stderr().is_terminal() {
        eprint!("\x1b]0;\x07");
    }
}

fn format_tokens(tokens: u64) -> String {
    if tokens >= 1_000_000 && tokens.is_multiple_of(1_000_000) {
        format!("{}m", tokens / 1_000_000)
    } else if tokens >= 1_000 && tokens.is_multiple_of(1_000) {
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
            serde_json::from_str(&launch_settings(&config, None, &[]).unwrap()).unwrap();

        assert_eq!(settings["theme"], "custom:clodex");
        assert_eq!(
            settings["permissions"]["allow"],
            serde_json::json!(["mcp__codebase-memory-mcp__search_code"])
        );
    }

    #[test]
    fn launch_settings_list_every_model_as_a_curated_picker_row() {
        let models = [picker::Entry {
            id: "gpt-6-luna".to_string(),
            display_name: "GPT-6-Luna".to_string(),
            description: "Codex".to_string(),
            agent: "codex-gpt-6-luna".to_string(),
        }];
        let settings: serde_json::Value =
            serde_json::from_str(&launch_settings(&AppConfig::default(), None, &models).unwrap())
                .unwrap();
        assert_eq!(
            settings["modelPicker"],
            serde_json::json!({"options": [{
                "model": "gpt-6-luna",
                "label": "GPT-6-Luna",
                "description": "Codex",
            }]})
        );

        let settings: serde_json::Value =
            serde_json::from_str(&launch_settings(&AppConfig::default(), None, &[]).unwrap())
                .unwrap();
        assert!(settings.get("modelPicker").is_none());
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
            false,
            &[],
            None,
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
    fn session_fast_is_exported_without_native_model_switching() {
        let routes = BTreeMap::from([("gpt-opus".into(), "gpt-opus-fast".into())]);
        let command = build_claude_command(
            vec![OsString::from("--resume"), OsString::from("session-id")],
            &mapping(),
            &AppConfig::default(),
            600_000,
            41_234,
            true,
            true,
            &[],
            Some(&routes),
        )
        .unwrap();
        let environment: HashMap<_, _> = command
            .get_envs()
            .filter_map(|(name, value)| value.map(|value| (name, value)))
            .collect();
        assert_eq!(environment[OsStr::new("CLODEX_FAST")], "1");
        assert_eq!(
            environment[OsStr::new("CLAUDE_CODE_DISABLE_FAST_MODE")],
            "1"
        );
        assert_eq!(environment[OsStr::new("ANTHROPIC_MODEL")], "gpt-opus");
        let exported: BTreeMap<String, String> = serde_json::from_str(
            environment[OsStr::new("CLODEX_FAST_ROUTES")]
                .to_str()
                .unwrap(),
        )
        .unwrap();
        assert_eq!(exported, routes);
        assert!(
            environment[OsStr::new("ANTHROPIC_CUSTOM_HEADERS")]
                .to_str()
                .unwrap()
                .contains("X-Clodex-Session-Fast:")
        );
        let arguments: Vec<_> = command.get_args().collect();
        assert_eq!(
            &arguments[2..],
            [OsStr::new("--resume"), OsStr::new("session-id")]
        );
    }

    #[test]
    fn session_fast_routes_follow_catalog_backend_and_provider() {
        let catalog: Catalog = serde_json::from_value(serde_json::json!({"models":[
            {"slug":"gpt-opus","display_name":"Opus","supported_in_api":true,"additional_speed_tiers":["fast"]},
            {"slug":"gpt-fable","display_name":"Fable","supported_in_api":true,"additional_speed_tiers":["fast"]},
            {"slug":"gpt-sonnet","display_name":"Sonnet","supported_in_api":true},
            {"slug":"gpt-private","display_name":"Private","supported_in_api":false,"additional_speed_tiers":["fast"]}
        ]})).unwrap();
        let routes = session_fast_routes(&catalog, &mapping(), &supervisor::CodexSupport::Builtin);
        assert_eq!(routes["gpt-opus"], "gpt-opus-fast");
        assert_eq!(routes["opus"], "gpt-opus-fast");
        assert_eq!(routes["fable"], "gpt-fable-fast");
        for model in ["gpt-sonnet", "sonnet", "haiku", "gpt-private"] {
            assert!(!routes.contains_key(model));
        }
        let mut mixed = mapping();
        mixed.opus = Route::anthropic("claude-opus-5-5");
        let routes = session_fast_routes(&catalog, &mixed, &supervisor::CodexSupport::Builtin);
        assert_eq!(routes["gpt-opus"], "gpt-opus-fast");
        for model in ["opus", "claude-opus-5-5", "anthropic/claude-opus-5-5"] {
            assert!(!routes.contains_key(model));
        }
        let support = supervisor::CodexSupport::Proxy("gpt-opus, gpt-opus-fast, gpt-fable".into());
        let routes = session_fast_routes(&catalog, &mapping(), &support);
        assert_eq!(routes["gpt-opus"], "gpt-opus-fast");
        assert!(!routes.contains_key("gpt-fable"));
        assert!(!routes.contains_key("fable"));
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
            serde_json::from_str(&launch_settings(&AppConfig::default(), None, &[]).unwrap())
                .unwrap();
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

    fn codex_route(model: &str, display_name: &str) -> Route {
        Route {
            model: model.to_string(),
            display_name: display_name.to_string(),
            provider: Provider::Codex,
        }
    }

    #[test]
    fn claude_routes_launch_with_claude_codes_own_login() {
        let mut mapping = mapping();
        mapping.opus = Route::anthropic("claude-opus-5-5");
        let command = build_claude_command(
            Vec::new(),
            &mapping,
            &AppConfig::default(),
            600_000,
            41_234,
            true,
            true,
            &[],
            None,
        )
        .unwrap();

        let environment: HashMap<_, _> = command
            .get_envs()
            .map(|(name, value)| (name.to_owned(), value.map(OsStr::to_owned)))
            .collect();
        let value = |name: &str| environment.get(OsStr::new(name)).cloned();
        assert_eq!(value("ANTHROPIC_AUTH_TOKEN"), Some(None));
        assert_eq!(value("ANTHROPIC_API_KEY"), Some(None));
        assert_eq!(
            value("CLAUDE_CODE_AUTO_MODE_MODEL"),
            Some(Some("anthropic/claude-sonnet-5".into()))
        );
        assert_eq!(value("CLAUDE_CODE_AUTO_MODE_SERVER"), None);
        // Claude Code's own Anthropic calls load its plugins and tools.
        assert_eq!(value("CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC"), None);
        assert_eq!(
            value("ANTHROPIC_DEFAULT_OPUS_MODEL"),
            Some(Some("anthropic/claude-opus-5-5".into()))
        );
        assert_eq!(
            value("ANTHROPIC_DEFAULT_OPUS_MODEL_DESCRIPTION"),
            Some(Some("Claude model on your Claude subscription".into()))
        );
        assert_eq!(
            value("ANTHROPIC_DEFAULT_FABLE_MODEL_DESCRIPTION"),
            Some(Some("Top available Codex model".into()))
        );
    }

    #[test]
    fn listed_models_enable_the_picker_and_a_subagent_per_model() {
        let models = vec![picker::Entry {
            id: "gpt-6-luna".to_string(),
            display_name: "GPT-6-Luna".to_string(),
            description: "Codex".to_string(),
            agent: "codex-gpt-6-luna".to_string(),
        }];
        let command = build_claude_command(
            vec![OsString::from("--resume")],
            &mapping(),
            &AppConfig::default(),
            600_000,
            41_234,
            true,
            true,
            &models,
            None,
        )
        .unwrap();

        let arguments: Vec<_> = command.get_args().collect();
        assert_eq!(arguments[2], "--agents");
        let agents: serde_json::Value =
            serde_json::from_str(arguments[3].to_str().unwrap()).unwrap();
        assert_eq!(agents["codex-gpt-6-luna"]["model"], "gpt-6-luna");
        assert_eq!(arguments[4], "--resume");
        assert!(command.get_envs().any(|(name, value)| {
            name == "CLAUDE_CODE_ENABLE_GATEWAY_MODEL_DISCOVERY" && value == Some(OsStr::new("1"))
        }));

        // A user's own --agents is left in charge.
        let command = build_claude_command(
            vec![OsString::from("--agents"), OsString::from("{}")],
            &mapping(),
            &AppConfig::default(),
            600_000,
            41_234,
            true,
            true,
            &models,
            None,
        )
        .unwrap();
        let arguments: Vec<_> = command.get_args().collect();
        assert_eq!(
            arguments
                .iter()
                .filter(|argument| **argument == "--agents")
                .count(),
            1
        );
    }

    fn mapping() -> ModelMapping {
        ModelMapping {
            fable: codex_route("gpt-fable", "Fable"),
            opus: codex_route("gpt-opus", "Opus"),
            sonnet: codex_route("gpt-sonnet", "Sonnet"),
            haiku_compatibility: codex_route("gpt-sonnet", "Sonnet"),
        }
    }
}
