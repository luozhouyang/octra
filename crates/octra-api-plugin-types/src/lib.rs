//! Plugin contracts shared by the Octra gateway runtime and built-in plugins.
//!
//! Plugins are configured per scope and compiled into an ordered chain by the
//! runtime. Stages receive narrow, typed contexts so a plugin can only mutate
//! the part of the request it is invoked for.

use std::fmt;
use std::net::IpAddr;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use axum::response::Response;
use bytes::Bytes;
use http::{HeaderMap, Method, StatusCode, Uri};
use octra_api_core::{PluginInstance, Route, Service};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use thiserror::Error;

mod services;

pub use services::{
    API_PLUGIN_API_VERSION, EventPublisher, EventSubscriber, GatewayEvent, JobInvocation,
    MAX_JOB_NAME_LEN, MAX_SCHEDULED_JOBS, MAX_SUBSCRIPTIONS, MAX_TOPIC_LEN, PluginServiceSource,
    ScheduledJob, ScheduledJobSpec, SubscriptionSpec, next_run_after, next_run_from_now,
    parse_schedule, topic_matches, validate_job_name, validate_topic_pattern,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PluginKind {
    Api,
    Llm,
    Mcp,
}

impl PluginKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Api => "api",
            Self::Llm => "llm",
            Self::Mcp => "mcp",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "api" => Some(Self::Api),
            "llm" => Some(Self::Llm),
            "mcp" => Some(Self::Mcp),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PluginRuntimeKind {
    Builtin,
    Wasm,
}

impl PluginRuntimeKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Builtin => "builtin",
            Self::Wasm => "wasm",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "builtin" => Some(Self::Builtin),
            "wasm" => Some(Self::Wasm),
            _ => None,
        }
    }
}

/// Configuration scope. Consumer overrides route, route overrides service,
/// and service overrides global for the same plugin.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Scope {
    Global,
    Service,
    Route,
    Consumer,
}

impl Scope {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Global => "global",
            Self::Service => "service",
            Self::Route => "route",
            Self::Consumer => "consumer",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "global" => Some(Self::Global),
            "service" => Some(Self::Service),
            "route" => Some(Self::Route),
            "consumer" => Some(Self::Consumer),
            _ => None,
        }
    }

    /// Global configs use an empty scope id so the database can enforce
    /// uniqueness with a normal unique index.
    pub fn normalize_id(self, scope_id: Option<&str>) -> Option<String> {
        match self {
            Self::Global => Some(String::new()),
            _ => scope_id
                .map(str::trim)
                .filter(|id| !id.is_empty())
                .map(str::to_string),
        }
    }
}

