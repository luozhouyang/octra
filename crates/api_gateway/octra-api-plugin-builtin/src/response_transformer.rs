//! Kong-compatible response transformation.

use std::sync::Arc;

use async_trait::async_trait;
use bytes::Bytes;
use http::{HeaderName, HeaderValue, header};
use octra_api_plugin_types::{
    APIGatewayPlugin, APIGatewayPluginFactory, BodyFilterContext, Capability, PluginAction,
    PluginError, PluginManifest, PluginPhase, PluginResult, ResponseContext,
};
use serde::Deserialize;
use serde_json::{Map, Value, json};

use crate::manifest;
use crate::transform::{apply_header_ops, is_json_content_type, json_append, split_pair};

pub struct ResponseTransformerFactory;

impl APIGatewayPluginFactory for ResponseTransformerFactory {
    fn manifest(&self) -> PluginManifest {
        manifest(
            "response-transformer",
            800,
            vec![Capability::ResponseHeaders, Capability::ResponseBody],
            "Transforms response headers and JSON bodies",
            json!({
                "type": "object",
                "properties": {
                    "remove": { "$ref": "#/$defs/plain" },
                    "rename": { "$ref": "#/$defs/plain" },
                    "replace": { "$ref": "#/$defs/typed" },
                    "add": { "$ref": "#/$defs/typed" },
                    "append": { "$ref": "#/$defs/typed" }
                },
                "$defs": {
                    "plain": {
                        "type": "object",
                        "properties": {
                            "json": { "type": "array", "items": { "type": "string" } },
                            "headers": { "type": "array", "items": { "type": "string" } }
                        }
                    },
                    "typed": {
                        "type": "object",
                        "properties": {
                            "json": { "type": "array", "items": { "type": "string" } },
                            "json_types": {
                                "type": "array",
                                "items": { "type": "string", "enum": ["boolean", "number", "string"] }
                            },
                            "headers": { "type": "array", "items": { "type": "string" } }
                        }
                    }
                }
            }),
        )
    }

    fn create(&self, _id: &str, config: Value) -> Result<Arc<dyn APIGatewayPlugin>, PluginError> {
        let config: Config = serde_json::from_value(config).map_err(|error| {
            PluginError::fail_closed(format!("invalid response-transformer config: {error}"))
        })?;
        validate(&config)?;
        Ok(Arc::new(ResponseTransformerPlugin {
            manifest: self.manifest(),
            config,
        }))
    }

    fn phases(&self) -> Vec<PluginPhase> {
        vec![
            PluginPhase::Response,
            PluginPhase::BodyFilter,
            PluginPhase::Log,
        ]
    }
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
struct Config {
    remove: Plain,
    rename: Plain,
    replace: Typed,
    add: Typed,
    append: Typed,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
struct Plain {
    json: Vec<String>,
    headers: Vec<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
struct Typed {
    json: Vec<String>,
    json_types: Vec<String>,
    headers: Vec<String>,
}

struct ResponseTransformerPlugin {
    manifest: PluginManifest,
    config: Config,
}

impl Config {
    fn has_body_ops(&self) -> bool {
        !self.remove.json.is_empty()
            || !self.rename.json.is_empty()
            || !self.replace.json.is_empty()
            || !self.add.json.is_empty()
            || !self.append.json.is_empty()
    }
}

fn validate(config: &Config) -> Result<(), PluginError> {
    validate_headers(&config.remove.headers, false)?;
    validate_rename_headers(&config.rename.headers)?;
    validate_headers(&config.replace.headers, true)?;
    validate_headers(&config.add.headers, true)?;
    validate_headers(&config.append.headers, true)?;

    validate_names(&config.remove.json)?;
    for entry in &config.rename.json {
        let (old, new) = split_pair(entry);
        validate_name(old)?;
        new.filter(|value| !value.is_empty())
            .ok_or_else(|| missing(entry))?;
    }
    validate_typed(&config.replace)?;
    validate_typed(&config.add)?;
    validate_typed(&config.append)?;
    Ok(())
}

fn validate_headers(entries: &[String], with_value: bool) -> Result<(), PluginError> {
    for entry in entries {
        let (name, value) = split_pair(entry);
        HeaderName::from_bytes(name.as_bytes()).map_err(|error| {
            PluginError::fail_closed(format!("invalid header '{name}': {error}"))
        })?;
        if with_value {
            let value = value
                .filter(|value| !value.is_empty())
                .ok_or_else(|| missing(entry))?;
            HeaderValue::from_str(value).map_err(|error| {
                PluginError::fail_closed(format!("invalid header value '{value}': {error}"))
            })?;
        }
    }
    Ok(())
}

fn validate_rename_headers(entries: &[String]) -> Result<(), PluginError> {
    for entry in entries {
        let (old, new) = split_pair(entry);
        HeaderName::from_bytes(old.as_bytes()).map_err(|error| {
            PluginError::fail_closed(format!("invalid header '{old}': {error}"))
        })?;
        let new = new
            .filter(|value| !value.is_empty())
            .ok_or_else(|| missing(entry))?;
        HeaderName::from_bytes(new.as_bytes()).map_err(|error| {
            PluginError::fail_closed(format!("invalid header rename target '{new}': {error}"))
        })?;
    }
    Ok(())
}

fn validate_names(entries: &[String]) -> Result<(), PluginError> {
    for entry in entries {
        validate_name(split_pair(entry).0)?;
    }
    Ok(())
}

fn validate_typed(action: &Typed) -> Result<(), PluginError> {
    for entry in &action.json {
        let (name, value) = split_pair(entry);
        validate_name(name)?;
        value
            .filter(|value| !value.is_empty())
            .ok_or_else(|| missing(entry))?;
    }
    for json_type in &action.json_types {
        if !matches!(json_type.as_str(), "boolean" | "number" | "string") {
            return Err(PluginError::fail_closed(format!(
                "unsupported json type '{json_type}'"
            )));
        }
    }
    Ok(())
}

fn validate_name(name: &str) -> Result<(), PluginError> {
    if name.is_empty() {
        Err(PluginError::fail_closed("transform key must not be empty"))
    } else {
        Ok(())
    }
}

fn missing(entry: &str) -> PluginError {
    PluginError::fail_closed(format!("transform entry '{entry}' has no value"))
}

#[async_trait]
impl APIGatewayPlugin for ResponseTransformerPlugin {
    fn manifest(&self) -> &PluginManifest {
        &self.manifest
    }

