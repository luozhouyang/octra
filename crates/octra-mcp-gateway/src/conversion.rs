//! OpenAPI 3.x conversion and HTTP tool execution.
//!
//! A `conversion-listener` or `conversion-only` server stores an OpenAPI
//! document under `config.openapi`. Every path/method operation becomes a
//! generated HTTP tool whose MCP `inputSchema` mirrors the path, query, header,
//! and JSON body inputs. Calls are executed with the shared reqwest client; no
//! MCP upstream connection is opened for these servers.
//!
//! Supported subset: OpenAPI 3.0/3.1 documents in JSON or YAML with inline or
//! component-referenced parameters and request bodies using `application/json`
//! (or a `+json` media type). Cookie parameters, `multipart` and form bodies,
//! and non-local schema references are not converted. Local `$ref` values in
//! schemas are preserved rather than inlined.

use std::collections::HashSet;
use std::time::Duration;

use octra_mcp_core::domain::{McpServer, McpTool, secret_reference, validate_secret_reference};
use octra_mcp_core::protocol::CallToolResult;
use openapiv3::{
    OpenAPI, Operation, Parameter, ParameterSchemaOrContent, PathItem, ReferenceOr, RequestBody,
    Schema,
};
use reqwest::header::{CONTENT_TYPE, HeaderMap, HeaderName, HeaderValue};
use reqwest::{Method, Url};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};

use crate::client::McpError;

/// Timeout for fetching a remote OpenAPI document during create/update/refresh.
pub const OPENAPI_FETCH_TIMEOUT: Duration = Duration::from_secs(10);
/// Upper bound on an OpenAPI document, inline or fetched.
pub const OPENAPI_DOCUMENT_MAX_BYTES: usize = 10 * 1024 * 1024;
/// Upper bound on the body returned by a generated HTTP tool.
pub const HTTP_RESPONSE_MAX_BYTES: usize = 1024 * 1024;
const AUTH_BEARER: &str = "bearer";
const AUTH_HEADER: &str = "header";
const AUTH_QUERY: &str = "query";

/// The `config.openapi` object of a conversion server.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct OpenApiConfig {
    /// Inline JSON or YAML document. Mutually exclusive with `document_url`.
    #[serde(default)]
    pub document: Option<String>,
    /// Remote JSON or YAML document, fetched on create/update/refresh only.
    #[serde(default)]
    pub document_url: Option<String>,
    /// Base URL used when the document's first server URL is relative.
    #[serde(default)]
    pub base_url: Option<String>,
    /// Optional outbound authentication applied to every generated tool.
    #[serde(default)]
    pub auth: Option<OpenApiAuth>,
}

/// Outbound authentication for generated HTTP tools.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct OpenApiAuth {
    /// `bearer`, `header`, or `query`.
    #[serde(rename = "type")]
    pub kind: String,
    /// `env:NAME` or `file:PATH` reference resolved at request time.
    #[serde(default)]
    pub secret_ref: String,
    /// Header name for `bearer`/`header`, or the query key for `query`.
    #[serde(default)]
    pub header: Option<String>,
    /// Alias accepted for the query parameter name.
    #[serde(default)]
    pub param: Option<String>,
}

impl OpenApiAuth {
    /// Normalized kind used for matching.
    pub fn kind(&self) -> String {
        self.kind.trim().to_ascii_lowercase()
    }

    /// Header name for header-style authentication.
    pub fn header_name(&self) -> Option<&str> {
        self.header.as_deref()
    }

    /// Query parameter name for query-style authentication. `header` is
    /// accepted as an alias for `param`.
    pub fn query_param(&self) -> Option<&str> {
        self.param.as_deref().or(self.header.as_deref())
    }

    fn to_json(&self) -> Value {
        let mut object = Map::new();
        object.insert("type".into(), json!(self.kind()));
        object.insert("secret_ref".into(), json!(self.secret_ref));
        if let Some(header) = &self.header {
            object.insert("header".into(), json!(header));
        }
        if let Some(param) = &self.param {
            object.insert("param".into(), json!(param));
        }
        Value::Object(object)
    }
}

impl OpenApiConfig {
    /// Parses `config.openapi`, returning `None` when the key is absent.
    pub fn from_config(config: &Value) -> Result<Option<Self>, String> {
        let Some(openapi) = config.get("openapi") else {
            return Ok(None);
        };
        let parsed: Self = serde_json::from_value(openapi.clone())
            .map_err(|err| format!("invalid config.openapi: {err}"))?;
        parsed.validate()?;
        Ok(Some(parsed))
    }

    /// Validates the document source, base URL, and auth settings.
    pub fn validate(&self) -> Result<(), String> {
        let document = self
            .document
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty());
        let document_url = self
            .document_url
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty());
        match (document, document_url) {
            (Some(_), Some(_)) => {
                return Err(
                    "config.openapi must set exactly one of document and document_url".into(),
                );
            }
            (None, None) => {
                return Err("config.openapi requires document or document_url".into());
            }
            _ => {}
        }
        if let Some(url) = document_url {
            validate_absolute_http_url("config.openapi.document_url", url)?;
        }
        if let Some(base_url) = self.base_url.as_deref() {
            let base_url = base_url.trim();
            if !base_url.is_empty() {
                validate_absolute_http_url("config.openapi.base_url", base_url)?;
            }
        }
        if let Some(auth) = &self.auth {
            auth.validate()?;
        }
        Ok(())
    }

    /// The effective configured base URL, if one was supplied.
    pub fn base_url(&self) -> Option<&str> {
        self.base_url
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
    }
}

