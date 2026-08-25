use std::env;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use crate::catalog::Catalog;
use crate::mapping::ModelMapping;

const CONFIG_VERSION: u32 = 1;
const DEFAULT_COMPACT_AT_PERCENT: u8 = 90;
const MAX_COMPACT_AT_PERCENT: u8 = 95;

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(default)]
pub struct AppConfig {
    pub version: u32,
    pub context: ContextConfig,
    pub codex: CodexConfig,
    pub permissions: PermissionsConfig,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(default)]
pub struct ContextConfig {
    /// `None` follows the smallest context window among routed models.
    pub max_tokens: Option<u64>,
    pub compact_at_percent: u8,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(default)]
pub struct CodexConfig {
    pub transport: CodexTransport,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum CodexTransport {
    #[default]
    Http,
    Websocket,
    Auto,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(default)]
pub struct PermissionsConfig {
    /// Exact Claude tool names that Clodex grants to every launched agent.
    pub trusted_tools: Vec<String>,
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            version: CONFIG_VERSION,
            context: ContextConfig::default(),
            codex: CodexConfig::default(),
            permissions: PermissionsConfig::default(),
        }
    }
}

impl Default for ContextConfig {
    fn default() -> Self {
        Self {
            max_tokens: None,
            compact_at_percent: DEFAULT_COMPACT_AT_PERCENT,
        }
    }
}

impl AppConfig {
    pub fn load() -> Result<Self> {
        Self::load_from(&config_path()?)
    }

    fn load_from(path: &Path) -> Result<Self> {
        if !path.exists() {
            return Ok(Self::default());
        }

        let bytes = fs::read(path)
            .with_context(|| format!("could not read clodex config at {}", path.display()))?;
        let config: Self = serde_json::from_slice(&bytes)
            .with_context(|| format!("invalid clodex config at {}", path.display()))?;

        config.validate()?;
        Ok(config)
    }

    pub fn save(&self) -> Result<()> {
        ensure_home_layout()?;
        self.save_to(&config_path()?)
    }

    fn save_to(&self, path: &Path) -> Result<()> {
        self.validate()?;

        let parent = path
            .parent()
            .context("clodex config path has no parent directory")?;
        fs::create_dir_all(parent).with_context(|| {
            format!(
                "could not create clodex config directory {}",
                parent.display()
            )
        })?;

        let temporary = path.with_extension("json.tmp");
        let mut serialized = serde_json::to_vec_pretty(self)?;
        serialized.push(b'\n');
        fs::write(&temporary, serialized).with_context(|| {
            format!(
                "could not write temporary clodex config {}",
                temporary.display()
            )
        })?;
        fs::rename(&temporary, path)
            .with_context(|| format!("could not save clodex config {}", path.display()))?;
        Ok(())
    }

    fn validate(&self) -> Result<()> {
        if self.version != CONFIG_VERSION {
            bail!(
                "unsupported clodex config version {}; expected {}",
                self.version,
                CONFIG_VERSION
            );
        }
        validate_compact_at_percent(self.context.compact_at_percent)?;
        self.permissions.validate()
    }

    pub fn render(&self) -> String {
        let trusted_tools = if self.permissions.trusted_tools.is_empty() {
            "none".to_string()
        } else {
            self.permissions.trusted_tools.join(", ")
        };
        format!(
            "Persistent clodex defaults\n\n\
             Context ceiling: {}\n\
             Compact at:     {}%\n\
             Codex transport: {}\n\
             Trusted tools:  {}\n",
            self.context.render_limit(),
            self.context.compact_at_percent,
            self.codex.transport.as_str(),
            trusted_tools
        )
    }

    pub fn render_effective_context(
        &self,
        catalog: &Catalog,
        mapping: &ModelMapping,
    ) -> Result<String> {
        let standard = smallest_mapped_context(catalog, mapping)?;
        let ceiling = routed_context_ceiling(catalog, mapping)?;
        let capacity = self.effective_context_capacity(catalog, mapping)?;
        debug_assert!(capacity <= ceiling);
        let trigger = capacity * u64::from(self.context.compact_at_percent) / 100;
        let clamped = self
            .context
            .max_tokens
            .is_some_and(|configured| configured > ceiling);

        Ok(format!(
            "Effective context configuration\n\n\
             Catalog standard: {}\n\
             Routed ceiling:   {}\n\
             Claude capacity:  {}{}\n\
             Auto-compact at:  {}%\n\
             Effective trigger: {}\n",
            format_tokens(standard),
            format_tokens(ceiling),
            format_tokens(capacity),
            if clamped {
                " (clamped to the routed ceiling)"
            } else {
                ""
            },
            self.context.compact_at_percent,
            format_tokens(trigger)
        ))
    }

