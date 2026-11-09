//! Content-free request audit records on the log stage.
//!
//! Records contain identity, routing and usage metadata only. Prompt text,
//! response text, tool arguments and headers are never copied.

use std::sync::Arc;

use async_trait::async_trait;
use octra_api_plugin_types::{PluginError, PluginManifest};
use octra_llm_core::{Usage, Value};
use octra_llm_plugin_types::{
    LlmLogContext, LlmLogEvent, LlmPlugin, LlmPluginFactory, LlmRequestMeta,
};
use serde::Deserialize;
use serde_json::json;

use crate::manifest;

const SCHEMA_VERSION: u32 = 1;

#[derive(Debug)]
pub struct RequestAudit {
    manifest: PluginManifest,
    include_usage: bool,
    include_error: bool,
    include_client: bool,
}

#[derive(Debug, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct Config {
    include_usage: bool,
    include_error: bool,
    include_client: bool,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            include_usage: true,
            include_error: true,
            include_client: false,
        }
    }
}

pub struct RequestAuditFactory;

impl LlmPluginFactory for RequestAuditFactory {
    fn manifest(&self) -> PluginManifest {
        let mut manifest = manifest(
            "llm-request-audit",
            "Emit content-free LLM request audit records",
        );
        manifest.timeout_ms = 50;
        manifest.capabilities = vec![octra_api_plugin_types::Capability::Log];
        manifest.config_schema = Some(json!({
            "$schema": "http://json-schema.org/draft-07/schema#",
            "title": "llm-request-audit config",
            "type": "object",
            "additionalProperties": false,
            "properties": {
                "include_usage": {
                    "type": "boolean",
                    "default": true
                },
                "include_error": {
                    "type": "boolean",
                    "default": true
                },
                "include_client": {
                    "type": "boolean",
                    "default": false
                }
            }
        }));
        manifest
    }

    fn create(&self, config: Value) -> Result<Arc<dyn LlmPlugin>, PluginError> {
        let config: Config = serde_json::from_value(config)
            .map_err(|error| PluginError::fail_closed(format!("invalid audit config: {error}")))?;
        Ok(Arc::new(RequestAudit {
            manifest: self.manifest(),
            include_usage: config.include_usage,
            include_error: config.include_error,
            include_client: config.include_client,
        }))
    }
}

#[async_trait]
impl LlmPlugin for RequestAudit {
    fn manifest(&self) -> &PluginManifest {
        &self.manifest
    }

    async fn on_log(&self, cx: &mut LlmLogContext<'_>) {
        let record = self.record(cx.meta, cx.event);
        tracing::info!(target: "octra.audit", audit = %record, "llm request audit");
    }
}

impl RequestAudit {
    fn record(&self, meta: &LlmRequestMeta<'_>, event: &LlmLogEvent<'_>) -> Value {
        let mut record = json!({
            "schema_version": SCHEMA_VERSION,
            "request_id": meta.request_id,
            "ingress": meta.ingress.as_str(),
            "consumer_id": meta.consumer_id,
            "virtual_key_id": meta.virtual_key_id,
            "session_id": meta.session_id,
            "intent": meta.intent,
            "model": event.model,
            "requested_model": event.requested_model,
            "provider_id": event.provider_id,
            "provider_name": event.provider_name,
            "account_id": event.account_id,
            "account_name": event.account_name,
            "status": event.status,
            "latency_ms": event.latency_ms,
            "streaming": event.streaming,
            "finish_reason": event.finish_reason,
            "cache_status": event.cache_status,
        });
        if self.include_client {
            record["client_ip"] = json!(meta.client_ip.map(|ip| ip.to_string()));
            record["user_agent"] = json!(meta.user_agent);
        }
        if self.include_usage {
            record["usage"] = usage_record(event.usage);
        }
        if self.include_error
            && let Some(error) = event.error
        {
            record["error"] = json!(error);
        }
        record
    }
}