impl OpenApiAuth {
    fn validate(&self) -> Result<(), String> {
        let kind = self.kind();
        if !matches!(kind.as_str(), AUTH_BEARER | AUTH_HEADER | AUTH_QUERY) {
            return Err(format!(
                "config.openapi.auth.type must be bearer, header, or query, got '{}'",
                self.kind
            ));
        }
        if self.secret_ref.trim().is_empty() {
            return Err("config.openapi.auth.secret_ref must not be empty".into());
        }
        validate_secret_reference(self.secret_ref.trim())
            .map_err(|err| format!("config.openapi.auth.secret_ref: {err}"))?;
        let scheme = self
            .secret_ref
            .split_once(':')
            .map(|(scheme, _)| scheme)
            .unwrap_or_default();
        if !matches!(scheme, "env" | "file") {
            return Err(
                "config.openapi.auth.secret_ref must use an env: or file: reference".into(),
            );
        }
        match kind.as_str() {
            AUTH_HEADER => {
                self.header_name()
                    .filter(|name| !name.trim().is_empty())
                    .ok_or_else(|| {
                        "config.openapi.auth.header is required for type 'header'".to_string()
                    })?;
            }
            AUTH_QUERY => {
                self.query_param()
                    .filter(|name| !name.trim().is_empty())
                    .ok_or_else(|| {
                        "config.openapi.auth.param is required for type 'query'".to_string()
                    })?;
            }
            _ => {}
        }
        Ok(())
    }
}

/// One operation converted into an MCP tool definition.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ConvertedTool {
    pub name: String,
    pub description: String,
    pub schema: Value,
    pub http_method: String,
    pub http_path: String,
    /// Stored in `mcp_tools.http_config_json` once a server is known.
    pub http_config: Value,
}

impl ConvertedTool {
    /// Converts the generated definition into a repository upsert record.
    pub fn into_discovered(self) -> octra_storage::mcp::DiscoveredTool {
        octra_storage::mcp::DiscoveredTool::http(
            self.name,
            self.description,
            self.schema,
            self.http_method,
            self.http_path,
            self.http_config,
        )
    }
}

/// Parses an OpenAPI 3.x JSON or YAML document without server-level config.
///
/// When the document has no absolute `servers[0].url`, generated tools carry
/// the relative URL (or an empty base). Server conversion validates that a
/// configured `base_url` fills the gap before persisting tools.
pub fn parse_openapi(document: &str) -> Result<Vec<ConvertedTool>, String> {
    let openapi = parse_document(document)?;
    // The parser itself is lenient about a missing base URL; server
    // conversion validates it before persisting generated tools.
    let base_url = effective_base(&openapi, Some("")).unwrap_or_default();
    convert_parsed(openapi, &base_url)
}

fn convert_parsed(openapi: OpenAPI, base_url: &str) -> Result<Vec<ConvertedTool>, String> {
    let mut tools = Vec::new();
    let mut used_names = HashSet::new();

    for (path, item) in openapi.paths.iter() {
        let Some(item) = item.as_item() else {
            continue;
        };
        for (method, operation) in item.iter() {
            let mut converted =
                convert_operation(&openapi, path, method, item, operation, base_url);
            converted.name = unique_name(&mut used_names, converted.name);
            tools.push(converted);
        }
    }

    if tools.is_empty() {
        return Err("OpenAPI document contains no operations".into());
    }
    Ok(tools)
}

/// Builds persistable tools for a conversion server, applying its base URL and
/// outbound auth. Errors are safe to return to control-plane callers.
pub fn build_tools(config: &OpenApiConfig, document: &str) -> Result<Vec<ConvertedTool>, String> {
    let openapi = parse_document(document)?;
    let base_url = effective_base(&openapi, config.base_url())?;
    let auth = config.auth.as_ref().map(OpenApiAuth::to_json);
    let mut tools = convert_parsed(openapi, &base_url)?;
    if let Some(auth) = auth {
        for tool in &mut tools {
            if let Some(object) = tool.http_config.as_object_mut() {
                object.insert("auth".into(), auth.clone());
            }
        }
    }
    Ok(tools)
}

/// Loads the inline document or fetches `document_url` once.
pub async fn load_document(
    http: &reqwest::Client,
    config: &OpenApiConfig,
) -> Result<String, String> {
    if let Some(document) = &config.document {
        return Ok(document.clone());
    }
    let url = config
        .document_url
        .as_deref()
        .map(str::trim)
        .filter(|url| !url.is_empty())
        .ok_or_else(|| "config.openapi requires document or document_url".to_string())?;

    let fetched = tokio::time::timeout(OPENAPI_FETCH_TIMEOUT, async {
        let mut response = http.get(url).send().await.map_err(fetch_error)?;
        let status = response.status();
        if !status.is_success() {
            return Err(format!(
                "OpenAPI document_url returned HTTP {}",
                status.as_u16()
            ));
        }
        let (bytes, truncated) = read_response_bytes(&mut response, OPENAPI_DOCUMENT_MAX_BYTES)
            .await
            .map_err(fetch_error)?;
        if truncated {
            return Err(format!(
                "OpenAPI document_url exceeds {OPENAPI_DOCUMENT_MAX_BYTES} bytes"
            ));
        }
        String::from_utf8(bytes)
            .map_err(|_| "OpenAPI document_url returned a non-UTF-8 body".to_string())
    })
    .await
    .map_err(|_| "timed out fetching OpenAPI document_url".to_string())??;

    Ok(fetched)
}

/// Generates the full tool set for a server's stored configuration.
pub async fn generate_tools(
    http: &reqwest::Client,
    config: &OpenApiConfig,
) -> Result<Vec<ConvertedTool>, String> {
    let document = load_document(http, config).await?;
    build_tools(config, &document)
}

