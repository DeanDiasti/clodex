use anyhow::{Result, bail};
use serde::Serialize;

use crate::catalog::{Catalog, Model};

#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
pub struct ModelMapping {
    pub fable: Route,
    pub opus: Route,
    pub sonnet: Route,
    /// Claude Code uses Haiku for background requests. It is intentionally
    /// hidden from the clodex picker, but still receives its own route.
    pub haiku_compatibility: Route,
}

#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
pub struct Route {
    pub model: String,
    pub display_name: String,
}

impl ModelMapping {
    pub fn from_catalog(catalog: &Catalog) -> Result<Self> {
        let models = catalog.routable_models();
        let Some(fable) = models.first() else {
            bail!("Codex did not return any visible, API-supported models");
        };

        let opus = models.get(1).unwrap_or(fable);
        let sonnet = models.get(2).unwrap_or(opus);
        let haiku = models.get(3).unwrap_or(sonnet);

        Ok(Self {
            fable: fable.into(),
            opus: opus.into(),
            sonnet: sonnet.into(),
            haiku_compatibility: haiku.into(),
        })
    }

    pub fn render(&self) -> String {
        format!(
            "Automatic Claude → Codex mapping\n\n\
             Fable   → {}\n\
             Opus    → {}\n\
             Sonnet  → {}\n\
             Haiku   → {} (background requests)\n\n\
             Reasoning effort is selected independently in Claude Code.\n",
            self.fable.display_name,
            self.opus.display_name,
            self.sonnet.display_name,
            self.haiku_compatibility.display_name
        )
    }
}

impl From<&Model> for Route {
    fn from(model: &Model) -> Self {
        Self {
            model: model.slug.clone(),
            display_name: model.display_name.clone(),
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
    fn maps_top_four_models_without_coupling_effort() {
        let catalog = Catalog {
            models: vec![
                model("luna", 4),
                model("sol", 2),
                model("astra", 1),
                model("terra", 3),
            ],
        };

        let mapping = ModelMapping::from_catalog(&catalog).unwrap();

        assert_eq!(mapping.fable.model, "astra");
        assert_eq!(mapping.opus.model, "sol");
        assert_eq!(mapping.sonnet.model, "terra");
        assert_eq!(mapping.haiku_compatibility.model, "luna");
    }

    #[test]
    fn gracefully_reuses_models_when_catalog_has_fewer_than_four() {
        let catalog = Catalog {
            models: vec![model("only", 1)],
        };

        let mapping = ModelMapping::from_catalog(&catalog).unwrap();

        assert_eq!(mapping.fable.model, "only");
        assert_eq!(mapping.opus.model, "only");
        assert_eq!(mapping.sonnet.model, "only");
    }

    #[test]
    fn two_models_reuse_the_second_for_later_routes() {
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
    fn renders_user_facing_roles_and_effort_guidance() {
        let catalog = Catalog {
            models: vec![
                model("astra", 1),
                model("sol", 2),
                model("terra", 3),
                model("luna", 4),
            ],
        };
        let output = ModelMapping::from_catalog(&catalog).unwrap().render();

        assert!(output.contains("Fable   → ASTRA"));
        assert!(output.contains("Opus    → SOL"));
        assert!(output.contains("Sonnet  → TERRA"));
        assert!(output.contains("Haiku   → LUNA (background requests)"));
        assert!(output.contains("Reasoning effort is selected independently"));
    }
}