    pub fn effective_context_capacity(
        &self,
        catalog: &Catalog,
        mapping: &ModelMapping,
    ) -> Result<u64> {
        // The routed ceiling is authoritative in both directions. `auto` opts
        // into the extended window the catalog advertises, and an explicit
        // value is clamped to it: a capacity above what Codex accepts leaves
        // Claude Code auto-compacting past the point where every request is
        // rejected, and a rejected compaction request cannot recover.
        match self.context.max_tokens {
            // A silent catalog leaves nothing to clamp against, so an explicit
            // value stays an escape hatch.
            Some(configured) => Ok(routed_context_ceiling(catalog, mapping)
                .map_or(configured, |ceiling| configured.min(ceiling))),
            None => routed_context_ceiling(catalog, mapping),
        }
    }
}

impl CodexTransport {
    pub fn parse(value: &str) -> Result<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "http" => Ok(Self::Http),
            "websocket" => Ok(Self::Websocket),
            "auto" => Ok(Self::Auto),
            _ => bail!("invalid Codex transport {value:?}; expected http, websocket, or auto"),
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Http => "http",
            Self::Websocket => "websocket",
            Self::Auto => "auto",
        }
    }
}

impl ContextConfig {
    pub fn set_compact_at_percent(&mut self, percent: u8) -> Result<()> {
        validate_compact_at_percent(percent)?;
        self.compact_at_percent = percent;
        Ok(())
    }

    pub fn render_limit(&self) -> String {
        self.max_tokens
            .map(format_tokens)
            .unwrap_or_else(|| "auto".to_string())
    }
}

impl PermissionsConfig {
    pub fn trust(&mut self, tool: &str) -> Result<bool> {
        validate_tool_name(tool)?;
        if self.trusted_tools.iter().any(|existing| existing == tool) {
            return Ok(false);
        }
        self.trusted_tools.push(tool.to_string());
        self.trusted_tools.sort();
        Ok(true)
    }

    pub fn forget(&mut self, tool: &str) -> Result<bool> {
        validate_tool_name(tool)?;
        let original_len = self.trusted_tools.len();
        self.trusted_tools.retain(|existing| existing != tool);
        Ok(self.trusted_tools.len() != original_len)
    }

    fn validate(&self) -> Result<()> {
        for tool in &self.trusted_tools {
            validate_tool_name(tool)?;
        }
        Ok(())
    }
}

pub fn parse_context_limit(value: &str) -> Result<Option<u64>> {
    let normalized = value.trim().to_ascii_lowercase().replace('_', "");
    if normalized == "auto" {
        return Ok(None);
    }

    let (number, multiplier) = if let Some(number) = normalized.strip_suffix('k') {
        (number, 1_000)
    } else if let Some(number) = normalized.strip_suffix('m') {
        (number, 1_000_000)
    } else {
        (normalized.as_str(), 1)
    };

    let base: u64 = number
        .parse()
        .with_context(|| format!("invalid context window {value:?}"))?;
    let tokens = base
        .checked_mul(multiplier)
        .with_context(|| format!("context window {value:?} is too large"))?;

    if tokens == 0 {
        bail!("context window must be greater than zero");
    }

    Ok(Some(tokens))
}

pub fn config_path() -> Result<PathBuf> {
    Ok(clodex_home()?.join("config.json"))
}

pub fn clodex_home() -> Result<PathBuf> {
    if let Some(directory) = env::var_os("CLODEX_HOME") {
        return Ok(PathBuf::from(directory));
    }

    // Retain the earlier development override while CLODEX_HOME becomes the
    // canonical public setting.
    if let Some(directory) = env::var_os("CLODEX_CONFIG_DIR") {
        return Ok(PathBuf::from(directory));
    }

    let home = env::var_os("HOME").context("HOME is not set")?;
    Ok(PathBuf::from(home).join(".clodex"))
}

pub fn ensure_home_layout() -> Result<()> {
    let home = clodex_home()?;
    for directory in [&home, &home.join("logs"), &home.join("run")] {
        fs::create_dir_all(directory)
            .with_context(|| format!("could not create {}", directory.display()))?;
    }
    Ok(())
}