fn usage_record(usage: &Usage) -> Value {
    json!({
        "input_tokens": usage.input_tokens,
        "output_tokens": usage.output_tokens,
        "cache_read_tokens": usage.cache_read_tokens,
        "cache_write_tokens": usage.cache_write_tokens,
        "reasoning_tokens": usage.reasoning_tokens,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use octra_llm_core::Protocol;
    use octra_llm_plugin_types::LlmRequestMeta;

    fn meta() -> LlmRequestMeta<'static> {
        LlmRequestMeta {
            request_id: "req-1",
            ingress: Protocol::Openai,
            consumer_id: Some("consumer-1"),
            virtual_key_id: Some("key-1"),
            virtual_key_family: None,
            consumer_group_ids: &[],
            session_id: None,
            client_ip: Some("127.0.0.1".parse().unwrap()),
            user_agent: Some("client/1.0"),
            intent: Some("coding"),
        }
    }

    fn event<'a>(usage: &'a Usage) -> LlmLogEvent<'a> {
        LlmLogEvent {
            model: "gpt-test",
            requested_model: Some("gpt-alias"),
            provider_id: Some("provider-1"),
            provider_name: Some("openai"),
            account_id: Some("account-1"),
            account_name: Some("primary"),
            status: 200,
            latency_ms: 42,
            usage,
            finish_reason: Some("stop"),
            streaming: true,
            error: Some("upstream hiccup"),
            cache_status: Some("hit"),
        }
    }

    #[test]
    fn record_has_metadata_but_no_content() {
        let usage = Usage {
            input_tokens: 10,
            output_tokens: 20,
            cache_read_tokens: Some(3),
            cache_write_tokens: Some(4),
            reasoning_tokens: Some(5),
            cache_read_included_in_input: false,
        };
        let plugin = RequestAuditFactory.create(json!({})).unwrap();
        let plugin = plugin.as_ref();
        let meta = meta();
        let event = event(&usage);
        let record = RequestAudit {
            manifest: manifest("llm-request-audit", ""),
            include_usage: true,
            include_error: true,
            include_client: false,
        }
        .record(&meta, &event);

        assert_eq!(record["schema_version"], SCHEMA_VERSION);
        assert_eq!(record["request_id"], "req-1");
        assert_eq!(record["requested_model"], "gpt-alias");
        assert_eq!(record["provider_id"], "provider-1");
        assert_eq!(record["account_name"], "primary");
        assert_eq!(record["streaming"], true);
        assert_eq!(record["intent"], "coding");
        assert_eq!(record["cache_status"], "hit");
        assert_eq!(record["usage"]["input_tokens"], 10);
        assert_eq!(record["usage"]["reasoning_tokens"], 5);
        assert_eq!(record["error"], "upstream hiccup");
        // Content and header-bearing fields are intentionally absent.
        for key in ["messages", "prompt", "content", "user_agent", "client_ip"] {
            assert!(record.get(key).is_none(), "unexpected field {key}");
        }
        assert!(plugin.manifest().name == "llm-request-audit");
    }

    #[test]
    fn optional_sections_can_be_disabled() {
        let usage = Usage::default();
        let plugin = RequestAudit {
            manifest: manifest("llm-request-audit", ""),
            include_usage: false,
            include_error: false,
            include_client: false,
        };
        let meta = meta();
        let event = event(&usage);
        let record = plugin.record(&meta, &event);
        assert!(record.get("usage").is_none());
        assert!(record.get("error").is_none());
    }

    #[test]
    fn cache_status_is_null_when_not_served_from_cache() {
        let plugin = RequestAudit {
            manifest: manifest("llm-request-audit", ""),
            include_usage: true,
            include_error: true,
            include_client: false,
        };
        let usage = Usage::default();
        let meta = meta();
        let mut event = event(&usage);
        event.cache_status = None;
        let record = plugin.record(&meta, &event);
        assert_eq!(record["cache_status"], Value::Null);
    }

    #[test]
    fn client_metadata_is_opt_in() {
        let plugin = RequestAudit {
            manifest: manifest("llm-request-audit", ""),
            include_usage: true,
            include_error: true,
            include_client: true,
        };
        let usage = Usage::default();
        let meta = meta();
        let event = event(&usage);
        let record = plugin.record(&meta, &event);
        assert_eq!(record["client_ip"], "127.0.0.1");
        assert_eq!(record["user_agent"], "client/1.0");
        assert!(record.get("messages").is_none());
    }

    #[test]
    fn unknown_config_is_rejected() {
        assert!(
            RequestAuditFactory
                .create(json!({"include_usage": true, "extra": 1}))
                .is_err()
        );
    }
}