// ----- parsing -----------------------------------------------------------

fn parse_document(document: &str) -> Result<OpenAPI, String> {
    let trimmed = document.trim();
    if trimmed.is_empty() {
        return Err("OpenAPI document is empty".into());
    }

    let json_error = match serde_json::from_str::<OpenAPI>(trimmed) {
        Ok(openapi) => return validate_version(openapi),
        Err(err) => err,
    };

    let converted = serde_yaml::from_str::<serde_yaml::Value>(trimmed)
        .map_err(|yaml_error| {
            format!("invalid OpenAPI document: JSON error: {json_error}; YAML error: {yaml_error}")
        })
        .and_then(|value| {
            serde_json::to_value(value).map_err(|err| format!("invalid YAML document: {err}"))
        })
        .and_then(|value| {
            serde_json::from_value::<OpenAPI>(value)
                .map_err(|err| format!("invalid OpenAPI document: {err}"))
        })?;
    validate_version(converted)
}

fn validate_version(openapi: OpenAPI) -> Result<OpenAPI, String> {
    let version = openapi.openapi.trim();
    let supported = version
        .split_once('.')
        .is_some_and(|(major, _)| major == "3");
    if !supported {
        return Err(format!(
            "unsupported OpenAPI version '{version}'; expected an OpenAPI 3.x document"
        ));
    }
    Ok(openapi)
}

fn effective_base(openapi: &OpenAPI, fallback: Option<&str>) -> Result<String, String> {
    let fallback = fallback
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string);

    let Some(server) = openapi.servers.first() else {
        return fallback.ok_or_else(|| {
            "config.openapi.base_url is required when the document has no servers[0].url"
                .to_string()
        });
    };
    let server_url = substitute_server_variables(&server.url, server);
    if is_absolute_http_url(&server_url) {
        return Ok(server_url.trim_end_matches('/').to_string());
    }
    let base = fallback.ok_or_else(|| {
        "config.openapi.base_url is required when servers[0].url is relative".to_string()
    })?;
    Ok(join_url(&base, &server_url))
}

fn substitute_server_variables(url: &str, server: &openapiv3::Server) -> String {
    let mut resolved = url.to_string();
    if let Some(variables) = &server.variables {
        for (name, variable) in variables {
            resolved = resolved.replace(&format!("{{{name}}}"), &variable.default);
        }
    }
    resolved
}

fn convert_operation(
    openapi: &OpenAPI,
    path: &str,
    method: &str,
    path_item: &PathItem,
    operation: &Operation,
    base_url: &str,
) -> ConvertedTool {
    let mut properties = Map::new();
    let mut required = Vec::new();
    let mut parameter_mappings = Vec::new();

    for parameter in combined_parameters(openapi, path_item, operation)
        .into_iter()
        .filter_map(|parameter| resolve_parameter(openapi, parameter))
    {
        let data = parameter.parameter_data_ref();
        let location = parameter_location(parameter);
        let prefix = match location {
            "path" => "path",
            "query" => "query",
            "header" => "header",
            _ => continue,
        };
        let arg = format!("{prefix}_{}", data.name);
        properties.insert(arg.clone(), parameter_schema(parameter));
        let is_required = location == "path" || data.required;
        if is_required {
            required.push(arg.clone());
        }
        parameter_mappings.push(json!({
            "arg": arg,
            "name": data.name,
            "in": location,
            "required": is_required,
        }));
    }

    let mut has_body = false;
    let mut body_required = false;
    let mut content_type = None;
    if let Some(request_body) = operation
        .request_body
        .as_ref()
        .and_then(|request_body| resolve_request_body(openapi, request_body))
        && let Some((schema, media_type)) = json_request_body(request_body)
    {
        properties.insert("body".into(), schema);
        has_body = true;
        body_required = request_body.required;
        if body_required {
            required.push("body".into());
        }
        content_type = Some(media_type);
    }

    let description = [
        operation.summary.as_deref(),
        operation.description.as_deref(),
    ]
    .into_iter()
    .flatten()
    .map(str::trim)
    .filter(|value| !value.is_empty())
    .collect::<Vec<_>>()
    .join("\n\n");

    let mut input_schema = Map::new();
    input_schema.insert("type".into(), json!("object"));
    input_schema.insert("properties".into(), Value::Object(properties));
    if !required.is_empty() {
        input_schema.insert("required".into(), json!(required));
    }

    let mut http_config = Map::new();
    http_config.insert("base_url".into(), json!(base_url));
    http_config.insert("parameters".into(), Value::Array(parameter_mappings));
    http_config.insert("body".into(), json!(has_body));
    http_config.insert("body_required".into(), json!(body_required));
    if let Some(content_type) = content_type {
        http_config.insert("content_type".into(), json!(content_type));
    }

    ConvertedTool {
        name: tool_name(operation, method, path),
        description,
        schema: Value::Object(input_schema),
        http_method: method.to_ascii_uppercase(),
        http_path: path.to_string(),
        http_config: Value::Object(http_config),
    }
}

fn combined_parameters<'a>(
    openapi: &OpenAPI,
    path_item: &'a PathItem,
    operation: &'a Operation,
) -> Vec<&'a ReferenceOr<Parameter>> {
    let mut resolved: Vec<&ReferenceOr<Parameter>> = Vec::new();
    for parameter in path_item
        .parameters
        .iter()
        .chain(operation.parameters.iter())
    {
        let Some((name, location)) = parameter_key(openapi, parameter) else {
            resolved.push(parameter);
            continue;
        };
        if let Some(existing) = resolved.iter_mut().find(|existing| {
            parameter_key(openapi, existing).is_some_and(|(existing_name, existing_location)| {
                existing_name == name && existing_location == location
            })
        }) {
            *existing = parameter;
        } else {
            resolved.push(parameter);
        }
    }
    resolved
}

