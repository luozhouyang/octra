//! OpenAPI 3.x conversion and HTTP tool execution.
//!
//! A `conversion-listener` or `conversion-only` server stores an OpenAPI
//! document under `config.openapi`. Every path/method operation becomes a
//! generated HTTP tool whose MCP `inputSchema` mirrors the path, query, header,
//! and JSON body inputs. Calls are executed with the shared reqwest client; no
//! MCP upstream connection is opened for these servers.
//!
//! Supported subset: OpenAPI 3.0/3.1 documents in JSON or YAML with path,
//! query, header, and cookie parameters using the OpenAPI styles, request
//! bodies in JSON, form-urlencoded, multipart, or plain text, and external
//! `$ref` values resolved from local files or HTTP(S) URLs. Local component
//! `$ref` values in schemas are preserved rather than inlined.

use std::collections::{HashMap, HashSet};
use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use base64::Engine;
use base64::engine::general_purpose::STANDARD as BASE64_STANDARD;
use octra_mcp_core::domain::{McpServer, McpTool, secret_reference, validate_secret_reference};
use octra_mcp_core::protocol::CallToolResult;
use openapiv3::{
    OpenAPI, Operation, Parameter, ParameterSchemaOrContent, PathItem, PathStyle, QueryStyle,
    ReferenceOr, RequestBody, Schema, SchemaKind, Type,
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
/// Maximum external reference hops while resolving one document.
pub const OPENAPI_REF_MAX_DEPTH: usize = 8;
/// Maximum documents (root plus external) loaded while resolving one parse.
pub const OPENAPI_REF_MAX_DOCUMENTS: usize = 16;
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
    /// Directory used to resolve relative external `$ref` paths when the
    /// document itself is inline. Defaults to the location of `document_url`.
    #[serde(default, alias = "base_dir", alias = "document_base")]
    pub document_base_dir: Option<String>,
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
        if let Some(base_dir) = self
            .document_base_dir
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
            && base_dir.contains('\0')
        {
            return Err("config.openapi.document_base_dir is not a valid path".into());
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

    /// The configured directory for relative external references.
    pub fn document_base_dir(&self) -> Option<&str> {
        self.document_base_dir
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
    let value = parse_document_value(document)?;
    // `parse_openapi` has no configured document location, so it cannot
    // resolve external references. Reject them instead of silently dropping
    // operations whose path item or parameter stays unresolved.
    reject_external_refs(&value)?;
    let openapi = openapi_from_value(value)?;
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
                convert_operation(&openapi, path, method, item, operation, base_url)?;
            converted.name = unique_name(&mut used_names, converted.name);
            tools.push(converted);
        }
    }

    if tools.is_empty() {
        return Err("OpenAPI document contains no operations".into());
    }
    Ok(tools)
}

/// Builds persistable tools for a conversion server without an HTTP client.
///
/// Local file references are resolved relative to `document_base_dir`;
/// external HTTP(S) references require [`build_tools_with_http`] or
/// [`generate_tools`].
pub async fn build_tools(
    config: &OpenApiConfig,
    document: &str,
) -> Result<Vec<ConvertedTool>, String> {
    build_tools_with_http(None, config, document).await
}

/// Builds persistable tools, resolving external references with `http`.
pub async fn build_tools_with_http(
    http: Option<&reqwest::Client>,
    config: &OpenApiConfig,
    document: &str,
) -> Result<Vec<ConvertedTool>, String> {
    let value = ExternalRefResolver::new(http, config)?
        .resolve_document(document)
        .await?;
    let openapi = openapi_from_value(value)?;
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

    crate::net_guard::ensure_safe_url(url).await?;
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
    build_tools_with_http(Some(http), config, &document).await
}

// ----- parsing and external references -----------------------------------

fn parse_document_value(document: &str) -> Result<Value, String> {
    let trimmed = document.trim();
    if trimmed.is_empty() {
        return Err("OpenAPI document is empty".into());
    }

    let json_error = match serde_json::from_str::<Value>(trimmed) {
        Ok(value) => return Ok(value),
        Err(err) => err,
    };

    serde_yaml::from_str::<serde_yaml::Value>(trimmed)
        .map_err(|yaml_error| {
            format!("invalid OpenAPI document: JSON error: {json_error}; YAML error: {yaml_error}")
        })
        .and_then(|value| {
            serde_json::to_value(value).map_err(|err| format!("invalid YAML document: {err}"))
        })
}

fn openapi_from_value(value: Value) -> Result<OpenAPI, String> {
    let openapi: OpenAPI =
        serde_json::from_value(value).map_err(|err| format!("invalid OpenAPI document: {err}"))?;
    validate_version(openapi)
}

fn reject_external_refs(value: &Value) -> Result<(), String> {
    match value {
        Value::Object(object) => {
            if let Some(reference) = object.get("$ref").and_then(Value::as_str) {
                let (document, _) = split_reference(reference);
                if !document.is_empty() {
                    return Err(format!(
                        "external OpenAPI reference '{reference}' requires document_url \
                         or document_base_dir"
                    ));
                }
            }
            for value in object.values() {
                reject_external_refs(value)?;
            }
        }
        Value::Array(items) => {
            for value in items {
                reject_external_refs(value)?;
            }
        }
        _ => {}
    }
    Ok(())
}

fn split_reference(reference: &str) -> (&str, &str) {
    match reference.split_once('#') {
        Some((document, fragment)) => (document, fragment),
        None => (reference, ""),
    }
}

#[derive(Debug, Clone)]
enum DocumentBase {
    Url(Url),
    Directory(PathBuf),
    None,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
enum DocumentSource {
    Url(Url),
    File(PathBuf),
}

impl DocumentSource {
    fn cache_key(&self) -> String {
        match self {
            DocumentSource::Url(url) => format!("url:{url}"),
            DocumentSource::File(path) => format!("file:{}", path.display()),
        }
    }
}

struct ExternalRefResolver<'a> {
    http: Option<&'a reqwest::Client>,
    base: DocumentBase,
    /// Canonical directory that local external references may not escape.
    file_root: Option<PathBuf>,
    cache: HashMap<String, Arc<Value>>,
    documents: usize,
}

impl<'a> ExternalRefResolver<'a> {
    fn new(http: Option<&'a reqwest::Client>, config: &OpenApiConfig) -> Result<Self, String> {
        let base = if let Some(url) = config
            .document_url
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
        {
            DocumentBase::Url(
                Url::parse(url)
                    .map_err(|err| format!("invalid config.openapi.document_url: {err}"))?,
            )
        } else if let Some(directory) = config.document_base_dir() {
            let directory = PathBuf::from(directory);
            let directory = if directory.is_absolute() {
                directory
            } else {
                std::env::current_dir()
                    .map_err(|err| format!("cannot resolve document_base_dir: {err}"))?
                    .join(directory)
            };
            DocumentBase::Directory(directory)
        } else {
            DocumentBase::None
        };
        let file_root = match &base {
            DocumentBase::Directory(directory) => {
                Some(std::fs::canonicalize(directory).map_err(|err| {
                    format!("cannot resolve config.openapi.document_base_dir: {err}")
                })?)
            }
            _ => None,
        };
        Ok(Self {
            http,
            base,
            file_root,
            cache: HashMap::new(),
            // The root document counts toward the per-parse document budget.
            documents: 1,
        })
    }

    async fn resolve_document(&mut self, document: &str) -> Result<Value, String> {
        let root = Arc::new(parse_document_value(document)?);
        let base = self.base.clone();
        let mut stack = HashSet::new();
        self.resolve_value(
            (*root).clone(),
            base,
            root,
            "inline".to_string(),
            true,
            0,
            &mut stack,
        )
        .await
    }

