//! Kong-style multi-window rate limiting for the LLM data plane.
//!
//! The plugin counts admitted requests in fixed UTC calendar windows. It can
//! count per virtual-key family, consumer, consumer group, requested model,
//! client IP or globally. Counters live in process memory and are keyed by the
//! stable plugin-config instance id, so a chain reload keeps them.

use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use std::sync::{Arc, OnceLock};

use async_trait::async_trait;
use chrono::{Datelike, TimeZone, Utc};
use octra_api_plugin_types::{Capability, PluginError, PluginManifest};
use octra_llm_core::{ErrorKind, GatewayError};
use octra_llm_plugin_types::{
    LlmAction, LlmPlugin, LlmPluginCreateContext, LlmPluginFactory, LlmPluginResult,
    LlmRequestContext, LlmScope, REQUEST_LLM_SCOPES,
};
use parking_lot::Mutex;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::manifest;

const MAX_RULES: usize = 32;
const MAX_BUCKETS: usize = 100_000;
const PRUNE_INTERVAL_MS: i64 = 60_000;

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
            limiter: limiter_for(&key),
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
            limiter: limiter_for(context.instance_key),
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

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct BucketKey {
    rule: String,
    subject: String,
    window: Window,
    window_start: i64,
}

#[derive(Debug, Default)]
struct Limiter {
    buckets: HashMap<BucketKey, u64>,
    last_prune_ms: i64,
}

impl Limiter {
    fn prune(&mut self, now_ms: i64) {
        if now_ms - self.last_prune_ms < PRUNE_INTERVAL_MS && self.buckets.len() < MAX_BUCKETS {
            return;
        }
        self.last_prune_ms = now_ms;
        let now = now_ms / 1000;
        self.buckets
            .retain(|key, _| window_end(key.window, key.window_start) > now);
        if self.buckets.len() > MAX_BUCKETS {
            let mut starts: Vec<i64> = self.buckets.keys().map(|key| key.window_start).collect();
            starts.sort_unstable();
            let cutoff = starts[starts.len() - MAX_BUCKETS];
            self.buckets.retain(|key, _| key.window_start >= cutoff);
        }
    }
}

fn window_end(window: Window, start: i64) -> i64 {
    start.saturating_add(window_reset(window, start))
}

fn limiters() -> &'static Mutex<HashMap<String, Arc<Mutex<Limiter>>>> {
    static LIMITERS: OnceLock<Mutex<HashMap<String, Arc<Mutex<Limiter>>>>> = OnceLock::new();
    LIMITERS.get_or_init(|| Mutex::new(HashMap::new()))
}

fn limiter_for(instance_key: &str) -> Arc<Mutex<Limiter>> {
    let mut limiters = limiters().lock();
    limiters
        .entry(instance_key.to_string())
        .or_insert_with(|| Arc::new(Mutex::new(Limiter::default())))
        .clone()
}

struct RateLimitPlugin {
    manifest: PluginManifest,
    config: CompiledConfig,
    limiter: Arc<Mutex<Limiter>>,
}

#[derive(Debug)]
struct RuleEval {
    label: String,
    limit: u64,
    remaining: u64,
    reset: i64,
    exceeded: bool,
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

    fn evaluate(
        &self,
        cx: &LlmRequestContext<'_>,
        now: i64,
        limiter: &mut Limiter,
    ) -> (Vec<RuleEval>, bool) {
        let total_rules = self.config.rules.len();
        let mut evals = Vec::new();
        let mut increment = Vec::new();
        let mut rejected = false;

        for rule in &self.config.rules {
            let subjects = self.subjects(rule, cx);
            if subjects.is_empty() {
                if self.config.fault_tolerant {
                    continue;
                }
                // The caller turns a missing subject into a fail-closed plugin
                // error; the empty label is the marker.
                evals.push(RuleEval {
                    label: String::new(),
                    limit: rule.limit,
                    remaining: 0,
                    reset: window_reset(rule.window, now),
                    exceeded: true,
                });
                rejected = true;
                continue;
            }

            let mut best_remaining = u64::MAX;
            let mut best_reset = window_reset(rule.window, now);
            let mut rule_exceeded = false;
            let mut exceeded_reset = 0;
            for subject in subjects {
                let start = window_start(rule.window, now);
                let key = BucketKey {
                    rule: rule.name.clone(),
                    subject,
                    window: rule.window,
                    window_start: start,
                };
                let used = *limiter.buckets.get(&key).unwrap_or(&0);
                let remaining = rule.limit.saturating_sub(used);
                let reset = window_reset(rule.window, now);
                if used >= rule.limit {
                    rule_exceeded = true;
                    exceeded_reset = exceeded_reset.max(reset);
                }
                if remaining < best_remaining {
                    best_remaining = remaining;
                    best_reset = reset;
                }
                increment.push(key);
            }
            if rule_exceeded {
                rejected = true;
                best_reset = exceeded_reset.max(best_reset);
            }
            let label = if total_rules == 1 {
                rule.window.title().to_string()
            } else {
                pascal_case(&rule.name)
            };
            evals.push(RuleEval {
                label,
                limit: rule.limit,
                remaining: if best_remaining == u64::MAX {
                    0
                } else {
                    best_remaining
                },
                reset: best_reset,
                exceeded: rule_exceeded,
            });
        }

        if !rejected {
            for key in increment {
                *limiter.buckets.entry(key).or_insert(0) += 1;
            }
            for eval in &mut evals {
                eval.remaining = eval.remaining.saturating_sub(1);
            }
        }
        (evals, rejected)
    }

    fn write_headers(&self, cx: &mut LlmRequestContext<'_>, evals: &[RuleEval], rejected: bool) {
        if self.config.hide_client_headers {
            return;
        }
        for eval in evals {
            if eval.label.is_empty() {
                continue;
            }
            cx.headers.insert(
                &format!("X-RateLimit-Limit-{}", eval.label),
                &header_number(eval.limit),
            );
            cx.headers.insert(
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
            cx.headers
                .insert("RateLimit-Limit", &header_number(worst.limit));
            cx.headers
                .insert("RateLimit-Remaining", &header_number(worst.remaining));
            cx.headers
                .insert("RateLimit-Reset", &worst.reset.max(1).to_string());
        }
        if rejected {
            let retry = evals
                .iter()
                .filter(|eval| eval.exceeded)
                .map(|eval| eval.reset.max(1))
                .max()
                .unwrap_or(1);
            cx.headers.insert("Retry-After", &retry.to_string());
        }
    }
}

#[async_trait]
impl LlmPlugin for RateLimitPlugin {
    fn manifest(&self) -> &PluginManifest {
        &self.manifest
    }

    async fn on_request(&self, cx: &mut LlmRequestContext<'_>) -> LlmPluginResult {
        let now_dt = Utc::now();
        let now = now_dt.timestamp();
        let (evals, rejected) = {
            let mut limiter = self.limiter.lock();
            limiter.prune(now_dt.timestamp_millis());
            self.evaluate(cx, now, &mut limiter)
        };

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
            headers: &mut headers,
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
