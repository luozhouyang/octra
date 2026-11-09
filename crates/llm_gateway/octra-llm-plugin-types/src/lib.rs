//! LLM plugin contracts.
//!
//! LLM requests move through a typed lifecycle rather than HTTP headers and
//! bytes: decode, route, call the provider, then either return a body or
//! stream canonical events. Each stage receives only the fields it may
//! mutate, so a plugin cannot accidentally rewrite protocol state outside its
//! stage.

use std::fmt;
use std::net::IpAddr;

use async_trait::async_trait;
use http::header::{HeaderName, HeaderValue};
use octra_api_plugin_types::{PluginError, PluginManifest};
use octra_llm_core::{ChatRequest, ChatResponse, GatewayError, Protocol, StreamEvent, Usage};
use serde::{Deserialize, Serialize};
use serde_json::Value;

mod counters;

pub use counters::{
    BudgetWindowView, CounterOutcome, CounterWindow, LlmCounterStore, MemoryCounterStore,
};

/// Where an LLM plugin configuration applies. Precedence runs from the most
/// specific scope to the least; the first config for a plugin name wins.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LlmScope {
    Global,
    Consumer,
    ConsumerGroup,
    VirtualKey,
    Model,
    Provider,
}

impl LlmScope {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Global => "global",
            Self::Consumer => "consumer",
            Self::ConsumerGroup => "consumer_group",
            Self::VirtualKey => "virtual_key",
            Self::Model => "model",
            Self::Provider => "provider",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "global" => Some(Self::Global),
            "consumer" => Some(Self::Consumer),
            "consumer_group" | "consumer-group" => Some(Self::ConsumerGroup),
            "virtual_key" | "virtual-key" => Some(Self::VirtualKey),
            "model" => Some(Self::Model),
            "provider" => Some(Self::Provider),
            _ => None,
        }
    }

    /// Global configs use an empty scope id so the storage unique index works.
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

impl fmt::Display for LlmScope {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// Why a plugin produced a short-circuit response.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LlmResponseSource {
    /// Served from a response cache; usage is not billed.
    Cache,
    /// Produced by a plugin without an upstream call.
    Synthetic,
    /// Produced to terminate the request early, for example a policy denial.
    Termination,
}

impl LlmResponseSource {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Cache => "cache",
            Self::Synthetic => "synthetic",
            Self::Termination => "termination",
        }
    }
}

pub enum LlmAction {
    Continue,
    /// Short-circuits the request stage with an already built response.
    ///
    /// Only the request stage honours this action. Other stages use it for
    /// nothing and fail closed when a plugin returns it there, because they
    /// cannot replace the response they already received.
    Respond {
        response: Box<ChatResponse>,
        source: LlmResponseSource,
    },
    Reject(GatewayError),
}

pub type LlmPluginResult = Result<LlmAction, PluginError>;

/// A stream stage may split, drop or reorder events; the chain feeds the new
/// list through the remaining plugins.
pub enum LlmStreamAction {
    Continue,
    Replace(Vec<StreamEvent>),
    Reject(GatewayError),
}

pub type LlmStreamResult = Result<LlmStreamAction, PluginError>;

/// Contract version a factory must implement to be registered. Bumped when a
/// context or trait signature changes incompatibly.
pub const LLM_PLUGIN_API_VERSION: u32 = 7;

const MAX_RESPONSE_HEADERS: usize = 128;
const MAX_HEADER_VALUE_BYTES: usize = 8 * 1024;

/// Headers a request- or response-stage plugin may add to the eventual HTTP
/// response.
///
/// The host owns the sink and attaches it after the data plane has produced a
/// response, so it works for JSON bodies, plugin rejections and SSE streams.
/// Streaming headers are committed before the stream starts; a streaming
/// plugin must add them at the request stage.
/// Names and values are validated here; hop-by-hop, body-framing and
/// host-reserved headers are rejected.
#[derive(Debug, Default)]
pub struct LlmResponseHeaders {
    entries: Vec<(HeaderName, HeaderValue)>,
}

impl LlmResponseHeaders {
    pub fn new() -> Self {
        Self::default()
    }

