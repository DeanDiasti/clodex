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
    print_tool("Codex CLI", "codex", &["--version"]);
    print_tool("Translation proxy", "claude-code-proxy", &["--version"]);

    let auth = Command::new("codex").args(["login", "status"]).output();
    match auth {
        Ok(output) if output.status.success() => {
            let stdout = String::from_utf8_lossy(&output.stdout);
            let stderr = String::from_utf8_lossy(&output.stderr);
            let status = if stdout.trim().is_empty() {
                stderr.trim()
            } else {
                stdout.trim()
            };
            println!("  {:<20} {}", "Codex authentication", status);
        }
        Ok(output) => {
            println!(
                "  {:<20} unavailable ({})",
                "Codex authentication",
                String::from_utf8_lossy(&output.stderr).trim()
            );
        }
        Err(_) => println!("  {:<20} unavailable", "Codex authentication"),
    }

    match auth::prepare_codex_credentials() {
        Ok(status) => println!("  {:<20} ready ({})", "Credential reuse", status.auth_mode),
        Err(error) => println!("  {:<20} unavailable ({error:#})", "Credential reuse"),
    }

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
    let resolved = Catalog::load_from_codex().and_then(|catalog| {
        let mapping = ModelMapping::from_catalog(&catalog)?;
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

fn print_tool(label: &str, executable: &str, args: &[&str]) {
    match Command::new(executable).args(args).output() {
        Ok(output) if output.status.success() => {
            let version = String::from_utf8_lossy(&output.stdout);
            println!("  {label:<20} {}", version.trim());
        }
        _ => println!("  {label:<20} not installed"),
    }
}
