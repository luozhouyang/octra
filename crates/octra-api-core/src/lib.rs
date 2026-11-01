//! Domain types for the Octra API gateway.
//!
//! These types describe services, routes, upstreams, targets, consumers, and
//! API credentials. They are shared by the storage layer, the compiled runtime
//! snapshot, and the control plane payloads.

use std::collections::BTreeMap;

use chrono::{DateTime, Utc};
use octra_core::Error;
use serde::{Deserialize, Serialize};

pub const MIN_TARGET_WEIGHT: u32 = 1;
pub const MAX_TARGET_WEIGHT: u32 = 1000;

pub const METHODS: [&str; 9] = [
    "GET", "HEAD", "POST", "PUT", "PATCH", "DELETE", "OPTIONS", "CONNECT", "TRACE",
];

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Service {
    pub id: String,
    pub name: String,
    pub protocol: String,
    pub description: String,
    pub enabled: bool,
    pub connect_timeout_ms: i64,
    pub request_timeout_ms: i64,
    pub retries: i64,
    pub require_auth: bool,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct CreateService {
    pub id: Option<String>,
    pub name: String,
    pub protocol: Option<String>,
    pub description: Option<String>,
    pub enabled: Option<bool>,
    pub connect_timeout_ms: Option<i64>,
    pub request_timeout_ms: Option<i64>,
    pub retries: Option<i64>,
    pub require_auth: Option<bool>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct UpdateService {
    pub name: Option<String>,
    pub protocol: Option<String>,
    pub description: Option<String>,
    pub enabled: Option<bool>,
    pub connect_timeout_ms: Option<i64>,
    pub request_timeout_ms: Option<i64>,
    pub retries: Option<i64>,
    pub require_auth: Option<bool>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Route {
    pub id: String,
    pub service_id: String,
    /// Exact host to match. `None` matches any host.
    pub host: Option<String>,
    pub path_prefix: String,
    pub methods: Vec<String>,
    pub headers: BTreeMap<String, String>,
    pub protocol: String,
    pub priority: i64,
    pub strip_prefix: bool,
    pub enabled: bool,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct CreateRoute {
    pub id: Option<String>,
    pub service_id: String,
    pub host: Option<String>,
    pub path_prefix: Option<String>,
    pub methods: Option<Vec<String>>,
    pub headers: Option<BTreeMap<String, String>>,
    pub protocol: Option<String>,
    pub priority: Option<i64>,
    pub strip_prefix: Option<bool>,
    pub enabled: Option<bool>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct UpdateRoute {
    pub service_id: Option<String>,
    /// `Some(None)` clears the host; `None` keeps the current host.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        with = "double_option"
    )]
    pub host: Option<Option<String>>,
    pub path_prefix: Option<String>,
    pub methods: Option<Vec<String>>,
    pub headers: Option<BTreeMap<String, String>>,
    pub protocol: Option<String>,
    pub priority: Option<i64>,
    pub strip_prefix: Option<bool>,
    pub enabled: Option<bool>,
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum LoadBalanceAlgorithm {
    #[default]
    RoundRobin,
    WeightedRoundRobin,
}

impl LoadBalanceAlgorithm {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::RoundRobin => "round_robin",
            Self::WeightedRoundRobin => "weighted_round_robin",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "round_robin" => Some(Self::RoundRobin),
            "weighted_round_robin" => Some(Self::WeightedRoundRobin),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Upstream {
    pub id: String,
    pub service_id: String,
    pub name: String,
    pub algorithm: LoadBalanceAlgorithm,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct CreateUpstream {
    pub id: Option<String>,
    pub service_id: String,
    pub name: String,
    pub algorithm: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct UpdateUpstream {
    pub service_id: Option<String>,
    pub name: Option<String>,
    pub algorithm: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Target {
    pub id: String,
    pub upstream_id: String,
    pub url: String,
    pub weight: u32,
    pub healthy: bool,
    pub enabled: bool,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct CreateTarget {
    pub id: Option<String>,
    pub upstream_id: String,
    pub url: String,
    pub weight: Option<i64>,
    pub healthy: Option<bool>,
    pub enabled: Option<bool>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct UpdateTarget {
    pub upstream_id: Option<String>,
    pub url: Option<String>,
    pub weight: Option<i64>,
    pub healthy: Option<bool>,
    pub enabled: Option<bool>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Consumer {
    pub id: String,
    pub name: String,
    pub kind: String,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct CreateConsumer {
    pub id: Option<String>,
    pub name: String,
    pub kind: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct UpdateConsumer {
    pub name: Option<String>,
    pub kind: Option<String>,
}

/// Stored API credential metadata. Plaintext keys are never persisted.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ApiCredential {
    pub id: String,
    pub consumer_id: String,
    pub name: String,
    pub kind: String,
    pub key_prefix: Option<String>,
    pub key_hash: Option<String>,
    pub enabled: bool,
    pub expires_at: Option<DateTime<Utc>>,
    pub last_used_at: Option<DateTime<Utc>>,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct CreateApiCredential {
    pub id: Option<String>,
    pub consumer_id: String,
    pub name: Option<String>,
    pub kind: Option<String>,
    pub enabled: Option<bool>,
    pub expires_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct UpdateApiCredential {
    pub consumer_id: Option<String>,
    pub name: Option<String>,
    pub enabled: Option<bool>,
    /// Replaces the stored hash only when present.
    pub key_hash: Option<String>,
    /// Replaces the stored prefix only when present.
    pub key_prefix: Option<String>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        with = "double_option"
    )]
    pub expires_at: Option<Option<DateTime<Utc>>>,
}

mod double_option {
    use serde::{Deserialize, Deserializer, Serialize, Serializer};

    pub fn serialize<T, S>(value: &Option<Option<T>>, serializer: S) -> Result<S::Ok, S::Error>
    where
        T: Serialize,
        S: Serializer,
    {
        match value {
            Some(inner) => inner.serialize(serializer),
            None => serializer.serialize_none(),
        }
    }

    pub fn deserialize<'de, T, D>(deserializer: D) -> Result<Option<Option<T>>, D::Error>
    where
        T: Deserialize<'de>,
        D: Deserializer<'de>,
    {
        Option::<T>::deserialize(deserializer).map(Some)
    }
}

/// Parses an HTTP method name, returning the upper-case form.
pub fn parse_method(value: &str) -> Option<String> {
    let upper = value.trim().to_ascii_uppercase();
    METHODS.contains(&upper.as_str()).then_some(upper)
}

fn invalid(message: impl Into<String>) -> Error {
    Error::Config(message.into())
}

fn validate_weight(weight: i64) -> Result<(), Error> {
    if (MIN_TARGET_WEIGHT as i64..=MAX_TARGET_WEIGHT as i64).contains(&weight) {
        Ok(())
    } else {
        Err(invalid(format!(
            "target weight must be between {MIN_TARGET_WEIGHT} and {MAX_TARGET_WEIGHT}, got {weight}"
        )))
    }
}

fn validate_methods(methods: &[String]) -> Result<(), Error> {
    for method in methods {
        if parse_method(method).is_none() {
            return Err(invalid(format!("invalid HTTP method '{method}'")));
        }
    }
    Ok(())
}

fn validate_name(name: &str) -> Result<(), Error> {
    if name.trim().is_empty() {
        Err(invalid("name must not be empty"))
    } else {
        Ok(())
    }
}

fn validate_timeouts(
    connect_timeout_ms: Option<i64>,
    request_timeout_ms: Option<i64>,
) -> Result<(), Error> {
    if connect_timeout_ms.is_some_and(|ms| ms <= 0) {
        return Err(invalid("connect_timeout_ms must be positive"));
    }
    if request_timeout_ms.is_some_and(|ms| ms <= 0) {
        return Err(invalid("request_timeout_ms must be positive"));
    }
    Ok(())
}

fn validate_algorithm(algorithm: Option<&String>) -> Result<(), Error> {
    if let Some(algorithm) = algorithm
        && LoadBalanceAlgorithm::parse(algorithm).is_none()
    {
        return Err(invalid(format!(
            "unknown load balance algorithm '{algorithm}'"
        )));
    }
    Ok(())
}

impl CreateService {
    pub fn validate(&self) -> Result<(), Error> {
        validate_name(&self.name)?;
        if self
            .protocol
            .as_deref()
            .is_some_and(|p| p.trim().is_empty())
        {
            return Err(invalid("protocol must not be empty"));
        }
        validate_timeouts(self.connect_timeout_ms, self.request_timeout_ms)?;
        if self.retries.is_some_and(|retries| retries < 0) {
            return Err(invalid("retries must not be negative"));
        }
        Ok(())
    }
}

impl UpdateService {
    pub fn validate(&self) -> Result<(), Error> {
        if let Some(name) = &self.name {
            validate_name(name)?;
        }
        validate_timeouts(self.connect_timeout_ms, self.request_timeout_ms)?;
        if self.retries.is_some_and(|retries| retries < 0) {
            return Err(invalid("retries must not be negative"));
        }
        Ok(())
    }
}

impl CreateRoute {
    pub fn validate(&self) -> Result<(), Error> {
        if self.service_id.trim().is_empty() {
            return Err(invalid("service_id must not be empty"));
        }
        if self
            .path_prefix
            .as_deref()
            .is_some_and(|p| !p.starts_with('/'))
        {
            return Err(invalid("path_prefix must start with '/'"));
        }
        if let Some(methods) = &self.methods {
            validate_methods(methods)?;
        }
        Ok(())
    }
}

impl UpdateRoute {
    pub fn validate(&self) -> Result<(), Error> {
        if self
            .path_prefix
            .as_deref()
            .is_some_and(|p| !p.starts_with('/'))
        {
            return Err(invalid("path_prefix must start with '/'"));
        }
        if let Some(methods) = &self.methods {
            validate_methods(methods)?;
        }
        Ok(())
    }
}

impl CreateUpstream {
    pub fn validate(&self) -> Result<(), Error> {
        if self.service_id.trim().is_empty() {
            return Err(invalid("service_id must not be empty"));
        }
        validate_name(&self.name)?;
        validate_algorithm(self.algorithm.as_ref())
    }
}

impl UpdateUpstream {
    pub fn validate(&self) -> Result<(), Error> {
        if let Some(name) = &self.name {
            validate_name(name)?;
        }
        validate_algorithm(self.algorithm.as_ref())
    }
}

impl CreateTarget {
    pub fn validate(&self) -> Result<(), Error> {
        if self.upstream_id.trim().is_empty() {
            return Err(invalid("upstream_id must not be empty"));
        }
        validate_target_url(&self.url)?;
        if let Some(weight) = self.weight {
            validate_weight(weight)?;
        }
        Ok(())
    }
}

impl UpdateTarget {
    pub fn validate(&self) -> Result<(), Error> {
        if let Some(url) = &self.url {
            validate_target_url(url)?;
        }
        if let Some(weight) = self.weight {
            validate_weight(weight)?;
        }
        Ok(())
    }
}

impl CreateConsumer {
    pub fn validate(&self) -> Result<(), Error> {
        validate_name(&self.name)
    }
}

impl UpdateConsumer {
    pub fn validate(&self) -> Result<(), Error> {
        if let Some(name) = &self.name {
            validate_name(name)?;
        }
        Ok(())
    }
}

impl CreateApiCredential {
    pub fn validate(&self) -> Result<(), Error> {
        if self.consumer_id.trim().is_empty() {
            return Err(invalid("consumer_id must not be empty"));
        }
        Ok(())
    }
}

impl UpdateApiCredential {
    pub fn validate(&self) -> Result<(), Error> {
        if self
            .key_hash
            .as_deref()
            .is_some_and(|hash| hash.trim().is_empty())
        {
            return Err(invalid("key_hash must not be empty"));
        }
        Ok(())
    }
}

/// A target URL must be absolute, use http or https, and carry a host.
pub fn validate_target_url(url: &str) -> Result<(), Error> {
    let rest = url
        .strip_prefix("http://")
        .or_else(|| url.strip_prefix("https://"))
        .ok_or_else(|| invalid("target url must use the http or https scheme"))?;

    let authority = rest.split(['/', '?', '#']).next().unwrap_or_default();
    let host_port = authority
        .rsplit_once('@')
        .map_or(authority, |(_, host)| host);
    let host = host_port.split(':').next().unwrap_or_default();

    if host.is_empty() {
        return Err(invalid("target url must include a host"));
    }
    if url.chars().any(char::is_whitespace) {
        return Err(invalid("target url must not contain whitespace"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn target_url_validation() {
        assert!(validate_target_url("http://127.0.0.1:9000").is_ok());
        assert!(validate_target_url("https://example.test/base").is_ok());
        assert!(validate_target_url("ftp://example.test").is_err());
        assert!(validate_target_url("http://").is_err());
        assert!(validate_target_url("/relative").is_err());
        assert!(validate_target_url("http://user:secret@127.0.0.1:9000").is_ok());
    }

    #[test]
    fn weight_bounds() {
        let mut target = CreateTarget {
            upstream_id: "u".into(),
            url: "http://a.test".into(),
            weight: Some(0),
            ..Default::default()
        };
        assert!(target.validate().is_err());
        target.weight = Some(1001);
        assert!(target.validate().is_err());
        target.weight = Some(1000);
        assert!(target.validate().is_ok());
    }

    #[test]
    fn method_validation() {
        assert_eq!(parse_method("get").as_deref(), Some("GET"));
        assert!(parse_method("fetch").is_none());
        let route = CreateRoute {
            service_id: "s".into(),
            methods: Some(vec!["NOPE".into()]),
            ..Default::default()
        };
        assert!(route.validate().is_err());
    }

    #[test]
    fn payloads_round_trip_through_json() {
        let route = CreateRoute {
            service_id: "s".into(),
            path_prefix: Some("/api".into()),
            strip_prefix: Some(true),
            ..Default::default()
        };
        let raw = serde_json::to_string(&route).unwrap();
        let back: CreateRoute = serde_json::from_str(&raw).unwrap();
        assert_eq!(route, back);

        let update: UpdateRoute = serde_json::from_str(r#"{"host": null}"#).unwrap();
        assert_eq!(update.host, Some(None));
    }
}