    #[allow(clippy::too_many_arguments)]
    fn resolve_value<'b>(
        &'b mut self,
        value: Value,
        base: DocumentBase,
        document_root: Arc<Value>,
        document_key: String,
        is_root: bool,
        depth: usize,
        stack: &'b mut HashSet<String>,
    ) -> Pin<Box<dyn Future<Output = Result<Value, String>> + Send + 'b>>
    where
        'a: 'b,
    {
        Box::pin(async move {
            match value {
                Value::Object(mut object) => {
                    if let Some(reference) = object
                        .get("$ref")
                        .and_then(Value::as_str)
                        .map(str::to_string)
                    {
                        return self
                            .resolve_reference(
                                &reference,
                                base,
                                document_root,
                                document_key,
                                is_root,
                                depth,
                                stack,
                            )
                            .await;
                    }
                    for value in object.values_mut() {
                        let resolved = self
                            .resolve_value(
                                value.clone(),
                                base.clone(),
                                document_root.clone(),
                                document_key.clone(),
                                is_root,
                                depth,
                                stack,
                            )
                            .await?;
                        *value = resolved;
                    }
                    Ok(Value::Object(object))
                }
                Value::Array(items) => {
                    let mut resolved = Vec::with_capacity(items.len());
                    for item in items {
                        resolved.push(
                            self.resolve_value(
                                item,
                                base.clone(),
                                document_root.clone(),
                                document_key.clone(),
                                is_root,
                                depth,
                                stack,
                            )
                            .await?,
                        );
                    }
                    Ok(Value::Array(resolved))
                }
                other => Ok(other),
            }
        })
    }

    #[allow(clippy::too_many_arguments)]
    async fn resolve_reference(
        &mut self,
        reference: &str,
        base: DocumentBase,
        document_root: Arc<Value>,
        document_key: String,
        is_root: bool,
        depth: usize,
        stack: &mut HashSet<String>,
    ) -> Result<Value, String> {
        let (target_document, fragment) = split_reference(reference);
        if target_document.is_empty() {
            if is_root {
                // Local references in the root document are preserved; the
                // OpenAPI parser resolves component parameters and bodies and
                // schema `$ref` values stay intact in the input schema.
                return Ok(json!({"$ref": reference}));
            }
            self.check_depth(reference, depth)?;
            let cycle_key = format!("{document_key}#{fragment}");
            enter_reference(&mut *stack, &cycle_key)?;
            let target = resolve_json_pointer(&document_root, fragment)?;
            let resolved = self
                .resolve_value(
                    target,
                    base,
                    document_root,
                    document_key,
                    false,
                    depth + 1,
                    stack,
                )
                .await;
            stack.remove(&cycle_key);
            return resolved;
        }

        self.check_depth(reference, depth)?;
        let source = resolve_document_source(&base, target_document)?;
        let target_key = source.cache_key();
        let cycle_key = format!("{target_key}#{fragment}");
        enter_reference(&mut *stack, &cycle_key)?;
        let target_document_root = self.load_source(&source).await?;
        let target = resolve_json_pointer(&target_document_root, fragment)?;
        let target_base = match &source {
            DocumentSource::Url(url) => DocumentBase::Url(url.clone()),
            DocumentSource::File(path) => DocumentBase::Directory(
                path.parent()
                    .map(Path::to_path_buf)
                    .unwrap_or_else(|| PathBuf::from("/")),
            ),
        };
        let resolved = self
            .resolve_value(
                target,
                target_base,
                target_document_root,
                target_key,
                false,
                depth + 1,
                stack,
            )
            .await;
        stack.remove(&cycle_key);
        resolved
    }

    fn check_depth(&self, reference: &str, depth: usize) -> Result<(), String> {
        if depth >= OPENAPI_REF_MAX_DEPTH {
            return Err(format!(
                "external OpenAPI reference depth exceeds {OPENAPI_REF_MAX_DEPTH} at '{reference}'"
            ));
        }
        Ok(())
    }

    async fn load_source(&mut self, source: &DocumentSource) -> Result<Arc<Value>, String> {
        let key = source.cache_key();
        if let Some(document) = self.cache.get(&key) {
            return Ok(document.clone());
        }
        if self.documents >= OPENAPI_REF_MAX_DOCUMENTS {
            return Err(format!(
                "external OpenAPI references exceed {OPENAPI_REF_MAX_DOCUMENTS} documents"
            ));
        }
        let document = match source {
            DocumentSource::File(path) => {
                let canonical = std::fs::canonicalize(path).map_err(|err| {
                    format!(
                        "cannot resolve external OpenAPI document '{}': {err}",
                        path.display()
                    )
                })?;
                if let Some(root) = &self.file_root
                    && !canonical.starts_with(root)
                {
                    return Err(format!(
                        "external OpenAPI document '{}' escapes document_base_dir",
                        path.display()
                    ));
                }
                let bytes = std::fs::read(&canonical).map_err(|err| {
                    format!(
                        "cannot read external OpenAPI document '{}': {err}",
                        path.display()
                    )
                })?;
                if bytes.len() > OPENAPI_DOCUMENT_MAX_BYTES {
                    return Err(format!(
                        "external OpenAPI document '{}' exceeds {OPENAPI_DOCUMENT_MAX_BYTES} bytes",
                        path.display()
                    ));
                }
                String::from_utf8(bytes).map_err(|_| {
                    format!(
                        "external OpenAPI document '{}' is not valid UTF-8",
                        path.display()
                    )
                })?
            }
            DocumentSource::Url(url) => {
                let http = self.http.ok_or_else(|| {
                    format!(
                        "cannot resolve external OpenAPI reference '{url}': \
                         no HTTP client is available"
                    )
                })?;
                crate::net_guard::ensure_safe_url(url.as_str()).await?;
                tokio::time::timeout(OPENAPI_FETCH_TIMEOUT, async {
                    let mut response = http.get(url.as_str()).send().await.map_err(|err| {
                        format!(
                            "cannot fetch external OpenAPI document '{url}': {}",
                            err.without_url()
                        )
                    })?;
                    let status = response.status();
                    if !status.is_success() {
                        return Err(format!(
                            "external OpenAPI document '{url}' returned HTTP {}",
                            status.as_u16()
                        ));
                    }
                    let (bytes, truncated) =
                        read_response_bytes(&mut response, OPENAPI_DOCUMENT_MAX_BYTES)
                            .await
                            .map_err(|err| {
                                format!(
                                    "cannot read external OpenAPI document '{url}': {}",
                                    err.without_url()
                                )
                            })?;
                    if truncated {
                        return Err(format!(
                            "external OpenAPI document '{url}' exceeds \
                             {OPENAPI_DOCUMENT_MAX_BYTES} bytes"
                        ));
                    }
                    String::from_utf8(bytes).map_err(|_| {
                        format!("external OpenAPI document '{url}' is not valid UTF-8")
                    })
                })
                .await
                .map_err(|_| format!("timed out fetching external OpenAPI document '{url}'"))??
            }
        };
        let value = Arc::new(
            parse_document_value(&document)
                .map_err(|err| format!("invalid external OpenAPI document '{key}': {err}"))?,
        );
        self.documents += 1;
        self.cache.insert(key, value.clone());
        Ok(value)
    }
}

fn enter_reference(stack: &mut HashSet<String>, key: &str) -> Result<(), String> {
    if !stack.insert(key.to_string()) {
        return Err(format!(
            "cycle detected in external OpenAPI reference '{key}'"
        ));
    }
    Ok(())
}

fn resolve_document_source(base: &DocumentBase, reference: &str) -> Result<DocumentSource, String> {
    if is_absolute_http_url(reference) {
        return Url::parse(reference)
            .map(DocumentSource::Url)
            .map_err(|err| format!("invalid external OpenAPI reference URL '{reference}': {err}"));
    }
    if reference.contains(':') && !reference.starts_with('.') {
        return Err(format!(
            "unsupported external OpenAPI reference scheme in '{reference}'"
        ));
    }
    match base {
        DocumentBase::Url(url) => {
            let joined = url.join(reference).map_err(|err| {
                format!("invalid external OpenAPI reference '{reference}': {err}")
            })?;
            if !is_absolute_http_url(joined.as_str()) {
                return Err(format!(
                    "unsupported external OpenAPI reference URL '{}'",
                    joined.as_str()
                ));
            }
            Ok(DocumentSource::Url(joined))
        }
        DocumentBase::Directory(directory) => {
            let path = Path::new(reference);
            let joined = if path.is_absolute() {
                path.to_path_buf()
            } else {
                directory.join(path)
            };
            let canonical = std::fs::canonicalize(&joined).map_err(|err| {
                format!("cannot resolve external OpenAPI reference '{reference}': {err}")
            })?;
            Ok(DocumentSource::File(canonical))
        }
        DocumentBase::None => Err(format!(
            "external OpenAPI reference '{reference}' requires document_url or document_base_dir"
        )),
    }
}

