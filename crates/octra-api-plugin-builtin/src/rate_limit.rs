//! Token-bucket rate limiting keyed by consumer, IP, route, service, or global.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use axum::body::Body;
use axum::response::Response;
use http::{HeaderValue, StatusCode, header};
use octra_api_plugin_types::{
    ApiGatewayPlugin, ApiGatewayPluginFactory, Capability, PluginAction, PluginError,
    PluginManifest, PluginResult, RequestContext,
};
use parking_lot::Mutex;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::manifest;

pub struct RateLimitFactory;

impl ApiGatewayPluginFactory for RateLimitFactory {
    fn manifest(&self) -> PluginManifest {
        manifest(
            "rate-limit",
            20,
            vec![Capability::Log],
            "Token-bucket rate limiting per consumer, IP, route, service, or globally",
            json!({
                "type": "object",
                "properties": {
                    "requests_per_second": { "type": "number", "exclusiveMinimum": 0 },
                    "burst": { "type": "integer", "minimum": 1 },
                    "key": { "type": "string", "enum": ["ip", "consumer", "route", "service", "global"] }
                }
            }),
        )
    }

    fn create(&self, _id: &str, config: Value) -> Result<Arc<dyn ApiGatewayPlugin>, PluginError> {
        let config: RateLimitConfig = serde_json::from_value(config).map_err(|error| {
            PluginError::fail_closed(format!("invalid rate-limit config: {error}"))
        })?;
        if !config.requests_per_second.is_finite() || config.requests_per_second <= 0.0 {
            return Err(PluginError::fail_closed(
                "rate-limit.requests_per_second must be greater than zero",
            ));
        }
        Ok(Arc::new(RateLimitPlugin {
            manifest: self.manifest(),
            config,
            limiter: Mutex::new(Limiter::default()),
        }))
    }
}

#[derive(Debug, Clone, Copy, Default, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum RateLimitKey {
    #[default]
    Ip,
    Consumer,
    Route,
    Service,
    Global,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
struct RateLimitConfig {
    requests_per_second: f64,
    burst: Option<u32>,
    key: RateLimitKey,
}

impl Default for RateLimitConfig {
    fn default() -> Self {
        Self {
            requests_per_second: 10.0,
            burst: None,
            key: RateLimitKey::Ip,
        }
    }
}

struct RateLimitPlugin {
    manifest: PluginManifest,
    config: RateLimitConfig,
    limiter: Mutex<Limiter>,
}

#[async_trait]
impl ApiGatewayPlugin for RateLimitPlugin {
    fn manifest(&self) -> &PluginManifest {
        &self.manifest
    }

    async fn on_request(&self, cx: &mut RequestContext<'_>) -> PluginResult {
        let key = match self.config.key {
            RateLimitKey::Ip => cx
                .client_ip
                .map(|ip| ip.to_string())
                .unwrap_or_else(|| "unknown".to_string()),
            RateLimitKey::Consumer => cx
                .consumer_id
                .map(str::to_string)
                .unwrap_or_else(|| "anonymous".to_string()),
            RateLimitKey::Route => cx.route.id.clone(),
            RateLimitKey::Service => cx.service.id.clone(),
            RateLimitKey::Global => "global".to_string(),
        };
        let burst = self
            .config
            .burst
            .unwrap_or_else(|| self.config.requests_per_second.ceil() as u32)
            .max(1);

        let retry_after = self
            .limiter
            .lock()
            .check(&key, self.config.requests_per_second, burst);
        match retry_after {
            None => Ok(PluginAction::Continue),
            Some(seconds) => Ok(PluginAction::ShortCircuit(too_many_requests(seconds))),
        }
    }
}

#[derive(Default)]
struct Limiter {
    buckets: HashMap<String, Bucket>,
}

struct Bucket {
    tokens: f64,
    last: Instant,
}

impl Limiter {
    /// Returns `None` when the request is allowed, otherwise the suggested
    /// `Retry-After` value in seconds.
    fn check(&mut self, key: &str, rate: f64, burst: u32) -> Option<u64> {
        let now = Instant::now();
        let capacity = f64::from(burst);
        let retry_after = {
            let bucket = self.buckets.entry(key.to_string()).or_insert(Bucket {
                tokens: capacity,
                last: now,
            });
            let elapsed = now.saturating_duration_since(bucket.last).as_secs_f64();
            bucket.tokens = (bucket.tokens + elapsed * rate).min(capacity);
            bucket.last = now;

            if bucket.tokens >= 1.0 {
                bucket.tokens -= 1.0;
                None
            } else {
                let missing = 1.0 - bucket.tokens;
                Some(((missing / rate).ceil() as u64).max(1))
            }
        };

        if retry_after.is_some() && self.buckets.len() > 10_000 {
            let cutoff = Duration::from_secs(600);
            self.buckets
                .retain(|_, bucket| bucket.last.elapsed() < cutoff);
        }

        retry_after
    }
}

fn too_many_requests(retry_after: u64) -> Response {
    let body = json!({
        "error": {
            "code": "rate_limited",
            "message": "rate limit exceeded"
        }
    })
    .to_string();
    let mut response = Response::new(Body::from(body));
    *response.status_mut() = StatusCode::TOO_MANY_REQUESTS;
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/json"),
    );
    if let Ok(value) = HeaderValue::from_str(&retry_after.to_string()) {
        response.headers_mut().insert(header::RETRY_AFTER, value);
    }
    response
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn allows_burst_then_denies() {
        let mut limiter = Limiter::default();
        assert!(limiter.check("ip", 1.0, 2).is_none());
        assert!(limiter.check("ip", 1.0, 2).is_none());
        assert_eq!(limiter.check("ip", 1.0, 2), Some(1));
    }

    #[test]
    fn keys_are_isolated() {
        let mut limiter = Limiter::default();
        assert!(limiter.check("a", 1.0, 1).is_none());
        assert!(limiter.check("b", 1.0, 1).is_none());
        assert_eq!(limiter.check("a", 1.0, 1), Some(1));
    }

    #[test]
    fn rejects_non_positive_rate() {
        let Err(error) = RateLimitFactory.create("test", json!({"requests_per_second": 0})) else {
            panic!("expected an error");
        };
        assert!(error.message.contains("requests_per_second"));
    }
}