fn parameter_key(
    openapi: &OpenAPI,
    parameter: &ReferenceOr<Parameter>,
) -> Option<(String, String)> {
    let parameter = resolve_parameter(openapi, parameter)?;
    let data = parameter.parameter_data_ref();
    Some((data.name.clone(), parameter_location(parameter).to_string()))
}

fn resolve_parameter<'a>(
    openapi: &'a OpenAPI,
    parameter: &'a ReferenceOr<Parameter>,
) -> Option<&'a Parameter> {
    match parameter {
        ReferenceOr::Item(parameter) => Some(parameter),
        ReferenceOr::Reference { reference } => {
            let name = component_name(reference, "#/components/parameters/")?;
            openapi.components.as_ref()?.parameters.get(name)?.as_item()
        }
    }
}

fn resolve_request_body<'a>(
    openapi: &'a OpenAPI,
    request_body: &'a ReferenceOr<RequestBody>,
) -> Option<&'a RequestBody> {
    match request_body {
        ReferenceOr::Item(request_body) => Some(request_body),
        ReferenceOr::Reference { reference } => {
            let name = component_name(reference, "#/components/requestBodies/")?;
            openapi
                .components
                .as_ref()?
                .request_bodies
                .get(name)?
                .as_item()
        }
    }
}

fn component_name<'a>(reference: &'a str, prefix: &str) -> Option<&'a str> {
    reference
        .strip_prefix(prefix)
        .filter(|name| !name.is_empty() && !name.contains('/'))
}

fn parameter_location(parameter: &Parameter) -> &'static str {
    match parameter {
        Parameter::Query { .. } => "query",
        Parameter::Header { .. } => "header",
        Parameter::Path { .. } => "path",
        Parameter::Cookie { .. } => "cookie",
    }
}

fn parameter_schema(parameter: &Parameter) -> Value {
    match &parameter.parameter_data_ref().format {
        ParameterSchemaOrContent::Schema(schema) => schema_value(schema),
        ParameterSchemaOrContent::Content(content) => content
            .values()
            .next()
            .and_then(|media| media.schema.as_ref())
            .map(schema_value)
            .unwrap_or_else(|| json!({"type": "string"})),
    }
}

fn schema_value(schema: &ReferenceOr<Schema>) -> Value {
    match schema {
        ReferenceOr::Reference { reference } => json!({"$ref": reference}),
        ReferenceOr::Item(schema) => {
            serde_json::to_value(schema).unwrap_or_else(|_| json!({"type": "object"}))
        }
    }
}

fn json_request_body(request_body: &RequestBody) -> Option<(Value, String)> {
    request_body
        .content
        .iter()
        .find(|(media_type, _)| is_json_media_type(media_type))
        .map(|(media_type, media)| {
            let schema = media
                .schema
                .as_ref()
                .map(schema_value)
                .unwrap_or_else(|| json!({}));
            (schema, media_type.clone())
        })
}

fn is_json_media_type(media_type: &str) -> bool {
    let essence = media_type
        .split(';')
        .next()
        .unwrap_or_default()
        .trim()
        .to_ascii_lowercase();
    essence == "application/json" || essence.ends_with("+json")
}

fn tool_name(operation: &Operation, method: &str, path: &str) -> String {
    let candidate = operation
        .operation_id
        .as_deref()
        .map(sanitize_name)
        .filter(|name| !name.is_empty())
        .unwrap_or_else(|| sanitize_name(&format!("{method}_{path}")));
    if candidate.is_empty() {
        "tool".to_string()
    } else {
        candidate
    }
}

fn sanitize_name(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    for character in raw.chars() {
        if character.is_ascii_alphanumeric() || matches!(character, '-' | '_') {
            out.push(character);
        } else if !out.ends_with('_') {
            out.push('_');
        }
    }
    let out = out.trim_matches('_').replace("__", "_");
    let mut out = out.chars().take(120).collect::<String>();
    if !out
        .chars()
        .next()
        .is_some_and(|first| first.is_ascii_alphanumeric())
    {
        out = format!("tool_{out}");
    }
    out
}

fn unique_name(used: &mut HashSet<String>, candidate: String) -> String {
    if used.insert(candidate.clone()) {
        return candidate;
    }
    for suffix in 2..=u32::MAX {
        let trimmed = candidate.chars().take(120).collect::<String>();
        let name = format!("{trimmed}_{suffix}");
        if used.insert(name.clone()) {
            return name;
        }
    }
    format!("{candidate}_duplicate")
}

// ----- HTTP execution ----------------------------------------------------

/// A concrete outbound HTTP request derived from one tool call.
#[derive(Debug, Clone, PartialEq)]
pub struct HttpRequestPlan {
    pub method: String,
    pub url: String,
    pub headers: Vec<(String, String)>,
    pub body: Option<Value>,
}