    /// Validates and inserts a header. Returns false for illegal names or
    /// values and for headers the host reserves for itself.
    pub fn insert(&mut self, name: &str, value: &str) -> bool {
        if self.entries.len() >= MAX_RESPONSE_HEADERS || value.len() > MAX_HEADER_VALUE_BYTES {
            tracing::warn!(
                header = name,
                "dropping LLM response header over the host limit"
            );
            return false;
        }
        let Ok(header_name) = HeaderName::from_bytes(name.as_bytes()) else {
            tracing::warn!(
                header = name,
                "dropping LLM response header with an invalid name"
            );
            return false;
        };
        if is_reserved_response_header(&header_name) {
            tracing::warn!(header = name, "dropping host-reserved LLM response header");
            return false;
        }
        let Ok(header_value) = HeaderValue::from_str(value) else {
            tracing::warn!(
                header = name,
                "dropping LLM response header with an invalid value"
            );
            return false;
        };
        self.entries
            .retain(|(existing, _)| existing.as_str() != header_name.as_str());
        self.entries.push((header_name, header_value));
        true
    }

    pub fn iter(&self) -> impl Iterator<Item = (&HeaderName, &HeaderValue)> {
        self.entries.iter().map(|(name, value)| (name, value))
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

fn is_reserved_response_header(name: &HeaderName) -> bool {
    let name = name.as_str();
    matches!(
        name,
        "connection"
            | "content-length"
            | "content-type"
            | "host"
            | "keep-alive"
            | "set-cookie"
            | "te"
            | "trailer"
            | "transfer-encoding"
            | "upgrade"
            | "www-authenticate"
            | "x-accel-buffering"
            | "x-request-id"
    ) || name.starts_with("proxy-")
}

/// Read-only request metadata shared by every stage. Headers other than the
/// user agent are not exposed, so credentials and cookies cannot leak into a
/// plugin.
pub struct LlmRequestMeta<'a> {
    pub request_id: &'a str,
    pub ingress: Protocol,
    pub consumer_id: Option<&'a str>,
    pub virtual_key_id: Option<&'a str>,
    /// Stable identity shared by all keys in one rotation family.
    pub virtual_key_family: Option<&'a str>,
    /// Consumer groups the authenticated consumer belongs to.
    pub consumer_group_ids: &'a [String],
    pub session_id: Option<&'a str>,
    pub client_ip: Option<IpAddr>,
    pub user_agent: Option<&'a str>,
    /// Client intent hint from the configured header, if any.
    pub intent: Option<&'a str>,
}

/// Per-request, per-plugin state. Plugins receive their own slot, so one
/// request can buffer data across stages without touching shared instances.
#[derive(Debug, Default)]
pub struct LlmRequestState {
    values: std::collections::BTreeMap<String, Value>,
}

impl LlmRequestState {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn slot(&mut self, plugin: &str) -> &mut Value {
        self.values.entry(plugin.to_string()).or_insert(Value::Null)
    }

    pub fn is_empty(&self) -> bool {
        self.values.is_empty()
    }
}

/// Provider-specific metadata available once a routing attempt is bound.
pub struct LlmProviderInfo<'a> {
    pub attempt: u32,
    pub target_model_id: &'a str,
    pub provider_id: &'a str,
    pub provider_name: &'a str,
    pub protocol: Protocol,
    pub account_id: Option<&'a str>,
    pub account_name: Option<&'a str>,
}

pub struct LlmRequestContext<'a> {
    pub meta: &'a LlmRequestMeta<'a>,
    /// Headers to attach to the final response. Shared by the whole request
    /// chain and owned by the data plane.
    pub response_headers: &'a mut LlmResponseHeaders,
    pub request: &'a mut ChatRequest,
    pub state: &'a mut Value,
}

pub struct LlmProviderRequestContext<'a> {
    pub meta: &'a LlmRequestMeta<'a>,
    pub provider: &'a LlmProviderInfo<'a>,
    pub request: &'a mut ChatRequest,
    pub state: &'a mut Value,
}

pub struct LlmResponseContext<'a> {
    pub meta: &'a LlmRequestMeta<'a>,
    pub request_model: &'a str,
    pub response: &'a mut ChatResponse,
    /// Headers to attach to the final response. Only non-streaming responses
    /// reach this stage; streaming headers are committed by `on_request`.
    pub response_headers: &'a mut LlmResponseHeaders,
    pub state: &'a mut Value,
}

