use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

/// Where Codex publishes the model catalog for a signed-in ChatGPT account.
const MODELS_URL: &str = "https://chatgpt.com/backend-api/codex/models";
/// The Codex client version the catalog is requested for. The service hides
/// models that need a newer client, so this moves with the vendored backend.
pub const CODEX_CLIENT_VERSION: &str = "0.159.0";
/// How long a fetched catalog is reused before it is checked again.
const CACHE_TTL: Duration = Duration::from_secs(5 * 60);

/// A fetched catalog as cached under the Clodex home.
#[derive(Deserialize, Serialize)]
struct CachedCatalog {
    fetched_at_ms: u64,
    client_version: String,
    #[serde(default)]
    etag: Option<String>,
    catalog: serde_json::Value,
}

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
    /// The extended ceiling the model can reach on the Codex transport. The
    /// catalog reports this separately from the standard `context_window`
    /// usage threshold.
    #[serde(default)]
    pub max_context_window: Option<u64>,
    /// The share of a window Codex treats as usable, as a percentage.
    #[serde(default)]
    pub effective_context_window_percent: Option<u64>,
    #[serde(default)]
    pub supported_reasoning_levels: Vec<ReasoningLevel>,
    #[serde(default)]
    pub additional_speed_tiers: Vec<String>,
    /// Whether Codex serves the model on its Responses Lite lane.
    #[serde(default)]
    pub use_responses_lite: Option<bool>,
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

const DEFAULT_EFFECTIVE_CONTEXT_PERCENT: u64 = 95;

impl Model {
    /// The largest prompt this model will actually accept, after applying the
    /// catalog's effective-window percentage. Prefers the extended ceiling and
    /// falls back to the standard usage threshold.
    pub fn usable_context_window(&self) -> Option<u64> {
        let window = self.max_context_window.max(self.context_window)?;
        let percent = self
            .effective_context_window_percent
            .filter(|percent| (1..=100).contains(percent))
            .unwrap_or(DEFAULT_EFFECTIVE_CONTEXT_PERCENT);
        Some(window * percent / 100)
    }
}

impl Catalog {
    /// The live catalog for the signed-in account. A catalog fetched in the
    /// last few minutes is reused. When the fetch fails, the last cached
    /// catalog is used, then the Codex CLI's, if it is installed.
    pub fn load() -> Result<Self> {
        let path = cache_path()?;
        let cached = read_cache(&path);
        if let Some(cached) = cached.as_ref().filter(|cached| is_fresh(cached)) {
            return parse(&cached.catalog);
        }
        match fetch(cached.as_ref().and_then(|cached| cached.etag.as_deref())) {
            Ok(Fetched::Changed { catalog, etag }) => {
                let parsed = parse(&catalog)?;
                write_cache(&path, catalog, etag);
                Ok(parsed)
            }
            Ok(Fetched::Unchanged) => {
                let cached = cached.context("the catalog was unchanged but not cached")?;
                let parsed = parse(&cached.catalog)?;
                write_cache(&path, cached.catalog, cached.etag);
                Ok(parsed)
            }
            Err(error) => match cached {
                Some(cached) => parse(&cached.catalog),
                None => Self::load_from_codex().map_err(|_| error),
            },
        }
    }