fn resolve_json_pointer(document: &Value, fragment: &str) -> Result<Value, String> {
    let pointer = fragment.strip_prefix('/').unwrap_or(fragment);
    if pointer.is_empty() {
        return Ok(document.clone());
    }
    let mut current = document;
    for token in pointer.split('/') {
        let token = decode_pointer_token(token);
        current = match current {
            Value::Object(object) => object.get(&token).ok_or_else(|| {
                format!("cannot resolve external OpenAPI reference fragment '#/{pointer}'")
            })?,
            Value::Array(items) => {
                let index = token.parse::<usize>().map_err(|_| {
                    format!("invalid array index in OpenAPI reference fragment '#/{pointer}'")
                })?;
                items.get(index).ok_or_else(|| {
                    format!("cannot resolve external OpenAPI reference fragment '#/{pointer}'")
                })?
            }
            _ => {
                return Err(format!(
                    "cannot resolve external OpenAPI reference fragment '#/{pointer}'"
                ));
            }
        };
    }
    Ok(current.clone())
}

fn decode_pointer_token(token: &str) -> String {
    let decoded = percent_decode(token);
    decoded.replace("~1", "/").replace("~0", "~")
}

fn percent_decode(value: &str) -> String {
    let bytes = value.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%'
            && index + 2 < bytes.len()
            && let (Some(high), Some(low)) =
                (hex_value(bytes[index + 1]), hex_value(bytes[index + 2]))
        {
            decoded.push(high * 16 + low);
            index += 3;
            continue;
        }
        decoded.push(bytes[index]);
        index += 1;
    }
    String::from_utf8_lossy(&decoded).into_owned()
}

fn hex_value(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
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
) -> Result<ConvertedTool, String> {
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
            "cookie" => "cookie",
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
            "style": parameter_style(parameter),
            "explode": parameter_explode(parameter),
            "allow_reserved": parameter_allow_reserved(parameter),
            "schema_type": parameter_schema_type(openapi, parameter),
        }));
    }

    let mut has_body = false;
    let mut body_required = false;
    let mut content_type = None;
    if let Some(request_body) = operation
        .request_body
        .as_ref()
        .and_then(|request_body| resolve_request_body(openapi, request_body))
        && let Some((schema, media_type)) = request_body_input(request_body)?
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

    Ok(ConvertedTool {
        name: tool_name(operation, method, path),
        description,
        schema: Value::Object(input_schema),
        http_method: method.to_ascii_uppercase(),
        http_path: path.to_string(),
        http_config: Value::Object(http_config),
    })
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

fn parameter_style(parameter: &Parameter) -> &'static str {
    match parameter {
        Parameter::Query { style, .. } => match style {
            QueryStyle::Form => "form",
            QueryStyle::SpaceDelimited => "spaceDelimited",
            QueryStyle::PipeDelimited => "pipeDelimited",
            QueryStyle::DeepObject => "deepObject",
        },
        Parameter::Header { .. } => "simple",
        Parameter::Path { style, .. } => match style {
            PathStyle::Simple => "simple",
            PathStyle::Label => "label",
            PathStyle::Matrix => "matrix",
        },
        Parameter::Cookie { .. } => "form",
    }
}

fn parameter_explode(parameter: &Parameter) -> bool {
    parameter
        .parameter_data_ref()
        .explode
        .unwrap_or_else(|| matches!(parameter_location(parameter), "query" | "cookie"))
}

fn parameter_allow_reserved(parameter: &Parameter) -> bool {
    match parameter {
        Parameter::Query { allow_reserved, .. } => *allow_reserved,
        _ => false,
    }
}

fn parameter_schema_type(openapi: &OpenAPI, parameter: &Parameter) -> &'static str {
    let schema = match &parameter.parameter_data_ref().format {
        ParameterSchemaOrContent::Schema(schema) => schema,
        ParameterSchemaOrContent::Content(content) => {
            match content
                .values()
                .next()
                .and_then(|media| media.schema.as_ref())
            {
                Some(schema) => schema,
                None => return "primitive",
            }
        }
    };
    schema_type_label(openapi, schema, 0)
}

fn schema_type_label(
    openapi: &OpenAPI,
    schema: &ReferenceOr<Schema>,
    depth: usize,
) -> &'static str {
    if depth > OPENAPI_REF_MAX_DEPTH {
        return "primitive";
    }
    match schema {
        ReferenceOr::Reference { reference } => {
            let Some(name) = component_name(reference, "#/components/schemas/") else {
                return "primitive";
            };
            let Some(resolved) = openapi
                .components
                .as_ref()
                .and_then(|components| components.schemas.get(name))
            else {
                return "primitive";
            };
            schema_type_label(openapi, resolved, depth + 1)
        }
        ReferenceOr::Item(schema) => match &schema.schema_kind {
            SchemaKind::Type(Type::Array(_)) => "array",
            SchemaKind::Type(Type::Object(_)) => "object",
            SchemaKind::Any(any) => match any.typ.as_deref() {
                Some("array") => "array",
                Some("object") => "object",
                _ => "primitive",
            },
            SchemaKind::OneOf { one_of } => schema_type_from_composition(openapi, one_of, depth),
            SchemaKind::AllOf { all_of } => schema_type_from_composition(openapi, all_of, depth),
            SchemaKind::AnyOf { any_of } => schema_type_from_composition(openapi, any_of, depth),
            _ => "primitive",
        },
    }
}

fn schema_type_from_composition(
    openapi: &OpenAPI,
    schemas: &[ReferenceOr<Schema>],
    depth: usize,
) -> &'static str {
    let mut kind = "primitive";
    for schema in schemas {
        match schema_type_label(openapi, schema, depth + 1) {
            "object" => return "object",
            "array" if kind == "primitive" => kind = "array",
            _ => {}
        }
    }
    kind
}

fn schema_value(schema: &ReferenceOr<Schema>) -> Value {
    match schema {
        ReferenceOr::Reference { reference } => json!({"$ref": reference}),
        ReferenceOr::Item(schema) => {
            serde_json::to_value(schema).unwrap_or_else(|_| json!({"type": "object"}))
        }
    }
}

fn request_body_input(request_body: &RequestBody) -> Result<Option<(Value, String)>, String> {
    if request_body.content.is_empty() {
        return Ok(None);
    }
    let selected = request_body
        .content
        .iter()
        .filter_map(|(media_type, media)| {
            request_body_media_priority(media_type).map(|priority| (priority, media_type, media))
        })
        .min_by_key(|(priority, _, _)| *priority);
    let Some((_, media_type, media)) = selected else {
        return Err(format!(
            "unsupported OpenAPI request body media types: {}",
            request_body
                .content
                .keys()
                .cloned()
                .collect::<Vec<_>>()
                .join(", ")
        ));
    };
    let schema = media.schema.as_ref().map(schema_value).unwrap_or_else(|| {
        if is_text_media_type(media_type) {
            json!({"type": "string"})
        } else {
            json!({"type": "object"})
        }
    });
    Ok(Some((schema, media_type.clone())))
}

fn request_body_media_priority(media_type: &str) -> Option<u8> {
    if is_json_media_type(media_type) {
        Some(0)
    } else if is_form_media_type(media_type) {
        Some(1)
    } else if is_multipart_media_type(media_type) {
        Some(2)
    } else if is_text_media_type(media_type) {
        Some(3)
    } else {
        None
    }
}

fn media_type_essence(media_type: &str) -> String {
    media_type
        .split(';')
        .next()
        .unwrap_or_default()
        .trim()
        .to_ascii_lowercase()
}

fn is_json_media_type(media_type: &str) -> bool {
    let essence = media_type_essence(media_type);
    essence == "application/json" || essence.ends_with("+json")
}

fn is_form_media_type(media_type: &str) -> bool {
    media_type_essence(media_type) == "application/x-www-form-urlencoded"
}