#[allow(dead_code)]
fn legacy_config_path() -> Result<PathBuf> {
    if let Some(directory) = env::var_os("CLODEX_CONFIG_DIR") {
        return Ok(PathBuf::from(directory).join("config.json"));
    }

    #[cfg(target_os = "macos")]
    {
        let home = env::var_os("HOME").context("HOME is not set")?;
        Ok(PathBuf::from(home)
            .join("Library")
            .join("Application Support")
            .join("clodex")
            .join("config.json"))
    }

    #[cfg(target_os = "windows")]
    {
        let app_data = env::var_os("APPDATA").context("APPDATA is not set")?;
        Ok(PathBuf::from(app_data).join("clodex").join("config.json"))
    }

    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    {
        if let Some(xdg_config) = env::var_os("XDG_CONFIG_HOME") {
            return Ok(PathBuf::from(xdg_config).join("clodex").join("config.json"));
        }
        let home = env::var_os("HOME").context("HOME is not set")?;
        Ok(PathBuf::from(home)
            .join(".config")
            .join("clodex")
            .join("config.json"))
    }
}

fn smallest_mapped_context(catalog: &Catalog, mapping: &ModelMapping) -> Result<u64> {
    routed_window(catalog, mapping, |model| model.context_window)
        .context("mapped Codex models did not report a context window")
}

/// The largest capacity every routed model will actually accept. Claude Code
/// must never be told it has more room than this.
pub fn routed_context_ceiling(catalog: &Catalog, mapping: &ModelMapping) -> Result<u64> {
    routed_window(catalog, mapping, |model| model.usable_context_window())
        .context("mapped Codex models did not report a context window")
}

fn routed_window(
    catalog: &Catalog,
    mapping: &ModelMapping,
    window: impl Fn(&crate::catalog::Model) -> Option<u64>,
) -> Option<u64> {
    let routed = [
        mapping.fable.model.as_str(),
        mapping.opus.model.as_str(),
        mapping.sonnet.model.as_str(),
    ];

    routed
        .iter()
        .filter_map(|slug| {
            catalog
                .models
                .iter()
                .find(|model| model.slug == *slug)
                .and_then(&window)
        })
        .min()
}

fn validate_compact_at_percent(percent: u8) -> Result<()> {
    if !(1..=MAX_COMPACT_AT_PERCENT).contains(&percent) {
        bail!(
            "auto-compaction percentage must be between 1 and {}",
            MAX_COMPACT_AT_PERCENT
        );
    }
    Ok(())
}

fn validate_tool_name(tool: &str) -> Result<()> {
    if tool.trim().is_empty() {
        bail!("trusted tool name cannot be empty");
    }
    if tool.contains(['\n', '\r', '\0']) {
        bail!("trusted tool name cannot contain a newline or NUL byte");
    }
    Ok(())
}

fn format_tokens(tokens: u64) -> String {
    format!("{tokens} tokens")
}

#[cfg(test)]
mod tests {
    use std::time::{SystemTime, UNIX_EPOCH};

    use pretty_assertions::assert_eq;

    use super::*;
    use crate::catalog::Model;

    fn temporary_config_path(test_name: &str) -> PathBuf {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        env::temp_dir().join(format!("clodex-{test_name}-{nonce}.json"))
    }

    fn model(slug: &str, priority: u32, context_window: u64) -> Model {
        Model {
            slug: slug.to_string(),
            display_name: slug.to_string(),
            description: String::new(),
            visibility: "list".to_string(),
            supported_in_api: true,
            priority,
            context_window: Some(context_window),
            max_context_window: None,
            effective_context_window_percent: None,
            supported_reasoning_levels: Vec::new(),
            additional_speed_tiers: Vec::new(),
        }
    }

    /// A catalog entry shaped like the live GPT-5.6 models, which advertise an
    /// extended ceiling alongside the standard usage threshold.
    fn extended_model(slug: &str, priority: u32, standard: u64, extended: u64) -> Model {
        Model {
            max_context_window: Some(extended),
            effective_context_window_percent: Some(95),
            ..model(slug, priority, standard)
        }
    }

    #[test]
    fn parses_context_limit_units() {
        assert_eq!(parse_context_limit("auto").unwrap(), None);
        assert_eq!(parse_context_limit(" AUTO ").unwrap(), None);
        assert_eq!(parse_context_limit("200k").unwrap(), Some(200_000));
        assert_eq!(parse_context_limit("200K").unwrap(), Some(200_000));
        assert_eq!(parse_context_limit("1m").unwrap(), Some(1_000_000));
        assert_eq!(parse_context_limit("1_000_000").unwrap(), Some(1_000_000));
        assert_eq!(parse_context_limit("256000").unwrap(), Some(256_000));
        assert!(parse_context_limit("0").is_err());
        assert!(parse_context_limit("lots").is_err());
        assert!(parse_context_limit("-1").is_err());
        assert!(parse_context_limit("1.5m").is_err());
        assert!(parse_context_limit("1mb").is_err());
        assert!(parse_context_limit("18446744073709551615m").is_err());
    }