    async fn response(&self, cx: &mut ResponseContext<'_>) -> PluginResult {
        apply_header_ops(
            cx.headers,
            &self.config.remove.headers,
            &self.config.rename.headers,
            &self.config.replace.headers,
            &self.config.add.headers,
            &self.config.append.headers,
            false,
        );
        if self.config.has_body_ops() && is_json_content_type(cx.headers) {
            cx.headers.remove(header::CONTENT_LENGTH);
        }
        Ok(PluginAction::Continue)
    }

    async fn body_filter(&self, cx: &mut BodyFilterContext<'_>) -> Result<(), PluginError> {
        if !self.config.has_body_ops() || !is_json_content_type(cx.headers) {
            return Ok(());
        }
        if let Some(chunk) = cx.chunk.take() {
            cx.ctx.body.extend_from_slice(&chunk);
        }
        if !cx.eof {
            cx.chunk = None;
            return Ok(());
        }
        let body = std::mem::take(&mut cx.ctx.body);
        cx.chunk = Some(transform_response_body(&self.config, body.freeze())?);
        Ok(())
    }
}

fn transform_response_body(config: &Config, body: Bytes) -> Result<Bytes, PluginError> {
    let mut object = match serde_json::from_slice::<Value>(&body) {
        Ok(Value::Object(object)) => object,
        _ => return Ok(body),
    };

    for entry in &config.remove.json {
        object.remove(split_pair(entry).0);
    }
    for entry in &config.rename.json {
        if let (old, Some(new)) = split_pair(entry)
            && let Some(value) = object.remove(old)
        {
            object.insert(new.to_string(), value);
        }
    }
    apply_typed(&mut object, &config.replace, Mode::Replace);
    apply_typed(&mut object, &config.add, Mode::Add);
    apply_typed(&mut object, &config.append, Mode::Append);

    serde_json::to_vec(&Value::Object(object))
        .map(Bytes::from)
        .map_err(|error| {
            PluginError::fail_closed(format!("cannot encode transformed response: {error}"))
        })
}

#[derive(Clone, Copy)]
enum Mode {
    Replace,
    Add,
    Append,
}

fn apply_typed(object: &mut Map<String, Value>, action: &Typed, mode: Mode) {
    for (index, entry) in action.json.iter().enumerate() {
        let (name, Some(raw)) = split_pair(entry) else {
            continue;
        };
        let value = cast(raw, action.json_types.get(index).map(String::as_str));
        match mode {
            Mode::Replace => {
                if object.contains_key(name) {
                    object.insert(name.to_string(), value);
                }
            }
            Mode::Add => {
                if !object.contains_key(name) {
                    object.insert(name.to_string(), value);
                }
            }
            Mode::Append => {
                let next = json_append(object.get(name), value);
                object.insert(name.to_string(), next);
            }
        }
    }
}

fn cast(raw: &str, json_type: Option<&str>) -> Value {
    match json_type {
        Some("number") => {
            if let Ok(number) = raw.parse::<i64>() {
                Value::Number(number.into())
            } else if let Ok(number) = raw.parse::<f64>() {
                serde_json::Number::from_f64(number)
                    .map(Value::Number)
                    .unwrap_or_else(|| Value::String(raw.to_string()))
            } else {
                Value::String(raw.to_string())
            }
        }
        Some("boolean") => Value::Bool(raw == "true"),
        _ => Value::String(raw.to_string()),
    }
}

impl octra_api_plugin_types::APIGatewayBackgroundPlugin for ResponseTransformerPlugin {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_invalid_configs() {
        assert!(
            ResponseTransformerFactory
                .create("test", json!({"add": {"json": ["missing-value"]}}))
                .is_err()
        );
        assert!(
            ResponseTransformerFactory
                .create(
                    "test",
                    json!({"add": {"json": ["key:value"], "json_types": ["date"]}})
                )
                .is_err()
        );
        assert!(
            ResponseTransformerFactory
                .create("test", json!({"rename": {"headers": ["x:bad header"]}}))
                .is_err()
        );
    }

    #[test]
    fn accepts_kong_style_configs() {
        let config = json!({
            "remove": {"headers": ["x-remove"], "json": ["drop"]},
            "rename": {"headers": ["x-old:x-new"], "json": ["old:new"]},
            "replace": {"headers": ["x-replace:value"], "json": ["count:9"], "json_types": ["number"]},
            "add": {"headers": ["x-add:value"], "json": ["added:true"], "json_types": ["boolean"]},
            "append": {"headers": ["x-append:value"], "json": ["list:item"]}
        });
        assert!(ResponseTransformerFactory.create("test", config).is_ok());
    }
}
