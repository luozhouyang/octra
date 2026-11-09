//! Kong-style multi-window rate limiting for the LLM data plane.
//!
//! The plugin counts admitted requests in fixed UTC calendar windows. It can
//! count per virtual-key family, consumer, consumer group, requested model,
//! client IP or globally. Counters go through the configured backend and are
//! keyed by the stable plugin-config instance id, so a chain reload keeps them
//! and a shared database backend can serve several daemon processes.

use std::hash::{Hash, Hasher};
use std::sync::Arc;

use async_trait::async_trait;
use chrono::{Datelike, TimeZone, Utc};
use octra_api_plugin_types::{Capability, PluginError, PluginManifest};
use octra_llm_core::{ErrorKind, GatewayError};
use octra_llm_plugin_types::{
    CounterWindow, LlmAction, LlmCounterStore, LlmPlugin, LlmPluginCreateContext, LlmPluginFactory,
    LlmPluginResult, LlmRequestContext, LlmScope, REQUEST_LLM_SCOPES,
};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::manifest;

const MAX_RULES: usize = 32;

pub struct RateLimitFactory;

impl LlmPluginFactory for RateLimitFactory {
    fn manifest(&self) -> PluginManifest {
        let mut manifest = manifest(
            "llm-rate-limit",
            "Reject requests over fixed second-to-year windows per key, consumer, group, model, IP or globally",
        );
        manifest.priority = 1000;
        manifest.timeout_ms = 50;
        manifest.capabilities = vec![Capability::RequestBody, Capability::ResponseHeaders];
        manifest.config_schema = Some(json!({
            "$schema": "http://json-schema.org/draft-07/schema#",
            "title": "llm-rate-limit config",
            "description": "Use either flat window fields or `rules`, not both. Flat window limits must be monotonically non-decreasing from shorter to longer windows.",
            "type": "object",
            "additionalProperties": false,
            "properties": {
                "second": {"type": "integer", "minimum": 1},
                "minute": {"type": "integer", "minimum": 1},
                "hour": {"type": "integer", "minimum": 1},
                "day": {"type": "integer", "minimum": 1},
                "month": {"type": "integer", "minimum": 1},
                "year": {"type": "integer", "minimum": 1},
                "limit_by": {
                    "type": "string",
                    "enum": ["virtual_key", "consumer", "consumer_group", "model", "client_ip", "global"],
                    "default": "virtual_key"
                },
                "policy": {
                    "type": "string",
                    "enum": ["local"],
                    "default": "local",
                    "description": "Only local counting is implemented."
                },
                "fault_tolerant": {"type": "boolean", "default": true},
                "hide_client_headers": {"type": "boolean", "default": false},
                "error_code": {
                    "type": "integer",
                    "minimum": 400,
                    "maximum": 599,
                    "default": 429
                },
                "error_message": {
                    "type": "string",
                    "minLength": 1,
                    "default": "LLM rate limit exceeded"
                },
                "code": {
                    "type": "string",
                    "minLength": 1,
                    "default": "rate_limited"
                },
                "rules": {
                    "type": "array",
                    "maxItems": 32,
                    "items": {
                        "type": "object",
                        "additionalProperties": false,
                        "required": ["name", "window", "limit"],
                        "properties": {
                            "name": {
                                "type": "string",
                                "pattern": "^[A-Za-z0-9_-]{1,32}$"
                            },
                            "window": {
                                "type": "string",
                                "enum": ["second", "minute", "hour", "day", "month", "year"]
                            },
                            "limit": {"type": "integer", "minimum": 1},
                            "limit_by": {
                                "type": "string",
                                "enum": ["virtual_key", "consumer", "consumer_group", "model", "client_ip", "global"]
                            }
                        }
                    }
                }
            },
            "oneOf": [
                {
                    "required": ["rules"],
                    "properties": {"rules": {"minItems": 1}},
                    "not": {
                        "anyOf": [
                            {"required": ["second"]},
                            {"required": ["minute"]},
                            {"required": ["hour"]},
                            {"required": ["day"]},
                            {"required": ["month"]},
                            {"required": ["year"]}
                        ]
                    }
                },
                {
                    "anyOf": [
                        {"required": ["second"]},
                        {"required": ["minute"]},
                        {"required": ["hour"]},
                        {"required": ["day"]},
                        {"required": ["month"]},
                        {"required": ["year"]}
                    ],
                    "properties": {"rules": {"maxItems": 0}}
                }
            ]
        }));
        manifest
    }

