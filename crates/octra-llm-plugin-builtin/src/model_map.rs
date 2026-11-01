//! Rewrites the requested model through a configured alias map.

use std::collections::BTreeMap;
use std::sync::Arc;

use async_trait::async_trait;
use octra_llm_plugin_api::{
    LlmAction, LlmPlugin, LlmPluginFactory, LlmPluginResult, LlmRequestContext,
};
use octra_plugin_api::{PluginError, PluginManifest};
use serde_json::Value;

use crate::manifest;

#[derive(Debug)]
pub struct ModelMap {
    manifest: PluginManifest,
    map: BTreeMap<String, String>,
}

pub struct ModelMapFactory;

impl LlmPluginFactory for ModelMapFactory {
    fn manifest(&self) -> PluginManifest {
        manifest(
            "llm-model-map",
            "Map client model aliases onto provider models",
        )
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
        cx.request
            .extra
            .entry("x-octra-requested-model".to_string())
            .or_insert_with(|| Value::String(cx.request.model.clone()));
        cx.request.model = target.clone();
        Ok(LlmAction::Continue)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use octra_llm_core::{ChatRequest, Protocol};
    use serde_json::json;

    fn context<'a>(request: &'a mut ChatRequest) -> LlmRequestContext<'a> {
        LlmRequestContext {
            request_id: "r",
            ingress: Protocol::Openai,
            consumer_id: None,
            virtual_key_id: None,
            request,
        }
    }

    #[tokio::test]
    async fn maps_aliases_and_records_the_original() {
        let plugin = ModelMapFactory
            .create(json!({"map": {"fast": "gpt-4o-mini"}}))
            .unwrap();
        let mut request = ChatRequest::new("fast", Vec::new());
        let mut cx = context(&mut request);
        plugin.on_request(&mut cx).await.unwrap();
        assert_eq!(request.model, "gpt-4o-mini");
        assert_eq!(
            request.extra.get("x-octra-requested-model"),
            Some(&json!("fast"))
        );
    }

    #[tokio::test]
    async fn leaves_unknown_models_alone() {
        let plugin = ModelMapFactory
            .create(json!({"map": {"fast": "gpt-4o-mini"}}))
            .unwrap();
        let mut request = ChatRequest::new("other", Vec::new());
        let mut cx = context(&mut request);
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
