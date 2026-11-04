//! Rewrites the requested model through a configured alias map.

use std::collections::BTreeMap;
use std::sync::Arc;

use async_trait::async_trait;
use octra_api_plugin_types::{PluginError, PluginManifest};
use octra_llm_core::StreamEvent;
use octra_llm_plugin_types::{
    LlmAction, LlmPlugin, LlmPluginFactory, LlmPluginResult, LlmRequestContext, LlmResponseContext,
    LlmScope, LlmStreamAction, LlmStreamContext, LlmStreamResult, REQUEST_LLM_SCOPES,
};
use serde_json::{Value, json};

use crate::manifest;

const REQUESTED_FIELD: &str = "x-octra-requested-model";
const STATE_REQUESTED: &str = "requested";
const STATE_TARGET: &str = "target";

#[derive(Debug)]
pub struct ModelMap {
    manifest: PluginManifest,
    map: BTreeMap<String, String>,
}

pub struct ModelMapFactory;

impl LlmPluginFactory for ModelMapFactory {
    fn manifest(&self) -> PluginManifest {
        let mut manifest = manifest(
            "llm-model-map",
            "Map client model aliases onto provider models",
        );
        manifest.priority = 900;
        manifest
    }

    fn supported_scopes(&self) -> &'static [LlmScope] {
        &REQUEST_LLM_SCOPES
    }

    fn create(&self, config: Value) -> Result<Arc<dyn LlmPlugin>, PluginError> {
        let map = config
            .get("map")
            .or_else(|| config.get("mappings"))
            .and_then(Value::as_object)
            .ok_or_else(|| PluginError::fail_closed("llm-model-map requires a `map` object"))?;
        let mut parsed = BTreeMap::new();
        for (alias, target) in map {
            let Some(target) = target.as_str().map(str::trim).filter(|v| !v.is_empty()) else {
                return Err(PluginError::fail_closed(format!(
                    "llm-model-map target for '{alias}' must be a non-empty string"
                )));
            };
            parsed.insert(alias.clone(), target.to_string());
        }
        Ok(Arc::new(ModelMap {
            manifest: self.manifest(),
            map: parsed,
        }))
    }
}

#[async_trait]
impl LlmPlugin for ModelMap {
    fn manifest(&self) -> &PluginManifest {
        &self.manifest
    }

    async fn on_request(&self, cx: &mut LlmRequestContext<'_>) -> LlmPluginResult {
        let Some(target) = self.map.get(&cx.request.model) else {
            return Ok(LlmAction::Continue);
        };
        let requested = cx.request.model.clone();
        cx.request
            .extra
            .entry(REQUESTED_FIELD.to_string())
            .or_insert_with(|| Value::String(requested.clone()));
        *cx.state = json!({
            STATE_REQUESTED: requested,
            STATE_TARGET: target,
        });
        cx.request.model = target.clone();
        Ok(LlmAction::Continue)
    }

    /// Restores the client-facing alias on the response model.
    async fn on_response(&self, cx: &mut LlmResponseContext<'_>) -> LlmPluginResult {
        let Some((requested, target)) = mapping(cx.state) else {
            return Ok(LlmAction::Continue);
        };
        if cx.response.model == target {
            cx.response.model = requested;
        }
        Ok(LlmAction::Continue)
    }

    /// Restores the alias on the stream's initial message identity.
    async fn on_stream_event(&self, cx: &mut LlmStreamContext<'_>) -> LlmStreamResult {
        let Some((requested, target)) = mapping(cx.state) else {
            return Ok(LlmStreamAction::Continue);
        };
        if let StreamEvent::MessageStart { model, .. } = cx.event
            && *model == target
        {
            *model = requested;
        }
        Ok(LlmStreamAction::Continue)
    }
}

fn mapping(state: &Value) -> Option<(String, String)> {
    let requested = state.get(STATE_REQUESTED)?.as_str()?.to_string();
    let target = state.get(STATE_TARGET)?.as_str()?.to_string();
    Some((requested, target))
}

