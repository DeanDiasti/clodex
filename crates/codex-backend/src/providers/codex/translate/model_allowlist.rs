use std::collections::HashSet;
use std::sync::RwLock;

use crate::config;

use super::request::ServiceTier;

pub const ALLOWED_MODELS: &[&str] = &[
    "gpt-5.2",
    "gpt-5.3-codex",
    "gpt-5.3-codex-spark",
    "gpt-5.4",
    "gpt-5.4-mini",
    "gpt-5.5",
    "gpt-5.6-luna",
    "gpt-5.6-sol",
    "gpt-5.6-terra",
    "gpt-6-astra",
    "gpt-6-luna",
    "gpt-6-sol",
];

pub const MODEL_ALIASES: &[(&str, &str)] = &[
    ("haiku", "gpt-6-luna"),
    ("claude-haiku-4-5", "gpt-6-luna"),
    ("claude-haiku-4-5-20251001", "gpt-6-luna"),
    ("sonnet", "gpt-5.6-terra"),
    ("claude-sonnet-4-6", "gpt-5.6-terra"),
    ("claude-sonnet-5", "gpt-5.6-terra"),
    ("opus", "gpt-6-sol"),
    ("claude-opus-4-7", "gpt-6-sol"),
    ("claude-opus-4-8", "gpt-6-sol"),
    ("claude-opus-5", "gpt-6-sol"),
    ("claude-opus-5-5", "gpt-6-sol"),
    ("fable", "gpt-6-sol"),
    ("claude-fable-5", "gpt-6-sol"),
];

/// One entry of the live Codex model catalog (`codex debug models`).
///
/// Clodex installs the catalog at startup so that every model Codex offers is
/// routable, including ones released after this code was vendored. The
/// hard-coded lists above remain the fallback when no catalog is installed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CatalogModel {
    pub slug: String,
    /// Whether the model is served by the Responses Lite lane, when the
    /// catalog says; otherwise the vendored lane table decides.
    pub responses_lite: Option<bool>,
}

static CATALOG: RwLock<Option<Vec<CatalogModel>>> = RwLock::new(None);

/// Replaces the installed live catalog.
pub fn install_catalog(models: Vec<CatalogModel>) {
    *CATALOG.write().expect("Codex catalog lock") = Some(models);
}

fn catalog_model(slug: &str) -> Option<CatalogModel> {
    CATALOG
        .read()
        .expect("Codex catalog lock")
        .as_ref()?
        .iter()
        .find(|model| model.slug == slug)
        .cloned()
}

/// Whether `slug` is a base model in the installed live catalog.
pub fn is_catalog_model(slug: &str) -> bool {
    catalog_model(slug).is_some()
}

/// Every base model this backend accepts: the live catalog, then the
/// vendored fallback list.
pub fn allowed_models() -> Vec<String> {
    let mut models: Vec<String> = CATALOG
        .read()
        .expect("Codex catalog lock")
        .as_ref()
        .map(|catalog| catalog.iter().map(|model| model.slug.clone()).collect())
        .unwrap_or_default();
    for model in ALLOWED_MODELS {
        if !models.iter().any(|existing| existing == model) {
            models.push((*model).to_string());
        }
    }
    models
}

fn is_allowed_model(model: &str) -> bool {
    ALLOWED_MODELS.contains(&model) || is_catalog_model(model)
}

#[derive(Debug, Clone)]
pub struct ResolvedModel {
    pub model: String,
    pub service_tier: Option<ServiceTier>,
}

fn fast_model_aliases() -> HashSet<String> {
    allowed_models()
        .iter()
        .map(|m| format!("{m}-fast"))
        .collect()
}

fn resolve_fast_model_alias(model: &str) -> ResolvedModel {
    let fast_set = fast_model_aliases();
    if fast_set.contains(model) {
        let base = model.trim_end_matches("-fast");
        ResolvedModel {
            model: base.to_string(),
            service_tier: Some(ServiceTier::Priority),
        }
    } else {
        ResolvedModel {
            model: model.to_string(),
            service_tier: None,
        }
    }
}

pub fn resolve_model_request(model: &str) -> ResolvedModel {
    resolve_model_request_with_config_override(model, true)
}

pub fn resolve_model_request_with_config_override(
    model: &str,
    apply_config_override: bool,
) -> ResolvedModel {
    let alias = MODEL_ALIASES
        .iter()
        .find(|(alias, _)| *alias == model)
        .map(|(_, target)| *target)
        .unwrap_or(model);

    let requested = resolve_fast_model_alias(alias);

    let override_model = apply_config_override.then(config::codex_model).flatten();
    let resolved = match override_model {
        Some(ref val) if !val.is_empty() => resolve_fast_model_alias(val),
        _ => requested.clone(),
    };

    ResolvedModel {
        model: resolved.model,
        service_tier: if requested.service_tier == Some(ServiceTier::Priority)
            || resolved.service_tier == Some(ServiceTier::Priority)
        {
            Some(ServiceTier::Priority)
        } else {
            resolved.service_tier
        },
    }
}

pub fn resolve_model(model: &str) -> String {
    resolve_model_request(model).model
}

#[derive(Debug, Clone)]
pub struct ModelNotAllowedError {
    pub model: String,
}

impl std::fmt::Display for ModelNotAllowedError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Model not allowed: {}", self.model)
    }
}

pub fn assert_allowed_model(model: &str) -> Result<(), ModelNotAllowedError> {
    if is_allowed_model(model) {
        Ok(())
    } else {
        Err(ModelNotAllowedError {
            model: model.to_string(),
        })
    }
}

