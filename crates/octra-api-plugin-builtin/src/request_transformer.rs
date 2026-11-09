//! Kong-compatible request transformation.

use std::sync::Arc;

use async_trait::async_trait;
use bytes::Bytes;
use http::uri::PathAndQuery;
use http::{HeaderMap, HeaderName, HeaderValue, Method, Uri, header};
use octra_api_plugin_types::{
    ApiGatewayPlugin, ApiGatewayPluginFactory, Capability, PluginAction, PluginError,
    PluginManifest, PluginResult, RequestContext,
};
use serde::Deserialize;
use serde_json::{Map, Value, json};

use crate::manifest;
use crate::transform::{
    MultipartBody, apply_header_ops, encode_form, form_add, form_append, form_remove, form_rename,
    form_replace, is_form_content_type, is_json_content_type, is_multipart_content_type,
    json_append, parse_form, split_pair,
};

pub struct RequestTransformerFactory;

impl ApiGatewayPluginFactory for RequestTransformerFactory {
    fn manifest(&self) -> PluginManifest {
        manifest(
            "request-transformer",
            801,
            vec![Capability::RequestHeaders, Capability::RequestBody],
            "Transforms request headers, querystring, body, method, and URI",
            json!({
                "type": "object",
                "properties": {
                    "http_method": { "type": "string" },
                    "remove": { "$ref": "#/$defs/plain" },
                    "rename": { "$ref": "#/$defs/plain" },
                    "replace": { "$ref": "#/$defs/replace" },
                    "add": { "$ref": "#/$defs/plain" },
                    "append": { "$ref": "#/$defs/plain" }
                },
                "$defs": {
                    "plain": {
                        "type": "object",
                        "properties": {
                            "body": { "type": "array", "items": { "type": "string" } },
                            "headers": { "type": "array", "items": { "type": "string" } },
                            "querystring": { "type": "array", "items": { "type": "string" } }
                        }
                    },
                    "replace": {
                        "type": "object",
                        "properties": {
                            "body": { "type": "array", "items": { "type": "string" } },
                            "headers": { "type": "array", "items": { "type": "string" } },
                            "querystring": { "type": "array", "items": { "type": "string" } },
                            "uri": { "type": "string" }
                        }
                    }
                }
            }),
        )
    }

    fn create(&self, _id: &str, config: Value) -> Result<Arc<dyn ApiGatewayPlugin>, PluginError> {
        let config: Config = serde_json::from_value(config).map_err(|error| {
            PluginError::fail_closed(format!("invalid request-transformer config: {error}"))
        })?;
        validate(&config)?;
        Ok(Arc::new(RequestTransformerPlugin {
            manifest: self.manifest(),
            config,
        }))
    }
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
struct Config {
    http_method: Option<String>,
    remove: Action,
    rename: Action,
    replace: ReplaceAction,
    add: Action,
    append: Action,
}

impl Config {
    fn has_body_ops(&self) -> bool {
        !self.remove.body.is_empty()
            || !self.rename.body.is_empty()
            || !self.replace.body.is_empty()
            || !self.add.body.is_empty()
            || !self.append.body.is_empty()
    }

