//! Kong-compatible rate limiting across fixed windows.
//!
//! The local policy keeps counters in-process. `cluster` and `redis` are
//! accepted for configuration compatibility but fall back to local counters.

use std::collections::HashMap;
use std::sync::{Arc, OnceLock};

use async_trait::async_trait;
use axum::body::Body;
use axum::response::Response;
use chrono::{Datelike, TimeZone, Utc};
use http::{HeaderName, HeaderValue, StatusCode, header};
use octra_api_plugin_types::{
    ApiGatewayPlugin, ApiGatewayPluginFactory, PluginAction, PluginError, PluginManifest,
    PluginPhase, PluginResult, RequestContext, ResponseContext,
};
use parking_lot::Mutex;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::manifest;

const STATE_KEY: &str = "rate-limiting";
/// Hard bound on per-identifier buckets in one limiter. Expired windows are
/// removed first; if the cardinality is still too high, the oldest arbitrary
/// entries are evicted rather than growing without bound.
const MAX_BUCKETS: usize = 100_000;

pub struct RateLimitingFactory;

impl ApiGatewayPluginFactory for RateLimitingFactory {
    fn manifest(&self) -> PluginManifest {
        manifest(
            "rate-limiting",
            910,
            vec![],
            "Rate limits requests across second, minute, hour, day, month, and year windows",
            json!({
                "type": "object",
                "properties": {
                    "second": { "type": "number", "exclusiveMinimum": 0 },
                    "minute": { "type": "number", "exclusiveMinimum": 0 },
                    "hour": { "type": "number", "exclusiveMinimum": 0 },
                    "day": { "type": "number", "exclusiveMinimum": 0 },
                    "month": { "type": "number", "exclusiveMinimum": 0 },
                    "year": { "type": "number", "exclusiveMinimum": 0 },
                    "limit_by": {
                        "type": "string",
                        "enum": ["consumer", "credential", "ip", "service", "header", "path"],
                        "default": "consumer"
                    },
                    "header_name": { "type": "string" },
                    "path": { "type": "string" },
                    "policy": {
                        "type": "string",
                        "enum": ["local", "cluster", "redis"],
                        "default": "local"
                    },
                    "fault_tolerant": { "type": "boolean", "default": true },
                    "hide_client_headers": { "type": "boolean", "default": false },
                    "error_code": { "type": "integer", "minimum": 100, "maximum": 599, "default": 429 },
                    "error_message": { "type": "string", "default": "API rate limit exceeded" },
                    "redis": { "type": ["object", "null"] },
                    "sync_rate": { "type": "number", "default": -1 }
                }
            }),
        )
    }

    fn create(&self, id: &str, config: Value) -> Result<Arc<dyn ApiGatewayPlugin>, PluginError> {
        let config: Config = serde_json::from_value(config).map_err(|error| {
            PluginError::fail_closed(format!("invalid rate-limiting config: {error}"))
        })?;
        validate(&config)?;
        if !matches!(config.policy, Policy::Local) {
            return Err(PluginError::fail_closed(format!(
                "rate-limiting policy '{}' is not supported; only 'local' is implemented",
                config.policy.as_str()
            )));
        }
        LIMITERS
            .get_or_init(|| Mutex::new(HashMap::new()))
            .lock()
            .entry(id.to_string())
            .or_default();
        Ok(Arc::new(RateLimitingPlugin {
            manifest: self.manifest(),
            id: id.to_string(),
            config,
        }))
    }

    fn phases(&self) -> Vec<PluginPhase> {
        vec![PluginPhase::Access, PluginPhase::Response, PluginPhase::Log]
    }
}

#[derive(Debug, Clone, Copy, Default, Deserialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
enum LimitBy {
    #[default]
    Consumer,
    Credential,
    Ip,
    Service,
    Header,
    Path,
}

#[derive(Debug, Clone, Copy, Default, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum Policy {
    #[default]
    Local,
    Cluster,
    Redis,
}