/// Read-only view of the response that is about to be encoded for the client.
///
/// The final-response hook runs after every response-stage transform and
/// before protocol encoding, so a plugin observes exactly the canonical
/// response the client will receive. It is best-effort and cannot mutate the
/// response; it exists for observation, logging and cache population.
pub struct LlmFinalResponseContext<'a> {
    pub meta: &'a LlmRequestMeta<'a>,
    pub request_model: &'a str,
    /// Read-only: this hook runs after every response transform.
    pub response: &'a ChatResponse,
    pub state: &'a mut Value,
}

pub struct LlmStreamContext<'a> {
    pub meta: &'a LlmRequestMeta<'a>,
    pub event: &'a mut StreamEvent,
    pub state: &'a mut Value,
}

/// Stream completion, after the upstream body ends and before the terminal
/// events are emitted. The hook may append non-terminal events to flush
/// buffered content; it cannot change the status code.
pub struct LlmStreamEndContext<'a> {
    pub meta: &'a LlmRequestMeta<'a>,
    /// Accumulated canonical response before the hook chain starts.
    pub response: &'a ChatResponse,
    /// Events appended by this stage. Earlier plugins' appends are visible
    /// here. Terminal events are rejected by the host.
    pub events: &'a mut Vec<StreamEvent>,
    pub state: &'a mut Value,
}

pub enum LlmStreamEndAction {
    Continue,
    Append(Vec<StreamEvent>),
}

pub type LlmStreamEndResult = Result<LlmStreamEndAction, PluginError>;

pub struct LlmErrorContext<'a> {
    pub meta: &'a LlmRequestMeta<'a>,
    pub provider_id: Option<&'a str>,
    pub account_id: Option<&'a str>,
    pub error: &'a mut GatewayError,
    pub state: &'a mut Value,
}

/// Content-free observation data; never carries prompt or response text.
pub struct LlmLogEvent<'a> {
    /// Effective model used for routing, after request-stage rewrites.
    pub model: &'a str,
    /// Client-facing model before rewrites, when it differs from `model`.
    pub requested_model: Option<&'a str>,
    pub provider_id: Option<&'a str>,
    pub provider_name: Option<&'a str>,
    pub account_id: Option<&'a str>,
    pub account_name: Option<&'a str>,
    pub status: u16,
    pub latency_ms: u64,
    pub usage: &'a Usage,
    pub finish_reason: Option<&'a str>,
    pub streaming: bool,
    pub error: Option<&'a str>,
    /// Cache outcome for this request, for example `"hit"`; `None` when the
    /// request was not served from a cache.
    pub cache_status: Option<&'a str>,
    /// Provenance of a short-circuit response: `"cache"`, `"synthetic"` or
    /// `"termination"`; `None` for upstream responses.
    pub response_source: Option<&'a str>,
    /// Host-computed cost in micro-USD. `None` when pricing is unavailable or
    /// the request never produced a complete billable response.
    pub cost_micros: Option<u64>,
}

pub struct LlmLogContext<'a> {
    pub meta: &'a LlmRequestMeta<'a>,
    pub event: &'a LlmLogEvent<'a>,
    /// Read-only: `on_log` is observational and host mutations are ignored.
    pub state: &'a Value,
}

/// LLM plugin stages. Every stage defaults to continuing unchanged.
#[async_trait]
pub trait LlmPlugin: Send + Sync {
    fn manifest(&self) -> &PluginManifest;

    /// Whether response/stream output can contain values restored from
    /// sensitive upstream placeholders. Cache plugins must not persist such
    /// content. Irreversible redaction returns false.
    fn restores_sensitive_content(&self) -> bool {
        false
    }

    async fn on_request(&self, _cx: &mut LlmRequestContext<'_>) -> LlmPluginResult {
        Ok(LlmAction::Continue)
    }

    async fn on_provider_request(
        &self,
        _cx: &mut LlmProviderRequestContext<'_>,
    ) -> LlmPluginResult {
        Ok(LlmAction::Continue)
    }

    async fn on_response(&self, _cx: &mut LlmResponseContext<'_>) -> LlmPluginResult {
        Ok(LlmAction::Continue)
    }

    async fn on_stream_event(&self, _cx: &mut LlmStreamContext<'_>) -> LlmStreamResult {
        Ok(LlmStreamAction::Continue)
    }

    /// Appends non-terminal events before the terminal stream events are
    /// emitted. Best-effort: errors and timeouts are logged and ignored.
    async fn on_stream_end(&self, _cx: &mut LlmStreamEndContext<'_>) -> LlmStreamEndResult {
        Ok(LlmStreamEndAction::Continue)
    }