pub fn uses_responses_lite(model: &str) -> bool {
    if let Some(lite) = catalog_model(model).and_then(|model| model.responses_lite) {
        return lite;
    }
    matches!(
        model,
        "gpt-5.6-luna"
            | "gpt-5.6-sol"
            | "gpt-5.6-terra"
            | "gpt-6-astra"
            | "gpt-6-luna"
            | "gpt-6-sol"
    )
}

/// Luna models exist only behind the Responses Lite lane; the full
/// Responses API resolves them to a `-free` variant and returns 404 (Model not
/// found gpt-5.6-luna-free-...). Hosted web_search requests must run on the
/// full lane, so luna is upgraded to its nearest full-lane sibling.
pub fn full_lane_web_search_model(model: &str) -> &str {
    match model {
        "gpt-5.6-luna" => "gpt-5.6-sol",
        "gpt-6-luna" => "gpt-6-sol",
        _ => model,
    }
}

pub fn is_valid_model_for_codex(model: &str) -> bool {
    if is_allowed_model(model) {
        return true;
    }
    let fast_set = fast_model_aliases();
    if fast_set.contains(model) {
        return true;
    }
    MODEL_ALIASES.iter().any(|(alias, _)| *alias == model)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn haiku_resolves_to_luna() {
        let r = resolve_model_request("haiku");
        assert_eq!(r.model, "gpt-6-luna");
    }

    #[test]
    fn web_search_upgrades_luna_to_full_lane_sibling() {
        assert_eq!(full_lane_web_search_model("gpt-5.6-luna"), "gpt-5.6-sol");
        assert_eq!(full_lane_web_search_model("gpt-5.6-sol"), "gpt-5.6-sol");
        assert_eq!(full_lane_web_search_model("gpt-5.6-terra"), "gpt-5.6-terra");
        assert_eq!(full_lane_web_search_model("gpt-5.4"), "gpt-5.4");
        assert_eq!(full_lane_web_search_model("gpt-6-luna"), "gpt-6-sol");
        assert_eq!(full_lane_web_search_model("gpt-6-sol"), "gpt-6-sol");
    }

    #[test]
    fn sonnet_resolves_to_terra() {
        let r = resolve_model_request("sonnet");
        assert_eq!(r.model, "gpt-5.6-terra");
    }

    #[test]
    fn sonnet_5_resolves_to_terra() {
        let r = resolve_model_request("claude-sonnet-5");
        assert_eq!(r.model, "gpt-5.6-terra");
    }

    #[test]
    fn opus_resolves_to_sol() {
        let r = resolve_model_request("opus");
        assert_eq!(r.model, "gpt-6-sol");
    }

    #[test]
    fn opus_aliases_resolve_to_sol() {
        for model in ["claude-opus-4-8", "claude-opus-5", "claude-opus-5-5"] {
            let r = resolve_model_request(model);
            assert_eq!(r.model, "gpt-6-sol");
        }
    }

    #[test]
    fn fable_5_resolves_to_sol() {
        for model in ["fable", "claude-fable-5"] {
            let r = resolve_model_request(model);
            assert_eq!(r.model, "gpt-6-sol");
        }
    }

    #[test]
    fn gpt_6_sol_fast_adds_priority() {
        let r = resolve_model_request("gpt-6-sol-fast");
        assert_eq!(r.model, "gpt-6-sol");
        assert_eq!(r.service_tier, Some(ServiceTier::Priority));
    }

    #[test]
    fn gpt_6_models_use_responses_lite() {
        assert!(uses_responses_lite("gpt-6-sol"));
        assert!(uses_responses_lite("gpt-6-luna"));
    }

    #[test]
    fn fast_suffix_adds_priority() {
        let r = resolve_model_request("gpt-5.6-sol-fast");
        assert_eq!(r.model, "gpt-5.6-sol");
        assert_eq!(r.service_tier, Some(ServiceTier::Priority));
    }

    #[test]
    fn allowed_models_accept_base() {
        assert!(assert_allowed_model("gpt-5.4").is_ok());
        assert!(assert_allowed_model("gpt-5.6-sol").is_ok());
        assert!(assert_allowed_model("gpt-5.6-terra").is_ok());
        assert!(assert_allowed_model("gpt-6-astra").is_ok());
        assert!(assert_allowed_model("gpt-5.6-luna").is_ok());
    }

    #[test]
    fn an_installed_catalog_admits_new_models_and_decides_their_lane() {
        install_catalog(vec![
            CatalogModel {
                slug: "gpt-9-test-lite".to_string(),
                responses_lite: Some(true),
            },
            CatalogModel {
                slug: "gpt-9-test-full".to_string(),
                responses_lite: Some(false),
            },
        ]);
        assert!(assert_allowed_model("gpt-9-test-lite").is_ok());
        assert!(is_valid_model_for_codex("gpt-9-test-full-fast"));
        assert!(uses_responses_lite("gpt-9-test-lite"));
        assert!(!uses_responses_lite("gpt-9-test-full"));
        let fast = resolve_model_request("gpt-9-test-lite-fast");
        assert_eq!(fast.model, "gpt-9-test-lite");
        assert_eq!(fast.service_tier, Some(ServiceTier::Priority));
        // The vendored list stays accepted alongside the catalog.
        assert!(assert_allowed_model("gpt-5.4").is_ok());
    }

    #[test]
    fn not_allowed_rejected() {
        assert!(assert_allowed_model("gpt-7").is_err());
    }
}
