use anyhow::{Result, bail};
use serde::Serialize;

use crate::catalog::{Catalog, Model};
use crate::config::RoutesConfig;

/// Marks a routed model the bridge sends to Anthropic rather than Codex.
/// Claude Code treats a namespaced ID exactly like the bare one, so this costs
/// nothing in model detection, and it cannot collide with the `claude-*`
/// names Claude Code sends while fast mode shadows a Codex model.
pub const ANTHROPIC_PREFIX: &str = "anthropic/";

#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
pub struct ModelMapping {
    pub fable: Route,
    pub opus: Route,
    pub sonnet: Route,
    /// Claude Code uses Haiku for background requests. It is intentionally
    /// hidden from the clodex picker and follows the Sonnet route.
    pub haiku_compatibility: Route,
}

#[derive(Clone, Copy, Debug, Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Provider {
    Codex,
    Anthropic,
}

#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
pub struct Route {
    pub model: String,
    pub display_name: String,
    pub provider: Provider,
}

impl Route {
    pub fn anthropic(model: &str) -> Self {
        Self {
            model: format!("{ANTHROPIC_PREFIX}{model}"),
            display_name: model.to_string(),
            provider: Provider::Anthropic,
        }
    }

    pub fn is_codex(&self) -> bool {
        self.provider == Provider::Codex
    }
}

impl ModelMapping {
    /// Applies configured Claude routes over the automatic Codex mapping. A
    /// role without a configured route keeps its Codex model.
    pub fn resolve(catalog: &Catalog, routes: &RoutesConfig) -> Result<Self> {
        let mut mapping = Self::from_catalog(catalog)?;
        let overrides = [
            (&routes.fable, &mut mapping.fable),
            (&routes.opus, &mut mapping.opus),
            (&routes.sonnet, &mut mapping.sonnet),
            (&routes.haiku, &mut mapping.haiku_compatibility),
        ];
        for (configured, route) in overrides {
            if let Some(model) = configured {
                *route = Route::anthropic(model);
            }
        }
        Ok(mapping)
    }

    /// Every role Claude Code can select, plus the hidden background role.
    pub fn routes(&self) -> [&Route; 4] {
        [
            &self.fable,
            &self.opus,
            &self.sonnet,
            &self.haiku_compatibility,
        ]
    }

    pub fn uses_anthropic(&self) -> bool {
        self.routes().iter().any(|route| !route.is_codex())
    }

    /// The Codex models the translation proxy must be able to route.
    pub fn codex_models(&self) -> Vec<&str> {
        let mut models: Vec<&str> = Vec::new();
        for route in self.routes() {
            if route.is_codex() && !models.contains(&route.model.as_str()) {
                models.push(&route.model);
            }
        }
        models
    }

    pub fn from_catalog(catalog: &Catalog) -> Result<Self> {
        let models = catalog.routable_models();
        let Some(fable) = models.first() else {
            bail!("Codex did not return any visible, API-supported models");
        };

        let opus = models.get(1).unwrap_or(fable);
        let sonnet = models.get(2).unwrap_or(opus);

        Ok(Self {
            fable: fable.into(),
            opus: opus.into(),
            sonnet: sonnet.into(),
            haiku_compatibility: sonnet.into(),
        })
    }

    pub fn render(&self) -> String {
        format!(
            "Claude Code role mapping\n\n\
             Fable   → {}\n\
             Opus    → {}\n\
             Sonnet  → {}\n\n\
             Haiku is hidden; background Haiku requests route to {}.\n\
             Reasoning effort is selected independently in Claude Code.\n",
            self.fable.label(),
            self.opus.label(),
            self.sonnet.label(),
            self.haiku_compatibility.label()
        )
    }
}

impl Route {
    fn label(&self) -> String {
        match self.provider {
            Provider::Codex => self.display_name.clone(),
            Provider::Anthropic => format!("{} (Claude subscription)", self.display_name),
        }
    }
}

impl From<&Model> for Route {
    fn from(model: &Model) -> Self {
        Self {
            model: model.slug.clone(),
            display_name: model.display_name.clone(),
            provider: Provider::Codex,
        }
    }
}

#[cfg(test)]
mod tests {
    use pretty_assertions::assert_eq;