    #[test]
    fn parses_codex_transport_names() {
        assert_eq!(CodexTransport::parse("http").unwrap(), CodexTransport::Http);
        assert_eq!(
            CodexTransport::parse(" WEBSOCKET ").unwrap(),
            CodexTransport::Websocket
        );
        assert_eq!(CodexTransport::parse("AUTO").unwrap(), CodexTransport::Auto);
        assert!(CodexTransport::parse("sse").is_err());
    }

    #[test]
    fn persists_and_loads_config() {
        let path = temporary_config_path("persist");
        let config = AppConfig {
            context: ContextConfig {
                max_tokens: Some(200_000),
                compact_at_percent: 80,
            },
            ..AppConfig::default()
        };

        config.save_to(&path).unwrap();
        let loaded = AppConfig::load_from(&path).unwrap();

        assert_eq!(loaded, config);
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn defaults_when_config_does_not_exist() {
        let path = temporary_config_path("missing");
        assert_eq!(AppConfig::load_from(&path).unwrap(), AppConfig::default());
    }

    fn extended_catalog() -> Catalog {
        Catalog {
            models: vec![
                extended_model("sol", 1, 272_000, 872_000),
                extended_model("terra", 2, 272_000, 872_000),
                extended_model("luna", 3, 272_000, 872_000),
            ],
        }
    }

    #[test]
    fn automatic_context_opts_into_the_extended_catalog_ceiling() {
        let catalog = extended_catalog();
        let mapping = ModelMapping::from_catalog(&catalog).unwrap();

        // 872_000 * 95%, not the 272_000 standard usage threshold.
        assert_eq!(
            AppConfig::default()
                .effective_context_capacity(&catalog, &mapping)
                .unwrap(),
            828_400
        );
    }

    #[test]
    fn explicit_context_is_clamped_to_what_codex_will_accept() {
        let catalog = extended_catalog();
        let mapping = ModelMapping::from_catalog(&catalog).unwrap();
        let config = AppConfig {
            context: ContextConfig {
                max_tokens: Some(1_000_000),
                compact_at_percent: 90,
            },
            ..AppConfig::default()
        };

        assert_eq!(
            config
                .effective_context_capacity(&catalog, &mapping)
                .unwrap(),
            828_400
        );

        let output = config.render_effective_context(&catalog, &mapping).unwrap();
        assert!(output.contains("Catalog standard: 272000 tokens"));
        assert!(output.contains("Routed ceiling:   828400 tokens"));
        assert!(output.contains("clamped to the routed ceiling"));
        assert!(output.contains("Effective trigger: 745560 tokens"));
    }

    #[test]
    fn an_explicit_value_below_the_ceiling_is_preserved() {
        let catalog = extended_catalog();
        let mapping = ModelMapping::from_catalog(&catalog).unwrap();
        let config = AppConfig {
            context: ContextConfig {
                max_tokens: Some(400_000),
                compact_at_percent: 90,
            },
            ..AppConfig::default()
        };

        assert_eq!(
            config
                .effective_context_capacity(&catalog, &mapping)
                .unwrap(),
            400_000
        );
        let output = config.render_effective_context(&catalog, &mapping).unwrap();
        assert!(!output.contains("clamped to the routed ceiling"));
    }

    #[test]
    fn the_ceiling_follows_the_smallest_routed_model() {
        let catalog = Catalog {
            models: vec![
                extended_model("sol", 1, 272_000, 872_000),
                extended_model("terra", 2, 272_000, 872_000),
                extended_model("luna", 3, 128_000, 128_000),
            ],
        };
        let mapping = ModelMapping::from_catalog(&catalog).unwrap();

        assert_eq!(
            AppConfig::default()
                .effective_context_capacity(&catalog, &mapping)
                .unwrap(),
            121_600
        );
    }

    #[test]
    fn a_catalog_without_an_extended_window_still_resolves() {
        let catalog = Catalog {
            models: vec![
                model("sol", 1, 300_000),
                model("terra", 2, 272_000),
                model("luna", 3, 128_000),
            ],
        };
        let mapping = ModelMapping::from_catalog(&catalog).unwrap();

        // No effective percentage reported, so the 95% default applies.
        assert_eq!(
            AppConfig::default()
                .effective_context_capacity(&catalog, &mapping)
                .unwrap(),
            121_600
        );
    }

    #[test]
    fn rejects_compaction_above_claude_codes_effective_maximum() {
        let mut context = ContextConfig::default();
        assert!(context.set_compact_at_percent(96).is_err());
        assert!(context.set_compact_at_percent(0).is_err());
        context.set_compact_at_percent(95).unwrap();
        assert_eq!(context.compact_at_percent, 95);
    }

    #[test]
    fn trusted_tools_are_sorted_and_deduplicated() {
        let mut permissions = PermissionsConfig::default();
        assert!(permissions.trust("mcp__z__read").unwrap());
        assert!(permissions.trust("mcp__a__search").unwrap());
        assert!(!permissions.trust("mcp__z__read").unwrap());
        assert_eq!(
            permissions.trusted_tools,
            ["mcp__a__search", "mcp__z__read"]
        );
        assert!(permissions.forget("mcp__z__read").unwrap());
        assert!(!permissions.forget("mcp__z__read").unwrap());
    }

    #[test]
    fn rejects_malformed_incompatible_and_unsafe_configs() {
        let malformed = temporary_config_path("malformed");
        fs::write(&malformed, b"{not json").unwrap();
        assert!(AppConfig::load_from(&malformed).is_err());
        fs::remove_file(malformed).unwrap();

        let incompatible = temporary_config_path("version");
        fs::write(
            &incompatible,
            br#"{"version":2,"context":{"max_tokens":null,"compact_at_percent":90}}"#,
        )
        .unwrap();
        let error = AppConfig::load_from(&incompatible).unwrap_err().to_string();
        assert!(error.contains("unsupported clodex config version 2"));
        fs::remove_file(incompatible).unwrap();

        let invalid_percent = temporary_config_path("percent");
        fs::write(
            &invalid_percent,
            br#"{"version":1,"context":{"max_tokens":null,"compact_at_percent":0}}"#,
        )
        .unwrap();
        assert!(AppConfig::load_from(&invalid_percent).is_err());
        fs::remove_file(invalid_percent).unwrap();
    }

    #[test]
    fn older_version_one_configs_receive_new_field_defaults() {
        let path = temporary_config_path("defaults");
        fs::write(
            &path,
            br#"{"version":1,"context":{"max_tokens":200000,"compact_at_percent":80}}"#,
        )
        .unwrap();

        let config = AppConfig::load_from(&path).unwrap();
        assert_eq!(config.codex.transport, CodexTransport::Http);
        assert!(config.permissions.trusted_tools.is_empty());
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn trusted_tool_names_reject_empty_multiline_and_nul_values() {
        let mut permissions = PermissionsConfig::default();
        for invalid in ["", "   ", "tool\nother", "tool\rother", "tool\0other"] {
            assert!(permissions.trust(invalid).is_err(), "{invalid:?}");
            assert!(permissions.forget(invalid).is_err(), "{invalid:?}");
        }
    }

    #[test]
    fn saves_atomically_with_parent_directories_and_trailing_newline() {
        let directory = temporary_config_path("nested");
        let path = directory.join("one").join("config.json");
        AppConfig::default().save_to(&path).unwrap();

        let bytes = fs::read(&path).unwrap();
        assert!(bytes.ends_with(b"\n"));
        assert!(!path.with_extension("json.tmp").exists());
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn renders_empty_and_populated_configuration() {
        let empty = AppConfig::default().render();
        assert!(empty.contains("Context ceiling: auto"));
        assert!(empty.contains("Codex transport: http"));
        assert!(empty.contains("Trusted tools:  none"));

        let mut configured = AppConfig::default();
        configured.context.max_tokens = Some(600_000);
        configured.codex.transport = CodexTransport::Websocket;
        configured.permissions.trust("mcp__memory__search").unwrap();
        let output = configured.render();
        assert!(output.contains("Context ceiling: 600000 tokens"));
        assert!(output.contains("Codex transport: websocket"));
        assert!(output.contains("Trusted tools:  mcp__memory__search"));
    }

    #[test]
    fn automatic_context_requires_a_window_for_at_least_one_mapped_model() {
        let mut missing = model("only", 1, 1);
        missing.context_window = None;
        let catalog = Catalog {
            models: vec![missing],
        };
        let mapping = ModelMapping::from_catalog(&catalog).unwrap();

        assert!(
            AppConfig::default()
                .effective_context_capacity(&catalog, &mapping)
                .is_err()
        );
        let mut explicit = AppConfig::default();
        explicit.context.max_tokens = Some(600_000);
        assert_eq!(
            explicit
                .effective_context_capacity(&catalog, &mapping)
                .unwrap(),
            600_000
        );
    }
}
