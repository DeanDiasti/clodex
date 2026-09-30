mod auth;
mod catalog;
mod compaction;
mod config;
mod doctor;
mod fast_bridge;
mod launcher;
mod mapping;
mod picker;
mod statusline;
mod supervisor;

use std::ffi::OsString;

use anyhow::Result;
use clap::{Args, Parser, Subcommand};

use crate::catalog::Catalog;
use crate::mapping::ModelMapping;

#[derive(Debug, Parser)]
#[command(
    name = "clodex",
    version,
    about = "Claude Code harness with Codex subscription models",
    after_help = "Launch: clodex [--fast] [--] [CLAUDE_ARGS]\nPlace --fast before Claude arguments to keep supported Codex models on the priority tier for this session, including subagents."
)]
struct Cli {
    /// Use the priority tier for supported Codex models in this session.
    #[arg(long)]
    fast: bool,
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Sign in to Codex with your ChatGPT account, or inspect the sign-in.
    Auth(AuthArgs),
    /// Inspect the live model catalog exposed by Codex.
    Models(ModelsArgs),
    /// Manage persistent defaults shared by every clodex instance.
    Config(ConfigArgs),
    /// Show the effective context settings for the current model catalog.
    Context,
    /// Check the local Claude, Codex sign-in, and backend prerequisites.
    Doctor,
    /// Print this launch's fast label from Claude status-line JSON on stdin.
    StatuslineFast,
    #[command(name = "__supervisor", hide = true)]
    Supervisor,
}

#[derive(Debug, Args)]
struct AuthArgs {
    #[command(subcommand)]
    command: Option<AuthCommand>,
}

#[derive(Debug, Subcommand)]
enum AuthCommand {
    /// Show which Codex sign-in Clodex uses and verify it can be read securely.
    Status,
    /// Sign in to Codex with your ChatGPT account. No Codex CLI is needed.
    Login {
        /// Sign in with a one-time code instead of a local browser, such as
        /// over SSH.
        #[arg(long)]
        device: bool,
    },
    /// Remove Clodex's own Codex sign-in.
    Logout,
    /// Refresh the Codex sign-in and sync the running backend.
    Sync,
}

#[derive(Debug, Args)]
struct ModelsArgs {
    #[command(subcommand)]
    command: Option<ModelsCommand>,

    /// Print machine-readable JSON.
    #[arg(long, global = true)]
    json: bool,
}

#[derive(Debug, Subcommand)]
enum ModelsCommand {
    /// List visible, API-supported models.
    List,
    /// Show the automatic Claude alias to Codex model mapping.
    Map,
}

#[derive(Debug, Args)]
struct ConfigArgs {
    #[command(subcommand)]
    command: Option<ConfigCommand>,
}

#[derive(Debug, Subcommand)]
enum ConfigCommand {
    /// Show all persistent settings.
    Show,
    /// Set the default context capacity, such as auto, 200k, or 256000.
    Context {
        /// Context capacity in tokens, or "auto".
        value: String,
    },
    /// Set the percentage at which Claude Code auto-compacts.
    CompactAt {
        /// Percentage from 1 through 95.
        percent: u8,
    },
    /// Select Codex HTTP SSE, WebSocket, or automatic transport.
    Transport {
        /// One of: http, websocket, or auto.
        value: String,
    },
    /// Select the built-in Codex translator or the external claude-code-proxy.
    Backend {
        /// One of: builtin or proxy.
        value: String,
    },
    /// Fold an oversized compaction into rounds that each fit the window.
    HierarchicalCompaction {
        /// One of: on or off.
        value: String,
    },
    /// Trust an exact Claude tool name in every Clodex agent.
    AllowTool {
        /// Tool name, such as mcp__codebase-memory-mcp__search_code.
        tool: String,
    },
    /// Route a Claude Code role to a Claude model on your Claude subscription.
    Route {
        /// One of: fable, opus, sonnet, or haiku.
        role: String,
        /// A Claude model ID, such as claude-opus-5-5, or "codex" to reset.
        model: String,
    },
    /// Remove a tool from Clodex's trusted allowlist.
    ForgetTool {
        /// Exact tool name to remove.
        tool: String,
    },
    /// Print the configuration file path.
    Path,
}

