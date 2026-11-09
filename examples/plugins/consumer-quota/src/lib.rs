//! Per-consumer fixed-window quota, backed by the plugin-owned SQLite database.
//!
//! The flow:
//!   1. Host applies `migrations/000_base.sql` at install time.
//!   2. `on_access` atomically increments `quota_usage` for the current
//!      fixed window and rejects with 429 once the limit is exceeded.
//!   3. The counter lives in the plugin's own `data.db`, so it survives
//!      restarts and upgrades.

#[allow(warnings)]
mod bindings;

use octra_api_plugin_sdk::{
    AdminRequest, ApiPlugin, Decision, Error, LogEntry, RequestContext, Response,
};
use serde::Deserialize;
use serde_json::json;

#[derive(Debug, Clone, Deserialize)]
struct Config {
    /// Maximum requests per consumer per window.
    limit: i64,
    /// Fixed window length in seconds.
    window_seconds: i64,
    /// Optional limit for unauthenticated requests. When absent, anonymous
    /// requests are not counted.
    #[serde(default)]
    anonymous_limit: Option<i64>,
    /// Status returned when the quota is exhausted.
    #[serde(default = "default_reject_status")]
    reject_status: u16,
}

fn default_reject_status() -> u16 {
    429
}

struct ConsumerQuota {
    config: Config,
}

impl ApiPlugin for ConsumerQuota {
    fn configure(config: serde_json::Value) -> Result<Self, Error> {
        let config: Config = serde_json::from_value(config)
            .map_err(|error| Error::config(format!("invalid config: {error}")))?;
        if config.limit < 1 {
            return Err(Error::config("limit must be at least 1"));
        }
        if !(1..=3600).contains(&config.window_seconds) {
            return Err(Error::config("window_seconds must be between 1 and 3600"));
        }
        if let Some(limit) = config.anonymous_limit {
            if limit < 0 {
                return Err(Error::config("anonymous_limit must not be negative"));
            }
        }
        Ok(Self { config })
    }

    fn on_access(&self, cx: &mut RequestContext) -> Result<Decision, Error> {
        let now = cx.now_unix();
        let window_start = now - now.rem_euclid(self.config.window_seconds);

        let (subject, limit) = match cx.consumer_id() {
            Some(consumer_id) => (format!("consumer:{consumer_id}"), self.config.limit),
            None => {
                let Some(limit) = self.config.anonymous_limit else {
                    return Ok(Decision::continue_request());
                };
                let ip = cx.client_ip().unwrap_or("unknown");
                (format!("ip:{ip}"), limit)
            }
        };

        // Atomic upsert: no read-modify-write race under concurrent requests.
        let count = cx.storage().increment(
            "quota_usage",
            &json!({ "consumer_id": subject, "window_start": window_start }),
            "count",
            1,
            Some(&json!({ "updated_at": now })),
        )?;

        if count > limit {
            let retry_after = self.config.window_seconds - (now - window_start);
            return Ok(Decision::reject(Response {
                status: self.config.reject_status,
                headers: vec![
                    ("retry-after".to_string(), retry_after.to_string()),
                    ("x-quota-limit".to_string(), limit.to_string()),
                    ("x-quota-remaining".to_string(), "0".to_string()),
                ],
                body: br#"{"message":"quota exceeded"}"#.to_vec(),
            }));
        }

        let remaining = limit - count;
        cx.set_request_header("x-quota-limit", limit.to_string());
        cx.set_request_header("x-quota-remaining", remaining.to_string());
        Ok(Decision::continue_request())
    }

    fn on_admin_request(&self, request: &AdminRequest) -> Result<Response, Error> {
        match request.path.as_str() {
            "/status" if request.method.eq_ignore_ascii_case("GET") => Ok(json_response(
                200,
                json!({
                    "limit": self.config.limit,
                    "window_seconds": self.config.window_seconds,
                    "anonymous_limit": self.config.anonymous_limit,
                    "reject_status": self.config.reject_status,
                }),
            )),
            path if path.starts_with("/usage/") => {
                let consumer = request.param("consumer").unwrap_or_default();
                let rows = request.storage().query(
                    "quota_usage",
                    &json!({
                        "filters": [{
                            "field": "consumer_id",
                            "op": "eq",
                            "value": format!("consumer:{consumer}")
                        }],
                        "limit": 20,
                    }),
                )?;
                Ok(json_response(
                    200,
                    json!({ "consumer": consumer, "usage": rows }),
                ))
            }
            _ => Ok(json_response(
                404,
                json!({ "message": "admin route not found" }),
            )),
        }
    }

    fn on_log(&self, entry: &LogEntry) {
        if entry.status == self.config.reject_status {
            octra_api_plugin_sdk::log::warn(&format!(
                "quota rejected request {} for consumer {:?}",
                entry.request_id, entry.consumer_id
            ));
        }
    }
}

fn json_response(status: u16, body: serde_json::Value) -> Response {
    Response {
        status,
        headers: vec![("content-type".to_string(), "application/json".to_string())],
        body: body.to_string().into_bytes(),
    }
}

octra_api_plugin_sdk::export_plugin!(ConsumerQuota, bindings);
