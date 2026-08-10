use std::process::Command;

use anyhow::Result;

use crate::auth;
use crate::config;

pub fn run() -> Result<()> {
    println!("clodex environment\n");

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
        "Clodex home",
        config::clodex_home()?.display()
    );

    println!("\nRun `clodex` in any repository to start a purple Clodex session.");
    Ok(())
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
