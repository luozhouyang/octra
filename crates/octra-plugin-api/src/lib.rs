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
}

/// Plugin metadata stored in the catalog and carried by every instance.
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
}

fn default_timeout_ms() -> u64 {
    250
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

pub enum PluginAction {
    Continue,
    ShortCircuit(Response),
}

pub type PluginResult = Result<PluginAction, PluginError>;

pub struct RequestContext<'a> {
    pub request_id: &'a str,
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
    pub route: &'a Route,
    pub service: &'a Service,
    pub target_url: &'a str,
}

pub struct ErrorContext<'a> {
    pub request_id: &'a str,
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

/// API Gateway plugin stages.
#[async_trait]
pub trait ApiGatewayPlugin: Send + Sync {
    fn manifest(&self) -> &PluginManifest;

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

/// Creates configured plugin instances. A factory validates the JSON config
/// once at chain-compile time, so the hot path never parses configuration.
pub trait ApiGatewayPluginFactory: Send + Sync {
    fn manifest(&self) -> PluginManifest;

    fn create(&self, config: Value) -> Result<Arc<dyn ApiGatewayPlugin>, PluginError>;

    fn validate(&self, config: &Value) -> Result<(), PluginError> {
        self.create(config.clone()).map(|_| ())
    }
}
