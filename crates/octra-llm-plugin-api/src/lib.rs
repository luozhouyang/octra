//! LLM plugin contracts.
//!
//! LLM requests move through a typed lifecycle rather than HTTP headers and
//! bytes: decode, route, call the provider, then either return a body or
//! stream canonical events. Each stage receives only the fields it may
//! mutate, so a plugin cannot accidentally rewrite protocol state outside its
//! stage.

use std::fmt;

use async_trait::async_trait;
use octra_llm_core::{ChatRequest, ChatResponse, GatewayError, Protocol, StreamEvent, Usage};
use octra_plugin_api::{PluginError, PluginManifest};
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Where an LLM plugin configuration applies. Precedence runs from the most
/// specific scope to the least; the first config for a plugin name wins.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LlmScope {
    Global,
    Consumer,
    VirtualKey,
    Model,
    Provider,
}

impl LlmScope {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Global => "global",
            Self::Consumer => "consumer",
            Self::VirtualKey => "virtual_key",
            Self::Model => "model",
            Self::Provider => "provider",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "global" => Some(Self::Global),
            "consumer" => Some(Self::Consumer),
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

pub enum LlmAction {
    Continue,
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

pub struct LlmRequestContext<'a> {
    pub request_id: &'a str,
    pub ingress: Protocol,
    pub consumer_id: Option<&'a str>,
    pub virtual_key_id: Option<&'a str>,
    pub request: &'a mut ChatRequest,
}

pub struct LlmProviderRequestContext<'a> {
    pub request_id: &'a str,
    pub target_model_id: &'a str,
    pub provider_id: &'a str,
    pub protocol: Protocol,
    pub account_id: Option<&'a str>,
    pub request: &'a mut ChatRequest,
}

pub struct LlmResponseContext<'a> {
    pub request_id: &'a str,
    pub ingress: Protocol,
    pub response: &'a mut ChatResponse,
    pub usage: &'a mut Usage,
}

pub struct LlmStreamContext<'a> {
    pub request_id: &'a str,
    pub ingress: Protocol,
    pub event: &'a mut StreamEvent,
    /// Per-request, per-plugin state so a plugin can buffer across chunks.
    pub state: &'a mut Value,
}

pub struct LlmErrorContext<'a> {
    pub request_id: &'a str,
    pub ingress: Protocol,
    pub error: &'a mut GatewayError,
}

pub struct LlmLogContext<'a> {
    pub request_id: &'a str,
    pub ingress: Protocol,
    pub consumer_id: Option<&'a str>,
    pub virtual_key_id: Option<&'a str>,
    pub model: &'a str,
    pub status: u16,
    pub latency_ms: u64,
    pub usage: &'a Usage,
    pub error: Option<&'a str>,
}

/// LLM plugin stages. Every stage defaults to continuing unchanged.
#[async_trait]
pub trait LlmPlugin: Send + Sync {
    fn manifest(&self) -> &PluginManifest;

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

    async fn on_error(&self, _cx: &mut LlmErrorContext<'_>) -> LlmPluginResult {
        Ok(LlmAction::Continue)
    }

    async fn on_log(&self, _cx: &LlmLogContext<'_>) {}
}

/// Creates configured LLM plugin instances. Configuration is validated once
/// when chains compile, never on the request path.
pub trait LlmPluginFactory: Send + Sync {
    fn manifest(&self) -> PluginManifest;

    fn create(&self, config: Value) -> Result<std::sync::Arc<dyn LlmPlugin>, PluginError>;

    fn validate(&self, config: &Value) -> Result<(), PluginError> {
        self.create(config.clone()).map(|_| ())
    }
}