impl fmt::Display for Scope {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Capability {
    RequestHeaders,
    ResponseHeaders,
    RequestBody,
    ResponseBody,
    Log,
    KeyValue,
    Network,
    Secret,
    StorageEntities,
    Auth,
    AdminApi,
    Events,
    Jobs,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LogLevel {
    Trace,
    Debug,
    Info,
    Warn,
    Error,
}

/// Plugin metadata stored in the catalog and carried by every instance.

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PluginPhase {
    Certificate,
    Rewrite,
    Request,
    UpstreamRequest,
    Response,
    ResponseBody,
    Error,
    Log,
}

#[derive(Debug, Clone, Default)]
pub struct ConsumerInfo {
    pub id: String,
    pub username: Option<String>,
    pub custom_id: Option<String>,
    pub acl_groups: Vec<String>,
    pub group_ids: Vec<String>,
    pub groups: Vec<String>,
}

#[derive(Debug, Clone, Default)]
pub struct CredentialInfo {
    pub id: String,
    pub kind: String,
    pub identifier: String,
}

#[derive(Debug, Clone, Default)]
pub struct AuthKey {
    pub credential_id: String,
    pub consumer_id: String,
}

#[derive(Debug, Clone, Default)]
pub struct AuthBasic {
    pub credential_id: String,
    pub consumer_id: String,
    pub username: String,
    pub password: String,
}

#[derive(Debug, Clone, Default)]
pub struct AuthJwt {
    pub credential_id: String,
    pub consumer_id: String,
    pub key: String,
    pub secret: String,
    pub algorithm: String,
    pub rsa_public_key: Option<String>,
}

#[derive(Debug, Default, Clone)]
pub struct GatewayState {
    pub consumer: Option<ConsumerInfo>,
    pub credential: Option<CredentialInfo>,
    pub anonymous: bool,
    pub values: std::collections::BTreeMap<String, Value>,
    pub request_body: Option<bytes::Bytes>,
    pub method_override: Option<http::Method>,
    pub consumers: std::collections::HashMap<String, ConsumerInfo>,
    pub consumer_by_username: std::collections::HashMap<String, String>,
    pub consumer_by_custom_id: std::collections::HashMap<String, String>,
    pub keyauth: std::collections::HashMap<String, AuthKey>,
    pub basicauth: std::collections::HashMap<String, AuthBasic>,
    pub jwt: std::collections::HashMap<String, AuthJwt>,
}

impl GatewayState {
    pub fn slot(&mut self, plugin: &str) -> &mut Value {
        self.values.entry(plugin.to_string()).or_insert(Value::Null)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PluginManifest {
    pub name: String,
    pub version: String,
    pub kind: PluginKind,
    pub runtime: PluginRuntimeKind,
    pub priority: i32,
    pub description: String,
    #[serde(default = "default_timeout_ms")]
    pub timeout_ms: u64,
    #[serde(default)]
    pub fail_policy: FailPolicy,
    /// Phases the plugin participates in. Empty means the WASM default
    /// (request, response, log); built-in factories declare their own set.
    #[serde(default)]
    pub phases: Vec<PluginPhase>,
    #[serde(default)]
    pub capabilities: Vec<Capability>,
    #[serde(default)]
    pub config_schema: Option<Value>,
    #[serde(default = "default_api_version")]
    pub api_version: String,
    #[serde(default)]
    pub entities: Vec<EntitySpec>,
    #[serde(default)]
    pub migrations: Vec<MigrationSpec>,
    /// Custom Admin API routes served by the plugin. Paths are relative to
    /// `/admin/plugins/{name}/api` and require the `admin_api` capability.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub admin_routes: Vec<AdminRouteSpec>,
    /// Event topics the plugin consumes. Requires the `events` capability.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub subscriptions: Vec<SubscriptionSpec>,
    /// Cron jobs the host runs for the plugin. Requires the `jobs` capability.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub jobs: Vec<ScheduledJobSpec>,
}

fn default_timeout_ms() -> u64 {
    250
}

fn default_api_version() -> String {
    API_PLUGIN_API_VERSION.to_string()
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MigrationSpec {
    pub version: String,
    pub file: String,
    #[serde(default)]
    pub destructive: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AdminRouteSpec {
    pub method: String,
    pub path: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub summary: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

/// Maximum number of custom Admin API routes a plugin may declare.
pub const MAX_ADMIN_ROUTES: usize = 32;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AdminPathSegment {
    Literal(String),
    Param(String),
}

/// Parses a manifest-declared Admin API path into literal and `{param}`
/// segments. Paths are relative to the plugin's Admin API mount.
pub fn parse_admin_route(path: &str) -> Result<Vec<AdminPathSegment>, String> {
    if !path.starts_with('/') {
        return Err("admin route path must start with '/'".to_string());
    }
    if path == "/" {
        return Ok(Vec::new());
    }
    if path.ends_with('/') {
        return Err("admin route path must not end with '/'".to_string());
    }

    let mut segments = Vec::new();
    let mut params = std::collections::HashSet::new();
    for raw in path[1..].split('/') {
        if raw.is_empty() {
            return Err(format!("admin route path '{path}' has an empty segment"));
        }
        if raw == "." || raw == ".." {
            return Err(format!(
                "admin route path '{path}' contains a relative segment"
            ));
        }
        if raw.contains('*') {
            return Err(format!(
                "admin route path '{path}' does not support wildcards"
            ));
        }
        if raw.starts_with('{') || raw.ends_with('}') {
            let name = raw
                .strip_prefix('{')
                .and_then(|value| value.strip_suffix('}'))
                .ok_or_else(|| format!("admin route path '{path}' has a malformed parameter"))?;
            if !is_param_name(name) {
                return Err(format!(
                    "admin route path '{path}' has an invalid parameter name"
                ));
            }
            if !params.insert(name.to_string()) {
                return Err(format!(
                    "admin route path '{path}' repeats parameter '{name}'"
                ));
            }
            segments.push(AdminPathSegment::Param(name.to_string()));
        } else {
            if raw.contains('{') || raw.contains('}') {
                return Err(format!(
                    "admin route path '{path}' has a malformed parameter"
                ));
            }
            segments.push(AdminPathSegment::Literal(raw.to_string()));
        }
    }
    Ok(segments)
}

/// Canonical form used to detect duplicate route declarations; parameters
/// normalize to `{}`.
pub fn canonical_admin_path(segments: &[AdminPathSegment]) -> String {
    if segments.is_empty() {
        return "/".to_string();
    }
    let parts: Vec<&str> = segments
        .iter()
        .map(|segment| match segment {
            AdminPathSegment::Literal(value) => value.as_str(),
            AdminPathSegment::Param(_) => "{}",
        })
        .collect();
    format!("/{}", parts.join("/"))
}

fn is_param_name(name: &str) -> bool {
    let mut chars = name.chars();
    match chars.next() {
        Some(first) if first.is_ascii_alphabetic() || first == '_' => {}
        _ => return false,
    }
    chars.all(|value| value.is_ascii_alphanumeric() || value == '_')
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AdminRouteMatch {
    Found {
        index: usize,
        params: Vec<(String, String)>,
    },
    MethodNotAllowed,
    NotFound,
}

/// Matches a request against declared Admin API routes. Paths are compared
/// exactly and case-sensitively; methods case-insensitively. When several
/// declarations match a path, the one with more literal segments wins, with
/// declaration order as the tie-breaker.
pub fn match_admin_route(routes: &[AdminRouteSpec], method: &str, path: &str) -> AdminRouteMatch {
    struct Candidate {
        index: usize,
        literals: usize,
        params: Vec<(String, String)>,
    }

    let request_segments = split_request_path(path);
    let mut best: Option<Candidate> = None;
    let mut path_matched = false;

    for (index, route) in routes.iter().enumerate() {
        let Ok(pattern) = parse_admin_route(&route.path) else {
            continue;
        };
        if pattern.len() != request_segments.len() {
            continue;
        }
        let mut params = Vec::new();
        let mut literals = 0;
        let mut matched = true;
        for (segment, value) in pattern.iter().zip(&request_segments) {
            match segment {
                AdminPathSegment::Literal(expected) => {
                    if expected != value {
                        matched = false;
                        break;
                    }
                    literals += 1;
                }
                AdminPathSegment::Param(name) => params.push((name.clone(), value.clone())),
            }
        }
        if !matched {
            continue;
        }
        path_matched = true;
        if !route.method.eq_ignore_ascii_case(method) {
            continue;
        }
        if best
            .as_ref()
            .is_none_or(|current| literals > current.literals)
        {
            best = Some(Candidate {
                index,
                literals,
                params,
            });
        }
    }

    match best {
        Some(candidate) => AdminRouteMatch::Found {
            index: candidate.index,
            params: candidate.params,
        },
        None if path_matched => AdminRouteMatch::MethodNotAllowed,
        None => AdminRouteMatch::NotFound,
    }
}

fn split_request_path(path: &str) -> Vec<String> {
    if path.is_empty() || path == "/" {
        return Vec::new();
    }
    path.trim_start_matches('/')
        .split('/')
        .filter(|segment| !segment.is_empty())
        .map(str::to_string)
        .collect()
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EntitySpec {
    pub name: String,
    pub primary_key: Vec<String>,
    pub fields: Vec<EntityFieldSpec>,
    #[serde(default)]
    pub unique: Vec<Vec<String>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EntityFieldSpec {
    pub name: String,
    #[serde(rename = "type")]
    pub field_type: String,
    #[serde(default)]
    pub nullable: bool,
}

impl PluginManifest {
    pub fn timeout(&self) -> Duration {
        Duration::from_millis(self.timeout_ms.max(1))
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FailPolicy {
    #[default]
    Open,
    Closed,
}

#[derive(Debug, Error)]
#[error("{message}")]
pub struct PluginError {
    pub message: String,
    pub fail_policy: FailPolicy,
}

impl PluginError {
    pub fn fail_open(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            fail_policy: FailPolicy::Open,
        }
    }

    pub fn fail_closed(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            fail_policy: FailPolicy::Closed,
        }
    }
}

#[derive(Debug, Error)]
pub enum StorageError {
    #[error("plugin storage is unavailable")]
    Unavailable,
    #[error("entity '{0}' is not declared by this plugin")]
    NotDeclared(String),
    #[error("invalid storage request: {0}")]
    InvalidRequest(String),
    #[error("storage backend error: {0}")]
    Backend(String),
}

pub enum PluginAction {
    Continue,
    ShortCircuit(Response),
}

pub type PluginResult = Result<PluginAction, PluginError>;

pub struct RequestContext<'a> {
    pub request_id: &'a str,
    pub state: &'a mut GatewayState,
    pub matched_path: &'a str,
    pub method: &'a Method,
    pub uri: &'a mut Uri,
    pub headers: &'a mut HeaderMap,
    pub client_ip: Option<IpAddr>,
    pub consumer_id: Option<&'a str>,
    pub credential_id: Option<&'a str>,
    pub route: &'a Route,
    pub service: &'a Service,
}

pub struct UpstreamRequestContext<'a> {
    pub request_id: &'a str,
    pub state: &'a mut GatewayState,
    pub method: &'a Method,
    pub uri: &'a mut Uri,
    pub headers: &'a mut HeaderMap,
    pub consumer_id: Option<&'a str>,
    pub route: &'a Route,
    pub service: &'a Service,
    pub target_url: &'a str,
}

pub struct ResponseContext<'a> {
    pub request_id: &'a str,
    pub state: &'a mut GatewayState,
    pub request_headers: &'a HeaderMap,
    pub status: &'a mut StatusCode,
    pub headers: &'a mut HeaderMap,
    pub consumer_id: Option<&'a str>,
    pub route: &'a Route,
    pub service: &'a Service,
    pub target_url: &'a str,
}

pub struct BodyContext<'a> {
    pub request_id: &'a str,
    pub state: &'a mut GatewayState,
    pub route: &'a Route,
    pub service: &'a Service,
    pub target_url: &'a str,
}

pub struct ErrorContext<'a> {
    pub request_id: &'a str,
    pub state: &'a mut GatewayState,
    pub method: &'a Method,
    pub uri: &'a Uri,
    pub headers: &'a HeaderMap,
    pub consumer_id: Option<&'a str>,
    pub route: &'a Route,
    pub service: &'a Service,
    pub target_url: &'a str,
    pub error: &'a str,
}

pub struct LogContext<'a> {
    pub request_id: &'a str,
    pub state: &'a mut GatewayState,
    pub method: &'a Method,
    pub path: &'a str,
    pub status: u16,
    pub latency: Duration,
    pub consumer_id: Option<&'a str>,
    pub route: &'a Route,
    pub service: &'a Service,
    pub target_url: Option<&'a str>,
    pub error: Option<&'a str>,
}

pub struct RequestBodyContext<'a> {
    pub request_id: &'a str,
    pub state: &'a mut GatewayState,
    pub method: &'a Method,
    pub uri: &'a mut Uri,
    pub headers: &'a mut HeaderMap,
    pub consumer_id: Option<&'a str>,
    pub route: &'a Route,
    pub service: &'a Service,
}

pub struct ResponseBodyContext<'a> {
    pub request_id: &'a str,
    pub state: &'a mut GatewayState,
    pub status: StatusCode,
    pub headers: &'a HeaderMap,
    pub consumer_id: Option<&'a str>,
    pub route: &'a Route,
    pub service: &'a Service,
    pub target_url: &'a str,
}

/// API Gateway plugin stages.
#[async_trait]
pub trait ApiGatewayPlugin: Send + Sync {
    fn manifest(&self) -> &PluginManifest;

