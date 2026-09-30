//! Lists every routable model in Claude Code's `/model` picker and offers each
//! one to subagents.
//!
//! Claude Code shows extra picker entries from its gateway-discovery cache
//! when discovery is enabled, `ANTHROPIC_BASE_URL` is set, and the cache names
//! that same base URL. Reading the cache needs no credential, so Clodex can
//! supply the list itself while Claude Code keeps its own subscription login.
//! Only the fetch that refreshes the cache needs a credential, and the bridge
//! declines it so the list Clodex wrote is not replaced.

use std::fs;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use serde_json::{Map, Value, json};

use crate::catalog::Catalog;
use crate::mapping::{ANTHROPIC_PREFIX, ModelMapping};

/// Current Claude models reachable through the subscription.
const CLAUDE_MODELS: [(&str, &str); 4] = [
    ("claude-fable-5-1", "Claude Fable 5.1"),
    ("claude-opus-5-5", "Claude Opus 5.5"),
    ("claude-sonnet-5-5", "Claude Sonnet 5.5"),
    ("claude-haiku-4-5", "Claude Haiku 4.5"),
];

const AGENT_PROMPT: &str = "You are a general-purpose agent. Complete the task you were delegated \
using the available tools, then report the result concisely to the agent that delegated it.";

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Entry {
    /// The model ID Claude Code sends, which the bridge routes by.
    pub id: String,
    pub display_name: String,
    pub description: String,
    /// The subagent type that runs on this model.
    pub agent: String,
}

/// Every Codex model the proxy can route, then, with a subscription, every
/// current Claude model.
pub fn entries(catalog: &Catalog, proxy_listing: &str, claude: bool) -> Vec<Entry> {
    let codex = catalog
        .routable_models()
        .into_iter()
        .filter(|model| crate::supervisor::proxy_lists_model(proxy_listing, &model.slug))
        .map(|model| {
            let description = if model.description.is_empty() {
                "Codex".to_string()
            } else {
                format!("Codex · {}", model.description)
            };
            Entry {
                agent: agent_name("codex", &model.slug),
                id: model.slug,
                display_name: model.display_name,
                description,
            }
        });
    let claude_models = CLAUDE_MODELS
        .iter()
        .filter(|_| claude)
        .map(|(model, display_name)| Entry {
            id: format!("{ANTHROPIC_PREFIX}{model}"),
            display_name: (*display_name).to_string(),
            description: "Claude subscription".to_string(),
            agent: agent_name("", model),
        });
    codex.chain(claude_models).collect()
}

/// Writes the entries a role does not already show into Claude Code's
/// discovery cache for `base_url`.
pub fn write_gateway_cache(
    base_url: &str,
    entries: &[Entry],
    mapping: &ModelMapping,
) -> Result<()> {
    let directory = claude_config_dir()?.join("cache");
    fs::create_dir_all(&directory)
        .with_context(|| format!("could not create {}", directory.display()))?;
    let path = directory.join("gateway-models.json");
    let temporary = directory.join(format!("gateway-models.json.{}.tmp", std::process::id()));
    fs::write(&temporary, gateway_cache(base_url, entries, mapping)?)
        .with_context(|| format!("could not write {}", temporary.display()))?;
    restrict(&temporary)?;
    fs::rename(&temporary, &path).with_context(|| format!("could not save {}", path.display()))
}

fn gateway_cache(base_url: &str, entries: &[Entry], mapping: &ModelMapping) -> Result<Vec<u8>> {
    // A role's model is already in the picker under the role's own name.
    let roles: Vec<&str> = mapping
        .routes()
        .iter()
        .map(|route| route.model.as_str())
        .collect();
    let models: Vec<Value> = entries
        .iter()
        .filter(|entry| !roles.contains(&entry.id.as_str()))
        .map(|entry| {
            json!({
                "id": entry.id,
                "display_name": entry.display_name,
                "description": entry.description,
            })
        })
        .collect();
    let fetched_at = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_millis());
    Ok(serde_json::to_vec(&json!({
        "baseUrl": base_url,
        "fetchedAt": fetched_at,
        "models": models,
    }))?)
}

/// One subagent type per model, so a subagent can run on either provider.
/// The Agent tool's own `model` parameter only accepts the four role aliases.
pub fn agents_json(entries: &[Entry]) -> String {
    let agents: Map<String, Value> = entries
        .iter()
        .map(|entry| {
            let definition = json!({
                "description": format!(
                    "General-purpose agent that runs on {} ({}). Use it when a task should run on this specific model.",
                    entry.display_name, entry.description
                ),
                "prompt": AGENT_PROMPT,
                "model": entry.id,
            });
            (entry.agent.clone(), definition)
        })
        .collect();
    Value::Object(agents).to_string()
}