fn is_multipart_media_type(media_type: &str) -> bool {
    media_type_essence(media_type) == "multipart/form-data"
}

fn is_text_media_type(media_type: &str) -> bool {
    media_type_essence(media_type) == "text/plain"
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

/// One part of a `multipart/form-data` request body.
#[derive(Debug, Clone, PartialEq)]
pub struct MultipartPart {
    pub name: String,
    pub text: Option<String>,
    pub bytes: Option<Vec<u8>>,
    pub content_type: Option<String>,
    pub filename: Option<String>,
}

/// The encoded body of an outbound generated HTTP request.
#[derive(Debug, Clone, PartialEq)]
pub enum HttpRequestBody {
    Json(Value),
    Form(Vec<(String, String)>),
    Multipart(Vec<MultipartPart>),
    Text(String),
}

/// A concrete outbound HTTP request derived from one tool call.
#[derive(Debug, Clone, PartialEq)]
pub struct HttpRequestPlan {
    pub method: String,
    pub url: String,
    pub headers: Vec<(String, String)>,
    pub body: Option<HttpRequestBody>,
}

#[derive(Debug, Clone)]
struct QueryPair {
    name: String,
    value: String,
    allow_reserved: bool,
}

enum SerializedValue {
    Primitive(String),
    Array(Vec<String>),
    Object(Vec<(String, String)>),
}

/// Builds the outbound request for a generated HTTP tool.
///
/// Path arguments are serialized according to their OpenAPI style, query
/// arguments use their declared style/explode rules, header and cookie
/// arguments become request headers, and the optional `body` argument uses the
/// selected media type. Authentication is applied last so it cannot be
/// overridden by caller-supplied headers.
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
    let mut query: Vec<QueryPair> = Vec::new();
    let mut headers = Vec::new();
    let mut cookies: Vec<(String, String)> = Vec::new();
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
        let style = parameter
            .get("style")
            .and_then(Value::as_str)
            .unwrap_or_else(|| default_style(location));
        let explode = parameter
            .get("explode")
            .and_then(Value::as_bool)
            .unwrap_or_else(|| default_explode(location));
        let allow_reserved = parameter
            .get("allow_reserved")
            .and_then(Value::as_bool)
            .unwrap_or(false);

        match location {
            "path" => {
                let serialized = serialize_path_parameter(name, value, style, explode);
                path = path.replace(&format!("{{{name}}}"), &serialized);
            }
            "query" => {
                push_query_parameter(&mut query, name, value, style, explode, allow_reserved);
            }
            "header" => {
                let value = serialize_header_parameter(value, explode);
                set_header(&mut headers, name, &value)?;
            }
            "cookie" => {
                cookies.extend(serialize_cookie_parameter(name, value, explode));
            }
            _ => {}
        }
    }

    if !cookies.is_empty() {
        merge_cookie_header(&mut headers, cookies)?;
    }

    if config.get("body").and_then(Value::as_bool).unwrap_or(false) {
        let body_required = config
            .get("body_required")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        match arguments.get("body").filter(|value| !value.is_null()) {
            Some(value) => {
                let content_type = config
                    .get("content_type")
                    .and_then(Value::as_str)
                    .unwrap_or("application/json");
                body = Some(serialize_http_body(tool, value, content_type)?);
            }
            None if body_required => return Err("missing required argument 'body'".into()),
            None => {}
        }
        if let Some(body) = &body {
            let content_type = config
                .get("content_type")
                .and_then(Value::as_str)
                .unwrap_or("application/json");
            if matches!(body, HttpRequestBody::Json(_) | HttpRequestBody::Text(_)) {
                set_header(&mut headers, CONTENT_TYPE.as_str(), content_type)?;
            }
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
                query.push(QueryPair {
                    name: param.to_string(),
                    value: secret,
                    allow_reserved: false,
                });
            }
            other => return Err(format!("unsupported HTTP tool auth type '{other}'")),
        }
    }

    let url = join_url(base_url, &path);
    let mut url = Url::parse(&url).map_err(|err| format!("invalid HTTP tool URL: {err}"))?;
    append_query(&mut url, &query);

    Ok(HttpRequestPlan {
        method: method.to_ascii_uppercase(),
        url: url.to_string(),
        headers,
        body,
    })
}

fn default_style(location: &str) -> &'static str {
    match location {
        "query" | "cookie" => "form",
        _ => "simple",
    }
}

fn default_explode(location: &str) -> bool {
    matches!(location, "query" | "cookie")
}

fn serialize_value(value: &Value) -> SerializedValue {
    match value {
        Value::Array(items) => SerializedValue::Array(items.iter().map(scalar_text).collect()),
        Value::Object(object) => SerializedValue::Object(
            object
                .iter()
                .map(|(key, value)| (key.clone(), scalar_text(value)))
                .collect(),
        ),
        other => SerializedValue::Primitive(scalar_text(other)),
    }
}

fn scalar_text(value: &Value) -> String {
    match value {
        Value::Null => String::new(),
        Value::String(value) => value.clone(),
        Value::Bool(value) => value.to_string(),
        Value::Number(value) => value.to_string(),
        Value::Array(items) => items.iter().map(scalar_text).collect::<Vec<_>>().join(","),
        Value::Object(_) => serde_json::to_string(value).unwrap_or_default(),
    }
}

fn serialize_path_parameter(name: &str, value: &Value, style: &str, explode: bool) -> String {
    match style {
        "label" => match serialize_value(value) {
            SerializedValue::Primitive(value) => format!(".{}", percent_encode(&value)),
            SerializedValue::Array(items) => {
                format!(
                    ".{}",
                    items
                        .iter()
                        .map(|value| percent_encode(value))
                        .collect::<Vec<_>>()
                        .join(".")
                )
            }
            SerializedValue::Object(entries) if explode => format!(
                ".{}",
                entries
                    .iter()
                    .map(|(key, value)| format!(
                        "{}={}",
                        percent_encode(key),
                        percent_encode(value)
                    ))
                    .collect::<Vec<_>>()
                    .join(".")
            ),
            SerializedValue::Object(entries) => format!(
                ".{}",
                entries
                    .iter()
                    .flat_map(|(key, value)| [percent_encode(key), percent_encode(value)])
                    .collect::<Vec<_>>()
                    .join(",")
            ),
        },
        "matrix" => match serialize_value(value) {
            SerializedValue::Primitive(value) => {
                format!(";{}={}", percent_encode(name), percent_encode(&value))
            }
            SerializedValue::Array(items) if explode => items
                .iter()
                .map(|value| format!(";{}={}", percent_encode(name), percent_encode(value)))
                .collect::<Vec<_>>()
                .join(""),
            SerializedValue::Array(items) => format!(
                ";{}={}",
                percent_encode(name),
                items
                    .iter()
                    .map(|value| percent_encode(value))
                    .collect::<Vec<_>>()
                    .join(",")
            ),
            SerializedValue::Object(entries) if explode => entries
                .iter()
                .map(|(key, value)| format!(";{}={}", percent_encode(key), percent_encode(value)))
                .collect::<Vec<_>>()
                .join(""),
            SerializedValue::Object(entries) => format!(
                ";{}={}",
                percent_encode(name),
                entries
                    .iter()
                    .flat_map(|(key, value)| [percent_encode(key), percent_encode(value)])
                    .collect::<Vec<_>>()
                    .join(",")
            ),
        },
        _ => match serialize_value(value) {
            SerializedValue::Primitive(value) => percent_encode(&value),
            SerializedValue::Array(items) => items
                .iter()
                .map(|value| percent_encode(value))
                .collect::<Vec<_>>()
                .join(","),
            SerializedValue::Object(entries) if explode => entries
                .iter()
                .map(|(key, value)| format!("{}={}", percent_encode(key), percent_encode(value)))
                .collect::<Vec<_>>()
                .join(","),
            SerializedValue::Object(entries) => entries
                .iter()
                .flat_map(|(key, value)| [percent_encode(key), percent_encode(value)])
                .collect::<Vec<_>>()
                .join(","),
        },
    }
}