/// Builds the outbound request for a generated HTTP tool.
///
/// Path arguments are percent-encoded into `{name}` placeholders, query
/// arguments are appended to the URL, header arguments become request headers,
/// and the optional `body` argument is serialized as JSON. Authentication is
/// applied last so it cannot be overridden by caller-supplied headers.
pub fn build_http_request(tool: &McpTool, arguments: &Value) -> Result<HttpRequestPlan, String> {
    if !tool.is_http() {
        return Err(format!("tool '{}' is not an HTTP tool", tool.name));
    }
    let method = tool
        .http_method
        .as_deref()
        .ok_or_else(|| format!("HTTP tool '{}' has no method", tool.name))?;
    let path = tool
        .http_path
        .as_deref()
        .ok_or_else(|| format!("HTTP tool '{}' has no path", tool.name))?;
    let config = tool
        .http_config
        .as_object()
        .ok_or_else(|| format!("HTTP tool '{}' has invalid http_config", tool.name))?;
    let base_url = config
        .get("base_url")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| format!("HTTP tool '{}' has no base_url", tool.name))?;

    let empty = Map::new();
    let arguments = match arguments {
        Value::Null => &empty,
        Value::Object(arguments) => arguments,
        _ => return Err("tool arguments must be a JSON object".into()),
    };

    let mut path = path.to_string();
    let mut query = Vec::new();
    let mut headers = Vec::new();
    let mut body = None;

    let parameters = config
        .get("parameters")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    for parameter in parameters {
        let Some(arg) = parameter.get("arg").and_then(Value::as_str) else {
            continue;
        };
        let Some(name) = parameter.get("name").and_then(Value::as_str) else {
            continue;
        };
        let location = parameter
            .get("in")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let required = parameter
            .get("required")
            .and_then(Value::as_bool)
            .unwrap_or(location == "path");
        let value = arguments.get(arg).filter(|value| !value.is_null());
        if value.is_none() && required {
            return Err(format!("missing required argument '{arg}'"));
        }
        let Some(value) = value else {
            continue;
        };

        match location {
            "path" => {
                let encoded = percent_encode(&join_scalars(value));
                path = path.replace(&format!("{{{name}}}"), &encoded);
            }
            "query" => {
                for value in scalar_values(value) {
                    query.push((name.to_string(), value));
                }
            }
            "header" => {
                let value = join_scalars(value);
                set_header(&mut headers, name, &value)?;
            }
            _ => {}
        }
    }

    if config.get("body").and_then(Value::as_bool).unwrap_or(false) {
        let body_required = config
            .get("body_required")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        match arguments.get("body").filter(|value| !value.is_null()) {
            Some(value) => body = Some(value.clone()),
            None if body_required => return Err("missing required argument 'body'".into()),
            None => {}
        }
        if body.is_some() {
            let content_type = config
                .get("content_type")
                .and_then(Value::as_str)
                .unwrap_or("application/json");
            set_header(&mut headers, CONTENT_TYPE.as_str(), content_type)?;
        }
    }

    if let Some(auth) = config.get("auth").filter(|value| !value.is_null()) {
        let auth: OpenApiAuth = serde_json::from_value(auth.clone())
            .map_err(|err| format!("invalid HTTP tool auth config: {err}"))?;
        let secret = resolve_secret_reference(auth.secret_ref.trim())
            .map_err(|err| format!("cannot resolve auth secret: {err}"))?;
        match auth.kind().as_str() {
            AUTH_BEARER => {
                let header = auth
                    .header
                    .as_deref()
                    .map(str::trim)
                    .filter(|name| !name.is_empty())
                    .unwrap_or("Authorization");
                set_header(&mut headers, header, &format!("Bearer {secret}"))?;
            }
            AUTH_HEADER => {
                let header = auth
                    .header
                    .as_deref()
                    .map(str::trim)
                    .filter(|name| !name.is_empty())
                    .ok_or_else(|| "HTTP tool auth is missing a header name".to_string())?;
                set_header(&mut headers, header, &secret)?;
            }
            AUTH_QUERY => {
                let param = auth
                    .query_param()
                    .map(str::trim)
                    .filter(|name| !name.is_empty())
                    .ok_or_else(|| {
                        "HTTP tool auth is missing a query parameter name".to_string()
                    })?;
                query.push((param.to_string(), secret));
            }
            other => return Err(format!("unsupported HTTP tool auth type '{other}'")),
        }
    }

    let url = join_url(base_url, &path);
    let mut url = Url::parse(&url).map_err(|err| format!("invalid HTTP tool URL: {err}"))?;
    if !query.is_empty() {
        let mut pairs = url.query_pairs_mut();
        for (name, value) in &query {
            pairs.append_pair(name, value);
        }
    }

    Ok(HttpRequestPlan {
        method: method.to_ascii_uppercase(),
        url: url.to_string(),
        headers,
        body,
    })
}

fn set_header(headers: &mut Vec<(String, String)>, name: &str, value: &str) -> Result<(), String> {
    let name = HeaderName::from_bytes(name.trim().as_bytes())
        .map_err(|_| format!("invalid header name '{name}'"))?;
    HeaderValue::from_str(value).map_err(|_| format!("invalid value for header '{name}'"))?;
    headers.retain(|(existing, _)| !existing.eq_ignore_ascii_case(name.as_str()));
    headers.push((name.as_str().to_string(), value.to_string()));
    Ok(())
}

fn scalar_values(value: &Value) -> Vec<String> {
    match value {
        Value::Null => Vec::new(),
        Value::Array(items) => items.iter().flat_map(scalar_values).collect(),
        Value::String(value) => vec![value.clone()],
        Value::Bool(value) => vec![value.to_string()],
        Value::Number(value) => vec![value.to_string()],
        Value::Object(_) => vec![value.to_string()],
    }
}

fn join_scalars(value: &Value) -> String {
    scalar_values(value).join(",")
}

fn percent_encode(value: &str) -> String {
    let mut encoded = String::with_capacity(value.len());
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~') {
            encoded.push(byte as char);
        } else {
            encoded.push_str(&format!("%{byte:02X}"));
        }
    }
    encoded
}

