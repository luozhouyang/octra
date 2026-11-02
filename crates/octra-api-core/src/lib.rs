//! Domain types for the Octra API gateway.
//!
//! These types describe services, routes, upstreams, targets, consumers,
//! plugins, and authentication credentials. They are shared by the storage
//! layer, the compiled runtime snapshot, and the control plane payloads.

use std::collections::BTreeMap;

use chrono::{DateTime, Utc};
use octra_core::Error;
use serde::{Deserialize, Serialize};

pub const MIN_TARGET_WEIGHT: u32 = 0;
pub const MAX_TARGET_WEIGHT: u32 = 1000;

pub const METHODS: [&str; 9] = [
    "GET", "HEAD", "POST", "PUT", "PATCH", "DELETE", "OPTIONS", "CONNECT", "TRACE",
];

const PROTOCOLS: [&str; 10] = [
    "http", "https", "grpc", "grpcs", "ws", "wss", "tcp", "tls", "udp", "udt",
];

const HASH_ON_VALUES: [&str; 8] = [
    "none",
    "consumer",
    "ip",
    "header",
    "cookie",
    "path",
    "query_arg",
    "uri_capture",
];

const JWT_ALGORITHMS: [&str; 6] = ["HS256", "HS384", "HS512", "RS256", "RS384", "RS512"];

// ---------------------------------------------------------------------------
// Service
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Service {
    pub id: String,
    pub name: String,
    pub protocol: String,
    pub host: String,
    pub port: i64,
    pub path: Option<String>,
    pub retries: i64,
    pub connect_timeout: i64,
    pub write_timeout: i64,
    pub read_timeout: i64,
    pub enabled: bool,
    pub description: String,
    pub created_at: i64,
    pub updated_at: i64,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct CreateService {
    pub id: Option<String>,
    pub name: String,
    pub protocol: Option<String>,
    pub host: Option<String>,
    pub port: Option<i64>,
    pub path: Option<String>,
    pub retries: Option<i64>,
    pub connect_timeout: Option<i64>,
    pub write_timeout: Option<i64>,
    pub read_timeout: Option<i64>,
    pub enabled: Option<bool>,
    pub description: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct UpdateService {
    pub name: Option<String>,
    pub protocol: Option<String>,
    pub host: Option<String>,
    pub port: Option<i64>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        with = "double_option"
    )]
    pub path: Option<Option<String>>,
    pub retries: Option<i64>,
    pub connect_timeout: Option<i64>,
    pub write_timeout: Option<i64>,
    pub read_timeout: Option<i64>,
    pub enabled: Option<bool>,
    pub description: Option<String>,
}

// ---------------------------------------------------------------------------
// Route
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Route {
    pub id: String,
    pub name: Option<String>,
    pub protocols: Vec<String>,
    pub methods: Vec<String>,
    pub hosts: Vec<String>,
    pub paths: Vec<String>,
    pub headers: BTreeMap<String, Vec<String>>,
    pub strip_path: bool,
    pub preserve_host: bool,
    pub request_buffering: bool,
    pub response_buffering: bool,
    pub regex_priority: i64,
    pub service_id: Option<String>,
    pub enabled: bool,
    pub tags: Vec<String>,
    pub created_at: i64,
    pub updated_at: i64,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct CreateRoute {
    pub id: Option<String>,
    pub name: Option<String>,
    pub protocols: Option<Vec<String>>,
    pub methods: Option<Vec<String>>,
    pub hosts: Option<Vec<String>>,
    pub paths: Option<Vec<String>>,
    pub headers: Option<BTreeMap<String, Vec<String>>>,
    pub strip_path: Option<bool>,
    pub preserve_host: Option<bool>,
    pub request_buffering: Option<bool>,
    pub response_buffering: Option<bool>,
    pub regex_priority: Option<i64>,
    pub service_id: Option<String>,
    pub enabled: Option<bool>,
    pub tags: Option<Vec<String>>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct UpdateRoute {
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        with = "double_option"
    )]
    pub name: Option<Option<String>>,
    pub protocols: Option<Vec<String>>,
    pub methods: Option<Vec<String>>,
    pub hosts: Option<Vec<String>>,
    pub paths: Option<Vec<String>>,
    pub headers: Option<BTreeMap<String, Vec<String>>>,
    pub strip_path: Option<bool>,
    pub preserve_host: Option<bool>,
    pub request_buffering: Option<bool>,
    pub response_buffering: Option<bool>,
    pub regex_priority: Option<i64>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        with = "double_option"
    )]
    pub service_id: Option<Option<String>>,
    pub enabled: Option<bool>,
    pub tags: Option<Vec<String>>,
}