fn push_query_parameter(
    query: &mut Vec<QueryPair>,
    name: &str,
    value: &Value,
    style: &str,
    explode: bool,
    allow_reserved: bool,
) {
    let push = |query: &mut Vec<QueryPair>, name: String, value: String| {
        query.push(QueryPair {
            name,
            value,
            allow_reserved,
        });
    };
    match style {
        "spaceDelimited" | "pipeDelimited" => {
            let delimiter = if style == "spaceDelimited" { " " } else { "|" };
            let value = match serialize_value(value) {
                SerializedValue::Primitive(value) => value,
                SerializedValue::Array(items) => items.join(delimiter),
                SerializedValue::Object(entries) => entries
                    .into_iter()
                    .flat_map(|(key, value)| [key, value])
                    .collect::<Vec<_>>()
                    .join(delimiter),
            };
            push(query, name.to_string(), value);
        }
        "deepObject" => {
            if let Value::Object(object) = value {
                for (key, value) in object {
                    let deep_name = format!("{name}[{key}]");
                    if let Value::Object(nested) = value {
                        for (nested_key, nested_value) in nested {
                            push(
                                query,
                                format!("{deep_name}[{nested_key}]"),
                                scalar_text(nested_value),
                            );
                        }
                    } else {
                        push(query, deep_name, scalar_text(value));
                    }
                }
            } else {
                push(query, name.to_string(), scalar_text(value));
            }
        }
        "form" if explode => match serialize_value(value) {
            SerializedValue::Primitive(value) => push(query, name.to_string(), value),
            SerializedValue::Array(items) => {
                for value in items {
                    push(query, name.to_string(), value);
                }
            }
            SerializedValue::Object(entries) => {
                for (key, value) in entries {
                    push(query, key, value);
                }
            }
        },
        _ => {
            let value = match serialize_value(value) {
                SerializedValue::Primitive(value) => value,
                SerializedValue::Array(items) => items.join(","),
                SerializedValue::Object(entries) => entries
                    .into_iter()
                    .flat_map(|(key, value)| [key, value])
                    .collect::<Vec<_>>()
                    .join(","),
            };
            push(query, name.to_string(), value);
        }
    }
}

fn serialize_header_parameter(value: &Value, explode: bool) -> String {
    match serialize_value(value) {
        SerializedValue::Primitive(value) => value,
        SerializedValue::Array(items) => items.join(","),
        SerializedValue::Object(entries) if explode => entries
            .into_iter()
            .map(|(key, value)| format!("{key}={value}"))
            .collect::<Vec<_>>()
            .join(","),
        SerializedValue::Object(entries) => entries
            .into_iter()
            .flat_map(|(key, value)| [key, value])
            .collect::<Vec<_>>()
            .join(","),
    }
}

fn serialize_cookie_parameter(name: &str, value: &Value, explode: bool) -> Vec<(String, String)> {
    match serialize_value(value) {
        SerializedValue::Primitive(value) => vec![(name.to_string(), value)],
        SerializedValue::Array(items) if explode => items
            .into_iter()
            .map(|value| (name.to_string(), value))
            .collect(),
        SerializedValue::Array(items) => vec![(name.to_string(), items.join(","))],
        SerializedValue::Object(entries) if explode => entries,
        SerializedValue::Object(entries) => vec![(
            name.to_string(),
            entries
                .into_iter()
                .flat_map(|(key, value)| [key, value])
                .collect::<Vec<_>>()
                .join(","),
        )],
    }
}

fn merge_cookie_header(
    headers: &mut Vec<(String, String)>,
    cookies: Vec<(String, String)>,
) -> Result<(), String> {
    let existing_index = headers
        .iter()
        .position(|(name, _)| name.eq_ignore_ascii_case("cookie"));
    let existing = existing_index
        .map(|index| headers.remove(index).1)
        .unwrap_or_default();
    let seen: HashSet<String> = existing
        .split(';')
        .filter_map(|part| {
            part.split_once('=')
                .map(|(name, _)| name.trim().to_string())
        })
        .collect();
    let mut merged: Vec<String> = if existing.trim().is_empty() {
        Vec::new()
    } else {
        vec![existing.trim().to_string()]
    };
    for (name, value) in cookies {
        if seen.contains(&name) {
            continue;
        }
        merged.push(format!("{name}={value}"));
    }
    if !merged.is_empty() {
        set_header(headers, "Cookie", &merged.join("; "))?;
    }
    Ok(())
}

fn append_query(url: &mut Url, pairs: &[QueryPair]) {
    if pairs.is_empty() {
        return;
    }
    let mut query = url.query().unwrap_or_default().to_string();
    for pair in pairs {
        if !query.is_empty() {
            query.push('&');
        }
        query.push_str(&percent_encode(&pair.name));
        query.push('=');
        if pair.allow_reserved {
            query.push_str(&percent_encode_allow_reserved(&pair.value));
        } else {
            query.push_str(&percent_encode(&pair.value));
        }
    }
    url.set_query(Some(&query));
}

fn serialize_http_body(
    tool: &McpTool,
    value: &Value,
    content_type: &str,
) -> Result<HttpRequestBody, String> {
    if is_json_media_type(content_type) {
        Ok(HttpRequestBody::Json(value.clone()))
    } else if is_form_media_type(content_type) {
        Ok(HttpRequestBody::Form(form_fields(value)?))
    } else if is_multipart_media_type(content_type) {
        Ok(HttpRequestBody::Multipart(multipart_parts(tool, value)?))
    } else if is_text_media_type(content_type) {
        value
            .as_str()
            .map(|value| HttpRequestBody::Text(value.to_string()))
            .ok_or_else(|| "text/plain body must be a string".to_string())
    } else {
        Err(format!(
            "unsupported HTTP tool content type '{content_type}'"
        ))
    }
}

fn form_fields(value: &Value) -> Result<Vec<(String, String)>, String> {
    let object = value
        .as_object()
        .ok_or_else(|| "form-urlencoded body must be a JSON object".to_string())?;
    let mut fields = Vec::new();
    for (name, value) in object {
        match value {
            Value::Array(items) => {
                for item in items {
                    fields.push((name.clone(), scalar_text(item)));
                }
            }
            Value::Object(entries) => {
                for (key, value) in entries {
                    fields.push((key.clone(), scalar_text(value)));
                }
            }
            other => fields.push((name.clone(), scalar_text(other))),
        }
    }
    Ok(fields)
}

fn multipart_parts(tool: &McpTool, value: &Value) -> Result<Vec<MultipartPart>, String> {
    let object = value
        .as_object()
        .ok_or_else(|| "multipart body must be a JSON object".to_string())?;
    let mut parts = Vec::new();
    for (name, value) in object {
        if let Value::Object(entries) = value
            && let Some(encoded) = entries.get("content_base64").and_then(Value::as_str)
        {
            let bytes = BASE64_STANDARD.decode(encoded).map_err(|err| {
                format!("multipart field '{name}' has invalid content_base64: {err}")
            })?;
            let content_type = entries
                .get("content_type")
                .or_else(|| entries.get("contentType"))
                .or_else(|| entries.get("media_type"))
                .and_then(Value::as_str)
                .map(str::to_string)
                .or_else(|| declared_multipart_content_type(tool, name));
            let filename = entries
                .get("filename")
                .or_else(|| entries.get("file_name"))
                .and_then(Value::as_str)
                .map(str::to_string);
            parts.push(MultipartPart {
                name: name.clone(),
                text: None,
                bytes: Some(bytes),
                content_type,
                filename,
            });
            continue;
        }
        let text = match value {
            Value::Object(_) | Value::Array(_) => serde_json::to_string(value)
                .map_err(|err| format!("cannot encode multipart field '{name}' as JSON: {err}"))?,
            other => scalar_text(other),
        };
        parts.push(MultipartPart {
            name: name.clone(),
            text: Some(text),
            bytes: None,
            content_type: None,
            filename: None,
        });
    }
    Ok(parts)
}

fn declared_multipart_content_type(tool: &McpTool, field: &str) -> Option<String> {
    let body = tool
        .schema
        .get("properties")
        .and_then(|properties| properties.get("body"))?;
    let property = body
        .get("properties")
        .and_then(|properties| properties.get(field))?;
    property
        .get("contentMediaType")
        .or_else(|| property.get("x-contentMediaType"))
        .or_else(|| property.get("contentEncoding"))
        .or_else(|| property.get("x-contentEncoding"))
        .and_then(Value::as_str)
        .map(str::to_string)
}

