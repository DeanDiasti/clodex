//! Reads the launch's fast policy for a custom Claude Code status line.

use std::collections::BTreeMap;
use std::io::{self, Read};

use anyhow::{Context, Result};
use serde_json::Value;

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