/// Resolves `env:` and `file:` secret references. Values never leave the
/// gateway host and are used only in outbound requests.
pub fn resolve_secret_reference(reference: &str) -> Result<String, String> {
    let (scheme, name) = reference
        .split_once(':')
        .ok_or_else(|| format!("invalid secret reference: {reference}"))?;
    match scheme {
        "env" => std::env::var(name)
            .map_err(|_| format!("environment variable {name} referenced by a secret is not set")),
        "file" => std::fs::read_to_string(name)
            .map(|value| value.trim_end_matches(['\r', '\n']).to_string())
            .map_err(|err| format!("cannot read secret file {name}: {err}")),
        other => Err(format!(
            "{other}: secret references cannot be resolved from the gateway host"
        )),
    }
}

/// Executes a generated HTTP tool and maps the response into MCP content.
///
/// Non-2xx responses are success-shaped tool results with `isError: true` and
/// a text block containing the status and bounded body, rather than JSON-RPC
/// errors; transport failures and timeouts stay JSON-RPC errors.
pub async fn execute_http_tool(
    http: &reqwest::Client,
    server: &McpServer,
    tool: &McpTool,
    arguments: Option<Value>,
    timeout: Duration,
) -> Result<CallToolResult, McpError> {
    let plan = build_http_request(tool, arguments.as_ref().unwrap_or(&Value::Null))
        .map_err(McpError::Config)?;
    let method = Method::from_bytes(plan.method.as_bytes())
        .map_err(|_| McpError::Config(format!("invalid HTTP method '{}'", plan.method)))?;

    let mut headers = HeaderMap::new();
    for (name, value) in &server.headers {
        let Some(raw) = value.as_str() else {
            continue;
        };
        let resolved = match secret_reference(value) {
            Some(reference) => match resolve_secret_reference(reference) {
                Ok(value) => Some(value),
                Err(err) => {
                    tracing::warn!(header = %name, error = %err, "skipping MCP server header");
                    None
                }
            },
            None => Some(raw.to_string()),
        };
        let Some(value) = resolved else { continue };
        let Ok(name) = HeaderName::from_bytes(name.as_bytes()) else {
            continue;
        };
        if let Ok(value) = HeaderValue::from_str(&value) {
            headers.insert(name, value);
        }
    }
    for (name, value) in &plan.headers {
        let Ok(name) = HeaderName::from_bytes(name.as_bytes()) else {
            continue;
        };
        let Ok(value) = HeaderValue::from_str(value) else {
            continue;
        };
        headers.insert(name, value);
    }

    let mut builder = http
        .request(method, &plan.url)
        .timeout(timeout)
        .headers(headers);
    if let Some(body) = &plan.body {
        let encoded = serde_json::to_vec(body)
            .map_err(|err| McpError::Internal(format!("cannot encode HTTP tool body: {err}")))?;
        builder = builder.body(encoded);
    }

    let exchange = async {
        let mut response = builder.send().await.map_err(transport_error)?;
        let status = response.status();
        let (bytes, truncated) = read_response_bytes(&mut response, HTTP_RESPONSE_MAX_BYTES)
            .await
            .map_err(transport_error)?;
        Ok::<_, McpError>((status, bytes, truncated))
    };
    let (status, mut bytes, truncated) = tokio::time::timeout(timeout, exchange)
        .await
        .map_err(|_| McpError::Timeout)??;

    tracing::info!(
        server = %server.name,
        tool = %tool.name,
        method = %plan.method,
        path = %tool.http_path.as_deref().unwrap_or_default(),
        status = status.as_u16(),
        "executed generated HTTP tool"
    );

    let truncated_suffix = if truncated { "\n[truncated]" } else { "" };
    if truncated {
        // Keep the returned text within the advertised bound even with the
        // truncation marker appended.
        let keep = HTTP_RESPONSE_MAX_BYTES.saturating_sub(truncated_suffix.len());
        bytes.truncate(keep);
    }
    let body_text = String::from_utf8_lossy(&bytes).into_owned();

    if !status.is_success() {
        let body = format!("{body_text}{truncated_suffix}");
        return Ok(CallToolResult {
            content: vec![json!({
                "type": "text",
                "text": format!("HTTP {}: {body}", status.as_u16()),
            })],
            structured_content: Some(json!({
                "status": status.as_u16(),
                "body": body,
            })),
            is_error: Some(true),
            extra: Map::new(),
        });
    }

    let parsed = if bytes.is_empty() {
        None
    } else {
        serde_json::from_slice::<Value>(&bytes).ok()
    };
    if let Some(value) = parsed {
        let text = serde_json::to_string(&value).unwrap_or_else(|_| body_text.clone());
        Ok(CallToolResult {
            content: vec![json!({"type": "text", "text": format!("{text}{truncated_suffix}")})],
            structured_content: Some(value),
            is_error: None,
            extra: Map::new(),
        })
    } else {
        Ok(CallToolResult {
            content: vec![json!({
                "type": "text",
                "text": format!("{body_text}{truncated_suffix}"),
            })],
            structured_content: None,
            is_error: None,
            extra: Map::new(),
        })
    }
}

async fn read_response_bytes(
    response: &mut reqwest::Response,
    limit: usize,
) -> Result<(Vec<u8>, bool), reqwest::Error> {
    let mut buffer = Vec::new();
    let mut truncated = false;
    while let Some(chunk) = response.chunk().await? {
        if buffer.len() + chunk.len() > limit {
            let take = limit.saturating_sub(buffer.len());
            buffer.extend_from_slice(&chunk[..take]);
            truncated = true;
            break;
        }
        buffer.extend_from_slice(&chunk);
    }
    Ok((buffer, truncated))
}