    /// Kong moves form parameters into the querystring when the method is
    /// changed to a bodyless method.
    fn merges_body_into_query(&self, headers: &HeaderMap) -> bool {
        self.http_method.as_ref().is_some_and(|method| {
            matches!(method.to_uppercase().as_str(), "GET" | "HEAD" | "TRACE")
        }) && is_form_content_type(headers)
    }
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
struct Action {
    body: Vec<String>,
    headers: Vec<String>,
    querystring: Vec<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
struct ReplaceAction {
    body: Vec<String>,
    headers: Vec<String>,
    querystring: Vec<String>,
    uri: Option<String>,
}

struct RequestTransformerPlugin {
    manifest: PluginManifest,
    config: Config,
}

fn validate(config: &Config) -> Result<(), PluginError> {
    if let Some(method) = &config.http_method {
        let method = method.to_uppercase();
        const METHODS: [&str; 9] = [
            "GET", "HEAD", "POST", "PUT", "DELETE", "CONNECT", "OPTIONS", "TRACE", "PATCH",
        ];
        if !METHODS.contains(&method.as_str()) {
            return Err(PluginError::fail_closed(format!(
                "unsupported http_method '{method}'"
            )));
        }
        Method::from_bytes(method.as_bytes()).map_err(|error| {
            PluginError::fail_closed(format!("invalid request-transformer http_method: {error}"))
        })?;
    }
    if let Some(uri) = &config.replace.uri {
        uri.parse::<PathAndQuery>().map_err(|error| {
            PluginError::fail_closed(format!("invalid request-transformer replace.uri: {error}"))
        })?;
    }

    for entry in &config.remove.headers {
        validate_header_name(split_pair(entry).0)?;
    }
    for entry in &config.rename.headers {
        let (old, new) = split_pair(entry);
        validate_header_name(old)?;
        let new = new
            .filter(|value| !value.is_empty())
            .ok_or_else(|| missing(entry))?;
        validate_header_name(new)?;
    }
    for entry in config
        .replace
        .headers
        .iter()
        .chain(&config.add.headers)
        .chain(&config.append.headers)
    {
        let (name, value) = split_pair(entry);
        validate_header_name(name)?;
        let value = value
            .filter(|value| !value.is_empty())
            .ok_or_else(|| missing(entry))?;
        HeaderValue::from_str(value).map_err(|error| {
            PluginError::fail_closed(format!("invalid header value '{value}': {error}"))
        })?;
    }

    for entry in &config.remove.body {
        validate_name(entry)?;
    }
    for entry in config.rename.body.iter().chain(&config.rename.querystring) {
        let (old, new) = split_pair(entry);
        validate_name(old)?;
        new.filter(|value| !value.is_empty())
            .ok_or_else(|| missing(entry))?;
    }
    for entry in config
        .replace
        .body
        .iter()
        .chain(&config.replace.querystring)
        .chain(&config.add.body)
        .chain(&config.add.querystring)
        .chain(&config.append.body)
        .chain(&config.append.querystring)
    {
        let (name, value) = split_pair(entry);
        validate_name(name)?;
        value
            .filter(|value| !value.is_empty())
            .ok_or_else(|| missing(entry))?;
    }
    Ok(())
}

fn validate_header_name(name: &str) -> Result<(), PluginError> {
    HeaderName::from_bytes(name.as_bytes())
        .map(|_| ())
        .map_err(|error| PluginError::fail_closed(format!("invalid header '{name}': {error}")))
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
impl ApiGatewayPlugin for RequestTransformerPlugin {
    fn manifest(&self) -> &PluginManifest {
        &self.manifest
    }

    async fn access(&self, cx: &mut RequestContext<'_>) -> PluginResult {
        if let Some(uri) = &self.config.replace.uri {
            replace_uri(cx.uri, uri);
        }

        if let Some(method) = &self.config.http_method
            && let Ok(method) = Method::from_bytes(method.to_uppercase().as_bytes())
        {
            cx.state.method_override = Some(method);
        }

        apply_header_ops(
            cx.headers,
            &self.config.remove.headers,
            &self.config.rename.headers,
            &self.config.replace.headers,
            &self.config.add.headers,
            &self.config.append.headers,
            true,
        );

        let merge = self.config.merges_body_into_query(cx.headers);
        if !merge {
            transform_query(cx.uri, &self.config);
        }
        if merge || self.config.has_body_ops() {
            let headers = cx.headers.clone();
            let mut body = cx.request_body().cloned().unwrap_or_default();
            if merge {
                merge_form_into_query(cx.uri, &body);
                transform_query(cx.uri, &self.config);
            }
            if self.config.has_body_ops() {
                if is_json_content_type(&headers) {
                    body = transform_json(&self.config, &body);
                } else if is_form_content_type(&headers) {
                    body = transform_form(&self.config, &body);
                } else if is_multipart_content_type(&headers) {
                    body = transform_multipart(&self.config, &headers, &body);
                }
            }
            cx.set_request_body(body);
        }
        Ok(PluginAction::Continue)
    }
}

fn replace_uri(uri: &mut Uri, value: &str) {
    let Ok(path_and_query) = value.parse::<PathAndQuery>() else {
        return;
    };
    let mut parts = uri.clone().into_parts();
    parts.path_and_query = Some(path_and_query);
    if let Ok(next) = Uri::from_parts(parts) {
        *uri = next;
    }
}

fn transform_query(uri: &mut Uri, config: &Config) {
    let has_ops = !config.remove.querystring.is_empty()
        || !config.rename.querystring.is_empty()
        || !config.replace.querystring.is_empty()
        || !config.add.querystring.is_empty()
        || !config.append.querystring.is_empty();
    if !has_ops {
        return;
    }

    let mut pairs = uri.query().map(parse_form).unwrap_or_default();

    for entry in &config.remove.querystring {
        form_remove(&mut pairs, split_pair(entry).0);
    }
    for entry in &config.rename.querystring {
        if let (old, Some(new)) = split_pair(entry) {
            form_rename(&mut pairs, old, new);
        }
    }
    for entry in &config.replace.querystring {
        if let (name, Some(value)) = split_pair(entry) {
            form_replace(&mut pairs, name, value);
        }
    }
    for entry in &config.add.querystring {
        if let (name, Some(value)) = split_pair(entry) {
            form_add(&mut pairs, name, value);
        }
    }
    for entry in &config.append.querystring {
        if let (name, Some(value)) = split_pair(entry) {
            form_append(&mut pairs, name, value);
        }
    }

    set_query(uri, &pairs);
}

fn merge_form_into_query(uri: &mut Uri, body: &[u8]) {
    let raw = String::from_utf8_lossy(body);
    let body_pairs = parse_form(&raw);
    if body_pairs.is_empty() {
        return;
    }
    let mut pairs = uri.query().map(parse_form).unwrap_or_default();
    pairs.extend(body_pairs);
    set_query(uri, &pairs);
}

fn set_query(uri: &mut Uri, pairs: &[(String, String)]) {
    let query = encode_form(pairs);
    let path = uri.path().to_string();
    let path_and_query = if query.is_empty() {
        path
    } else {
        format!("{path}?{query}")
    };
    if let Ok(path_and_query) = path_and_query.parse::<PathAndQuery>() {
        let mut parts = uri.clone().into_parts();
        parts.path_and_query = Some(path_and_query);
        if let Ok(next) = Uri::from_parts(parts) {
            *uri = next;
        }
    }
}

fn transform_json(config: &Config, body: &[u8]) -> Bytes {
    let mut object = if body.is_empty() {
        Map::new()
    } else {
        match serde_json::from_slice::<Value>(body) {
            Ok(Value::Object(object)) => object,
            _ => return Bytes::copy_from_slice(body),
        }
    };

    for entry in &config.remove.body {
        object.remove(split_pair(entry).0);
    }
    for entry in &config.rename.body {
        if let (old, Some(new)) = split_pair(entry)
            && let Some(value) = object.remove(old)
        {
            object.insert(new.to_string(), value);
        }
    }
    for entry in &config.replace.body {
        if let (name, Some(value)) = split_pair(entry)
            && object.contains_key(name)
        {
            object.insert(name.to_string(), Value::String(value.to_string()));
        }
    }
    for entry in &config.add.body {
        if let (name, Some(value)) = split_pair(entry)
            && !object.contains_key(name)
        {
            object.insert(name.to_string(), Value::String(value.to_string()));
        }
    }
    for entry in &config.append.body {
        if let (name, Some(value)) = split_pair(entry) {
            let next = json_append(object.get(name), Value::String(value.to_string()));
            object.insert(name.to_string(), next);
        }
    }

    serde_json::to_vec(&Value::Object(object))
        .map(Bytes::from)
        .unwrap_or_else(|_| Bytes::copy_from_slice(body))
}

fn transform_form(config: &Config, body: &[u8]) -> Bytes {
    let raw = String::from_utf8_lossy(body);
    let mut pairs = parse_form(&raw);

    for entry in &config.remove.body {
        form_remove(&mut pairs, split_pair(entry).0);
    }
    for entry in &config.rename.body {
        if let (old, Some(new)) = split_pair(entry) {
            form_rename(&mut pairs, old, new);
        }
    }
    for entry in &config.replace.body {
        if let (name, Some(value)) = split_pair(entry) {
            form_replace(&mut pairs, name, value);
        }
    }
    for entry in &config.add.body {
        if let (name, Some(value)) = split_pair(entry) {
            form_add(&mut pairs, name, value);
        }
    }
    for entry in &config.append.body {
        if let (name, Some(value)) = split_pair(entry) {
            form_append(&mut pairs, name, value);
        }
    }

    Bytes::from(encode_form(&pairs))
}

fn transform_multipart(config: &Config, headers: &HeaderMap, body: &[u8]) -> Bytes {
    let Some(content_type) = headers
        .get(header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
    else {
        return Bytes::copy_from_slice(body);
    };
    let Some(mut multipart) = MultipartBody::parse(content_type, body) else {
        return Bytes::copy_from_slice(body);
    };

    for entry in &config.remove.body {
        multipart.remove(split_pair(entry).0);
    }
    for entry in &config.rename.body {
        if let (old, Some(new)) = split_pair(entry) {
            multipart.rename(old, new);
        }
    }
    for entry in &config.replace.body {
        if let (name, Some(value)) = split_pair(entry) {
            multipart.replace(name, value);
        }
    }
    for entry in &config.add.body {
        if let (name, Some(value)) = split_pair(entry) {
            multipart.add(name, value);
        }
    }

    Bytes::from(multipart.to_bytes())
}

impl octra_api_plugin_types::ApiGatewayBackgroundPlugin for RequestTransformerPlugin {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_invalid_configs() {
        assert!(
            RequestTransformerFactory
                .create("test", json!({"remove": {"headers": ["bad header"]}}))
                .is_err()
        );
        assert!(
            RequestTransformerFactory
                .create("test", json!({"add": {"body": ["missing-value"]}}))
                .is_err()
        );
        assert!(
            RequestTransformerFactory
                .create("test", json!({"http_method": "BREW"}))
                .is_err()
        );
        assert!(
            RequestTransformerFactory
                .create("test", json!({"replace": {"uri": "not a uri"}}))
                .is_err()
        );
    }

    #[test]
    fn accepts_kong_style_configs() {
        let config = json!({
            "http_method": "POST",
            "remove": {"headers": ["x-drop"], "querystring": ["drop"], "body": ["drop"]},
            "rename": {"headers": ["x-old:x-new"], "querystring": ["old:new"], "body": ["old:new"]},
            "replace": {
                "headers": ["x-replace:value"],
                "querystring": ["key:value"],
                "body": ["key:value"],
                "uri": "/rewritten"
            },
            "add": {"headers": ["x-add:value"], "querystring": ["key:value"], "body": ["key:value"]},
            "append": {"body": ["list:value"]}
        });
        assert!(RequestTransformerFactory.create("test", config).is_ok());
    }
}