    fn supported_scopes(&self) -> &'static [LlmScope] {
        &REQUEST_LLM_SCOPES
    }

    fn validate(&self, config: &Value) -> Result<(), PluginError> {
        CompiledConfig::parse(config.clone()).map(|_| ())
    }

    fn create(&self, config: Value) -> Result<Arc<dyn LlmPlugin>, PluginError> {
        let config = CompiledConfig::parse(config.clone())?;
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        config.fingerprint().hash(&mut hasher);
        let key = format!("adhoc:{:x}", hasher.finish());
        Ok(Arc::new(RateLimitPlugin {
            manifest: self.manifest(),
            config,
            instance_key: key,
            counters: Arc::new(octra_llm_plugin_types::MemoryCounterStore::default()),
        }))
    }

    fn create_with(
        &self,
        config: Value,
        context: &LlmPluginCreateContext<'_>,
    ) -> Result<Arc<dyn LlmPlugin>, PluginError> {
        Ok(Arc::new(RateLimitPlugin {
            manifest: self.manifest(),
            config: CompiledConfig::parse(config)?,
            instance_key: context.instance_key.to_string(),
            counters: context.counters.clone(),
        }))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Deserialize)]
#[serde(rename_all = "snake_case")]
enum Window {
    Second,
    Minute,
    Hour,
    Day,
    Month,
    Year,
}

