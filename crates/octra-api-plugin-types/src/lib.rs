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
use octra_api_core::{Route, Service};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use thiserror::Error;

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

#[derive(Debug, Default)]
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
}

fn default_timeout_ms() -> u64 {
    250
}

fn default_api_version() -> String {
    "1".to_string()
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MigrationSpec {
    pub version: String,
    pub file: String,
    #[serde(default)]
    pub destructive: bool,
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

    fn auth(&self) -> bool {
        false
    }

    fn validate(&self, config: &Value) -> Result<(), PluginError> {
        self.create("validation", config.clone()).map(|_| ())
    }
}
