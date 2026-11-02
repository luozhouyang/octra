//! Overrides sampling parameters and extra fields on the request.

use std::sync::Arc;

use async_trait::async_trait;
use octra_api_plugin_types::{PluginError, PluginManifest};
use octra_llm_plugin_types::{
    LlmAction, LlmPlugin, LlmPluginFactory, LlmPluginResult, LlmRequestContext,
};
use serde_json::{Map, Value};

use crate::manifest;

#[derive(Debug, Default, Clone)]
struct Overrides {
    temperature: Option<f64>,
    top_p: Option<f64>,
    top_k: Option<u32>,
    max_tokens: Option<u32>,
    stop: Option<Vec<String>>,
    set: Map<String, Value>,
    delete: Vec<String>,
    extra: Map<String, Value>,
    extra_delete: Vec<String>,
}

#[derive(Debug)]
pub struct ParamOverride {
    manifest: PluginManifest,
    overrides: Overrides,
}

pub struct ParamOverrideFactory;

impl ParamOverrideFactory {
    fn parse(config: &Value) -> Result<Overrides, PluginError> {
        let mut overrides = Overrides::default();
        let Some(set) = config.get("set").and_then(Value::as_object) else {
            return Err(PluginError::fail_closed(
                "llm-param-override requires a `set` object",
            ));
        };
        for (key, value) in set {
            match key.as_str() {
                "temperature" => overrides.temperature = Some(number(value, key)?),
                "top_p" => overrides.top_p = Some(number(value, key)?),
                "top_k" => overrides.top_k = Some(integer(value, key)? as u32),
                "max_tokens" => overrides.max_tokens = Some(integer(value, key)? as u32),
                "stop" => {
                    let values = value.as_array().ok_or_else(|| {
                        PluginError::fail_closed("llm-param-override `stop` must be an array")
                    })?;
                    overrides.stop = Some(
                        values
                            .iter()
                            .filter_map(Value::as_str)
                            .map(str::to_string)
                            .collect(),
                    );
                }
                other => {
                    overrides.set.insert(other.to_string(), value.clone());
                }
            }
        }
        overrides.delete = string_list(config.get("delete"), "delete")?;
        overrides.extra_delete = string_list(config.get("extra_delete"), "extra_delete")?;
        if let Some(extra) = config.get("extra") {
            overrides.extra = extra.as_object().cloned().ok_or_else(|| {
                PluginError::fail_closed("llm-param-override `extra` must be an object")
            })?;
        }
        Ok(overrides)
    }
}

fn number(value: &Value, key: &str) -> Result<f64, PluginError> {
    value.as_f64().ok_or_else(|| {
        PluginError::fail_closed(format!("llm-param-override `{key}` must be a number"))
    })
}

fn integer(value: &Value, key: &str) -> Result<i64, PluginError> {
    value.as_i64().ok_or_else(|| {
        PluginError::fail_closed(format!("llm-param-override `{key}` must be an integer"))
    })
}

fn string_list(value: Option<&Value>, key: &str) -> Result<Vec<String>, PluginError> {
    let Some(value) = value else {
        return Ok(Vec::new());
    };
    value
        .as_array()
        .map(|values| {
            values
                .iter()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect()
        })
        .ok_or_else(|| {
            PluginError::fail_closed(format!("llm-param-override `{key}` must be an array"))
        })
}

impl LlmPluginFactory for ParamOverrideFactory {
    fn manifest(&self) -> PluginManifest {
        manifest(
            "llm-param-override",
            "Set or delete sampling parameters and extra request fields",
        )
    }

    fn create(&self, config: Value) -> Result<Arc<dyn LlmPlugin>, PluginError> {
        Ok(Arc::new(ParamOverride {
            manifest: self.manifest(),
            overrides: Self::parse(&config)?,
        }))
    }
}

#[async_trait]
impl LlmPlugin for ParamOverride {
    fn manifest(&self) -> &PluginManifest {
        &self.manifest
    }

    async fn on_request(&self, cx: &mut LlmRequestContext<'_>) -> LlmPluginResult {
        let o = &self.overrides;
        if let Some(value) = o.temperature {
            cx.request.temperature = Some(value);
        }
        if let Some(value) = o.top_p {
            cx.request.top_p = Some(value);
        }
        if let Some(value) = o.top_k {
            cx.request.top_k = Some(value);
        }
        if let Some(value) = o.max_tokens {
            cx.request.max_tokens = Some(value);
        }
        if let Some(value) = &o.stop {
            cx.request.stop = value.clone();
        }
        for key in &o.delete {
            match key.as_str() {
                "temperature" => cx.request.temperature = None,
                "top_p" => cx.request.top_p = None,
                "top_k" => cx.request.top_k = None,
                "max_tokens" => cx.request.max_tokens = None,
                "stop" => cx.request.stop.clear(),
                other => {
                    apply_extra(cx, other, None);
                }
            }
        }
        for (key, value) in &o.set {
            apply_extra(cx, key, Some(value.clone()));
        }
        for key in &o.extra_delete {
            cx.request.extra.remove(key);
        }
        for (key, value) in &o.extra {
            cx.request.extra.insert(key.clone(), value.clone());
        }
        Ok(LlmAction::Continue)
    }
}

fn apply_extra(cx: &mut LlmRequestContext<'_>, key: &str, value: Option<Value>) {
    match value {
        Some(value) => {
            cx.request.extra.insert(key.to_string(), value);
        }
        None => {
            cx.request.extra.remove(key);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use octra_llm_core::{ChatRequest, Protocol};
    use octra_llm_plugin_types::LlmRequestMeta;
    use serde_json::json;

    fn context<'a>(
        meta: &'a LlmRequestMeta<'a>,
        request: &'a mut ChatRequest,
        state: &'a mut serde_json::Value,
    ) -> LlmRequestContext<'a> {
        LlmRequestContext {
            meta,
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
            session_id: None,
            client_ip: None,
            user_agent: None,
            intent: None,
        }
    }

    #[tokio::test]
    async fn sets_and_deletes_parameters() {
        let plugin = ParamOverrideFactory
            .create(json!({
                "set": {"temperature": 0.1, "max_tokens": 64, "service_tier": "flex"},
                "delete": ["top_k"],
                "extra": {"metadata": {"tenant": "a"}},
                "extra_delete": ["legacy"]
            }))
            .unwrap();
        let mut request = ChatRequest::new("m", Vec::new());
        request.temperature = Some(0.9);
        request.top_k = Some(40);
        request.extra.insert("legacy".into(), json!(true));
        let meta = meta();
        let mut state = serde_json::Value::Null;
        let mut cx = context(&meta, &mut request, &mut state);
        plugin.on_request(&mut cx).await.unwrap();
        assert_eq!(request.temperature, Some(0.1));
        assert_eq!(request.max_tokens, Some(64));
        assert_eq!(request.top_k, None);
        assert_eq!(request.extra.get("service_tier"), Some(&json!("flex")));
        assert_eq!(request.extra.get("metadata"), Some(&json!({"tenant": "a"})));
        assert!(request.extra.get("legacy").is_none());
    }

    #[test]
    fn rejects_bad_configuration() {
        assert!(ParamOverrideFactory.create(json!({})).is_err());
        assert!(
            ParamOverrideFactory
                .create(json!({"set": {"temperature": "hot"}}))
                .is_err()
        );
        assert!(
            ParamOverrideFactory
                .create(json!({"set": {}, "delete": "top_k"}))
                .is_err()
        );
    }
}