    /// Whether this instance needs the full request body buffered before the
    /// request stage. The default is false so header-only plugins (including
    /// key-auth with `key_in_body = false`) do not defeat request streaming.
    fn needs_request_body(&self) -> bool {
        false
    }

    /// Whether this instance needs the full response body buffered before the
    /// response-body stage. Header-only response plugins return false.
    fn needs_response_body(&self) -> bool {
        false
    }

    /// Whether the plugin can consume `on_response_body` chunks and therefore
    /// does not need the full body buffered. Plugins that opt in must preserve
    /// chunk-boundary-independent semantics or maintain their own state.
    fn streams_response_body(&self) -> bool {
        false
    }

    async fn on_rewrite(&self, _cx: &mut RequestContext<'_>) -> PluginResult {
        Ok(PluginAction::Continue)
    }

    async fn on_request_buffer(
        &self,
        _cx: &mut RequestBodyContext<'_>,
        body: Bytes,
    ) -> Result<Bytes, PluginError> {
        Ok(body)
    }

    async fn on_response_buffer(
        &self,
        _cx: &mut ResponseBodyContext<'_>,
        body: Bytes,
    ) -> Result<Bytes, PluginError> {
        Ok(body)
    }

    async fn on_request(&self, _cx: &mut RequestContext<'_>) -> PluginResult {
        Ok(PluginAction::Continue)
    }

