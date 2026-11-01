//! Configurable request and response header manipulation.

use std::collections::BTreeMap;
use std::sync::Arc;

use async_trait::async_trait;
use http::{HeaderMap, HeaderName, HeaderValue};
use octra_plugin_api::{
    ApiGatewayPlugin, ApiGatewayPluginFactory, Capability, PluginAction, PluginError,
    PluginManifest, PluginResult, ResponseContext, UpstreamRequestContext,
};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::manifest;

pub struct HeaderRewriteFactory;

impl ApiGatewayPluginFactory for HeaderRewriteFactory {
    fn manifest(&self) -> PluginManifest {
        manifest(
            "header-rewrite",
            100,
            vec![Capability::RequestHeaders, Capability::ResponseHeaders],
            "Sets, adds, or removes request and response headers",
            json!({
                "type": "object",
                "properties": {
                    "request": { "$ref": "#/$defs/ops" },
                    "response": { "$ref": "#/$defs/ops" }
                },
                "$defs": {
                    "ops": {
                        "type": "object",
                        "properties": {
                            "set": { "type": "object", "additionalProperties": { "type": "string" } },
                            "add": { "type": "object", "additionalProperties": { "type": "array", "items": { "type": "string" } } },
                            "remove": { "type": "array", "items": { "type": "string" } }
                        }
                    }
                }
            }),
        )
    }

    fn create(&self, config: Value) -> Result<Arc<dyn ApiGatewayPlugin>, PluginError> {
        let config: HeaderConfig = serde_json::from_value(config).map_err(|error| {
            PluginError::fail_closed(format!("invalid header-rewrite config: {error}"))
        })?;
        validate_ops(&config.request)?;
        validate_ops(&config.response)?;
        Ok(Arc::new(HeaderRewritePlugin {
            manifest: self.manifest(),
            config,
        }))
    }
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
struct HeaderConfig {
    request: HeaderOps,
    response: HeaderOps,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
struct HeaderOps {
    set: BTreeMap<String, String>,
    add: BTreeMap<String, Vec<String>>,
    remove: Vec<String>,
}

fn validate_ops(ops: &HeaderOps) -> Result<(), PluginError> {
    for name in &ops.remove {
        HeaderName::from_bytes(name.as_bytes()).map_err(|error| {
            PluginError::fail_closed(format!("invalid header name '{name}': {error}"))
        })?;
    }
    for (name, value) in &ops.set {
        validate_header(name, value)?;
    }
    for (name, values) in &ops.add {
        HeaderName::from_bytes(name.as_bytes()).map_err(|error| {
            PluginError::fail_closed(format!("invalid header name '{name}': {error}"))
        })?;
        for value in values {
            validate_value(name, value)?;
        }
    }
    Ok(())
}

fn validate_header(name: &str, value: &str) -> Result<(), PluginError> {
    HeaderName::from_bytes(name.as_bytes()).map_err(|error| {
        PluginError::fail_closed(format!("invalid header name '{name}': {error}"))
    })?;
    validate_value(name, value)
}

fn validate_value(name: &str, value: &str) -> Result<(), PluginError> {
    HeaderValue::from_str(value).map_err(|error| {
        PluginError::fail_closed(format!("invalid value for header '{name}': {error}"))
    })?;
    Ok(())
}

struct HeaderRewritePlugin {
    manifest: PluginManifest,
    config: HeaderConfig,
}

#[async_trait]
impl ApiGatewayPlugin for HeaderRewritePlugin {
    fn manifest(&self) -> &PluginManifest {
        &self.manifest
    }

    async fn on_upstream_request(&self, cx: &mut UpstreamRequestContext<'_>) -> PluginResult {
        apply(&self.config.request, cx.headers);
        Ok(PluginAction::Continue)
    }

    async fn on_response(&self, cx: &mut ResponseContext<'_>) -> PluginResult {
        apply(&self.config.response, cx.headers);
        Ok(PluginAction::Continue)
    }
}

fn apply(ops: &HeaderOps, headers: &mut HeaderMap) {
    for name in &ops.remove {
        if let Ok(name) = HeaderName::from_bytes(name.as_bytes()) {
            headers.remove(name);
        }
    }
    for (name, value) in &ops.set {
        if let (Ok(name), Ok(value)) = (
            HeaderName::from_bytes(name.as_bytes()),
            HeaderValue::from_str(value),
        ) {
            headers.insert(name, value);
        }
    }
    for (name, values) in &ops.add {
        let Ok(name) = HeaderName::from_bytes(name.as_bytes()) else {
            continue;
        };
        for value in values {
            if let Ok(value) = HeaderValue::from_str(value) {
                headers.append(name.clone(), value);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn applies_set_add_and_remove() {
        let config: HeaderConfig = serde_json::from_value(json!({
            "request": {
                "set": { "x-set": "one" },
                "add": { "x-add": ["a", "b"] },
                "remove": ["x-drop"]
            }
        }))
        .unwrap();
        let mut headers = HeaderMap::new();
        headers.insert("x-drop", HeaderValue::from_static("gone"));
        apply(&config.request, &mut headers);

        assert_eq!(headers.get("x-set").unwrap(), "one");
        assert_eq!(headers.get_all("x-add").iter().count(), 2);
        assert!(headers.get("x-drop").is_none());
    }

    #[test]
    fn rejects_invalid_header_names() {
        let Err(error) =
            HeaderRewriteFactory.create(json!({"request": {"set": {"bad header": "x"}}}))
        else {
            panic!("expected an error");
        };
        assert!(error.message.contains("invalid header name"));
    }
}
