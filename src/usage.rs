//! Codex subscription usage, mapped onto Claude Code's rate-limit headers.
//!
//! Claude Code renders its 5h and 7d status bars from
//! `anthropic-ratelimit-unified-*` response headers. Behind a custom base URL
//! those headers never arrive, so the bars disappear even though the session
//! is spending a real subscription quota — the Codex one.
//!
//! Codex reports that quota at `/backend-api/wham/usage`. Translating it into
//! the headers Claude Code already understands means an existing status line
//! keeps working untouched, rather than every user having to script against a
//! clodex-specific side channel.

use serde::Deserialize;

const USAGE_URL: &str = "https://chatgpt.com/backend-api/wham/usage";
/// Codex reports window lengths in seconds; these are the two Claude Code
/// renders.
const FIVE_HOUR_SECONDS: u64 = 18_000;
const SEVEN_DAY_SECONDS: u64 = 604_800;

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Window {
    /// Share of the window consumed, as a fraction. Claude Code's headers are
    /// fractions even though Codex reports whole percentages.
    pub utilization: f64,
    /// Unix seconds at which the window resets.
    pub resets_at: u64,
}

#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct RateLimits {
    pub five_hour: Option<Window>,
    pub seven_day: Option<Window>,
}

impl RateLimits {
    pub fn is_empty(&self) -> bool {
        self.five_hour.is_none() && self.seven_day.is_none()
    }

    /// The headers Claude Code parses into its status line payload.
    pub fn headers(&self) -> Vec<(&'static str, String)> {
        let mut headers = Vec::new();
        if let Some(window) = self.five_hour {
            headers.push((
                "anthropic-ratelimit-unified-5h-utilization",
                format!("{:.4}", window.utilization),
            ));
            headers.push((
                "anthropic-ratelimit-unified-5h-reset",
                window.resets_at.to_string(),
            ));
        }
        if let Some(window) = self.seven_day {
            headers.push((
                "anthropic-ratelimit-unified-7d-utilization",
                format!("{:.4}", window.utilization),
            ));
            headers.push((
                "anthropic-ratelimit-unified-7d-reset",
                window.resets_at.to_string(),
            ));
        }
        if !headers.is_empty() {
            headers.push(("anthropic-ratelimit-unified-status", "allowed".to_string()));
        }
        headers
    }
}

#[derive(Debug, Deserialize)]
struct UsageResponse {
    #[serde(default)]
    rate_limit: Option<RateLimit>,
    #[serde(default)]
    additional_rate_limits: Vec<AdditionalRateLimit>,
}

#[derive(Debug, Deserialize)]
struct AdditionalRateLimit {
    #[serde(default)]
    rate_limit: Option<RateLimit>,
}

#[derive(Debug, Deserialize)]
struct RateLimit {
    #[serde(default)]
    primary_window: Option<UsageWindow>,
    #[serde(default)]
    secondary_window: Option<UsageWindow>,
}

#[derive(Debug, Deserialize)]
struct UsageWindow {
    #[serde(default)]
    used_percent: f64,
    #[serde(default)]
    limit_window_seconds: u64,
    #[serde(default)]
    reset_at: u64,
}

/// Maps Codex's windows onto Claude Code's two.
///
/// Codex names its windows `primary` and `secondary` per limit, and which one
/// is the weekly window depends on the plan — a Pro account reports the weekly
/// limit as its primary and no secondary at all. Classifying by window length
/// rather than by position keeps that from mattering.
pub fn parse(body: &str) -> Option<RateLimits> {
    let usage: UsageResponse = serde_json::from_str(body).ok()?;
    let mut limits = RateLimits::default();

    let windows = usage
        .rate_limit
        .iter()
        .chain(
            usage
                .additional_rate_limits
                .iter()
                .filter_map(|extra| extra.rate_limit.as_ref()),
        )
        .flat_map(|limit| {
            limit
                .primary_window
                .iter()
                .chain(limit.secondary_window.iter())
        });

    for window in windows {
        let slot = match window.limit_window_seconds {
            FIVE_HOUR_SECONDS => &mut limits.five_hour,
            SEVEN_DAY_SECONDS => &mut limits.seven_day,
            _ => continue,
        };
        let candidate = Window {
            utilization: (window.used_percent / 100.0).clamp(0.0, 1.0),
            resets_at: window.reset_at,
        };
        // Several limits can report the same window. The binding one is the
        // fullest, which is what the user needs to see.
        if slot.is_none_or(|existing| candidate.utilization > existing.utilization) {
            *slot = Some(candidate);
        }
    }

    if limits.is_empty() {
        None
    } else {
        Some(limits)
    }
}

/// Writes the latest reading where a status line can read it.
///
/// Claude Code only populates its own rate-limit state for a Claude
/// subscription login; behind `ANTHROPIC_AUTH_TOKEN` it ignores the headers
/// entirely. A snapshot on disk gives a status line a source that does not
/// depend on how Claude Code authenticates.
pub fn write_snapshot(limits: &RateLimits) {
    let Ok(home) = crate::config::clodex_home() else {
        return;
    };
    let path = home.join("run").join("usage.json");
    let Some(parent) = path.parent() else {
        return;
    };
    if std::fs::create_dir_all(parent).is_err() {
        return;
    }

    let window = |window: Option<Window>| {
        window.map(|window| {
            serde_json::json!({
                "used_percent": (window.utilization * 100.0).round(),
                "resets_at": window.resets_at,
            })
        })
    };
    let snapshot = serde_json::json!({
        "five_hour": window(limits.five_hour),
        "seven_day": window(limits.seven_day),
        "updated_at": std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|since| since.as_secs())
            .unwrap_or_default(),
    });

    // Written via a temporary file so a status line never reads a partial
    // snapshot mid-write.
    let temporary = path.with_extension(format!("json.{}.tmp", std::process::id()));
    if serde_json::to_vec(&snapshot)
        .map(|bytes| std::fs::write(&temporary, bytes))
        .is_ok()
    {
        let _ = std::fs::rename(&temporary, &path);
    }
}