fn transport_error(err: reqwest::Error) -> McpError {
    if err.is_timeout() {
        McpError::Timeout
    } else {
        McpError::Transport(format!("HTTP tool request failed: {}", err.without_url()))
    }
}

fn fetch_error(err: reqwest::Error) -> String {
    if err.is_timeout() {
        "timed out fetching OpenAPI document_url".to_string()
    } else {
        format!("cannot fetch OpenAPI document_url: {}", err.without_url())
    }
}

fn validate_absolute_http_url(field: &str, value: &str) -> Result<(), String> {
    if value.trim() != value || value.chars().any(char::is_whitespace) {
        return Err(format!("{field} must be an absolute http(s) URL"));
    }
    let lower = value.to_ascii_lowercase();
    let rest = lower
        .strip_prefix("http://")
        .or_else(|| lower.strip_prefix("https://"))
        .ok_or_else(|| format!("{field} must be an absolute http(s) URL"))?;
    let authority = rest.split(['/', '?', '#']).next().unwrap_or_default();
    if authority.is_empty() {
        return Err(format!("{field} must include a host"));
    }
    Ok(())
}

fn is_absolute_http_url(value: &str) -> bool {
    let lower = value.to_ascii_lowercase();
    lower.starts_with("http://") || lower.starts_with("https://")
}