fn main() -> Result<()> {
    let (fast, mut passthrough) = launch_arguments(std::env::args_os().skip(1).collect());
    if should_launch_claude(&passthrough) {
        if passthrough.first().is_some_and(|argument| argument == "--") {
            passthrough.remove(0);
        }
        return launcher::run(passthrough, fast);
    }

    let cli = Cli::parse();

    match cli.command {
        None => launcher::run(Vec::new(), cli.fast),
        Some(Command::Auth(args)) => run_auth(args),
        Some(Command::Models(args)) => run_models(args),
        Some(Command::Config(args)) => run_config(args),
        Some(Command::Context) => run_context(),
        Some(Command::Doctor) => doctor::run(),
        Some(Command::StatuslineFast) => statusline::run(),
        Some(Command::Supervisor) => supervisor::run(),
    }
}

// Only the leading flag belongs to Clodex. Prompt text and everything after
// `--` remain Claude arguments, including a literal `--fast`.
fn launch_arguments(mut arguments: Vec<OsString>) -> (bool, Vec<OsString>) {
    let fast = arguments
        .first()
        .is_some_and(|argument| argument == "--fast");
    if fast {
        arguments.remove(0);
    }
    (fast, arguments)
}

fn should_launch_claude(arguments: &[OsString]) -> bool {
    let Some(first) = arguments.first().and_then(|value| value.to_str()) else {
        return !arguments.is_empty();
    };
    !matches!(
        first,
        "auth"
            | "models"
            | "config"
            | "context"
            | "doctor"
            | "statusline-fast"
            | "__supervisor"
            | "-h"
            | "--help"
            | "-V"
            | "--version"
    )
}

fn run_auth(args: AuthArgs) -> Result<()> {
    match args.command.unwrap_or(AuthCommand::Status) {
        AuthCommand::Status => {
            let status = auth::prepare_codex_credentials()?;
            println!("Codex sign-in is ready.");
            println!("  Source: {}", status.source.describe());
            if let Some(account) = status.account {
                println!("  Account: {account}");
            }
            println!("  File: {}", status.path.display());
            println!("  Token: loaded securely in memory and not displayed");
        }
        AuthCommand::Login { device } => {
            let summary = auth::login(device)?;
            match summary.account {
                Some(account) => println!("Signed in to Codex as {account}."),
                None => println!("Signed in to Codex."),
            }
            println!("  Saved to {}", summary.path.display());
        }
        AuthCommand::Logout => {
            if auth::logout()? {
                println!("Removed Clodex's Codex sign-in.");
            } else {
                println!("Clodex had no Codex sign-in of its own.");
            }
            if auth::codex_auth_path()?.exists() {
                println!("Clodex will fall back to the Codex CLI's login.");
            }
        }
        AuthCommand::Sync => {
            supervisor::sync_active_credentials()?;
            println!("Refreshed the Codex sign-in and synchronized the running backend.");
        }
    }
    Ok(())
}

fn run_models(args: ModelsArgs) -> Result<()> {
    let catalog = Catalog::load()?;

    match args.command.unwrap_or(ModelsCommand::List) {
        ModelsCommand::List => {
            if args.json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&catalog.routable_models())?
                );
            } else {
                print!("{}", catalog.render());
            }
        }
        ModelsCommand::Map => {
            let config = config::AppConfig::load()?;
            let mapping = ModelMapping::resolve(&catalog, &config.routes)?;
            if args.json {
                println!("{}", serde_json::to_string_pretty(&mapping)?);
            } else {
                print!("{}", mapping.render());
            }
        }
    }

    Ok(())
}