/// Fetches current usage using the Codex login clodex already reuses.
pub async fn fetch(client: &reqwest::Client) -> Option<RateLimits> {
    let credentials = crate::auth::load_codex_credentials(false).ok()?;
    let mut request = client
        .get(USAGE_URL)
        .bearer_auth(credentials.access_token())
        .timeout(std::time::Duration::from_secs(10));
    if let Some(account) = credentials.account_id() {
        request = request.header("chatgpt-account-id", account);
    }

    let response = match request.send().await {
        Ok(response) => response,
        Err(error) => {
            eprintln!("clodex usage fetch failed: {error}");
            return None;
        }
    };
    if !response.status().is_success() {
        eprintln!("clodex usage endpoint returned {}", response.status());
        return None;
    }
    let limits = parse(&response.text().await.ok()?)?;
    write_snapshot(&limits);
    Some(limits)
}

#[cfg(test)]
mod tests {
    use super::*;

    const PRO_PLAN: &str = r#"{
        "plan_type": "pro",
        "rate_limit": {
            "primary_window": {
                "used_percent": 77,
                "limit_window_seconds": 604800,
                "reset_at": 1788276117
            },
            "secondary_window": null
        },
        "additional_rate_limits": [{
            "limit_name": "GPT-5.3-Codex-Spark",
            "rate_limit": {
                "primary_window": {
                    "used_percent": 0,
                    "limit_window_seconds": 18000,
                    "reset_at": 1787719302
                },
                "secondary_window": {
                    "used_percent": 0,
                    "limit_window_seconds": 604800,
                    "reset_at": 1788306102
                }
            }
        }]
    }"#;

    #[test]
    fn a_pro_plan_reports_its_weekly_limit_as_the_primary_window() {
        let limits = parse(PRO_PLAN).unwrap();

        // The weekly window is primary on Pro, and must not be mistaken for
        // the five-hour one just because of its position.
        let seven_day = limits.seven_day.unwrap();
        assert!((seven_day.utilization - 0.77).abs() < 1e-9);
        assert_eq!(seven_day.resets_at, 1_788_276_117);
        assert_eq!(limits.five_hour.unwrap().utilization, 0.0);
    }

    #[test]
    fn the_fullest_window_wins_when_limits_overlap() {
        let body = r#"{
            "rate_limit": {
                "primary_window": {"used_percent": 20, "limit_window_seconds": 604800, "reset_at": 100}
            },
            "additional_rate_limits": [{
                "rate_limit": {
                    "primary_window": {"used_percent": 91, "limit_window_seconds": 604800, "reset_at": 200}
                }
            }]
        }"#;

        let limits = parse(body).unwrap();

        let seven_day = limits.seven_day.unwrap();
        assert!((seven_day.utilization - 0.91).abs() < 1e-9);
        assert_eq!(seven_day.resets_at, 200);
    }

    #[test]
    fn unknown_window_lengths_are_ignored() {
        let body = r#"{
            "rate_limit": {
                "primary_window": {"used_percent": 50, "limit_window_seconds": 3600, "reset_at": 1}
            }
        }"#;

        assert_eq!(parse(body), None);
    }

    #[test]
    fn a_response_without_limits_yields_nothing() {
        assert_eq!(parse(r#"{"plan_type":"free"}"#), None);
        assert_eq!(parse("not json"), None);
    }

    #[test]
    fn headers_use_the_fractions_claude_code_expects() {
        let limits = parse(PRO_PLAN).unwrap();
        let headers = limits.headers();
        let value = |name: &str| {
            headers
                .iter()
                .find(|(header, _)| *header == name)
                .map(|(_, value)| value.clone())
        };

        assert_eq!(
            value("anthropic-ratelimit-unified-7d-utilization"),
            Some("0.7700".to_string())
        );
        assert_eq!(
            value("anthropic-ratelimit-unified-7d-reset"),
            Some("1788276117".to_string())
        );
        assert_eq!(
            value("anthropic-ratelimit-unified-status"),
            Some("allowed".to_string())
        );
    }

    #[test]
    fn a_snapshot_is_written_as_whole_percentages() {
        let home = std::env::temp_dir().join(format!("clodex-usage-{}", std::process::id()));
        // SAFETY: single-threaded test setup for a process-local override.
        unsafe { std::env::set_var("CLODEX_HOME", &home) };

        write_snapshot(&parse(PRO_PLAN).unwrap());

        let written: serde_json::Value =
            serde_json::from_slice(&std::fs::read(home.join("run").join("usage.json")).unwrap())
                .unwrap();
        assert_eq!(written["seven_day"]["used_percent"], 77.0);
        assert_eq!(written["seven_day"]["resets_at"], 1_788_276_117u64);
        assert_eq!(written["five_hour"]["used_percent"], 0.0);
        std::fs::remove_dir_all(&home).unwrap();
    }

    #[test]
    fn an_over_full_window_is_clamped() {
        let body = r#"{
            "rate_limit": {
                "primary_window": {"used_percent": 140, "limit_window_seconds": 18000, "reset_at": 5}
            }
        }"#;

        assert_eq!(parse(body).unwrap().five_hour.unwrap().utilization, 1.0);
    }
}