impl Policy {
    fn as_str(&self) -> &'static str {
        match self {
            Self::Local => "local",
            Self::Cluster => "cluster",
            Self::Redis => "redis",
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
struct Config {
    second: Option<f64>,
    minute: Option<f64>,
    hour: Option<f64>,
    day: Option<f64>,
    month: Option<f64>,
    year: Option<f64>,
    limit_by: LimitBy,
    header_name: Option<String>,
    path: Option<String>,
    policy: Policy,
    fault_tolerant: bool,
    hide_client_headers: bool,
    error_code: u16,
    error_message: String,
    redis: Option<Value>,
    sync_rate: f64,
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
            limit_by: LimitBy::Consumer,
            header_name: None,
            path: None,
            policy: Policy::Local,
            fault_tolerant: true,
            hide_client_headers: false,
            error_code: 429,
            error_message: "API rate limit exceeded".into(),
            redis: None,
            sync_rate: -1.0,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum Period {
    Second,
    Minute,
    Hour,
    Day,
    Month,
    Year,
}

impl Period {
    fn title(&self) -> &'static str {
        match self {
            Self::Second => "Second",
            Self::Minute => "Minute",
            Self::Hour => "Hour",
            Self::Day => "Day",
            Self::Month => "Month",
            Self::Year => "Year",
        }
    }
}

struct RateLimitingPlugin {
    manifest: PluginManifest,
    id: String,
    config: Config,
}

fn validate(config: &Config) -> Result<(), PluginError> {
    let limits = configured_limits(config);
    if limits.is_empty() {
        return Err(PluginError::fail_closed(
            "rate-limiting requires at least one of second, minute, hour, day, month, or year",
        ));
    }
    if limits
        .iter()
        .any(|(_, limit)| !limit.is_finite() || *limit <= 0.0)
    {
        return Err(PluginError::fail_closed(
            "rate-limiting limits must be greater than zero",
        ));
    }
    for (index, (period, limit)) in limits.iter().enumerate() {
        for (next_period, next_limit) in limits.iter().skip(index + 1) {
            if *next_limit < *limit {
                return Err(PluginError::fail_closed(format!(
                    "the limit for {} cannot be lower than the limit for {}",
                    next_period.title().to_lowercase(),
                    period.title().to_lowercase()
                )));
            }
        }
    }
    if config.limit_by == LimitBy::Header {
        let name = config
            .header_name
            .as_deref()
            .filter(|name| !name.is_empty())
            .ok_or_else(|| PluginError::fail_closed("limit_by=header requires header_name"))?;
        HeaderName::from_bytes(name.as_bytes()).map_err(|error| {
            PluginError::fail_closed(format!("invalid rate-limiting header_name: {error}"))
        })?;
    }
    if config.limit_by == LimitBy::Path
        && config
            .path
            .as_deref()
            .filter(|path| !path.is_empty())
            .is_none()
    {
        return Err(PluginError::fail_closed("limit_by=path requires path"));
    }
    StatusCode::from_u16(config.error_code).map_err(|error| {
        PluginError::fail_closed(format!("invalid rate-limiting error_code: {error}"))
    })?;
    Ok(())
}

fn configured_limits(config: &Config) -> Vec<(Period, f64)> {
    let mut limits = Vec::new();
    for (period, limit) in [
        (Period::Second, config.second),
        (Period::Minute, config.minute),
        (Period::Hour, config.hour),
        (Period::Day, config.day),
        (Period::Month, config.month),
        (Period::Year, config.year),
    ] {
        if let Some(limit) = limit {
            limits.push((period, limit));
        }
    }
    limits
}

#[derive(Default)]
struct Limiter {
    buckets: HashMap<(String, Period, i64), u64>,
}

static LIMITERS: OnceLock<Mutex<HashMap<String, Limiter>>> = OnceLock::new();

impl RateLimitingPlugin {
    /// Auth plugins run earlier in the chain (priorities 1005–1250 versus 910),
    /// but the request context is built before they do, so fall back to the
    /// authenticated state they populated.
    fn consumer_identifier(cx: &RequestContext<'_>) -> Option<String> {
        cx.consumer_id.map(str::to_string).or_else(|| {
            cx.state
                .consumer
                .as_ref()
                .map(|consumer| consumer.id.clone())
        })
    }

    fn credential_identifier(cx: &RequestContext<'_>) -> Option<String> {
        cx.credential_id.map(str::to_string).or_else(|| {
            cx.state
                .credential
                .as_ref()
                .map(|credential| credential.id.clone())
        })
    }

    fn identifier(&self, cx: &RequestContext<'_>) -> String {
        let primary = match self.config.limit_by {
            LimitBy::Consumer => {
                Self::consumer_identifier(cx).or_else(|| Self::credential_identifier(cx))
            }
            LimitBy::Credential => Self::credential_identifier(cx),
            LimitBy::Ip => None,
            LimitBy::Service => Some(cx.service.id.clone()),
            LimitBy::Header => self.config.header_name.as_deref().and_then(|name| {
                cx.headers
                    .get(name)
                    .and_then(|value| value.to_str().ok())
                    .map(str::to_string)
            }),
            LimitBy::Path => self
                .config
                .path
                .as_deref()
                .and_then(|path| (cx.uri.path() == path).then(|| path.to_string())),
        };

        primary
            .or_else(|| cx.client_ip.map(|ip| ip.to_string()))
            .unwrap_or_else(|| "anonymous".to_string())
    }

    fn check(&self, cx: &RequestContext<'_>) -> Check {
        let identifier = self.identifier(cx);
        let now = Utc::now().timestamp();
        let limits = configured_limits(&self.config);
        let mut limiters = LIMITERS.get_or_init(|| Mutex::new(HashMap::new())).lock();
        let limiter = limiters.entry(self.id.clone()).or_default();

        let mut windows = Vec::with_capacity(limits.len());
        let mut exceeded = false;
        for (period, limit) in limits {
            let start = window_start(period, now);
            let reset = window_reset(period, now);
            let usage = limiter
                .buckets
                .get(&(identifier.clone(), period, start))
                .copied()
                .unwrap_or(0);
            let remaining = (limit - usage as f64).max(0.0);
            if usage as f64 >= limit {
                exceeded = true;
            }
            windows.push(Window {
                period,
                limit,
                remaining: (remaining - 1.0).max(0.0),
                reset,
                key: (identifier.clone(), period, start),
            });
        }

        if limiter.buckets.len() > MAX_BUCKETS {
            limiter
                .buckets
                .retain(|(_, _, start), _| now - start < 366 * 86_400);
        }
        if limiter.buckets.len() > MAX_BUCKETS {
            let excess = limiter.buckets.len() - MAX_BUCKETS;
            let victims: Vec<_> = limiter.buckets.keys().take(excess).cloned().collect();
            for key in victims {
                limiter.buckets.remove(&key);
            }
        }

        if !exceeded {
            for window in &windows {
                *limiter.buckets.entry(window.key.clone()).or_insert(0) += 1;
            }
        }

        Check { windows, exceeded }
    }
}

impl Drop for RateLimitingPlugin {
    fn drop(&mut self) {
        if let Some(limiters) = LIMITERS.get() {
            limiters.lock().remove(&self.id);
        }
    }
}

struct Window {
    period: Period,
    limit: f64,
    remaining: f64,
    reset: i64,
    key: (String, Period, i64),
}

struct Check {
    windows: Vec<Window>,
    exceeded: bool,
}

#[async_trait]
impl ApiGatewayPlugin for RateLimitingPlugin {
    fn manifest(&self) -> &PluginManifest {
        &self.manifest
    }

    async fn access(&self, cx: &mut RequestContext<'_>) -> PluginResult {
        let check = self.check(cx);
        let headers = client_headers(&check);
        store_state(cx, &headers, check.exceeded.then(|| worst_reset(&check)));

        if !check.exceeded {
            return Ok(PluginAction::Continue);
        }

        let body = json!({"message": self.config.error_message}).to_string();
        let mut response = Response::new(Body::from(body));
        *response.status_mut() =
            StatusCode::from_u16(self.config.error_code).unwrap_or(StatusCode::TOO_MANY_REQUESTS);
        response.headers_mut().insert(
            header::CONTENT_TYPE,
            HeaderValue::from_static("application/json"),
        );
        Ok(PluginAction::ShortCircuit(response))
    }

    async fn response(&self, cx: &mut ResponseContext<'_>) -> PluginResult {
        if self.config.hide_client_headers {
            return Ok(PluginAction::Continue);
        }
        let Some(value) = cx.state.values.get(STATE_KEY) else {
            return Ok(PluginAction::Continue);
        };
        if let Some(headers) = value.get("headers").and_then(Value::as_object) {
            for (name, value) in headers {
                if let (Ok(name), Some(value)) = (
                    HeaderName::from_bytes(name.as_bytes()),
                    value
                        .as_str()
                        .and_then(|value| HeaderValue::from_str(value).ok()),
                ) {
                    cx.headers.insert(name, value);
                }
            }
        }
        if let Some(retry) = value.get("retry_after").and_then(Value::as_str)
            && let Ok(value) = HeaderValue::from_str(retry)
        {
            cx.headers.insert(header::RETRY_AFTER, value);
        }
        Ok(PluginAction::Continue)
    }
}

fn client_headers(check: &Check) -> Vec<(String, String)> {
    let mut headers = Vec::new();
    for window in &check.windows {
        headers.push((
            format!("X-RateLimit-Limit-{}", window.period.title()),
            number(window.limit),
        ));
        headers.push((
            format!("X-RateLimit-Remaining-{}", window.period.title()),
            number(window.remaining),
        ));
    }
    if let Some(worst) = check.windows.iter().min_by(|left, right| {
        left.remaining
            .total_cmp(&right.remaining)
            .then_with(|| right.reset.cmp(&left.reset))
    }) {
        headers.push(("RateLimit-Limit".into(), number(worst.limit)));
        headers.push(("RateLimit-Remaining".into(), number(worst.remaining)));
        headers.push(("RateLimit-Reset".into(), worst.reset.to_string()));
    }
    headers
}

fn worst_reset(check: &Check) -> String {
    check
        .windows
        .iter()
        .filter(|window| window.remaining <= 0.0)
        .max_by_key(|window| window.reset)
        .or_else(|| check.windows.iter().max_by_key(|window| window.reset))
        .map(|window| window.reset.to_string())
        .unwrap_or_else(|| "1".into())
}

fn store_state(
    cx: &mut RequestContext<'_>,
    headers: &[(String, String)],
    retry_after: Option<String>,
) {
    if cx.state.values.contains_key(STATE_KEY) {
        return;
    }
    let headers: serde_json::Map<String, Value> = headers
        .iter()
        .map(|(name, value)| (name.clone(), Value::String(value.clone())))
        .collect();
    let mut value = serde_json::Map::new();
    value.insert("headers".into(), Value::Object(headers));
    if let Some(retry_after) = retry_after {
        value.insert("retry_after".into(), Value::String(retry_after));
    }
    cx.state
        .values
        .insert(STATE_KEY.to_string(), Value::Object(value));
}

fn number(value: f64) -> String {
    if value.fract() == 0.0 {
        format!("{}", value as i64)
    } else {
        format!("{value}")
    }
}

fn window_start(period: Period, now: i64) -> i64 {
    match period {
        Period::Second => now,
        Period::Minute => now - now.rem_euclid(60),
        Period::Hour => now - now.rem_euclid(3_600),
        Period::Day => now - now.rem_euclid(86_400),
        Period::Month => {
            let time = Utc.timestamp_opt(now, 0).single().unwrap_or_default();
            Utc.with_ymd_and_hms(time.year(), time.month(), 1, 0, 0, 0)
                .single()
                .map(|start| start.timestamp())
                .unwrap_or(now - now.rem_euclid(86_400))
        }
        Period::Year => {
            let time = Utc.timestamp_opt(now, 0).single().unwrap_or_default();
            Utc.with_ymd_and_hms(time.year(), 1, 1, 0, 0, 0)
                .single()
                .map(|start| start.timestamp())
                .unwrap_or(now - now.rem_euclid(86_400))
        }
    }
}

fn window_reset(period: Period, now: i64) -> i64 {
    match period {
        Period::Second => 1,
        Period::Minute => 60 - now.rem_euclid(60),
        Period::Hour => 3_600 - now.rem_euclid(3_600),
        Period::Day => 86_400 - now.rem_euclid(86_400),
        Period::Month => {
            let start = window_start(Period::Month, now);
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
        Period::Year => {
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

    #[test]
    fn rejects_missing_limits_and_bad_order() {
        assert!(RateLimitingFactory.create("test", json!({})).is_err());
        assert!(
            RateLimitingFactory
                .create("test", json!({"minute": 1, "hour": 10, "second": 20}))
                .is_err()
        );
        assert!(
            RateLimitingFactory
                .create("test", json!({"minute": 1, "limit_by": "header"}))
                .is_err()
        );
        assert!(
            RateLimitingFactory
                .create("test", json!({"minute": 1, "policy": "redis"}))
                .is_err()
        );
    }

    #[test]
    fn accepts_kong_style_configs() {
        let plugin = RateLimitingFactory
            .create(
                "test",
                json!({"second": 10, "minute": 100, "limit_by": "consumer"}),
            )
            .unwrap();
        assert_eq!(plugin.manifest().name, "rate-limiting");
    }
}