    use super::*;
    use crate::catalog::Model;

    fn model(slug: &str, priority: u32) -> Model {
        Model {
            slug: slug.to_string(),
            display_name: slug.to_uppercase(),
            description: String::new(),
            visibility: "list".to_string(),
            supported_in_api: true,
            priority,
            context_window: None,
            max_context_window: None,
            effective_context_window_percent: None,
            supported_reasoning_levels: Vec::new(),
            additional_speed_tiers: Vec::new(),
        }
    }

    #[test]
    fn maps_top_three_models_without_coupling_effort() {
        let catalog = Catalog {
            models: vec![model("luna", 3), model("sol", 1), model("terra", 2)],
        };

        let mapping = ModelMapping::from_catalog(&catalog).unwrap();

        assert_eq!(mapping.fable.model, "sol");
        assert_eq!(mapping.opus.model, "terra");
        assert_eq!(mapping.sonnet.model, "luna");
        assert_eq!(mapping.haiku_compatibility.model, "luna");
    }

    #[test]
    fn gracefully_reuses_models_when_catalog_has_fewer_than_three() {
        let catalog = Catalog {
            models: vec![model("only", 1)],
        };

        let mapping = ModelMapping::from_catalog(&catalog).unwrap();

        assert_eq!(mapping.fable.model, "only");
        assert_eq!(mapping.opus.model, "only");
        assert_eq!(mapping.sonnet.model, "only");
    }

    #[test]
    fn two_models_reuse_the_second_for_sonnet_and_haiku() {
        let catalog = Catalog {
            models: vec![model("first", 1), model("second", 2)],
        };

        let mapping = ModelMapping::from_catalog(&catalog).unwrap();

        assert_eq!(mapping.fable.model, "first");
        assert_eq!(mapping.opus.model, "second");
        assert_eq!(mapping.sonnet.model, "second");
        assert_eq!(mapping.haiku_compatibility.model, "second");
    }

    #[test]
    fn refuses_an_empty_routable_catalog() {
        let catalog = Catalog { models: vec![] };
        assert!(ModelMapping::from_catalog(&catalog).is_err());
    }

    #[test]
    fn configured_claude_routes_replace_only_their_roles() {
        let catalog = Catalog {
            models: vec![model("sol", 1), model("terra", 2), model("luna", 3)],
        };
        let routes = RoutesConfig {
            opus: Some("claude-opus-5-5".to_string()),
            haiku: Some("claude-haiku-4-5".to_string()),
            ..RoutesConfig::default()
        };

        let mapping = ModelMapping::resolve(&catalog, &routes).unwrap();

        assert_eq!(mapping.fable.model, "sol");
        assert_eq!(mapping.opus.model, "anthropic/claude-opus-5-5");
        assert_eq!(mapping.opus.provider, Provider::Anthropic);
        assert_eq!(mapping.sonnet.model, "luna");
        assert_eq!(
            mapping.haiku_compatibility.model,
            "anthropic/claude-haiku-4-5"
        );
        assert!(mapping.uses_anthropic());
        assert_eq!(mapping.codex_models(), ["sol", "luna"]);
        assert!(
            mapping
                .render()
                .contains("Opus    → claude-opus-5-5 (Claude subscription)")
        );
    }

    #[test]
    fn default_routes_keep_the_codex_mapping() {
        let catalog = Catalog {
            models: vec![model("sol", 1), model("terra", 2), model("luna", 3)],
        };
        let mapping = ModelMapping::resolve(&catalog, &RoutesConfig::default()).unwrap();
        assert_eq!(mapping, ModelMapping::from_catalog(&catalog).unwrap());
        assert!(!mapping.uses_anthropic());
    }

    #[test]
    fn renders_user_facing_roles_and_effort_guidance() {
        let catalog = Catalog {
            models: vec![model("sol", 1), model("terra", 2), model("luna", 3)],
        };
        let output = ModelMapping::from_catalog(&catalog).unwrap().render();

        assert!(output.contains("Fable   → SOL"));
        assert!(output.contains("Opus    → TERRA"));
        assert!(output.contains("Sonnet  → LUNA"));
        assert!(output.contains("background Haiku requests route to LUNA"));
        assert!(output.contains("Reasoning effort is selected independently"));
    }
}
