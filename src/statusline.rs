//! Session-scoped fast-mode state for custom Claude status lines.
use std::collections::BTreeMap;
use std::io::{self, Read};

use anyhow::Result;
use serde_json::Value;

pub fn run() -> Result<()> {
    if std::env::var("CLODEX_FAST").as_deref() != Ok("1") {
        return Ok(());
    }
    let routes: BTreeMap<String, String> = serde_json::from_str(
        &std::env::var("CLODEX_FAST_ROUTES").unwrap_or_else(|_| "{}".to_owned()),
    )?;
    let mut input = String::new();
    io::stdin().read_to_string(&mut input)?;
    let input: Value = serde_json::from_str(&input)?;
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
    fn label_tracks_current_model_instead_of_only_the_startup_preference() {
        let routes = BTreeMap::from([
            ("gpt-test".into(), "gpt-test-fast".into()),
            ("opus".into(), "gpt-test-fast".into()),
        ]);
        for model in ["gpt-test", "gpt-test-fast", "gpt-test[1m]", "opus"] {
            assert_eq!(
                label(&serde_json::json!({"model":{"id":model}}), &routes),
                "FAST"
            );
        }
        assert_eq!(
            label(&serde_json::json!({"model":{"id":"unsupported"}}), &routes),
            "FAST unavailable"
        );
        assert_eq!(label(&serde_json::json!({}), &routes), "FAST (session)");
    }
}