    async fn on_upstream_request(&self, _cx: &mut UpstreamRequestContext<'_>) -> PluginResult {
        Ok(PluginAction::Continue)
    }

    async fn on_response(&self, _cx: &mut ResponseContext<'_>) -> PluginResult {
        Ok(PluginAction::Continue)
    }

    async fn on_response_body(
        &self,
        _cx: &BodyContext<'_>,
        chunk: Bytes,
    ) -> Result<Bytes, PluginError> {
        Ok(chunk)
    }

    async fn on_error(&self, _cx: &mut ErrorContext<'_>) -> PluginResult {
        Ok(PluginAction::Continue)
    }

    async fn on_log(&self, _cx: &LogContext<'_>) {}

    /// Handles an event this plugin subscribed to. Errors are retried by the
    /// event bus according to its retry policy.
    async fn on_event(&self, _event: &GatewayEvent) -> Result<(), PluginError> {
        Ok(())
    }

    /// Runs one scheduled job. Errors are recorded on the job row; the next
    /// cron occurrence still runs.
    async fn on_job(&self, _job: &JobInvocation) -> Result<(), PluginError> {
        Ok(())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EntityQuery {
    #[serde(default)]
    pub filters: Vec<EntityFilter>,
    #[serde(default)]
    pub order_by: Vec<EntityOrder>,
    #[serde(default)]
    pub limit: Option<u32>,
    #[serde(default)]
    pub offset: Option<u32>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EntityFilter {
    pub field: String,
    pub op: FilterOp,
    pub value: Value,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FilterOp {
    Eq,
    Ne,
    Lt,
    Lte,
    Gt,
    Gte,
    In,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EntityOrder {
    pub field: String,
    #[serde(default)]
    pub descending: bool,
}

/// Host-side view of one configured plugin instance. Built-in factories ignore
/// it; WASM factories use it for storage, logging, and capability checks.
pub trait PluginHost: Send + Sync {
    fn plugin_name(&self) -> &str;
    fn plugin_instance_id(&self) -> &str;
    fn capabilities(&self) -> &[Capability];
    fn storage(&self) -> &dyn PluginStorage;
    fn log(&self, level: LogLevel, message: &str);
}

/// Narrow, synchronous storage API exposed to plugins. Entity operations only
/// accept names declared by the plugin manifest; the host generates SQL.
pub trait PluginStorage: Send + Sync {
    fn kv_get(&self, key: &str) -> Result<Option<Vec<u8>>, StorageError>;
    fn kv_put(&self, key: &str, value: &[u8], ttl: Option<Duration>) -> Result<(), StorageError>;
    fn kv_delete(&self, key: &str) -> Result<bool, StorageError>;
    fn kv_incr(&self, key: &str, delta: i64, ttl: Option<Duration>) -> Result<i64, StorageError>;

    fn get(&self, entity: &str, key: &Value) -> Result<Option<Value>, StorageError>;
    fn upsert(&self, entity: &str, values: &Value) -> Result<(), StorageError>;
    fn increment(
        &self,
        entity: &str,
        key: &Value,
        field: &str,
        delta: i64,
        extra: Option<&Value>,
    ) -> Result<i64, StorageError>;
    fn delete(&self, entity: &str, key: &Value) -> Result<bool, StorageError>;
    fn query(&self, entity: &str, query: &EntityQuery) -> Result<Vec<Value>, StorageError>;
}

/// Creates configured plugin instances. A factory validates the JSON config
/// once at chain-compile time, so the hot path never parses configuration.
pub trait ApiGatewayPluginFactory: Send + Sync {
    fn manifest(&self) -> PluginManifest;

    fn create(&self, id: &str, config: Value) -> Result<Arc<dyn ApiGatewayPlugin>, PluginError>;

    fn create_with_host(
        &self,
        host: Arc<dyn PluginHost>,
        id: &str,
        config: Value,
    ) -> Result<Arc<dyn ApiGatewayPlugin>, PluginError> {
        let _ = host;
        self.create(id, config)
    }

    fn phases(&self) -> Vec<PluginPhase> {
        vec![PluginPhase::Request, PluginPhase::Log]
    }

    fn validate(&self, config: &Value) -> Result<(), PluginError> {
        self.create("validation", config.clone()).map(|_| ())
    }
}

/// Resolves plugin factories from one or more catalogs. The daemon uses a
/// composite resolver (built-ins plus installed WASM artifacts); tests and
/// early boot can use a registry directly.
pub trait PluginFactoryResolver: Send + Sync {
    fn factory(&self, name: &str) -> Option<Arc<dyn ApiGatewayPluginFactory>>;
    fn manifests(&self) -> Vec<PluginManifest>;
}

/// Builds the host handle passed to a factory for one configured instance.
pub trait PluginHostProvider: Send + Sync {
    fn host_for(&self, instance: &PluginInstance) -> Arc<dyn PluginHost>;
}

#[cfg(test)]
mod tests {
    use super::*;

    fn route(method: &str, path: &str) -> AdminRouteSpec {
        AdminRouteSpec {
            method: method.to_string(),
            path: path.to_string(),
            summary: None,
            description: None,
        }
    }

    #[test]
    fn parses_admin_route_paths() {
        assert_eq!(
            parse_admin_route("/status").unwrap(),
            vec![AdminPathSegment::Literal("status".to_string())]
        );
        assert_eq!(
            parse_admin_route("/consumers/{consumer}/usage").unwrap(),
            vec![
                AdminPathSegment::Literal("consumers".to_string()),
                AdminPathSegment::Param("consumer".to_string()),
                AdminPathSegment::Literal("usage".to_string()),
            ]
        );
        assert!(parse_admin_route("/").unwrap().is_empty());
    }

    #[test]
    fn rejects_invalid_admin_route_paths() {
        for path in [
            "",
            "status",
            "/status/",
            "/a//b",
            "/a/../b",
            "/a/.",
            "/a/*",
            "/a/{1bad}",
            "/a/{bad-name}",
            "/a/{x}/{x}",
            "/a/{x",
            "/a/x}",
        ] {
            assert!(
                parse_admin_route(path).is_err(),
                "expected '{path}' to fail"
            );
        }
    }

    #[test]
    fn canonicalizes_parameter_segments() {
        let segments = parse_admin_route("/consumers/{consumer}/usage").unwrap();
        assert_eq!(canonical_admin_path(&segments), "/consumers/{}/usage");
        assert_eq!(canonical_admin_path(&[]), "/");
    }

    #[test]
    fn matches_literal_and_parameter_routes() {
        let routes = vec![
            route("GET", "/status"),
            route("GET", "/consumers/{consumer}/usage"),
        ];
        assert_eq!(
            match_admin_route(&routes, "GET", "/status"),
            AdminRouteMatch::Found {
                index: 0,
                params: Vec::new()
            }
        );
        assert_eq!(
            match_admin_route(&routes, "get", "/consumers/c-1/usage"),
            AdminRouteMatch::Found {
                index: 1,
                params: vec![("consumer".to_string(), "c-1".to_string())]
            }
        );
        assert_eq!(
            match_admin_route(&routes, "DELETE", "/status"),
            AdminRouteMatch::MethodNotAllowed
        );
        assert_eq!(
            match_admin_route(&routes, "GET", "/unknown"),
            AdminRouteMatch::NotFound
        );
    }

    #[test]
    fn prefers_literal_segments_over_parameters() {
        let routes = vec![route("GET", "/{id}"), route("GET", "/status")];
        assert_eq!(
            match_admin_route(&routes, "GET", "/status"),
            AdminRouteMatch::Found {
                index: 1,
                params: Vec::new()
            }
        );
        assert_eq!(
            match_admin_route(&routes, "GET", "/other"),
            AdminRouteMatch::Found {
                index: 0,
                params: vec![("id".to_string(), "other".to_string())]
            }
        );
    }

    #[test]
    fn prefers_a_route_with_the_matching_method() {
        let routes = vec![route("GET", "/items"), route("POST", "/items")];
        assert_eq!(
            match_admin_route(&routes, "POST", "/items"),
            AdminRouteMatch::Found {
                index: 1,
                params: Vec::new()
            }
        );
    }
}
