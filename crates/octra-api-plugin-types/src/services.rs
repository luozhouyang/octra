//! Events, scheduled jobs, and the service surfaces a plugin can expose
//! outside the request path.
//!
//! A plugin declares the topics it consumes and the jobs it wants to run in
//! its manifest. The host persists both, delivers events at least once, and
//! invokes the plugin through the same typed contract used by request stages.

use std::str::FromStr;
use std::sync::Arc;

use async_trait::async_trait;
use chrono::{TimeZone, Utc};
use cron::Schedule;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::PluginError;

/// Current third-party plugin contract version.
pub const API_PLUGIN_API_VERSION: &str = "3";

/// Maximum number of event subscriptions a plugin may declare.
pub const MAX_SUBSCRIPTIONS: usize = 32;
/// Maximum number of scheduled jobs a plugin may declare.
pub const MAX_SCHEDULED_JOBS: usize = 16;
/// Maximum length of a topic name or pattern.
pub const MAX_TOPIC_LEN: usize = 128;
/// Maximum length of a job name.
pub const MAX_JOB_NAME_LEN: usize = 64;

/// One event topic a plugin wants to consume. `topic` may be an exact name,
/// `*`, or a prefix ending in `.*` (for example `api.*`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SubscriptionSpec {
    pub topic: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

/// One cron job a plugin wants the host to run. `schedule` is a six-field
/// cron expression with seconds first: `sec min hour day-of-month month
/// day-of-week`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScheduledJobSpec {
    pub name: String,
    pub schedule: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// Per-run timeout. Defaults to the manifest's `timeout_ms`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout_ms: Option<u64>,
}

/// Event delivered to a subscriber. Payloads are content-free by convention:
/// plugins receive identifiers and metrics, not credentials or bodies.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct GatewayEvent {
    pub id: String,
    pub topic: String,
    pub source: String,
    pub timestamp: i64,
    pub payload: Value,
}

/// One scheduled invocation. `id` identifies this run, not the job.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct JobInvocation {
    pub id: String,
    pub name: String,
    pub scheduled_for: i64,
    pub attempt: u32,
}

/// A compiled subscription. The runtime holds the plugin instance so the host
/// can deliver without looking it up on every event.
#[async_trait]
pub trait EventSubscriber: Send + Sync {
    fn subscriber_id(&self) -> &str;
    fn plugin_name(&self) -> &str;
    fn instance_id(&self) -> &str;
    fn topics(&self) -> &[String];
    async fn deliver(&self, event: &GatewayEvent) -> Result<(), PluginError>;
}

/// A compiled job declaration.
#[async_trait]
pub trait ScheduledJob: Send + Sync {
    /// Unique host-side id: `<plugin>#<instance>#<job>`.
    fn job_id(&self) -> &str;
    fn plugin_name(&self) -> &str;
    fn instance_id(&self) -> &str;
    fn job_name(&self) -> &str;
    fn schedule(&self) -> &str;
    fn timeout(&self) -> std::time::Duration {
        std::time::Duration::from_secs(30)
    }
    async fn run(&self, job: &JobInvocation) -> Result<(), PluginError>;
}

/// Source of the currently enabled subscriptions and jobs. Runtimes expose a
/// snapshot so an install, upgrade or reload is visible on the next tick.
pub trait PluginServiceSource: Send + Sync {
    fn subscribers(&self) -> Vec<Arc<dyn EventSubscriber>>;
    fn jobs(&self) -> Vec<Arc<dyn ScheduledJob>>;
}

/// Fire-and-forget publisher used by the data and control planes. It must not
/// block the request path.
pub trait EventPublisher: Send + Sync {
    fn publish(&self, topic: &str, source: &str, payload: Value);
}

/// Returns true when `pattern` selects `topic`.
pub fn topic_matches(pattern: &str, topic: &str) -> bool {
    if pattern == "*" {
        return true;
    }
    if let Some(prefix) = pattern.strip_suffix(".*") {
        return topic
            .strip_prefix(prefix)
            .is_some_and(|rest| rest.starts_with('.'));
    }
    pattern == topic
}

