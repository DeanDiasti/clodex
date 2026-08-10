mod auth;
mod catalog;
mod config;
mod doctor;
mod fast_bridge;
mod launcher;
mod mapping;
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
    about = "Claude Code harness with Codex subscription models"
)]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Inspect reuse of the existing Codex CLI login.
    Auth(AuthArgs),
    /// Inspect the live model catalog exposed by Codex.
    Models(ModelsArgs),
    /// Manage persistent defaults shared by every clodex instance.
    Config(ConfigArgs),
    /// Show the effective context settings for the current model catalog.
    Context,
    /// Check the local Claude, Codex, and proxy prerequisites.
    Doctor,
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
    /// Verify that the existing Codex login can be reused securely.
    Status,
    /// Ask Codex to refresh its managed login and sync the active proxy.
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
    /// Trust an exact Claude tool name in every Clodex agent.
    AllowTool {
        /// Tool name, such as mcp__codebase-memory-mcp__search_code.
        tool: String,
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
    let mut passthrough: Vec<OsString> = std::env::args_os().skip(1).collect();
    if should_launch_claude(&passthrough) {
        if passthrough.first().is_some_and(|argument| argument == "--") {
            passthrough.remove(0);
        }
        return launcher::run(passthrough);
    }

    let cli = Cli::parse();

    match cli.command {
        None => launcher::run(Vec::new()),
        Some(Command::Auth(args)) => run_auth(args),
        Some(Command::Models(args)) => run_models(args),
        Some(Command::Config(args)) => run_config(args),
        Some(Command::Context) => run_context(),
        Some(Command::Doctor) => doctor::run(),
        Some(Command::Supervisor) => supervisor::run(),
    }
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
            println!("Codex credential reuse is ready.");
            println!("  Authentication: {}", status.auth_mode);
            println!("  Source: {}", status.source.display());
            println!("  Token: loaded securely in memory and not displayed");
        }
        AuthCommand::Sync => {
            supervisor::sync_active_credentials()?;
            println!("Codex refreshed its managed login and Clodex synchronized the proxy.");
        }
    }
    Ok(())
}

fn run_models(args: ModelsArgs) -> Result<()> {
    let catalog = Catalog::load_from_codex()?;

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
            let mapping = ModelMapping::from_catalog(&catalog)?;
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
            let catalog = Catalog::load_from_codex()?;
            let mapping = ModelMapping::from_catalog(&catalog)?;
            let effective = config.effective_context_capacity(&catalog, &mapping)?;
            match config.context.max_tokens {
                Some(requested) if effective < requested => println!(
                    "Context ceiling set to {requested} tokens. The current Codex catalog caps Clodex at {effective} tokens."
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
    let catalog = Catalog::load_from_codex()?;
    let mapping = ModelMapping::from_catalog(&catalog)?;
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