impl Window {
    fn title(self) -> &'static str {
        match self {
            Self::Second => "Second",
            Self::Minute => "Minute",
            Self::Hour => "Hour",
            Self::Day => "Day",
            Self::Month => "Month",
            Self::Year => "Year",
        }
    }

    fn as_str(self) -> &'static str {
        match self {
            Self::Second => "second",
            Self::Minute => "minute",
            Self::Hour => "hour",
            Self::Day => "day",
            Self::Month => "month",
            Self::Year => "year",
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
enum LimitBy {
    #[default]
    VirtualKey,
    Consumer,
    ConsumerGroup,
    Model,
    ClientIp,
    Global,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
enum Policy {
    #[default]
    Local,
    Cluster,
    Redis,
}

impl Policy {
    fn as_str(self) -> &'static str {
        match self {
            Self::Local => "local",
            Self::Cluster => "cluster",
            Self::Redis => "redis",
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct RuleConfig {
    name: String,
    window: Window,
    limit: u64,
    limit_by: Option<LimitBy>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct Config {
    second: Option<u64>,
    minute: Option<u64>,
    hour: Option<u64>,
    day: Option<u64>,
    month: Option<u64>,
    year: Option<u64>,
    limit_by: LimitBy,
    policy: Policy,
    fault_tolerant: bool,
    hide_client_headers: bool,
    error_code: u16,
    error_message: String,
    code: String,
    rules: Vec<RuleConfig>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            second: None,
            minute: None,
            hour: None,
            day: None,
            month: None,
            year: None,
            limit_by: LimitBy::VirtualKey,
            policy: Policy::Local,
            fault_tolerant: true,
            hide_client_headers: false,
            error_code: 429,
            error_message: "LLM rate limit exceeded".into(),
            code: "rate_limited".into(),
            rules: Vec::new(),
        }
    }
}

#[derive(Debug, Clone)]
struct Rule {
    name: String,
    window: Window,
    limit: u64,
    limit_by: LimitBy,
}

#[derive(Debug, Clone)]
struct CompiledConfig {
    rules: Vec<Rule>,
    fault_tolerant: bool,
    hide_client_headers: bool,
    error_code: u16,
    error_message: String,
    code: String,
}

impl CompiledConfig {
    fn parse(value: Value) -> Result<Self, PluginError> {
        let config: Config = serde_json::from_value(value).map_err(|error| {
            PluginError::fail_closed(format!("invalid rate limit config: {error}"))
        })?;
        if !matches!(config.policy, Policy::Local) {
            return Err(PluginError::fail_closed(format!(
                "llm-rate-limit policy '{}' is not implemented; use 'local'",
                config.policy.as_str()
            )));
        }
        if !(400..=599).contains(&config.error_code) {
            return Err(PluginError::fail_closed(
                "llm-rate-limit error_code must be a 4xx or 5xx status",
            ));
        }
        if config.code.trim().is_empty() {
            return Err(PluginError::fail_closed(
                "llm-rate-limit code must not be empty",
            ));
        }
        if config.error_message.trim().is_empty() {
            return Err(PluginError::fail_closed(
                "llm-rate-limit error_message must not be empty",
            ));
        }

        let flat = flat_limits(&config);
        if !config.rules.is_empty() && !flat.is_empty() {
            return Err(PluginError::fail_closed(
                "llm-rate-limit accepts either flat window fields or `rules`, not both",
            ));
        }

        let rules: Vec<Rule> = if config.rules.is_empty() {
            validate_monotonic(&flat)?;
            flat.iter()
                .map(|(window, limit)| Rule {
                    name: window.as_str().to_string(),
                    window: *window,
                    limit: *limit,
                    limit_by: config.limit_by,
                })
                .collect()
        } else {
            config
                .rules
                .iter()
                .map(|rule| Rule {
                    name: rule.name.trim().to_string(),
                    window: rule.window,
                    limit: rule.limit,
                    limit_by: rule.limit_by.unwrap_or(config.limit_by),
                })
                .collect()
        };
        if rules.is_empty() {
            return Err(PluginError::fail_closed(
                "llm-rate-limit requires at least one window or rule",
            ));
        }
        if rules.len() > MAX_RULES {
            return Err(PluginError::fail_closed(format!(
                "llm-rate-limit accepts at most {MAX_RULES} rules"
            )));
        }
        let mut names = std::collections::HashSet::new();
        for rule in &rules {
            if rule.name.is_empty()
                || rule.name.len() > 32
                || !rule
                    .name
                    .chars()
                    .all(|ch| ch.is_ascii_alphanumeric() || ch == '_' || ch == '-')
            {
                return Err(PluginError::fail_closed(format!(
                    "llm-rate-limit rule name '{}' must match [A-Za-z0-9_-]{{1,32}}",
                    rule.name
                )));
            }
            if !names.insert(rule.name.clone()) {
                return Err(PluginError::fail_closed(format!(
                    "llm-rate-limit rule name '{}' is duplicated",
                    rule.name
                )));
            }
            if rule.limit == 0 {
                return Err(PluginError::fail_closed(
                    "llm-rate-limit limits must be greater than zero",
                ));
            }
        }

        Ok(Self {
            rules,
            fault_tolerant: config.fault_tolerant,
            hide_client_headers: config.hide_client_headers,
            error_code: config.error_code,
            error_message: config.error_message,
            code: config.code,
        })
    }

    fn fingerprint(&self) -> String {
        let rules: Vec<Value> = self
            .rules
            .iter()
            .map(|rule| {
                json!({
                    "name": rule.name,
                    "window": rule.window.as_str(),
                    "limit": rule.limit,
                    "limit_by": limit_by_name(rule.limit_by),
                })
            })
            .collect();
        json!({
            "rules": rules,
            "fault_tolerant": self.fault_tolerant,
            "hide_client_headers": self.hide_client_headers,
            "error_code": self.error_code,
            "error_message": self.error_message,
            "code": self.code,
        })
        .to_string()
    }
}

fn flat_limits(config: &Config) -> Vec<(Window, u64)> {
    let mut limits = Vec::new();
    for (window, limit) in [
        (Window::Second, config.second),
        (Window::Minute, config.minute),
        (Window::Hour, config.hour),
        (Window::Day, config.day),
        (Window::Month, config.month),
        (Window::Year, config.year),
    ] {
        if let Some(limit) = limit {
            limits.push((window, limit));
        }
    }
    limits
}

fn validate_monotonic(limits: &[(Window, u64)]) -> Result<(), PluginError> {
    for pair in limits.windows(2) {
        let (shorter, shorter_limit) = pair[0];
        let (longer, longer_limit) = pair[1];
        if longer_limit < shorter_limit {
            return Err(PluginError::fail_closed(format!(
                "llm-rate-limit {} limit cannot be lower than the {} limit",
                longer.as_str(),
                shorter.as_str()
            )));
        }
    }
    Ok(())
}

fn limit_by_name(limit_by: LimitBy) -> &'static str {
    match limit_by {
        LimitBy::VirtualKey => "virtual_key",
        LimitBy::Consumer => "consumer",
        LimitBy::ConsumerGroup => "consumer_group",
        LimitBy::Model => "model",
        LimitBy::ClientIp => "client_ip",
        LimitBy::Global => "global",
    }
}

struct RateLimitPlugin {
    manifest: PluginManifest,
    config: CompiledConfig,
    instance_key: String,
    counters: Arc<dyn LlmCounterStore>,
}

#[derive(Debug)]
struct RuleEval {
    label: String,
    limit: u64,
    remaining: u64,
    reset: i64,
    exceeded: bool,
}

struct PendingRule {
    label: String,
    limit: u64,
    reset: i64,
    /// Indices into the counter-window list for this rule's subjects.
    windows: Vec<usize>,
}

impl RateLimitPlugin {
    fn subjects(&self, rule: &Rule, cx: &LlmRequestContext<'_>) -> Vec<String> {
        let key_subject = cx
            .meta
            .virtual_key_family
            .or(cx.meta.virtual_key_id)
            .filter(|value| !value.trim().is_empty())
            .map(|value| format!("vk:{value}"));
        match rule.limit_by {
            LimitBy::VirtualKey => vec![key_subject.unwrap_or_else(|| "vk:anonymous".to_string())],
            LimitBy::Consumer => cx
                .meta
                .consumer_id
                .filter(|value| !value.trim().is_empty())
                .map(|value| format!("consumer:{value}"))
                .or(key_subject)
                .or_else(|| cx.meta.client_ip.map(|ip| format!("ip:{ip}")))
                .into_iter()
                .collect(),
            LimitBy::ConsumerGroup => cx
                .meta
                .consumer_group_ids
                .iter()
                .map(|value| format!("group:{value}"))
                .collect(),
            LimitBy::Model => {
                let model = cx.request.model.trim();
                if model.is_empty() {
                    Vec::new()
                } else {
                    vec![format!("model:{model}")]
                }
            }
            LimitBy::ClientIp => cx
                .meta
                .client_ip
                .map(|ip| format!("ip:{ip}"))
                .into_iter()
                .collect(),
            LimitBy::Global => vec!["global".to_string()],
        }
    }

    async fn evaluate(
        &self,
        cx: &LlmRequestContext<'_>,
        now: i64,
    ) -> Result<(Vec<RuleEval>, bool), PluginError> {
        let total_rules = self.config.rules.len();
        let mut pending = Vec::new();
        let mut keys: Vec<String> = Vec::new();
        let mut specs: Vec<(i64, i64, u64)> = Vec::new();
        let mut reject_without_counter = false;

        for rule in &self.config.rules {
            let label = if total_rules == 1 {
                rule.window.title().to_string()
            } else {
                pascal_case(&rule.name)
            };
            let reset = window_reset(rule.window, now);
            let subjects = self.subjects(rule, cx);
            if subjects.is_empty() {
                if self.config.fault_tolerant {
                    continue;
                }
                // The caller turns a missing subject into a fail-closed plugin
                // error; the empty label is the marker.
                pending.push(PendingRule {
                    label: String::new(),
                    limit: rule.limit,
                    reset,
                    windows: Vec::new(),
                });
                reject_without_counter = true;
                continue;
            }

            let start = window_start(rule.window, now);
            let end = start.saturating_add(reset);
            let mut window_indices = Vec::with_capacity(subjects.len());
            for subject in subjects {
                window_indices.push(keys.len());
                keys.push(format!(
                    "rate:{}:{}:{}:{}:{}",
                    self.instance_key,
                    rule.name,
                    subject,
                    rule.window.as_str(),
                    start
                ));
                specs.push((start, end, rule.limit));
            }
            pending.push(PendingRule {
                label,
                limit: rule.limit,
                reset,
                windows: window_indices,
            });
        }

        let (counts, accepted) = if reject_without_counter || keys.is_empty() {
            (vec![0; keys.len()], false)
        } else {
            let windows: Vec<CounterWindow<'_>> = keys
                .iter()
                .zip(&specs)
                .map(|(key, (start, end, limit))| CounterWindow {
                    key,
                    window_start: *start,
                    window_end: *end,
                    limit: *limit,
                })
                .collect();
            let outcome = self
                .counters
                .admit_windows(&windows)
                .await
                .map_err(|error| PluginError {
                    message: format!("llm-rate-limit counter backend failed: {error}"),
                    fail_policy: self.manifest.fail_policy,
                })?;
            (outcome.counts, outcome.accepted)
        };

        let mut evals = Vec::with_capacity(pending.len());
        let mut rejected = reject_without_counter;
        for rule in pending {
            if rule.label.is_empty() {
                evals.push(RuleEval {
                    label: rule.label,
                    limit: rule.limit,
                    remaining: 0,
                    reset: rule.reset.max(1),
                    exceeded: true,
                });
                continue;
            }
            let mut best_remaining = u64::MAX;
            let mut rule_exceeded = false;
            for index in &rule.windows {
                let used = counts.get(*index).copied().unwrap_or(0);
                best_remaining = best_remaining.min(rule.limit.saturating_sub(used));
                // Counts are post-increment on an accepted admission and
                // pre-increment on a rejected one.
                if !accepted && used >= rule.limit {
                    rule_exceeded = true;
                }
            }
            if rule_exceeded {
                rejected = true;
            }
            evals.push(RuleEval {
                label: rule.label,
                limit: rule.limit,
                remaining: if best_remaining == u64::MAX {
                    0
                } else {
                    best_remaining
                },
                reset: rule.reset.max(1),
                exceeded: rule_exceeded,
            });
        }
        Ok((evals, rejected))
    }

    fn write_headers(&self, cx: &mut LlmRequestContext<'_>, evals: &[RuleEval], rejected: bool) {
        if self.config.hide_client_headers {
            return;
        }
        for eval in evals {
            if eval.label.is_empty() {
                continue;
            }
            cx.response_headers.insert(
                &format!("X-RateLimit-Limit-{}", eval.label),
                &header_number(eval.limit),
            );
            cx.response_headers.insert(
                &format!("X-RateLimit-Remaining-{}", eval.label),
                &header_number(eval.remaining),
            );
        }
        if let Some(worst) =
            evals
                .iter()
                .filter(|eval| !eval.label.is_empty())
                .min_by(|left, right| {
                    left.remaining
                        .cmp(&right.remaining)
                        .then_with(|| left.reset.cmp(&right.reset))
                })
        {
            cx.response_headers
                .insert("RateLimit-Limit", &header_number(worst.limit));
            cx.response_headers
                .insert("RateLimit-Remaining", &header_number(worst.remaining));
            cx.response_headers
                .insert("RateLimit-Reset", &worst.reset.max(1).to_string());
        }
        if rejected {
            let retry = evals
                .iter()
                .filter(|eval| eval.exceeded)
                .map(|eval| eval.reset.max(1))
                .max()
                .unwrap_or(1);
            cx.response_headers
                .insert("Retry-After", &retry.to_string());
        }
    }
}

#[async_trait]
impl LlmPlugin for RateLimitPlugin {
    fn manifest(&self) -> &PluginManifest {
        &self.manifest
    }

    async fn on_request(&self, cx: &mut LlmRequestContext<'_>) -> LlmPluginResult {
        let now = Utc::now().timestamp();
        let (evals, rejected) = self.evaluate(cx, now).await?;

        // Missing subject with fault_tolerant=false: fail closed.
        if !self.config.fault_tolerant && evals.iter().any(|eval| eval.label.is_empty()) {
            return Err(PluginError::fail_closed(
                "llm-rate-limit subject is unavailable",
            ));
        }

        self.write_headers(cx, &evals, rejected);

        if rejected {
            return Ok(LlmAction::Reject(
                GatewayError::new(
                    self.config.error_code,
                    ErrorKind::RateLimited,
                    self.config.error_message.clone(),
                )
                .with_code(self.config.code.clone()),
            ));
        }
        Ok(LlmAction::Continue)
    }
}

fn header_number(value: u64) -> String {
    value.to_string()
}

fn pascal_case(value: &str) -> String {
    value
        .split(['-', '_'])
        .filter(|part| !part.is_empty())
        .map(|part| {
            let mut chars = part.chars();
            match chars.next() {
                Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
                None => String::new(),
            }
        })
        .collect()
}

fn window_start(window: Window, now: i64) -> i64 {
    match window {
        Window::Second => now,
        Window::Minute => now - now.rem_euclid(60),
        Window::Hour => now - now.rem_euclid(3_600),
        Window::Day => now - now.rem_euclid(86_400),
        Window::Month => {
            let time = Utc.timestamp_opt(now, 0).single().unwrap_or_default();
            Utc.with_ymd_and_hms(time.year(), time.month(), 1, 0, 0, 0)
                .single()
                .map(|start| start.timestamp())
                .unwrap_or(now - now.rem_euclid(86_400))
        }
        Window::Year => {
            let time = Utc.timestamp_opt(now, 0).single().unwrap_or_default();
            Utc.with_ymd_and_hms(time.year(), 1, 1, 0, 0, 0)
                .single()
                .map(|start| start.timestamp())
                .unwrap_or(now - now.rem_euclid(86_400))
        }
    }
}

fn window_reset(window: Window, now: i64) -> i64 {
    match window {
        Window::Second => 1,
        Window::Minute => 60 - now.rem_euclid(60),
        Window::Hour => 3_600 - now.rem_euclid(3_600),
        Window::Day => 86_400 - now.rem_euclid(86_400),
        Window::Month => {
            let start = window_start(Window::Month, now);
            let time = Utc.timestamp_opt(start, 0).single().unwrap_or_default();
            let (year, month) = if time.month() == 12 {
                (time.year() + 1, 1)
            } else {
                (time.year(), time.month() + 1)
            };
            Utc.with_ymd_and_hms(year, month, 1, 0, 0, 0)
                .single()
                .map(|next| (next.timestamp() - now).max(1))
                .unwrap_or(86_400)
        }
        Window::Year => {
            let time = Utc.timestamp_opt(now, 0).single().unwrap_or_default();
            Utc.with_ymd_and_hms(time.year() + 1, 1, 1, 0, 0, 0)
                .single()
                .map(|next| (next.timestamp() - now).max(1))
                .unwrap_or(86_400)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use octra_llm_core::ChatRequest;
    use octra_llm_plugin_types::{LlmRequestContext, LlmRequestMeta, LlmResponseHeaders};

    fn plugin(config: Value) -> Arc<dyn LlmPlugin> {
        RateLimitFactory.create(config).unwrap()
    }

    fn meta() -> LlmRequestMeta<'static> {
        LlmRequestMeta {
            request_id: "r",
            ingress: octra_llm_core::Protocol::Openai,
            consumer_id: None,
            virtual_key_id: None,
            virtual_key_family: None,
            consumer_group_ids: &[],
            session_id: None,
            client_ip: None,
            user_agent: None,
            intent: None,
        }
    }

    #[tokio::test]
    async fn missing_consumer_subject_fails_closed_when_configured() {
        let plugin = plugin(json!({
            "rules": [{"name": "consumer", "window": "minute", "limit": 1, "limit_by": "consumer"}],
            "fault_tolerant": false
        }));
        let meta = meta();
        let mut request = ChatRequest::new("m", Vec::new());
        let mut state = Value::Null;
        let mut headers = LlmResponseHeaders::default();
        let mut cx = LlmRequestContext {
            meta: &meta,
            response_headers: &mut headers,
            request: &mut request,
            state: &mut state,
        };
        assert!(plugin.on_request(&mut cx).await.is_err());
    }

    #[test]
    fn rejects_empty_and_mixed_configs() {
        assert!(RateLimitFactory.validate(&json!({})).is_err());
        assert!(
            RateLimitFactory
                .validate(&json!({"minute": 5, "rules": [
                    {"name": "a", "window": "minute", "limit": 1}
                ]}))
                .is_err()
        );
    }

    #[test]
    fn rejects_bad_windows_and_non_local_policy() {
        assert!(
            RateLimitFactory
                .validate(&json!({"rules": [
                    {"name": "a", "window": "fortnight", "limit": 1}
                ]}))
                .is_err()
        );
        assert!(
            RateLimitFactory
                .validate(&json!({"minute": 1, "policy": "redis"}))
                .is_err()
        );
        assert!(
            RateLimitFactory
                .validate(&json!({"minute": 1, "hour": 2, "second": 3}))
                .is_err()
        );
    }

    #[test]
    fn accepts_flat_and_rules_forms() {
        assert!(
            RateLimitFactory
                .validate(&json!({"second": 10, "minute": 100}))
                .is_ok()
        );
        assert!(
            RateLimitFactory
                .validate(&json!({"rules": [
                    {"name": "key-minute", "window": "minute", "limit": 10, "limit_by": "virtual_key"},
                    {"name": "group-hour", "window": "hour", "limit": 100, "limit_by": "consumer_group"}
                ]}))
                .is_ok()
        );
    }

    #[test]
    fn window_math_matches_calendar_boundaries() {
        let now = 1_700_000_000;
        assert_eq!(window_start(Window::Minute, now) % 60, 0);
        assert_eq!(window_start(Window::Hour, now) % 3_600, 0);
        assert_eq!(window_start(Window::Day, now) % 86_400, 0);
        assert!(window_reset(Window::Minute, now) >= 1);
        assert!(window_reset(Window::Year, now) >= 1);
    }

    #[test]
    fn pascal_case_handles_separators() {
        assert_eq!(pascal_case("key-minute"), "KeyMinute");
        assert_eq!(pascal_case("group_hour"), "GroupHour");
        assert_eq!(pascal_case("simple"), "Simple");
    }

    #[test]
    fn flat_rules_are_built_from_windows() {
        let compiled = CompiledConfig::parse(json!({"second": 1, "minute": 10})).unwrap();
        assert_eq!(compiled.rules.len(), 2);
        assert_eq!(compiled.rules[0].name, "second");
        assert_eq!(compiled.rules[1].name, "minute");
    }

    #[test]
    fn plugin_can_be_created() {
        let _ = plugin(json!({"minute": 1}));
    }
}