fn join_url(base: &str, path: &str) -> String {
    let base = base.trim_end_matches('/');
    let path = path.trim_start_matches('/');
    if path.is_empty() {
        base.to_string()
    } else {
        format!("{base}/{path}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use octra_mcp_core::domain::{MCP_TOOL_KIND_HTTP, MCP_TOOL_KIND_MCP};

    fn openapi_json() -> String {
        json!({
            "openapi": "3.0.3",
            "info": {"title": "Pets", "version": "1.0.0"},
            "servers": [{"url": "https://api.example.test/v1"}],
            "paths": {
                "/pets/{petId}": {
                    "get": {
                        "operationId": "getPet",
                        "summary": "Get a pet",
                        "description": "Returns one pet",
                        "parameters": [
                            {"name": "petId", "in": "path", "required": true, "schema": {"type": "string"}},
                            {"name": "verbose", "in": "query", "schema": {"type": "boolean"}},
                            {"name": "X-Tenant", "in": "header", "required": true, "schema": {"type": "string"}}
                        ],
                        "responses": {"200": {"description": "ok"}}
                    }
                },
                "/pets": {
                    "post": {
                        "operationId": "create pet!",
                        "requestBody": {
                            "required": true,
                            "content": {
                                "application/json": {
                                    "schema": {
                                        "type": "object",
                                        "required": ["name"],
                                        "properties": {"name": {"type": "string"}}
                                    }
                                }
                            }
                        },
                        "responses": {"201": {"description": "created"}}
                    }
                }
            }
        })
        .to_string()
    }

    #[test]
    fn parses_json_path_query_header_and_body_inputs() {
        let tools = parse_openapi(&openapi_json()).unwrap();
        assert_eq!(tools.len(), 2);
        let get = tools.iter().find(|tool| tool.name == "getPet").unwrap();
        assert_eq!(get.http_method, "GET");
        assert_eq!(get.http_path, "/pets/{petId}");
        assert_eq!(get.description, "Get a pet\n\nReturns one pet");
        assert_eq!(
            get.schema["properties"]["path_petId"],
            json!({"type": "string"})
        );
        assert_eq!(
            get.schema["properties"]["query_verbose"],
            json!({"type": "boolean"})
        );
        assert_eq!(
            get.schema["properties"]["header_X-Tenant"],
            json!({"type": "string"})
        );
        assert_eq!(
            get.schema["required"],
            json!(["path_petId", "header_X-Tenant"])
        );
        assert_eq!(get.http_config["base_url"], "https://api.example.test/v1");
        assert_eq!(get.http_config["parameters"][0]["arg"], "path_petId");
        assert_eq!(get.http_config["parameters"][1]["in"], "query");
        assert_eq!(get.http_config["body"], false);

        let post = tools.iter().find(|tool| tool.name == "create_pet").unwrap();
        assert_eq!(post.http_method, "POST");
        assert_eq!(post.schema["properties"]["body"]["type"], "object");
        assert_eq!(post.schema["required"], json!(["body"]));
        assert_eq!(post.http_config["body"], true);
        assert_eq!(post.http_config["content_type"], "application/json");
    }

    #[test]
    fn parses_yaml_and_resolves_a_relative_server_against_the_config() {
        let yaml = r#"
openapi: 3.0.3
info:
  title: Pets
  version: 1.0.0
servers:
  - url: /v1
paths:
  /pets:
    get:
      operationId: listPets
      responses:
        '200':
          description: ok
"#;
        let config = OpenApiConfig {
            document: Some(yaml.to_string()),
            base_url: Some("https://api.example.test".into()),
            ..OpenApiConfig::default()
        };
        let tools = build_tools(&config, yaml).unwrap();
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0].name, "listPets");
        assert_eq!(
            tools[0].http_config["base_url"],
            "https://api.example.test/v1"
        );

        // Without a base the server-level generation is rejected.
        let config = OpenApiConfig {
            document: Some(yaml.to_string()),
            ..OpenApiConfig::default()
        };
        let error = build_tools(&config, yaml).unwrap_err();
        assert!(error.contains("base_url is required"), "{error}");
    }

    #[test]
    fn disambiguates_duplicate_operation_ids_and_falls_back_to_method_path() {
        let document = json!({
            "openapi": "3.0.3",
            "info": {"title": "Dup", "version": "1"},
            "paths": {
                "/a": {"get": {"operationId": "same", "responses": {"200": {"description": "ok"}}}},
                "/b": {"post": {"operationId": "same", "responses": {"200": {"description": "ok"}}}},
                "/c": {"delete": {"responses": {"204": {"description": "gone"}}}}
            }
        })
        .to_string();
        let tools = parse_openapi(&document).unwrap();
        assert_eq!(
            tools
                .iter()
                .map(|tool| tool.name.as_str())
                .collect::<Vec<_>>(),
            vec!["same", "same_2", "delete_c"]
        );
    }

    #[test]
    fn accepts_openapi_31_documents_with_30_compatible_schemas() {
        let document = json!({
            "openapi": "3.1.0",
            "info": {"title": "Next", "version": "1"},
            "servers": [{"url": "https://api.example.test"}],
            "paths": {
                "/next": {
                    "get": {
                        "operationId": "nextTool",
                        "responses": {"200": {"description": "ok"}}
                    }
                }
            }
        })
        .to_string();
        let tools = parse_openapi(&document).unwrap();
        assert_eq!(tools[0].name, "nextTool");
    }

    #[test]
    fn rejects_invalid_empty_and_unsupported_documents() {
        for document in [
            "",
            "not an openapi document",
            r#"{"openapi":"2.0","info":{"title":"x","version":"1"},"paths":{}}"#,
            r#"{"openapi":"3.0.3","info":{"title":"x","version":"1"},"paths":{}}"#,
        ] {
            assert!(
                parse_openapi(document).is_err(),
                "document should be rejected: {document}"
            );
        }
    }

    #[test]
    fn validates_openapi_config_shape_and_auth() {
        let config = json!({
            "openapi": {
                "document": "{}",
                "document_url": "https://example.test/openapi.json"
            }
        });
        assert!(OpenApiConfig::from_config(&config).is_err());

        let config = json!({
            "openapi": {
                "document": "{}",
                "auth": {"type": "query", "secret_ref": "env:TOKEN"}
            }
        });
        assert!(OpenApiConfig::from_config(&config).is_err());

        let config = json!({
            "openapi": {
                "document": "{}",
                "auth": {"type": "header", "secret_ref": "keyring:token", "header": "X-Key"}
            }
        });
        assert!(OpenApiConfig::from_config(&config).is_err());
    }

    fn http_tool(config: Value) -> McpTool {
        McpTool {
            id: "tool-1".into(),
            server_id: "server-1".into(),
            name: "get_pet".into(),
            description: "Get a pet".into(),
            schema: json!({"type": "object"}),
            kind: MCP_TOOL_KIND_HTTP.into(),
            http_method: Some("GET".into()),
            http_path: Some("/pets/{petId}".into()),
            http_config: config,
            enabled: true,
            created_at: "2026-01-01T00:00:00.000Z".into(),
        }
    }

    #[test]
    fn builds_path_query_header_and_body_requests() {
        let config = json!({
            "base_url": "https://api.example.test",
            "parameters": [
                {"arg": "path_petId", "name": "petId", "in": "path", "required": true},
                {"arg": "query_tags", "name": "tag", "in": "query", "required": false},
                {"arg": "header_X-Tenant", "name": "X-Tenant", "in": "header", "required": true}
            ],
            "body": true,
            "body_required": true,
            "content_type": "application/json"
        });
        let request = build_http_request(
            &http_tool(config.clone()),
            &json!({
                "path_petId": "a b/c",
                "query_tags": ["one", "two"],
                "header_X-Tenant": "team-a",
                "body": {"name": "Fido"}
            }),
        )
        .unwrap();
        assert_eq!(request.method, "GET");
        assert_eq!(
            request.url,
            "https://api.example.test/pets/a%20b%2Fc?tag=one&tag=two"
        );
        assert!(
            request
                .headers
                .iter()
                .any(|(name, value)| name == "x-tenant" && value == "team-a")
        );
        assert_eq!(request.body, Some(json!({"name": "Fido"})));

        let error = build_http_request(&http_tool(config), &json!({"path_petId": "1"}));
        assert!(error.unwrap_err().contains("X-Tenant"));
    }

    #[test]
    fn applies_bearer_and_query_auth_last() {
        let path = std::env::temp_dir().join(format!(
            "octra-openapi-auth-{}-{}",
            std::process::id(),
            uuid::Uuid::new_v4()
        ));
        std::fs::write(&path, "secret-value\n").unwrap();
        let reference = format!("file:{}", path.display());

        let config = json!({
            "base_url": "https://api.example.test",
            "parameters": [],
            "body": false,
            "auth": {"type": "header", "secret_ref": reference.clone(), "header": "X-Api-Key"}
        });
        let request = build_http_request(
            &http_tool(config),
            &json!({"header_X-Api-Key": "caller-value"}),
        )
        .unwrap();
        assert!(
            request
                .headers
                .iter()
                .any(|(name, value)| name == "x-api-key" && value == "secret-value")
        );

        let config = json!({
            "base_url": "https://api.example.test",
            "parameters": [],
            "body": false,
            "auth": {"type": "query", "secret_ref": reference, "param": "token"}
        });
        let request = build_http_request(&http_tool(config), &json!({})).unwrap();
        assert!(request.url.ends_with("?token=secret-value"));

        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn rejects_non_http_tools() {
        let mut tool = http_tool(json!({}));
        tool.kind = MCP_TOOL_KIND_MCP.into();
        assert!(build_http_request(&tool, &json!({})).is_err());
    }
}