#[cfg(test)]
mod tests {
    use super::*;
    use octra_llm_core::{ChatRequest, ChatResponse, Message, Protocol, Usage};
    use octra_llm_plugin_types::LlmRequestMeta;
    use octra_llm_plugin_types::LlmResponseHeaders;
    use serde_json::json;

    fn context<'a>(
        meta: &'a LlmRequestMeta<'a>,
        headers: &'a mut LlmResponseHeaders,
        request: &'a mut ChatRequest,
        state: &'a mut serde_json::Value,
    ) -> LlmRequestContext<'a> {
        LlmRequestContext {
            meta,
            headers,
            request,
            state,
        }
    }

    fn meta() -> LlmRequestMeta<'static> {
        LlmRequestMeta {
            request_id: "r",
            ingress: Protocol::Openai,
            consumer_id: None,
            virtual_key_id: None,
            virtual_key_family: None,
            consumer_group_ids: &[],
            session_id: None,
            client_ip: None,
            user_agent: None,
            intent: None,
        }
    }

    #[tokio::test]
    async fn maps_aliases_and_records_the_original() {
        let plugin = ModelMapFactory
            .create(json!({"map": {"fast": "gpt-4o-mini"}}))
            .unwrap();
        let mut request = ChatRequest::new("fast", Vec::new());
        let meta = meta();
        let mut state = serde_json::Value::Null;
        let mut headers = LlmResponseHeaders::default();
        let mut cx = context(&meta, &mut headers, &mut request, &mut state);
        plugin.on_request(&mut cx).await.unwrap();
        assert_eq!(request.model, "gpt-4o-mini");
        assert_eq!(
            request.extra.get("x-octra-requested-model"),
            Some(&json!("fast"))
        );
    }

    #[tokio::test]
    async fn restores_the_alias_on_response_and_stream_start() {
        let plugin = ModelMapFactory
            .create(json!({"map": {"fast": "gpt-4o-mini"}}))
            .unwrap();
        let meta = meta();
        let mut state = serde_json::Value::Null;
        let mut headers = LlmResponseHeaders::default();
        let mut request = ChatRequest::new("fast", Vec::new());
        {
            let mut cx = context(&meta, &mut headers, &mut request, &mut state);
            plugin.on_request(&mut cx).await.unwrap();
        }

        let mut response = ChatResponse::new("id", "gpt-4o-mini", Message::user("hello"));
        let mut usage = Usage::default();
        let mut cx = octra_llm_plugin_types::LlmResponseContext {
            meta: &meta,
            request_model: "fast",
            response: &mut response,
            usage: &mut usage,
            state: &mut state,
        };
        plugin.on_response(&mut cx).await.unwrap();
        assert_eq!(response.model, "fast");

        let mut event = octra_llm_core::StreamEvent::MessageStart {
            id: "id".into(),
            model: "gpt-4o-mini".into(),
        };
        let mut cx = octra_llm_plugin_types::LlmStreamContext {
            meta: &meta,
            event: &mut event,
            state: &mut state,
        };
        plugin.on_stream_event(&mut cx).await.unwrap();
        match event {
            octra_llm_core::StreamEvent::MessageStart { model, .. } => {
                assert_eq!(model, "fast");
            }
            _ => panic!("expected message start"),
        }
    }

    #[tokio::test]
    async fn leaves_unknown_models_alone() {
        let plugin = ModelMapFactory
            .create(json!({"map": {"fast": "gpt-4o-mini"}}))
            .unwrap();
        let mut request = ChatRequest::new("other", Vec::new());
        let meta = meta();
        let mut state = serde_json::Value::Null;
        let mut headers = LlmResponseHeaders::default();
        let mut cx = context(&meta, &mut headers, &mut request, &mut state);
        plugin.on_request(&mut cx).await.unwrap();
        assert_eq!(request.model, "other");
        assert!(request.extra.is_empty());
    }

    #[test]
    fn rejects_missing_map() {
        assert!(ModelMapFactory.create(json!({})).is_err());
        assert!(ModelMapFactory.create(json!({"map": {"a": 1}})).is_err());
    }
}