    /// The last catalog `load` cached, without contacting the network.
    pub fn load_cached() -> Result<Self> {
        let cached =
            read_cache(&cache_path()?).context("no Codex model catalog has been cached yet")?;
        parse(&cached.catalog)
    }

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

enum Fetched {
    Changed {
        catalog: serde_json::Value,
        etag: Option<String>,
    },
    Unchanged,
}

fn fetch(etag: Option<&str>) -> Result<Fetched> {
    let credentials = crate::auth::load_codex_credentials(false)?;
    let credentials = if crate::auth::needs_refresh(&credentials) {
        crate::auth::load_codex_credentials(true).unwrap_or(credentials)
    } else {
        credentials
    };
    let client = reqwest::blocking::Client::builder()
        .connect_timeout(Duration::from_secs(10))
        .timeout(Duration::from_secs(20))
        .build()?;
    let mut request = client
        .get(MODELS_URL)
        .query(&[("client_version", CODEX_CLIENT_VERSION)])
        .bearer_auth(credentials.access_token())
        .header("originator", "codex_cli_rs")
        .header(
            reqwest::header::USER_AGENT,
            format!("codex_cli_rs/{CODEX_CLIENT_VERSION}"),
        );
    if let Some(account_id) = credentials.account_id() {
        request = request.header("chatgpt-account-id", account_id);
    }
    if let Some(etag) = etag {
        request = request.header(reqwest::header::IF_NONE_MATCH, etag);
    }
    let response = request
        .send()
        .context("could not reach the Codex model catalog")?;
    if response.status() == reqwest::StatusCode::NOT_MODIFIED {
        return Ok(Fetched::Unchanged);
    }
    if !response.status().is_success() {
        bail!(
            "the Codex model catalog returned HTTP {}",
            response.status().as_u16()
        );
    }
    let etag = response
        .headers()
        .get(reqwest::header::ETAG)
        .and_then(|value| value.to_str().ok())
        .map(str::to_string);
    let catalog = response
        .json()
        .context("the Codex model catalog was not valid JSON")?;
    Ok(Fetched::Changed { catalog, etag })
}

fn parse(catalog: &serde_json::Value) -> Result<Catalog> {
    Catalog::deserialize(catalog)
        .context("Codex returned a model catalog that clodex could not parse")
}

fn cache_path() -> Result<PathBuf> {
    Ok(crate::config::clodex_home()?
        .join("cache")
        .join("codex-models.json"))
}

fn read_cache(path: &Path) -> Option<CachedCatalog> {
    let cached: CachedCatalog = serde_json::from_slice(&fs::read(path).ok()?).ok()?;
    // A catalog requested for another client version may list other models.
    (cached.client_version == CODEX_CLIENT_VERSION).then_some(cached)
}

fn is_fresh(cached: &CachedCatalog) -> bool {
    now_ms().saturating_sub(cached.fetched_at_ms) < CACHE_TTL.as_millis() as u64
}

/// Caching is best effort; a catalog that cannot be saved is still used.
fn write_cache(path: &Path, catalog: serde_json::Value, etag: Option<String>) {
    let cached = CachedCatalog {
        fetched_at_ms: now_ms(),
        client_version: CODEX_CLIENT_VERSION.to_string(),
        etag,
        catalog,
    };
    let Some(directory) = path.parent() else {
        return;
    };
    let temporary = path.with_extension(format!("json.{}.tmp", std::process::id()));
    let saved = fs::create_dir_all(directory)
        .and_then(|()| fs::write(&temporary, serde_json::to_vec(&cached)?))
        .and_then(|()| fs::rename(&temporary, path));
    if saved.is_err() {
        let _ = fs::remove_file(&temporary);
    }
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_millis() as u64)
}

fn format_context_window(tokens: u64) -> String {
    if tokens >= 1_000_000 && tokens.is_multiple_of(1_000_000) {
        format!("{}m context", tokens / 1_000_000)
    } else if tokens >= 1_000 && tokens.is_multiple_of(1_000) {
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
            max_context_window: None,
            effective_context_window_percent: None,
            supported_reasoning_levels: Vec::new(),
            additional_speed_tiers: Vec::new(),
            use_responses_lite: None,
        }
    }

    #[test]
    fn cache_is_fresh_for_a_few_minutes_and_bound_to_the_client_version() {
        let directory = std::env::temp_dir().join(format!(
            "clodex-catalog-cache-{}-{}",
            std::process::id(),
            now_ms()
        ));
        let path = directory.join("codex-models.json");
        let catalog =
            serde_json::json!({"models": [{"slug": "gpt-6-luna", "display_name": "GPT-6 Luna"}]});

        write_cache(&path, catalog, Some("W/\"tag\"".to_string()));
        let cached = read_cache(&path).unwrap();
        assert!(is_fresh(&cached));
        assert_eq!(cached.etag.as_deref(), Some("W/\"tag\""));
        assert_eq!(parse(&cached.catalog).unwrap().models[0].slug, "gpt-6-luna");

        let stale = CachedCatalog {
            fetched_at_ms: now_ms() - CACHE_TTL.as_millis() as u64 - 1,
            ..cached
        };
        assert!(!is_fresh(&stale));

        let other_version = CachedCatalog {
            client_version: "0.1.0".to_string(),
            ..stale
        };
        fs::write(&path, serde_json::to_vec(&other_version).unwrap()).unwrap();
        assert!(read_cache(&path).is_none());
        fs::remove_dir_all(directory).unwrap();
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
