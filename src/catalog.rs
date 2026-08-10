use std::collections::HashSet;
use std::process::Command;

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Deserialize)]
pub struct Catalog {
    pub models: Vec<Model>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct Model {
    pub slug: String,
    pub display_name: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub visibility: String,
    #[serde(default)]
    pub supported_in_api: bool,
    #[serde(default = "default_priority")]
    pub priority: u32,
    #[serde(default)]
    pub context_window: Option<u64>,
    #[serde(default)]
    pub supported_reasoning_levels: Vec<ReasoningLevel>,
    #[serde(default)]
    pub additional_speed_tiers: Vec<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct ReasoningLevel {
    pub effort: String,
    #[serde(default)]
    pub description: String,
}

const fn default_priority() -> u32 {
    u32::MAX
}

impl Catalog {
    pub fn load_from_codex() -> Result<Self> {
        let output = Command::new("codex")
            .args(["debug", "models"])
            .output()
            .context("could not run `codex debug models`; is Codex CLI installed?")?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            bail!(
                "`codex debug models` failed with {}: {}",
                output.status,
                stderr.trim()
            );
        }

        serde_json::from_slice(&output.stdout)
            .context("Codex returned a model catalog that clodex could not parse")
    }

    pub fn routable_models(&self) -> Vec<Model> {
        let mut models: Vec<_> = self
            .models
            .iter()
            .filter(|model| model.visibility == "list" && model.supported_in_api)
            .cloned()
            .collect();

        models.sort_by_key(|model| {
            (
                model.priority,
                if model.slug.starts_with("gpt-") { 0 } else { 1 },
            )
        });
        let mut display_names = HashSet::new();
        models.retain(|model| display_names.insert(model.display_name.clone()));
        models
    }

    pub fn render(&self) -> String {
        let models = self.routable_models();
        let mut output = String::from("Codex models available to clodex\n\n");

        for model in models {
            let context = model
                .context_window
                .map(format_context_window)
                .unwrap_or_else(|| "unknown context".to_string());
            let efforts = model
                .supported_reasoning_levels
                .iter()
                .map(|level| level.effort.as_str())
                .collect::<Vec<_>>()
                .join(", ");

            output.push_str(&format!(
                "  {:<22} {:<12} {}\n",
                model.slug, context, efforts
            ));
            if !model.description.is_empty() {
                output.push_str(&format!("    {}\n", model.description));
            }
        }

        output
    }
}

fn format_context_window(tokens: u64) -> String {
    if tokens >= 1_000_000 && tokens % 1_000_000 == 0 {
        format!("{}m context", tokens / 1_000_000)
    } else if tokens >= 1_000 && tokens % 1_000 == 0 {
        format!("{}k context", tokens / 1_000)
    } else {
        format!("{tokens} context")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn model(slug: &str, priority: u32, visibility: &str, supported: bool) -> Model {
        Model {
            slug: slug.to_string(),
            display_name: slug.to_string(),
            description: String::new(),
            visibility: visibility.to_string(),
            supported_in_api: supported,
            priority,
            context_window: None,
            supported_reasoning_levels: Vec::new(),
            additional_speed_tiers: Vec::new(),
        }
    }

    #[test]
    fn routable_models_filters_and_sorts_catalog() {
        let catalog = Catalog {
            models: vec![
                model("third", 3, "list", true),
                model("hidden", 1, "hide", true),
                model("unsupported", 0, "list", false),
                model("first", 1, "list", true),
                model("second", 2, "list", true),
            ],
        };

        let slugs: Vec<_> = catalog
            .routable_models()
            .into_iter()
            .map(|model| model.slug)
            .collect();

        assert_eq!(slugs, ["first", "second", "third"]);
    }

    #[test]
    fn routable_models_prefers_canonical_slug_over_internal_alias() {
        let mut internal = model("codex-auto-review", 2, "list", true);
        internal.display_name = "GPT-Terra".to_string();
        let mut canonical = model("gpt-terra", 2, "list", true);
        canonical.display_name = "GPT-Terra".to_string();

        let catalog = Catalog {
            models: vec![internal, canonical],
        };

        let models = catalog.routable_models();
        assert_eq!(models.len(), 1);
        assert_eq!(models[0].slug, "gpt-terra");
    }

    #[test]
    fn context_window_format_is_compact() {
        assert_eq!(format_context_window(272_000), "272k context");
        assert_eq!(format_context_window(1_000_000), "1m context");
        assert_eq!(format_context_window(1_050_000), "1050k context");
        assert_eq!(format_context_window(999), "999 context");
    }

    #[test]
    fn parses_catalog_defaults_for_optional_fields() {
        let catalog: Catalog =
            serde_json::from_str(r#"{"models":[{"slug":"gpt-test","display_name":"Test"}]}"#)
                .unwrap();
        let model = &catalog.models[0];

        assert_eq!(model.description, "");
        assert_eq!(model.visibility, "");
        assert!(!model.supported_in_api);
        assert_eq!(model.priority, u32::MAX);
        assert_eq!(model.context_window, None);
        assert!(model.supported_reasoning_levels.is_empty());
        assert!(model.additional_speed_tiers.is_empty());
    }

    #[test]
    fn renders_context_effort_description_and_unknown_windows() {
        let mut detailed = model("gpt-detailed", 1, "list", true);
        detailed.context_window = Some(272_000);
        detailed.description = "Useful model".to_string();
        detailed.supported_reasoning_levels = vec![
            ReasoningLevel {
                effort: "low".to_string(),
                description: String::new(),
            },
            ReasoningLevel {
                effort: "high".to_string(),
                description: String::new(),
            },
        ];
        let unknown = model("gpt-unknown", 2, "list", true);

        let output = Catalog {
            models: vec![detailed, unknown],
        }
        .render();

        assert!(output.contains("gpt-detailed"));
        assert!(output.contains("272k context"));
        assert!(output.contains("low, high"));
        assert!(output.contains("Useful model"));
        assert!(output.contains("unknown context"));
    }

    #[test]
    fn duplicate_display_names_keep_the_highest_priority_entry() {
        let mut lower = model("gpt-lower", 5, "list", true);
        lower.display_name = "Same".to_string();
        let mut higher = model("internal-higher", 1, "list", true);
        higher.display_name = "Same".to_string();

        let models = Catalog {
            models: vec![lower, higher],
        }
        .routable_models();

        assert_eq!(models.len(), 1);
        assert_eq!(models[0].slug, "internal-higher");
    }
}