/// Agent names allow lowercase letters, digits, and hyphens.
fn agent_name(prefix: &str, model: &str) -> String {
    let slug: String = model
        .to_ascii_lowercase()
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() {
                character
            } else {
                '-'
            }
        })
        .collect();
    if prefix.is_empty() {
        slug
    } else {
        format!("{prefix}-{slug}")
    }
}

fn claude_config_dir() -> Result<PathBuf> {
    if let Some(directory) = std::env::var_os("CLAUDE_CONFIG_DIR") {
        return Ok(PathBuf::from(directory));
    }
    let home = std::env::var_os("HOME").context("HOME is not set")?;
    Ok(PathBuf::from(home).join(".claude"))
}

#[cfg(unix)]
fn restrict(path: &std::path::Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(0o600))
        .with_context(|| format!("could not restrict {}", path.display()))
}

#[cfg(not(unix))]
fn restrict(_path: &std::path::Path) -> Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use pretty_assertions::assert_eq;

    use super::*;
    use crate::catalog::Model;
    use crate::mapping::Route;

    fn model(slug: &str, priority: u32) -> Model {
        Model {
            slug: slug.to_string(),
            display_name: slug.to_uppercase(),
            description: format!("{slug} model."),
            visibility: "list".to_string(),
            supported_in_api: true,
            priority,
            context_window: Some(272_000),
            max_context_window: None,
            effective_context_window_percent: None,
            supported_reasoning_levels: Vec::new(),
            additional_speed_tiers: Vec::new(),
        }
    }

    fn catalog() -> Catalog {
        Catalog {
            models: vec![
                model("gpt-6.1-sol", 1),
                model("gpt-6-sol", 2),
                model("gpt-6-luna", 3),
            ],
        }
    }

    const LISTING: &str = "codex: claude-opus-5, gpt-6-sol, gpt-6-sol-fast, gpt-6-luna";

    #[test]
    fn lists_proxy_supported_codex_models_then_claude_models() {
        let ids: Vec<_> = entries(&catalog(), LISTING, true)
            .into_iter()
            .map(|entry| entry.id)
            .collect();
        assert_eq!(
            ids,
            [
                "gpt-6-sol",
                "gpt-6-luna",
                "anthropic/claude-fable-5-1",
                "anthropic/claude-opus-5-5",
                "anthropic/claude-sonnet-5-5",
                "anthropic/claude-haiku-4-5",
            ]
        );
    }

    #[test]
    fn claude_models_need_a_subscription() {
        let entries = entries(&catalog(), LISTING, false);
        assert!(
            entries
                .iter()
                .all(|entry| !entry.id.starts_with(ANTHROPIC_PREFIX))
        );
    }

    #[test]
    fn cache_names_the_bridge_and_skips_models_a_role_shows() {
        let mut mapping = ModelMapping::from_catalog(&catalog()).unwrap();
        mapping.opus = Route::anthropic("claude-opus-5-5");
        let entries = entries(&catalog(), LISTING, true);

        let cache: Value = serde_json::from_slice(
            &gateway_cache("http://127.0.0.1:41234", &entries, &mapping).unwrap(),
        )
        .unwrap();

        assert_eq!(cache["baseUrl"], "http://127.0.0.1:41234");
        let ids: Vec<_> = cache["models"]
            .as_array()
            .unwrap()
            .iter()
            .map(|model| model["id"].as_str().unwrap())
            .collect();
        // Fable is gpt-6.1-sol (unsupported), Opus is Claude Opus, and Sonnet
        // and Haiku are gpt-6-luna.
        assert_eq!(
            ids,
            [
                "gpt-6-sol",
                "anthropic/claude-fable-5-1",
                "anthropic/claude-sonnet-5-5",
                "anthropic/claude-haiku-4-5",
            ]
        );
        assert_eq!(cache["models"][0]["display_name"], "GPT-6-SOL");
        assert_eq!(
            cache["models"][0]["description"],
            "Codex · gpt-6-sol model."
        );
    }

    #[test]
    fn every_model_gets_a_subagent_on_its_own_provider() {
        let agents: Value =
            serde_json::from_str(&agents_json(&entries(&catalog(), LISTING, true))).unwrap();
        assert_eq!(agents["codex-gpt-6-luna"]["model"], "gpt-6-luna");
        assert_eq!(
            agents["claude-sonnet-5-5"]["model"],
            "anthropic/claude-sonnet-5-5"
        );
        assert_eq!(agents["claude-sonnet-5-5"]["prompt"], AGENT_PROMPT);
        assert_eq!(agents.as_object().unwrap().len(), 6);
    }

    #[test]
    fn agent_names_are_lowercase_hyphenated() {
        assert_eq!(agent_name("codex", "gpt-5.6-Sol"), "codex-gpt-5-6-sol");
        assert_eq!(agent_name("", "claude-opus-5-5"), "claude-opus-5-5");
    }
}
