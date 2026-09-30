use std::process::Command;

use anyhow::Result;

use crate::auth;
use crate::catalog::Catalog;
use crate::config;
use crate::mapping::ModelMapping;

pub fn run() -> Result<()> {
    println!("clodex environment\n");
    let app_config = config::AppConfig::load()?;

    print_tool("Claude Code", "claude", &["--version"]);
    if Command::new("codex").arg("--version").output().is_ok() {
        print_tool("Codex CLI", "codex", &["--version"]);
    } else {
        println!("  {:<20} not installed (optional)", "Codex CLI");
    }
    match app_config.codex.backend {
        config::CodexBackend::Builtin => println!(
            "  {:<20} built in (claude-code-proxy {} Codex path)",
            "Codex backend",
            codex_backend_version()
        ),
        config::CodexBackend::Proxy => {
            print_tool("Translation proxy", "claude-code-proxy", &["--version"]);
        }
    }

    match auth::prepare_codex_credentials() {
        Ok(status) => {
            let account = status
                .account
                .map(|account| format!(", {account}"))
                .unwrap_or_default();
            println!(
                "  {:<20} ready ({}{account})",
                "Codex sign-in",
                status.source.describe()
            );
        }
        Err(error) => println!("  {:<20} unavailable ({error:#})", "Codex sign-in"),
    }

    print_claude_login(&app_config);
    println!(
        "  {:<20} {}",
        "Configured transport",
        app_config.codex.transport.as_str()
    );
    print_context_ceiling(&app_config);
    println!(
        "  {:<20} {}",
        "Clodex home",
        config::clodex_home()?.display()
    );

    println!("\nRun `clodex` in any repository to start a purple Clodex session.");
    Ok(())
}

/// Reports the capacity Claude Code will actually receive. A configured value
/// above the routed ceiling is clamped rather than passed through, because
/// Codex rejects an oversized prompt with an error that compaction cannot
/// recover from.
fn print_context_ceiling(app_config: &config::AppConfig) {
    let resolved = Catalog::load().and_then(|catalog| {
        let mapping = ModelMapping::resolve(&catalog, &app_config.routes)?;
        let ceiling = config::routed_context_ceiling(&catalog, &mapping)?;
        let capacity = app_config.effective_context_capacity(&catalog, &mapping)?;
        Ok((ceiling, capacity))
    });

    match resolved {
        Ok((ceiling, capacity)) => {
            let note = match app_config.context.max_tokens {
                Some(configured) if configured > ceiling => {
                    format!(" (clamped from {configured})")
                }
                _ => String::new(),
            };
            println!("  {:<20} {capacity} of {ceiling}{note}", "Context capacity");
        }
        Err(error) => println!("  {:<20} unavailable ({error:#})", "Context capacity"),
    }
}

/// Claude routes reuse Claude Code's own subscription login, so only its
/// presence matters; Clodex never reads the credential itself.
fn print_claude_login(app_config: &config::AppConfig) {
    let routed = app_config.routes != config::RoutesConfig::default();
    let status = match crate::launcher::claude_login_status() {
        Ok(status) => status,
        Err(error) => format!("unavailable ({error:#})"),
    };
    let note = if routed {
        ""
    } else {
        " (no Claude routes configured)"
    };
    println!("  {:<20} {status}{note}", "Claude login");
}

fn codex_backend_version() -> &'static str {
    codex_backend::VENDORED_VERSION
}

fn print_tool(label: &str, executable: &str, args: &[&str]) {
    match Command::new(executable).args(args).output() {
        Ok(output) if output.status.success() => {
            let version = String::from_utf8_lossy(&output.stdout);
            println!("  {label:<20} {}", version.trim());
        }
        _ => println!("  {label:<20} not installed"),
    }
}