/// Validates a topic name or wildcard pattern.
pub fn validate_topic_pattern(pattern: &str) -> Result<(), String> {
    if pattern.is_empty() {
        return Err("topic must not be empty".to_string());
    }
    if pattern.len() > MAX_TOPIC_LEN {
        return Err(format!("topic exceeds {MAX_TOPIC_LEN} bytes"));
    }
    if pattern == "*" {
        return Ok(());
    }

    let mut parts = pattern.split('.');
    let count = parts.clone().count();
    for (index, part) in parts.by_ref().enumerate() {
        if part.is_empty() {
            return Err(format!("topic '{pattern}' has an empty segment"));
        }
        if part == "*" {
            if index + 1 != count {
                return Err(format!(
                    "topic '{pattern}' may only use '*' as its final segment"
                ));
            }
        } else if !part
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || ch == '_' || ch == '-')
        {
            return Err(format!(
                "topic '{pattern}' may only contain alphanumerics, '_' and '-'"
            ));
        }
    }
    Ok(())
}

/// Validates a job name.
pub fn validate_job_name(name: &str) -> Result<(), String> {
    if name.is_empty() {
        return Err("job name must not be empty".to_string());
    }
    if name.len() > MAX_JOB_NAME_LEN {
        return Err(format!("job name exceeds {MAX_JOB_NAME_LEN} bytes"));
    }
    if !name
        .chars()
        .all(|ch| ch.is_ascii_alphanumeric() || ch == '_' || ch == '-')
    {
        return Err(format!(
            "job name '{name}' may only contain alphanumerics, '_' and '-'"
        ));
    }
    Ok(())
}

/// Parses a six-field cron expression (seconds first).
pub fn parse_schedule(schedule: &str) -> Result<Schedule, String> {
    Schedule::from_str(schedule).map_err(|error| format!("invalid cron schedule: {error}"))
}

/// Next occurrence strictly after `after`, expressed as Unix seconds.
pub fn next_run_after(schedule: &str, after: i64) -> Result<Option<i64>, String> {
    let schedule = parse_schedule(schedule)?;
    let after = Utc
        .timestamp_opt(after, 0)
        .single()
        .ok_or_else(|| "timestamp is out of range".to_string())?;
    Ok(schedule.after(&after).next().map(|next| next.timestamp()))
}

/// Next occurrence from now, expressed as Unix seconds.
pub fn next_run_from_now(schedule: &str) -> Result<Option<i64>, String> {
    next_run_after(schedule, Utc::now().timestamp())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn topic_patterns_match_exactly_and_by_prefix() {
        assert!(topic_matches(
            "api.request.completed",
            "api.request.completed"
        ));
        assert!(!topic_matches(
            "api.request.completed",
            "api.request.failed"
        ));
        assert!(topic_matches("api.*", "api.request.completed"));
        assert!(!topic_matches("api.*", "llm.request.completed"));
        assert!(topic_matches("*", "anything.at.all"));
    }

    #[test]
    fn topic_validation_rejects_bad_patterns() {
        assert!(validate_topic_pattern("api.request.completed").is_ok());
        assert!(validate_topic_pattern("api.*").is_ok());
        assert!(validate_topic_pattern("*").is_ok());
        for bad in [
            "",
            "api..completed",
            "api.*.completed",
            "api request",
            "a/b",
        ] {
            assert!(validate_topic_pattern(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn job_names_are_restricted() {
        assert!(validate_job_name("cleanup").is_ok());
        assert!(validate_job_name("quota-reset_1").is_ok());
        for bad in ["", "with space", "a/b"] {
            assert!(validate_job_name(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn cron_schedules_produce_a_next_run() {
        let next = next_run_after("0 0 0 * * *", 1_700_000_000).unwrap();
        assert!(next.is_some());
        assert!(next.unwrap() > 1_700_000_000);
        assert!(next_run_after("not a cron", 0).is_err());
    }
}