fn set_header(headers: &mut Vec<(String, String)>, name: &str, value: &str) -> Result<(), String> {
    let name = HeaderName::from_bytes(name.trim().as_bytes())
        .map_err(|_| format!("invalid header name '{name}'"))?;
    HeaderValue::from_str(value).map_err(|_| format!("invalid value for header '{name}'"))?;
    headers.retain(|(existing, _)| !existing.eq_ignore_ascii_case(name.as_str()));
    headers.push((name.as_str().to_string(), value.to_string()));
    Ok(())
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

fn percent_encode_allow_reserved(value: &str) -> String {
    let mut encoded = String::with_capacity(value.len());
    for byte in value.bytes() {
        let reserved = matches!(
            byte,
            b':' | b'/'
                | b'?'
                | b'['
                | b']'
                | b'@'
                | b'!'
                | b'$'
                | b'\''
                | b'('
                | b')'
                | b'*'
                | b'+'
                | b','
                | b';'
                | b'='
                | b'%'
        );
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~') || reserved {
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
    match &plan.body {
        Some(HttpRequestBody::Json(body)) => {
            let encoded = serde_json::to_vec(body).map_err(|err| {
                McpError::Internal(format!("cannot encode HTTP tool body: {err}"))
            })?;
            builder = builder.body(encoded);
        }
        Some(HttpRequestBody::Form(fields)) => {
            builder = builder.form(fields);
        }
        Some(HttpRequestBody::Multipart(parts)) => {
            let mut form = reqwest::multipart::Form::new();
            for part in parts {
                if let Some(bytes) = &part.bytes {
                    let mut multipart_part = reqwest::multipart::Part::bytes(bytes.clone());
                    if let Some(content_type) = &part.content_type {
                        multipart_part = multipart_part.mime_str(content_type).map_err(|err| {
                            McpError::Config(format!(
                                "invalid multipart content type for field '{}': {err}",
                                part.name
                            ))
                        })?;
                    }
                    if let Some(filename) = &part.filename {
                        multipart_part = multipart_part.file_name(filename.clone());
                    }
                    form = form.part(part.name.clone(), multipart_part);
                } else {
                    form = form.text(part.name.clone(), part.text.clone().unwrap_or_default());
                }
            }
            builder = builder.multipart(form);
        }
        Some(HttpRequestBody::Text(body)) => {
            builder = builder.body(body.clone());
        }
        None => {}
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
        assert_eq!(get.http_config["parameters"][0]["style"], "simple");
        assert_eq!(get.http_config["parameters"][0]["explode"], false);
        assert_eq!(get.http_config["parameters"][1]["in"], "query");
        assert_eq!(get.http_config["parameters"][1]["style"], "form");
        assert_eq!(get.http_config["parameters"][1]["explode"], true);
        assert_eq!(get.http_config["body"], false);

        let post = tools.iter().find(|tool| tool.name == "create_pet").unwrap();
        assert_eq!(post.http_method, "POST");
        assert_eq!(post.schema["properties"]["body"]["type"], "object");
        assert_eq!(post.schema["required"], json!(["body"]));
        assert_eq!(post.http_config["body"], true);
        assert_eq!(post.http_config["content_type"], "application/json");
    }

    #[test]
    fn records_parameter_serialization_metadata_and_cookie_inputs() {
        let document = json!({
            "openapi": "3.0.3",
            "info": {"title": "Styles", "version": "1"},
            "servers": [{"url": "https://api.example.test"}],
            "paths": {
                "/items/{itemId}": {
                    "get": {
                        "operationId": "styles",
                        "parameters": [
                            {"name": "itemId", "in": "path", "required": true, "schema": {"type": "string"}},
                            {"name": "tags", "in": "query", "explode": false, "schema": {"type": "array", "items": {"type": "string"}}},
                            {"name": "filter", "in": "query", "style": "deepObject", "schema": {"type": "object"}},
                            {"name": "session", "in": "cookie", "required": true, "schema": {"type": "string"}},
                            {"name": "X-Flags", "in": "header", "style": "simple", "schema": {"type": "array", "items": {"type": "string"}}}
                        ],
                        "responses": {"200": {"description": "ok"}}
                    }
                }
            }
        })
        .to_string();
        let tools = parse_openapi(&document).unwrap();
        let config = &tools[0].http_config;
        let parameters = config["parameters"].as_array().unwrap();
        let by_name = |name: &str| {
            parameters
                .iter()
                .find(|parameter| parameter["name"] == name)
                .unwrap()
        };
        assert_eq!(by_name("itemId")["style"], "simple");
        assert_eq!(by_name("itemId")["explode"], false);
        assert_eq!(by_name("itemId")["schema_type"], "primitive");
        assert_eq!(by_name("tags")["style"], "form");
        assert_eq!(by_name("tags")["explode"], false);
        assert_eq!(by_name("tags")["schema_type"], "array");
        assert_eq!(by_name("filter")["style"], "deepObject");
        assert_eq!(by_name("filter")["explode"], true);
        assert_eq!(by_name("filter")["schema_type"], "object");
        assert_eq!(by_name("session")["arg"], "cookie_session");
        assert_eq!(by_name("session")["in"], "cookie");
        assert_eq!(by_name("session")["style"], "form");
        assert_eq!(by_name("session")["explode"], true);
        assert_eq!(by_name("X-Flags")["style"], "simple");
        assert_eq!(by_name("X-Flags")["explode"], false);
        assert_eq!(by_name("X-Flags")["schema_type"], "array");
        assert_eq!(
            tools[0].schema["required"],
            json!(["path_itemId", "cookie_session"])
        );
        assert_eq!(
            tools[0].schema["properties"]["cookie_session"],
            json!({"type": "string"})
        );
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
        let runtime = tokio::runtime::Runtime::new().unwrap();
        let config = OpenApiConfig {
            document: Some(yaml.to_string()),
            base_url: Some("https://api.example.test".into()),
            ..OpenApiConfig::default()
        };
        let tools = runtime.block_on(build_tools(&config, yaml)).unwrap();
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
        let error = runtime.block_on(build_tools(&config, yaml)).unwrap_err();
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
    fn rejects_external_refs_without_a_document_location() {
        let document = json!({
            "openapi": "3.0.3",
            "info": {"title": "External", "version": "1"},
            "servers": [{"url": "https://api.example.test"}],
            "paths": {
                "/pets": {"$ref": "./paths.yaml#/pets"}
            }
        })
        .to_string();
        let error = parse_openapi(&document).unwrap_err();
        assert!(
            error.contains("requires document_url or document_base_dir"),
            "{error}"
        );
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

        let config = json!({
            "openapi": {
                "document": "{}",
                "document_base_dir": "/tmp/openapi"
            }
        });
        let parsed = OpenApiConfig::from_config(&config).unwrap().unwrap();
        assert_eq!(parsed.document_base_dir(), Some("/tmp/openapi"));
    }

    fn http_tool(config: Value) -> McpTool {
        http_tool_with(config, "GET", "/pets/{petId}")
    }

    fn http_tool_with(config: Value, method: &str, path: &str) -> McpTool {
        McpTool {
            id: "tool-1".into(),
            server_id: "server-1".into(),
            name: "get_pet".into(),
            description: "Get a pet".into(),
            schema: json!({"type": "object"}),
            kind: MCP_TOOL_KIND_HTTP.into(),
            http_method: Some(method.into()),
            http_path: Some(path.into()),
            http_config: config,
            enabled: true,
            created_at: "2026-01-01T00:00:00.000Z".into(),
        }
    }

    fn header_value<'a>(request: &'a HttpRequestPlan, name: &str) -> Option<&'a str> {
        request
            .headers
            .iter()
            .find(|(header, _)| header.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.as_str())
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
        assert_eq!(header_value(&request, "x-tenant"), Some("team-a"));
        assert_eq!(
            request.body,
            Some(HttpRequestBody::Json(json!({"name": "Fido"})))
        );

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
        assert_eq!(header_value(&request, "x-api-key"), Some("secret-value"));

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

    #[test]
    fn serializes_path_parameters_by_style_and_explode() {
        let cases = [
            ("simple", false, json!(["a", "b c"]), "/items/a,b%20c"),
            (
                "simple",
                false,
                json!({"role": "admin", "firstName": "Alex"}),
                "/items/firstName,Alex,role,admin",
            ),
            (
                "simple",
                true,
                json!({"role": "admin", "firstName": "Alex"}),
                "/items/firstName=Alex,role=admin",
            ),
            ("label", false, json!(["a", "b", "c"]), "/items/.a.b.c"),
            (
                "label",
                false,
                json!({"role": "admin", "firstName": "Alex"}),
                "/items/.firstName,Alex,role,admin",
            ),
            (
                "label",
                true,
                json!({"role": "admin", "firstName": "Alex"}),
                "/items/.firstName=Alex.role=admin",
            ),
            ("matrix", false, json!(["a", "b", "c"]), "/items/;id=a,b,c"),
            (
                "matrix",
                true,
                json!(["a", "b", "c"]),
                "/items/;id=a;id=b;id=c",
            ),
            (
                "matrix",
                true,
                json!({"role": "admin", "firstName": "Alex"}),
                "/items/;firstName=Alex;role=admin",
            ),
            ("matrix", false, json!(5), "/items/;id=5"),
        ];
        for (style, explode, value, expected) in cases {
            let config = json!({
                "base_url": "https://api.example.test",
                "parameters": [{
                    "arg": "path_id",
                    "name": "id",
                    "in": "path",
                    "required": true,
                    "style": style,
                    "explode": explode,
                }],
                "body": false
            });
            let request = build_http_request(
                &http_tool_with(config, "GET", "/items/{id}"),
                &json!({"path_id": value}),
            )
            .unwrap();
            assert_eq!(
                request.url,
                format!("https://api.example.test{expected}"),
                "{style} {explode}"
            );
        }
    }

    #[test]
    fn serializes_query_parameters_by_style_and_explode() {
        let config = json!({
            "base_url": "https://api.example.test",
            "parameters": [
                {"arg": "query_tags", "name": "tags", "in": "query", "style": "form", "explode": true},
                {"arg": "query_ids", "name": "ids", "in": "query", "style": "form", "explode": false},
                {"arg": "query_filter", "name": "filter", "in": "query", "style": "form", "explode": true},
                {"arg": "query_labels", "name": "labels", "in": "query", "style": "form", "explode": false},
                {"arg": "query_spaced", "name": "spaced", "in": "query", "style": "spaceDelimited", "explode": false},
                {"arg": "query_piped", "name": "piped", "in": "query", "style": "pipeDelimited", "explode": false},
                {"arg": "query_deep", "name": "deep", "in": "query", "style": "deepObject", "explode": true},
                {"arg": "query_raw", "name": "raw", "in": "query", "style": "form", "explode": true, "allow_reserved": true}
            ],
            "body": false
        });
        let request = build_http_request(
            &http_tool_with(config, "GET", "/items"),
            &json!({
                "query_tags": ["one", "two"],
                "query_ids": ["a", "b"],
                "query_filter": {"role": "admin", "firstName": "Alex"},
                "query_labels": {"role": "admin", "firstName": "Alex"},
                "query_spaced": ["a b", "c"],
                "query_piped": ["a", "b"],
                "query_deep": {"role": "admin", "nested": {"first": "Alex"}},
                "query_raw": "/v1/a b"
            }),
        )
        .unwrap();
        let url = &request.url;
        assert!(url.contains("tags=one&tags=two"), "{url}");
        assert!(url.contains("ids=a%2Cb"), "{url}");
        assert!(url.contains("firstName=Alex"), "{url}");
        assert!(url.contains("role=admin"), "{url}");
        assert!(
            url.contains("labels=firstName%2CAlex%2Crole%2Cadmin"),
            "{url}"
        );
        assert!(url.contains("spaced=a%20b%20c"), "{url}");
        assert!(url.contains("piped=a%7Cb"), "{url}");
        assert!(url.contains("deep%5Brole%5D=admin"), "{url}");
        assert!(url.contains("deep%5Bnested%5D%5Bfirst%5D=Alex"), "{url}");
        assert!(url.contains("raw=/v1/a%20b"), "{url}");
        assert!(!url.contains("raw=%2Fv1"), "{url}");
    }

    #[test]
    fn serializes_header_and_cookie_parameters_and_merges_caller_cookies() {
        let config = json!({
            "base_url": "https://api.example.test",
            "parameters": [
                {"arg": "header_X-Flags", "name": "X-Flags", "in": "header", "style": "simple", "explode": false},
                {"arg": "header_X-Prefs", "name": "X-Prefs", "in": "header", "style": "simple", "explode": true},
                {"arg": "cookie_session", "name": "session", "in": "cookie", "required": true, "style": "form", "explode": true},
                {"arg": "cookie_tags", "name": "tags", "in": "cookie", "style": "form", "explode": true},
                {"arg": "cookie_roles", "name": "roles", "in": "cookie", "style": "form", "explode": true},
                {"arg": "header_Cookie", "name": "Cookie", "in": "header", "style": "simple", "explode": false}
            ],
            "body": false
        });
        let request = build_http_request(
            &http_tool_with(config, "GET", "/items"),
            &json!({
                "header_X-Flags": ["a", "b"],
                "header_X-Prefs": {"role": "admin", "active": true},
                "cookie_session": "generated",
                "cookie_tags": ["one", "two"],
                "cookie_roles": {"role": "admin", "active": true},
                "header_Cookie": "caller=1; session=caller"
            }),
        )
        .unwrap();
        assert_eq!(header_value(&request, "x-flags"), Some("a,b"));
        assert_eq!(
            header_value(&request, "x-prefs"),
            Some("active=true,role=admin")
        );
        assert_eq!(
            header_value(&request, "cookie"),
            Some("caller=1; session=caller; tags=one; tags=two; active=true; role=admin")
        );
    }

    #[test]
    fn builds_form_multipart_and_text_bodies() {
        let form_config = json!({
            "base_url": "https://api.example.test",
            "parameters": [],
            "body": true,
            "body_required": true,
            "content_type": "application/x-www-form-urlencoded"
        });
        let request = build_http_request(
            &http_tool_with(form_config, "POST", "/form"),
            &json!({"body": {
                "name": "Fido",
                "tags": ["a", "b"],
                "meta": {"color": "brown"}
            }}),
        )
        .unwrap();
        let Some(HttpRequestBody::Form(fields)) = request.body else {
            panic!("expected form body");
        };
        assert!(fields.contains(&("name".to_string(), "Fido".to_string())));
        assert!(fields.contains(&("tags".to_string(), "a".to_string())));
        assert!(fields.contains(&("tags".to_string(), "b".to_string())));
        assert!(fields.contains(&("color".to_string(), "brown".to_string())));

        let multipart_config = json!({
            "base_url": "https://api.example.test",
            "parameters": [],
            "body": true,
            "body_required": true,
            "content_type": "multipart/form-data"
        });
        let mut multipart_tool = http_tool_with(multipart_config, "POST", "/multipart");
        multipart_tool.schema = json!({
            "type": "object",
            "properties": {
                "body": {
                    "type": "object",
                    "properties": {
                        "file": {"type": "string", "x-contentMediaType": "image/png"}
                    }
                }
            }
        });
        let request = build_http_request(
            &multipart_tool,
            &json!({"body": {
                "name": "Fido",
                "file": {"content_base64": "aGk="},
                "meta": {"a": 1},
                "tags": ["x"]
            }}),
        )
        .unwrap();
        let Some(HttpRequestBody::Multipart(parts)) = request.body else {
            panic!("expected multipart body");
        };
        let part = |name: &str| parts.iter().find(|part| part.name == name).unwrap();
        assert_eq!(part("name").text.as_deref(), Some("Fido"));
        assert_eq!(part("meta").text.as_deref(), Some("{\"a\":1}"));
        assert_eq!(part("tags").text.as_deref(), Some("[\"x\"]"));
        assert_eq!(part("file").bytes.as_deref(), Some(b"hi".as_slice()));
        assert_eq!(part("file").content_type.as_deref(), Some("image/png"));

        let text_config = json!({
            "base_url": "https://api.example.test",
            "parameters": [],
            "body": true,
            "body_required": true,
            "content_type": "text/plain"
        });
        let request = build_http_request(
            &http_tool_with(text_config, "POST", "/text"),
            &json!({"body": "hello world"}),
        )
        .unwrap();
        assert_eq!(
            request.body,
            Some(HttpRequestBody::Text("hello world".to_string()))
        );
        assert_eq!(header_value(&request, "content-type"), Some("text/plain"));
    }

    #[test]
    fn chooses_supported_request_body_media_types_and_rejects_unsupported_ones() {
        let document = json!({
            "openapi": "3.0.3",
            "info": {"title": "Bodies", "version": "1"},
            "servers": [{"url": "https://api.example.test"}],
            "paths": {
                "/bodies": {
                    "post": {
                        "operationId": "bodies",
                        "requestBody": {
                            "content": {
                                "text/plain": {"schema": {"type": "string"}},
                                "application/x-www-form-urlencoded": {"schema": {"type": "object"}},
                                "application/json": {"schema": {"type": "object"}}
                            }
                        },
                        "responses": {"200": {"description": "ok"}}
                    }
                }
            }
        })
        .to_string();
        let tools = parse_openapi(&document).unwrap();
        assert_eq!(tools[0].http_config["content_type"], "application/json");

        let unsupported = json!({
            "openapi": "3.0.3",
            "info": {"title": "Bodies", "version": "1"},
            "servers": [{"url": "https://api.example.test"}],
            "paths": {
                "/bodies": {
                    "post": {
                        "operationId": "bodies",
                        "requestBody": {
                            "content": {"application/xml": {"schema": {"type": "object"}}}
                        },
                        "responses": {"200": {"description": "ok"}}
                    }
                }
            }
        })
        .to_string();
        let error = parse_openapi(&unsupported).unwrap_err();
        assert!(
            error.contains("unsupported OpenAPI request body media types"),
            "{error}"
        );
        assert!(error.contains("application/xml"), "{error}");
    }

    fn temp_dir(label: &str) -> PathBuf {
        let path = std::env::temp_dir().join(format!(
            "octra-openapi-{label}-{}-{}",
            std::process::id(),
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(&path).unwrap();
        path
    }

    #[tokio::test]
    async fn resolves_local_file_external_refs() {
        let directory = temp_dir("file-refs");
        let shared = r#"
components:
  parameters:
    Tag:
      name: tag
      in: query
      schema:
        type: string
  requestBodies:
    PetBody:
      required: true
      content:
        application/json:
          schema:
            $ref: '#/components/schemas/Pet'
  schemas:
    Pet:
      type: object
      properties:
        name:
          type: string
paths:
  /pets:
    get:
      operationId: listPets
      parameters:
        - $ref: '#/components/parameters/Tag'
      responses:
        '200':
          description: ok
    post:
      operationId: createPet
      requestBody:
        $ref: '#/components/requestBodies/PetBody'
      responses:
        '201':
          description: created
"#;
        std::fs::write(directory.join("common.yaml"), shared).unwrap();
        let document = json!({
            "openapi": "3.0.3",
            "info": {"title": "External", "version": "1"},
            "servers": [{"url": "https://api.example.test"}],
            "paths": {
                "/pets": {"$ref": "./common.yaml#/paths/~1pets"}
            }
        })
        .to_string();
        let config = OpenApiConfig {
            document: Some(document.clone()),
            document_base_dir: Some(directory.display().to_string()),
            ..OpenApiConfig::default()
        };
        let tools = build_tools(&config, &document).await.unwrap();
        assert_eq!(tools.len(), 2);
        let get = tools.iter().find(|tool| tool.name == "listPets").unwrap();
        assert_eq!(get.http_config["parameters"][0]["name"], "tag");
        assert_eq!(get.schema["properties"]["query_tag"]["type"], "string");
        let post = tools.iter().find(|tool| tool.name == "createPet").unwrap();
        assert_eq!(post.http_config["content_type"], "application/json");
        assert_eq!(post.schema["properties"]["body"]["type"], "object");
        assert_eq!(
            post.schema["properties"]["body"]["properties"]["name"]["type"],
            "string"
        );
        let _ = std::fs::remove_dir_all(directory);
    }

    #[tokio::test]
    async fn resolves_http_external_refs_via_a_mock_server() {
        use axum::{Json, Router, routing::get};
        use tokio::net::TcpListener;

        let app = Router::new().route(
            "/shared.json",
            get(|| async {
                Json(json!({
                    "components": {
                        "parameters": {
                            "Tag": {
                                "name": "tag",
                                "in": "query",
                                "schema": {"type": "string"}
                            }
                        }
                    }
                }))
            }),
        );
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let handle = tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        let document = json!({
            "openapi": "3.0.3",
            "info": {"title": "HTTP refs", "version": "1"},
            "servers": [{"url": "https://api.example.test"}],
            "paths": {
                "/pets": {
                    "get": {
                        "operationId": "remoteRef",
                        "parameters": [{
                            "$ref": format!("http://{address}/shared.json#/components/parameters/Tag")
                        }],
                        "responses": {"200": {"description": "ok"}}
                    }
                }
            }
        })
        .to_string();
        let config = OpenApiConfig {
            document: Some(document.clone()),
            ..OpenApiConfig::default()
        };
        let client = reqwest::Client::new();
        let tools = build_tools_with_http(Some(&client), &config, &document)
            .await
            .unwrap();
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0].http_config["parameters"][0]["name"], "tag");
        handle.abort();
    }

    #[tokio::test]
    async fn rejects_external_ref_cycles_and_depth_limits() {
        let cycle_directory = temp_dir("cycle-refs");
        std::fs::write(
            cycle_directory.join("a.yaml"),
            "components:\n  parameters:\n    A:\n      $ref: './b.yaml#/components/parameters/B'\n",
        )
        .unwrap();
        std::fs::write(
            cycle_directory.join("b.yaml"),
            "components:\n  parameters:\n    B:\n      $ref: './a.yaml#/components/parameters/A'\n",
        )
        .unwrap();
        let document = json!({
            "openapi": "3.0.3",
            "info": {"title": "Cycles", "version": "1"},
            "servers": [{"url": "https://api.example.test"}],
            "paths": {
                "/pets": {
                    "get": {
                        "operationId": "cycle",
                        "parameters": [{"$ref": "./a.yaml#/components/parameters/A"}],
                        "responses": {"200": {"description": "ok"}}
                    }
                }
            }
        })
        .to_string();
        let config = OpenApiConfig {
            document: Some(document.clone()),
            document_base_dir: Some(cycle_directory.display().to_string()),
            ..OpenApiConfig::default()
        };
        let error = build_tools(&config, &document).await.unwrap_err();
        assert!(error.contains("cycle detected"), "{error}");
        let _ = std::fs::remove_dir_all(cycle_directory);

        let depth_directory = temp_dir("depth-refs");
        for index in 0..12 {
            let contents = if index == 11 {
                "next:\n  value: end\n".to_string()
            } else {
                format!("next:\n  $ref: './d{}.yaml#/next'\n", index + 1)
            };
            std::fs::write(depth_directory.join(format!("d{index}.yaml")), contents).unwrap();
        }
        let document = json!({
            "openapi": "3.0.3",
            "info": {"title": "Depth", "version": "1"},
            "servers": [{"url": "https://api.example.test"}],
            "paths": {
                "/pets": {
                    "get": {
                        "operationId": "depth",
                        "parameters": [{"$ref": "./d0.yaml#/next"}],
                        "responses": {"200": {"description": "ok"}}
                    }
                }
            }
        })
        .to_string();
        let config = OpenApiConfig {
            document: Some(document.clone()),
            document_base_dir: Some(depth_directory.display().to_string()),
            ..OpenApiConfig::default()
        };
        let error = build_tools(&config, &document).await.unwrap_err();
        assert!(error.contains("depth exceeds"), "{error}");
        let _ = std::fs::remove_dir_all(depth_directory);
    }
}