fn run_config(args: ConfigArgs) -> Result<()> {
    match args.command.unwrap_or(ConfigCommand::Show) {
        ConfigCommand::Show => {
            let config = config::AppConfig::load()?;
            print!("{}", config.render());
            println!("  File: {}", config::config_path()?.display());
        }
        ConfigCommand::Context { value } => {
            let mut config = config::AppConfig::load()?;
            config.context.max_tokens = config::parse_context_limit(&value)?;
            config.save()?;
            let catalog = Catalog::load()?;
            let mapping = ModelMapping::resolve(&catalog, &config.routes)?;
            let effective = config.effective_context_capacity(&catalog, &mapping)?;
            match config.context.max_tokens {
                Some(requested) if effective < requested => println!(
                    "Context ceiling set to {requested} tokens. The routed models cap Clodex at {effective} tokens."
                ),
                _ => println!(
                    "Default context window set to {} for all clodex instances.",
                    config.context.render_limit()
                ),
            }
        }
        ConfigCommand::CompactAt { percent } => {
            let mut config = config::AppConfig::load()?;
            config.context.set_compact_at_percent(percent)?;
            config.save()?;
            println!(
                "Auto-compaction set to {}% for all clodex instances.",
                percent
            );
        }
        ConfigCommand::Transport { value } => {
            let mut config = config::AppConfig::load()?;
            config.codex.transport = config::CodexTransport::parse(&value)?;
            config.save()?;
            println!(
                "Codex transport set to {}. Close every active Clodex session, then start a new one to apply it.",
                config.codex.transport.as_str()
            );
        }
        ConfigCommand::Backend { value } => {
            let mut config = config::AppConfig::load()?;
            config.codex.backend = config::CodexBackend::parse(&value)?;
            config.save()?;
            println!(
                "Codex backend set to {}. Close every active Clodex session, then start a new one to apply it.",
                config.codex.backend.as_str()
            );
        }
        ConfigCommand::HierarchicalCompaction { value } => {
            let enabled = match value.trim().to_ascii_lowercase().as_str() {
                "on" | "true" | "enabled" => true,
                "off" | "false" | "disabled" => false,
                _ => anyhow::bail!("invalid value {value:?}; expected on or off"),
            };
            let mut config = config::AppConfig::load()?;
            config.compaction.hierarchical = enabled;
            config.save()?;
            println!(
                "Hierarchical compaction {}. Start a new Clodex session to apply it.",
                if enabled { "enabled" } else { "disabled" }
            );
        }
        ConfigCommand::Route { role, model } => {
            let role = config::Role::parse(&role)?;
            let mut config = config::AppConfig::load()?;
            config.routes.set(role, &model)?;
            config.save()?;
            println!(
                "{} now routes to {}. Start a new Clodex session to apply it.",
                role.as_str(),
                if model.trim().eq_ignore_ascii_case("codex") {
                    "the automatic Codex model"
                } else {
                    "your Claude subscription"
                }
            );
        }
        ConfigCommand::AllowTool { tool } => {
            let mut config = config::AppConfig::load()?;
            if config.permissions.trust(&tool)? {
                config.save()?;
                println!("Trusted {tool} for all future clodex sessions and subagents.");
            } else {
                println!("{tool} is already trusted.");
            }
        }
        ConfigCommand::ForgetTool { tool } => {
            let mut config = config::AppConfig::load()?;
            if config.permissions.forget(&tool)? {
                config.save()?;
                println!("Removed {tool} from the Clodex trusted-tool allowlist.");
            } else {
                println!("{tool} was not in the Clodex trusted-tool allowlist.");
            }
        }
        ConfigCommand::Path => println!("{}", config::config_path()?.display()),
    }

    Ok(())
}

fn run_context() -> Result<()> {
    let config = config::AppConfig::load()?;
    let catalog = Catalog::load()?;
    let mapping = ModelMapping::resolve(&catalog, &config.routes)?;
    print!("{}", config.render_effective_context(&catalog, &mapping)?);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn arguments(values: &[&str]) -> Vec<OsString> {
        values.iter().map(OsString::from).collect()
    }

    #[test]
    fn clodex_commands_are_parsed_locally() {
        for command in [
            "auth",
            "models",
            "config",
            "context",
            "doctor",
            "statusline-fast",
            "__supervisor",
            "-h",
            "--help",
            "-V",
            "--version",
        ] {
            assert!(!should_launch_claude(&arguments(&[command])), "{command}");
        }
    }

    #[test]
    fn claude_flags_prompts_and_separator_are_passed_through() {
        for values in [
            &["--resume"][..],
            &["-p", "review this repository"][..],
            &["--", "--resume"][..],
            &["unknown-subcommand"][..],
        ] {
            assert!(should_launch_claude(&arguments(values)), "{values:?}");
        }
    }

    #[test]
    fn session_fast_is_only_consumed_before_claude_arguments() {
        assert_eq!(
            launch_arguments(arguments(&["--fast", "--resume"])),
            (true, arguments(&["--resume"]))
        );
        for args in [
            &["-p", "--fast"][..],
            &["--", "--fast"][..],
            &["--resume", "--fast"][..],
        ] {
            assert_eq!(launch_arguments(arguments(args)), (false, arguments(args)));
        }
        assert_eq!(
            launch_arguments(arguments(&["--fast", "--", "--fast"])),
            (true, arguments(&["--", "--fast"]))
        );
    }

    #[test]
    fn no_arguments_selects_the_default_launcher_path() {
        assert!(!should_launch_claude(&[]));
    }

    #[cfg(unix)]
    #[test]
    fn non_utf8_first_argument_is_safely_passed_through() {
        use std::os::unix::ffi::OsStringExt;

        assert!(should_launch_claude(&[OsString::from_vec(vec![0xff])]));
    }
}