    async fn on_error(&self, _cx: &mut LlmErrorContext<'_>) -> LlmPluginResult {
        Ok(LlmAction::Continue)
    }

    /// Observes the final canonical response after the response chain has run
    /// and before protocol encoding. The hook is best-effort: a failure or
    /// timeout is logged and never changes the response, and it cannot mutate
    /// the response it receives.
    async fn on_final_response(&self, _cx: &mut LlmFinalResponseContext<'_>) {}

    async fn on_log(&self, _cx: &LlmLogContext<'_>) {}
}

/// Host-provided plugin state shared across chain reloads and, when backed by
/// storage, across process restarts.
pub trait LlmPluginStateStore: Send + Sync {
    fn get(&self, key: &str) -> Option<Value>;
    fn put(&self, key: &str, value: Value);
}

/// Identity and host services available when one configured instance is
/// created. `instance_key` stays stable across reloads of the same config.
pub struct LlmPluginCreateContext<'a> {
    pub instance_key: &'a str,
    pub plugin_name: &'a str,
    pub scope: &'a str,
    pub scope_id: &'a str,
    pub state: std::sync::Arc<dyn LlmPluginStateStore>,
    /// Shared counters for rate limits and budget guards.
    pub counters: std::sync::Arc<dyn LlmCounterStore>,
}

/// Scopes every plugin supports unless it narrows the set. Request-stage
/// plugins should exclude `Provider`, because the request chain is built
/// before a provider is selected.
pub const ALL_LLM_SCOPES: [LlmScope; 6] = [
    LlmScope::Global,
    LlmScope::Consumer,
    LlmScope::ConsumerGroup,
    LlmScope::VirtualKey,
    LlmScope::Model,
    LlmScope::Provider,
];

/// Scopes for plugins that only participate in the client-facing request,
/// response and stream chain.
pub const REQUEST_LLM_SCOPES: [LlmScope; 5] = [
    LlmScope::Global,
    LlmScope::Consumer,
    LlmScope::ConsumerGroup,
    LlmScope::VirtualKey,
    LlmScope::Model,
];

/// Creates configured LLM plugin instances. Configuration is validated once
/// when chains compile, never on the request path.
pub trait LlmPluginFactory: Send + Sync {
    fn manifest(&self) -> PluginManifest;

    fn api_version(&self) -> u32 {
        LLM_PLUGIN_API_VERSION
    }

    /// Scopes this plugin can honour. The host rejects configurations outside
    /// this set instead of compiling a chain where the plugin silently never
    /// runs. Defaults to every scope.
    fn supported_scopes(&self) -> &'static [LlmScope] {
        &ALL_LLM_SCOPES
    }

    fn create(&self, config: Value) -> Result<std::sync::Arc<dyn LlmPlugin>, PluginError>;

    /// Preferred constructor: receives the stable instance identity and the
    /// host state store. Defaults to [`Self::create`] for stateless plugins.
    fn create_with(
        &self,
        config: Value,
        _context: &LlmPluginCreateContext<'_>,
    ) -> Result<std::sync::Arc<dyn LlmPlugin>, PluginError> {
        self.create(config)
    }

    fn validate(&self, config: &Value) -> Result<(), PluginError> {
        self.create(config.clone()).map(|_| ())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn response_headers_reject_reserved_and_invalid_names() {
        let mut headers = LlmResponseHeaders::default();
        assert!(headers.insert("X-RateLimit-Limit-Minute", "10"));
        assert!(headers.insert("Retry-After", "1"));
        assert!(!headers.insert("content-length", "10"));
        assert!(!headers.insert("transfer-encoding", "chunked"));
        assert!(!headers.insert("x-request-id", "nope"));
        assert!(!headers.insert("bad header", "value"));
        assert!(!headers.insert("X-Bad", "line\nbreak"));
        assert_eq!(headers.len(), 2);
    }

    #[test]
    fn response_headers_replace_duplicates() {
        let mut headers = LlmResponseHeaders::default();
        assert!(headers.insert("X-Test", "one"));
        assert!(headers.insert("x-test", "two"));
        assert_eq!(headers.len(), 1);
        let values: Vec<String> = headers
            .iter()
            .map(|(_, value)| value.to_str().unwrap().to_string())
            .collect();
        assert_eq!(values, vec!["two"]);
    }
}
