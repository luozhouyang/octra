//! Per-virtual-key permissions parsed from `permissions_json`.
//!
//! A virtual key may narrow the models, providers and accounts it can use and
//! carry token or cost limits. The policy is additive to the consumer policy:
//! both must permit a request, so a key can only tighten access.

use chrono::{DateTime, Datelike, SecondsFormat, TimeZone, Timelike, Utc};
use octra_storage::LlmVirtualKeyUsage;
use serde::Deserialize;

const MAX_LIMITS: usize = 5;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum LimitWindow {
    Hour,
    Day,
    Week,
    Month,
}

impl LimitWindow {
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "hour" => Some(Self::Hour),
            "day" => Some(Self::Day),
            "week" => Some(Self::Week),
            "month" => Some(Self::Month),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Hour => "hour",
            Self::Day => "day",
            Self::Week => "week",
            Self::Month => "month",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VkLimit {
    pub window: LimitWindow,
    pub tokens: Option<u64>,
    pub cost_micros: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Exceeded {
    pub window: LimitWindow,
    pub usage: LlmVirtualKeyUsage,
    pub limit: VkLimit,
}

/// Parsed `permissions_json` for one virtual key.
#[derive(Debug, Clone, Default)]
pub struct VirtualKeyPolicy {
    allowed_models: Option<Vec<String>>,
    allowed_providers: Option<Vec<String>>,
    allowed_accounts: Option<Vec<String>>,
    limits: Vec<VkLimit>,
    deny_all: bool,
}

impl VirtualKeyPolicy {
    /// Parses `permissions_json`. Unknown fields are ignored for backward
    /// compatibility with documents written before permissions were enforced.
    pub fn parse(raw: &str) -> Result<Self, String> {
        if raw.trim().is_empty() {
            return Ok(Self::default());
        }
        let raw: RawPolicy =
            serde_json::from_str(raw).map_err(|error| format!("invalid permissions: {error}"))?;
        let mut limits = Vec::new();
        let mut windows = Vec::new();
        for (index, limit) in raw.limits.unwrap_or_default().into_iter().enumerate() {
            let window = LimitWindow::parse(&limit.window).ok_or_else(|| {
                format!(
                    "limit {}: unknown window '{}' (expected hour, day, week or month)",
                    index + 1,
                    limit.window
                )
            })?;
            if windows.contains(&window) {
                return Err(format!("duplicate limit window '{}'", window.as_str()));
            }
            if limit.tokens.is_none() && limit.cost_micros.is_none() {
                return Err(format!(
                    "limit for window '{}' must set tokens or cost_micros",
                    window.as_str()
                ));
            }
            if limit.tokens == Some(0) || limit.cost_micros == Some(0) {
                return Err(format!(
                    "limit values for window '{}' must be positive",
                    window.as_str()
                ));
            }
            windows.push(window);
            limits.push(VkLimit {
                window,
                tokens: limit.tokens,
                cost_micros: limit.cost_micros,
            });
        }
        if limits.len() > MAX_LIMITS {
            return Err(format!("at most {MAX_LIMITS} limits are allowed"));
        }
        Ok(Self {
            allowed_models: raw.allowed_models,
            allowed_providers: raw.allowed_providers,
            allowed_accounts: raw.allowed_accounts,
            limits,
            deny_all: false,
        })
    }

    /// A policy that refuses every request. Used when stored permissions are
    /// malformed so a broken document fails closed instead of opening access.
    pub fn deny_all() -> Self {
        Self {
            deny_all: true,
            ..Self::default()
        }
    }

    pub fn limits(&self) -> &[VkLimit] {
        &self.limits
    }

    pub fn has_account_restrictions(&self) -> bool {
        !self.deny_all && self.allowed_accounts.is_some()
    }

    pub fn permits_model(&self, model: &str) -> bool {
        !self.deny_all && permits(&self.allowed_models, &[model])
    }

    pub fn permits_provider(&self, id: &str, name: &str) -> bool {
        !self.deny_all && permits(&self.allowed_providers, &[id, name])
    }

    pub fn permits_account(&self, id: &str, name: &str) -> bool {
        !self.deny_all && permits(&self.allowed_accounts, &[id, name])
    }

    /// Returns the first limit that has no remaining budget. `used >= limit`
    /// rejects, since the next call cannot be admitted without a reservation.
    pub fn exceeded(
        &self,
        usage_for: impl Fn(LimitWindow) -> LlmVirtualKeyUsage,
    ) -> Option<Exceeded> {
        if self.deny_all {
            return None;
        }
        for limit in &self.limits {
            let usage = usage_for(limit.window);
            let tokens_hit = limit
                .tokens
                .is_some_and(|limit| usage.tokens.max(0) as u64 >= limit);
            let cost_hit = limit
                .cost_micros
                .is_some_and(|limit| usage.cost_micros.max(0) as u64 >= limit);
            if tokens_hit || cost_hit {
                return Some(Exceeded {
                    window: limit.window,
                    usage,
                    limit: limit.clone(),
                });
            }
        }
        None
    }
}

fn permits(allowed: &Option<Vec<String>>, candidates: &[&str]) -> bool {
    match allowed {
        None => true,
        Some(allowed) => allowed
            .iter()
            .any(|value| value == "*" || candidates.contains(&value.as_str())),
    }
}

/// Start of the UTC calendar window as the `ai_usage.created_at` format.
pub fn window_start(window: LimitWindow, now: DateTime<Utc>) -> String {
    let date = now.date_naive();
    let naive = match window {
        LimitWindow::Hour => date.and_hms_opt(now.hour(), 0, 0),
        LimitWindow::Day => date.and_hms_opt(0, 0, 0),
        LimitWindow::Week => {
            let days = now.weekday().num_days_from_monday() as i64;
            (date - chrono::Duration::days(days)).and_hms_opt(0, 0, 0)
        }
        LimitWindow::Month => chrono::NaiveDate::from_ymd_opt(now.year(), now.month(), 1)
            .and_then(|date| date.and_hms_opt(0, 0, 0)),
    }
    .expect("UTC calendar window is representable");
    Utc.from_utc_datetime(&naive)
        .to_rfc3339_opts(SecondsFormat::Millis, true)
}

#[derive(Debug, Default, Deserialize)]
struct RawPolicy {
    #[serde(default)]
    allowed_models: Option<Vec<String>>,
    #[serde(default)]
    allowed_providers: Option<Vec<String>>,
    #[serde(default)]
    allowed_accounts: Option<Vec<String>>,
    #[serde(default)]
    limits: Option<Vec<RawLimit>>,
}

#[derive(Debug, Deserialize)]
struct RawLimit {
    window: String,
    #[serde(default)]
    tokens: Option<u64>,
    #[serde(default)]
    cost_micros: Option<u64>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn parse(value: serde_json::Value) -> Result<VirtualKeyPolicy, String> {
        VirtualKeyPolicy::parse(&value.to_string())
    }

    fn usage(tokens: i64, cost_micros: i64) -> LlmVirtualKeyUsage {
        LlmVirtualKeyUsage {
            tokens,
            cost_micros,
        }
    }

    #[test]
    fn empty_permissions_are_unrestricted() {
        for raw in ["", "{}"] {
            let policy = VirtualKeyPolicy::parse(raw).unwrap();
            assert!(policy.permits_model("any"));
            assert!(policy.permits_provider("any", "any"));
            assert!(policy.permits_account("any", "any"));
            assert!(policy.limits().is_empty());
        }
    }

    #[test]
    fn unknown_fields_are_ignored() {
        let policy = parse(json!({"chat": true})).unwrap();
        assert!(policy.permits_model("any"));
    }

    #[test]
    fn known_fields_must_have_valid_types() {
        assert!(parse(json!({"allowed_models": "gpt-5"})).is_err());
        assert!(parse(json!({"limits": {}})).is_err());
    }

    #[test]
    fn limits_are_validated() {
        assert!(parse(json!({"limits": [{"window": "year", "tokens": 1}]})).is_err());
        assert!(parse(json!({"limits": [{"window": "day"}]})).is_err());
        assert!(parse(json!({"limits": [{"window": "day", "tokens": 0}]})).is_err());
        assert!(
            parse(json!({"limits": [
                {"window": "day", "tokens": 1},
                {"window": "day", "cost_micros": 1}
            ]}))
            .is_err()
        );
    }

    #[test]
    fn matching_uses_exact_values_and_star() {
        let policy = parse(json!({
            "allowed_models": ["gpt-5", "*"],
            "allowed_providers": ["openai"],
            "allowed_accounts": ["acc-1", "primary"]
        }))
        .unwrap();
        assert!(policy.permits_model("anything"));
        assert!(!policy.permits_provider("anthropic", "Anthropic"));
        assert!(policy.permits_provider("openai", "OpenAI"));
        assert!(policy.permits_account("acc-1", "ignored"));
        assert!(policy.permits_account("ignored", "primary"));
        assert!(!policy.permits_account("acc-2", "secondary"));
        assert!(policy.has_account_restrictions());

        let restricted = parse(json!({"allowed_models": ["gpt-5"]})).unwrap();
        assert!(restricted.permits_model("gpt-5"));
        assert!(!restricted.permits_model("gpt-5-mini"));
        assert!(!restricted.has_account_restrictions());
    }

    #[test]
    fn deny_all_refuses_everything() {
        let policy = VirtualKeyPolicy::deny_all();
        assert!(!policy.permits_model("any"));
        assert!(!policy.permits_provider("any", "any"));
        assert!(!policy.permits_account("any", "any"));
        assert!(policy.limits().is_empty());
        assert!(!policy.has_account_restrictions());
    }

    #[test]
    fn window_starts_are_utc_calendar_boundaries() {
        // 2026-11-02 is a Monday.
        let now = Utc.with_ymd_and_hms(2026, 11, 2, 13, 45, 30).unwrap();
        assert_eq!(
            window_start(LimitWindow::Hour, now),
            "2026-11-02T13:00:00.000Z"
        );
        assert_eq!(
            window_start(LimitWindow::Day, now),
            "2026-11-02T00:00:00.000Z"
        );
        assert_eq!(
            window_start(LimitWindow::Week, now),
            "2026-11-02T00:00:00.000Z"
        );
        assert_eq!(
            window_start(LimitWindow::Month, now),
            "2026-11-01T00:00:00.000Z"
        );

        let sunday = Utc.with_ymd_and_hms(2026, 11, 8, 23, 59, 0).unwrap();
        assert_eq!(
            window_start(LimitWindow::Week, sunday),
            "2026-11-02T00:00:00.000Z"
        );
    }

    #[test]
    fn exceeded_rejects_at_the_limit() {
        let policy = parse(json!({
            "limits": [{"window": "day", "tokens": 100, "cost_micros": 500}]
        }))
        .unwrap();

        let below = policy.exceeded(|_| usage(99, 499));
        assert!(below.is_none());

        let tokens = policy.exceeded(|_| usage(100, 0)).unwrap();
        assert_eq!(tokens.window, LimitWindow::Day);
        assert_eq!(tokens.usage.tokens, 100);

        let cost = policy.exceeded(|_| usage(0, 500)).unwrap();
        assert_eq!(cost.usage.cost_micros, 500);
    }
}