// ---------------------------------------------------------------------------
// Upstream
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
pub enum LoadBalanceAlgorithm {
    #[default]
    #[serde(rename = "round-robin")]
    RoundRobin,
    #[serde(rename = "consistent-hashing")]
    ConsistentHashing,
    #[serde(rename = "least-connections")]
    LeastConnections,
    #[serde(rename = "latency")]
    Latency,
    #[serde(rename = "weighted-round-robin")]
    WeightedRoundRobin,
}

impl LoadBalanceAlgorithm {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::RoundRobin => "round-robin",
            Self::ConsistentHashing => "consistent-hashing",
            Self::LeastConnections => "least-connections",
            Self::Latency => "latency",
            Self::WeightedRoundRobin => "weighted-round-robin",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value.replace('_', "-").as_str() {
            "round-robin" => Some(Self::RoundRobin),
            "consistent-hashing" => Some(Self::ConsistentHashing),
            "least-connections" => Some(Self::LeastConnections),
            "latency" => Some(Self::Latency),
            "weighted-round-robin" => Some(Self::WeightedRoundRobin),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Upstream {
    pub id: String,
    pub name: String,
    pub algorithm: LoadBalanceAlgorithm,
    pub hash_on: String,
    pub hash_fallback: String,
    pub slots: i64,
    pub healthchecks: serde_json::Value,
    pub host_header: Option<String>,
    pub tags: Vec<String>,
    pub created_at: i64,
    pub updated_at: i64,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct CreateUpstream {
    pub id: Option<String>,
    pub name: String,
    pub algorithm: Option<String>,
    pub hash_on: Option<String>,
    pub hash_fallback: Option<String>,
    pub slots: Option<i64>,
    pub healthchecks: Option<serde_json::Value>,
    pub host_header: Option<String>,
    pub tags: Option<Vec<String>>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct UpdateUpstream {
    pub name: Option<String>,
    pub algorithm: Option<String>,
    pub hash_on: Option<String>,
    pub hash_fallback: Option<String>,
    pub slots: Option<i64>,
    pub healthchecks: Option<serde_json::Value>,
    pub host_header: Option<String>,
    pub tags: Option<Vec<String>>,
}

// ---------------------------------------------------------------------------
// Target
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Target {
    pub id: String,
    pub upstream_id: String,
    /// `host:port`, without a scheme.
    pub target: String,
    pub weight: u32,
    pub tags: Vec<String>,
    pub created_at: i64,
    pub updated_at: i64,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct CreateTarget {
    pub id: Option<String>,
    pub upstream_id: String,
    pub target: String,
    pub weight: Option<i64>,
    pub tags: Option<Vec<String>>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct UpdateTarget {
    pub upstream_id: Option<String>,
    pub target: Option<String>,
    pub weight: Option<i64>,
    pub tags: Option<Vec<String>>,
}

// ---------------------------------------------------------------------------
// Consumer
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Consumer {
    pub id: String,
    pub username: Option<String>,
    pub custom_id: Option<String>,
    pub tags: Vec<String>,
    pub created_at: i64,
    pub updated_at: i64,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct CreateConsumer {
    pub id: Option<String>,
    pub username: Option<String>,
    pub custom_id: Option<String>,
    pub tags: Option<Vec<String>>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct UpdateConsumer {
    pub username: Option<String>,
    pub custom_id: Option<String>,
    pub tags: Option<Vec<String>>,
}

/// A policy document used by the LLM gateway for model/routing authorization.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Policy {
    pub id: String,
    pub name: String,
    pub scope: String,
    pub consumer_id: Option<String>,
    pub document: serde_json::Value,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct CreatePolicy {
    pub id: Option<String>,
    pub name: String,
    pub scope: Option<String>,
    pub consumer_id: Option<String>,
    pub document: Option<serde_json::Value>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct UpdatePolicy {
    pub name: Option<String>,
    pub scope: Option<String>,
    pub consumer_id: Option<String>,
    pub document: Option<serde_json::Value>,
}

// ---------------------------------------------------------------------------
// Plugin instances
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct PluginInstance {
    pub id: String,
    pub name: String,
    pub instance_name: Option<String>,
    pub config: serde_json::Value,
    pub enabled: bool,
    pub protocols: Vec<String>,
    pub ordering: Option<serde_json::Value>,
    pub service_id: Option<String>,
    pub route_id: Option<String>,
    pub consumer_id: Option<String>,
    pub tags: Vec<String>,
    pub created_at: i64,
    pub updated_at: i64,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct CreatePluginInstance {
    pub id: Option<String>,
    pub name: String,
    pub instance_name: Option<String>,
    pub config: Option<serde_json::Value>,
    pub enabled: Option<bool>,
    pub protocols: Option<Vec<String>>,
    pub ordering: Option<serde_json::Value>,
    pub service_id: Option<String>,
    pub route_id: Option<String>,
    pub consumer_id: Option<String>,
    pub tags: Option<Vec<String>>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct UpdatePluginInstance {
    pub name: Option<String>,
    pub instance_name: Option<String>,
    pub config: Option<serde_json::Value>,
    pub enabled: Option<bool>,
    pub protocols: Option<Vec<String>>,
    pub ordering: Option<serde_json::Value>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        with = "double_option"
    )]
    pub service_id: Option<Option<String>>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        with = "double_option"
    )]
    pub route_id: Option<Option<String>>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        with = "double_option"
    )]
    pub consumer_id: Option<Option<String>>,
    pub tags: Option<Vec<String>>,
}

// ---------------------------------------------------------------------------
// Credentials
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct KeyauthCredential {
    pub id: String,
    pub consumer_id: String,
    pub key: String,
    pub ttl: Option<i64>,
    pub tags: Vec<String>,
    pub created_at: i64,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct CreateKeyauthCredential {
    pub id: Option<String>,
    pub consumer_id: String,
    pub key: Option<String>,
    pub ttl: Option<i64>,
    pub tags: Option<Vec<String>>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct UpdateKeyauthCredential {
    pub key: Option<String>,
    pub ttl: Option<i64>,
    pub tags: Option<Vec<String>>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct BasicauthCredential {
    pub id: String,
    pub consumer_id: String,
    pub username: String,
    pub password: String,
    pub tags: Vec<String>,
    pub created_at: i64,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct CreateBasicauthCredential {
    pub id: Option<String>,
    pub consumer_id: String,
    pub username: String,
    pub password: String,
    pub tags: Option<Vec<String>>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct UpdateBasicauthCredential {
    pub username: Option<String>,
    pub password: Option<String>,
    pub tags: Option<Vec<String>>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct JwtSecret {
    pub id: String,
    pub consumer_id: String,
    pub key: String,
    pub secret: String,
    pub algorithm: String,
    pub rsa_public_key: Option<String>,
    pub tags: Vec<String>,
    pub created_at: i64,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct CreateJwtSecret {
    pub id: Option<String>,
    pub consumer_id: String,
    pub key: String,
    pub secret: String,
    pub algorithm: Option<String>,
    pub rsa_public_key: Option<String>,
    pub tags: Option<Vec<String>>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct UpdateJwtSecret {
    pub key: Option<String>,
    pub secret: Option<String>,
    pub algorithm: Option<String>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        with = "double_option"
    )]
    pub rsa_public_key: Option<Option<String>>,
    pub tags: Option<Vec<String>>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Acl {
    pub id: String,
    pub consumer_id: String,
    pub group: String,
    pub tags: Vec<String>,
    pub created_at: i64,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct CreateAcl {
    pub id: Option<String>,
    pub consumer_id: String,
    pub group: String,
    pub tags: Option<Vec<String>>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct UpdateAcl {
    pub group: Option<String>,
    pub tags: Option<Vec<String>>,
}

// ---------------------------------------------------------------------------
// Validation
// ---------------------------------------------------------------------------

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

fn validate_protocols(protocols: &[String]) -> Result<(), Error> {
    for protocol in protocols {
        let value = protocol.trim().to_ascii_lowercase();
        if !PROTOCOLS.contains(&value.as_str()) {
            return Err(invalid(format!("invalid protocol '{protocol}'")));
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

fn validate_optional_name(name: Option<&String>, field: &str) -> Result<(), Error> {
    if let Some(name) = name
        && name.trim().is_empty()
    {
        return Err(invalid(format!("{field} must not be empty")));
    }
    Ok(())
}

fn validate_port(port: i64) -> Result<(), Error> {
    if (1..=65535).contains(&port) {
        Ok(())
    } else {
        Err(invalid(format!(
            "port must be between 1 and 65535, got {port}"
        )))
    }
}

fn validate_timeouts(
    connect_timeout: Option<i64>,
    write_timeout: Option<i64>,
    read_timeout: Option<i64>,
) -> Result<(), Error> {
    for (field, value) in [
        ("connect_timeout", connect_timeout),
        ("write_timeout", write_timeout),
        ("read_timeout", read_timeout),
    ] {
        if value.is_some_and(|ms| ms <= 0) {
            return Err(invalid(format!("{field} must be positive")));
        }
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

fn validate_hash_on(value: Option<&String>, field: &str) -> Result<(), Error> {
    if let Some(value) = value
        && !HASH_ON_VALUES.contains(&value.as_str())
    {
        return Err(invalid(format!("invalid {field} '{value}'")));
    }
    Ok(())
}

/// A target is `host:port`, optionally with an IPv6 host in brackets.
pub fn validate_target(target: &str) -> Result<(), Error> {
    let target = target.trim();
    if target.is_empty() {
        return Err(invalid("target must not be empty"));
    }
    if target.chars().any(char::is_whitespace) {
        return Err(invalid("target must not contain whitespace"));
    }
    if target.contains("://") {
        return Err(invalid("target must not include a scheme"));
    }

    let (host, port) = if let Some(rest) = target.strip_prefix('[') {
        let (host, tail) = rest
            .split_once(']')
            .ok_or_else(|| invalid("target has an invalid IPv6 host"))?;
        let port = tail
            .strip_prefix(':')
            .ok_or_else(|| invalid("target must include a port"))?;
        (host, port)
    } else {
        target
            .rsplit_once(':')
            .ok_or_else(|| invalid("target must be host:port"))?
    };

    if host.is_empty() {
        return Err(invalid("target must include a host"));
    }
    let port = port
        .parse::<u16>()
        .map_err(|_| invalid("target port must be a number between 1 and 65535"))?;
    if port == 0 {
        return Err(invalid("target port must be between 1 and 65535"));
    }
    Ok(())
}

impl CreateService {
    pub fn validate(&self) -> Result<(), Error> {
        validate_name(&self.name)?;
        if let Some(host) = &self.host {
            validate_name(host)?;
        }
        if let Some(protocol) = &self.protocol {
            validate_protocols(std::slice::from_ref(protocol))?;
        }
        if let Some(port) = self.port {
            validate_port(port)?;
        }
        validate_timeouts(self.connect_timeout, self.write_timeout, self.read_timeout)?;
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
        if let Some(host) = &self.host {
            validate_name(host)?;
        }
        if let Some(protocol) = &self.protocol {
            validate_protocols(std::slice::from_ref(protocol))?;
        }
        if let Some(port) = self.port {
            validate_port(port)?;
        }
        validate_timeouts(self.connect_timeout, self.write_timeout, self.read_timeout)?;
        if self.retries.is_some_and(|retries| retries < 0) {
            return Err(invalid("retries must not be negative"));
        }
        Ok(())
    }
}

impl CreateRoute {
    pub fn validate(&self) -> Result<(), Error> {
        validate_optional_name(self.name.as_ref(), "name")?;
        if let Some(protocols) = &self.protocols {
            validate_protocols(protocols)?;
        }
        if let Some(methods) = &self.methods {
            validate_methods(methods)?;
        }
        if let Some(paths) = &self.paths {
            for path in paths {
                let path = path.trim();
                if !path.starts_with('/') && !path.starts_with('~') {
                    return Err(invalid("paths must start with '/' or '~'"));
                }
            }
        }
        if let Some(service_id) = &self.service_id
            && service_id.trim().is_empty()
        {
            return Err(invalid("service_id must not be empty"));
        }
        Ok(())
    }
}

impl UpdateRoute {
    pub fn validate(&self) -> Result<(), Error> {
        if let Some(name) = &self.name {
            validate_optional_name(name.as_ref(), "name")?;
        }
        if let Some(protocols) = &self.protocols {
            validate_protocols(protocols)?;
        }
        if let Some(methods) = &self.methods {
            validate_methods(methods)?;
        }
        if let Some(paths) = &self.paths {
            for path in paths {
                let path = path.trim();
                if !path.starts_with('/') && !path.starts_with('~') {
                    return Err(invalid("paths must start with '/' or '~'"));
                }
            }
        }
        Ok(())
    }
}

impl CreateUpstream {
    pub fn validate(&self) -> Result<(), Error> {
        validate_name(&self.name)?;
        validate_algorithm(self.algorithm.as_ref())?;
        validate_hash_on(self.hash_on.as_ref(), "hash_on")?;
        validate_hash_on(self.hash_fallback.as_ref(), "hash_fallback")?;
        if self.slots.is_some_and(|slots| slots <= 0) {
            return Err(invalid("slots must be positive"));
        }
        Ok(())
    }
}

impl UpdateUpstream {
    pub fn validate(&self) -> Result<(), Error> {
        if let Some(name) = &self.name {
            validate_name(name)?;
        }
        validate_algorithm(self.algorithm.as_ref())?;
        validate_hash_on(self.hash_on.as_ref(), "hash_on")?;
        validate_hash_on(self.hash_fallback.as_ref(), "hash_fallback")?;
        if self.slots.is_some_and(|slots| slots <= 0) {
            return Err(invalid("slots must be positive"));
        }
        Ok(())
    }
}

impl CreateTarget {
    pub fn validate(&self) -> Result<(), Error> {
        if self.upstream_id.trim().is_empty() {
            return Err(invalid("upstream_id must not be empty"));
        }
        validate_target(&self.target)?;
        if let Some(weight) = self.weight {
            validate_weight(weight)?;
        }
        Ok(())
    }
}

impl UpdateTarget {
    pub fn validate(&self) -> Result<(), Error> {
        if let Some(target) = &self.target {
            validate_target(target)?;
        }
        if let Some(weight) = self.weight {
            validate_weight(weight)?;
        }
        Ok(())
    }
}

impl CreateConsumer {
    pub fn validate(&self) -> Result<(), Error> {
        validate_identity(self.username.as_ref(), self.custom_id.as_ref())
    }
}

impl UpdateConsumer {
    pub fn validate(&self) -> Result<(), Error> {
        validate_optional_name(self.username.as_ref(), "username")?;
        validate_optional_name(self.custom_id.as_ref(), "custom_id")
    }
}

fn validate_identity(username: Option<&String>, custom_id: Option<&String>) -> Result<(), Error> {
    validate_optional_name(username, "username")?;
    validate_optional_name(custom_id, "custom_id")?;
    let has_username = username.is_some_and(|value| !value.trim().is_empty());
    let has_custom_id = custom_id.is_some_and(|value| !value.trim().is_empty());
    if !has_username && !has_custom_id {
        return Err(invalid("consumer requires username or custom_id"));
    }
    Ok(())
}

impl CreatePluginInstance {
    pub fn validate(&self) -> Result<(), Error> {
        validate_name(&self.name)?;
        validate_optional_name(self.instance_name.as_ref(), "instance_name")?;
        if let Some(protocols) = &self.protocols {
            validate_protocols(protocols)?;
        }
        if self.service_id.as_deref().is_some_and(str::is_empty)
            || self.route_id.as_deref().is_some_and(str::is_empty)
            || self.consumer_id.as_deref().is_some_and(str::is_empty)
        {
            return Err(invalid("plugin scope ids must not be empty"));
        }
        Ok(())
    }
}

impl UpdatePluginInstance {
    pub fn validate(&self) -> Result<(), Error> {
        if let Some(name) = &self.name {
            validate_name(name)?;
        }
        if let Some(protocols) = &self.protocols {
            validate_protocols(protocols)?;
        }
        Ok(())
    }
}

impl CreateKeyauthCredential {
    pub fn validate(&self) -> Result<(), Error> {
        if self.consumer_id.trim().is_empty() {
            return Err(invalid("consumer_id must not be empty"));
        }
        if self.key.as_deref().is_some_and(|key| key.trim().is_empty()) {
            return Err(invalid("key must not be empty"));
        }
        Ok(())
    }
}

impl UpdateKeyauthCredential {
    pub fn validate(&self) -> Result<(), Error> {
        if self.key.as_deref().is_some_and(|key| key.trim().is_empty()) {
            return Err(invalid("key must not be empty"));
        }
        Ok(())
    }
}

impl CreateBasicauthCredential {
    pub fn validate(&self) -> Result<(), Error> {
        if self.consumer_id.trim().is_empty() {
            return Err(invalid("consumer_id must not be empty"));
        }
        validate_name(&self.username)?;
        validate_name(&self.password)
    }
}

impl UpdateBasicauthCredential {
    pub fn validate(&self) -> Result<(), Error> {
        if let Some(username) = &self.username {
            validate_name(username)?;
        }
        if let Some(password) = &self.password {
            validate_name(password)?;
        }
        Ok(())
    }
}

impl CreateJwtSecret {
    pub fn validate(&self) -> Result<(), Error> {
        if self.consumer_id.trim().is_empty() {
            return Err(invalid("consumer_id must not be empty"));
        }
        validate_name(&self.key)?;
        validate_name(&self.secret)?;
        validate_jwt_algorithm(self.algorithm.as_deref())
    }
}

impl UpdateJwtSecret {
    pub fn validate(&self) -> Result<(), Error> {
        if let Some(key) = &self.key {
            validate_name(key)?;
        }
        if let Some(secret) = &self.secret {
            validate_name(secret)?;
        }
        validate_jwt_algorithm(self.algorithm.as_deref())
    }
}

fn validate_jwt_algorithm(algorithm: Option<&str>) -> Result<(), Error> {
    if let Some(algorithm) = algorithm
        && !JWT_ALGORITHMS.contains(&algorithm)
    {
        return Err(invalid(format!("unsupported JWT algorithm '{algorithm}'")));
    }
    Ok(())
}

impl CreateAcl {
    pub fn validate(&self) -> Result<(), Error> {
        if self.consumer_id.trim().is_empty() {
            return Err(invalid("consumer_id must not be empty"));
        }
        validate_name(&self.group)
    }
}

impl UpdateAcl {
    pub fn validate(&self) -> Result<(), Error> {
        if let Some(group) = &self.group {
            validate_name(group)?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn target_validation() {
        assert!(validate_target("127.0.0.1:9000").is_ok());
        assert!(validate_target("[::1]:9000").is_ok());
        assert!(validate_target("example.test:80").is_ok());
        assert!(validate_target("ftp://example.test").is_err());
        assert!(validate_target("http://example.test").is_err());
        assert!(validate_target("example.test").is_err());
        assert!(validate_target("example.test:0").is_err());
    }

    #[test]
    fn weight_bounds() {
        let mut target = CreateTarget {
            upstream_id: "u".into(),
            target: "a.test:80".into(),
            weight: Some(0),
            ..Default::default()
        };
        assert!(target.validate().is_ok());
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
            methods: Some(vec!["NOPE".into()]),
            ..Default::default()
        };
        assert!(route.validate().is_err());
    }

    #[test]
    fn payloads_round_trip_through_json() {
        let route = CreateRoute {
            service_id: Some("s".into()),
            paths: Some(vec!["/api".into()]),
            strip_path: Some(true),
            ..Default::default()
        };
        let raw = serde_json::to_string(&route).unwrap();
        let back: CreateRoute = serde_json::from_str(&raw).unwrap();
        assert_eq!(route, back);

        let update: UpdateRoute = serde_json::from_str(r#"{"name": null}"#).unwrap();
        assert_eq!(update.name, Some(None));
    }
}
