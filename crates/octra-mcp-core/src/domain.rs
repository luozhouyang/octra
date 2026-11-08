//! Persisted MCP Gateway entities and their default ACL policy.

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};

use crate::protocol::Transport;

/// Invocation lifecycle statuses stored in `mcp_invocations.status`.
pub mod status {
    pub const OK: &str = "ok";
    pub const ERROR: &str = "error";
    pub const TIMEOUT: &str = "timeout";
    pub const DENIED: &str = "denied";
    pub const NOT_FOUND: &str = "not_found";
    pub const INVALID: &str = "invalid";
}

/// `mcp_sessions.status` values. A terminated session keeps its row for audit
/// and answers later requests on the same id with "session not found".
pub mod session_status {
    pub const OPEN: &str = "open";
    pub const TERMINATED: &str = "terminated";
}

/// The ACL policy when a tool has no `mcp_tool_acls` rows at all.
///
/// Octra ships open-local by default: a tool without explicit rows is callable
/// by every authenticated (or, when no API keys exist, anonymous) consumer.
/// Flip this to `false` to make that case default-deny instead; tools with
/// rows are always governed by those rows either way.
pub const ACL_DEFAULT_ALLOW: bool = true;

/// Kong-style MCP server modes that Octra implements today.
///
/// `passthrough-listener` is the legacy transport-oriented behaviour and the
/// default for existing rows. `listener` aggregates the tools of its
/// [`McpServer::sources`]. `upstream-server` is a plain upstream that only
/// contributes tools to listeners. The OpenAPI conversion modes generate HTTP
/// tools from a document: `conversion-listener` serves them directly while
/// `conversion-only` only contributes them to listeners.
pub const MCP_SERVER_MODES: &[&str] = &[
    "passthrough-listener",
    "listener",
    "upstream-server",
    "conversion-listener",
    "conversion-only",
];

pub const MCP_SERVER_MODE_PASSTHROUGH_LISTENER: &str = "passthrough-listener";
pub const MCP_SERVER_MODE_LISTENER: &str = "listener";
pub const MCP_SERVER_MODE_UPSTREAM_SERVER: &str = "upstream-server";
pub const MCP_SERVER_MODE_CONVERSION_LISTENER: &str = "conversion-listener";
pub const MCP_SERVER_MODE_CONVERSION_ONLY: &str = "conversion-only";

/// Mode assigned to servers created before modes existed and to requests that
/// omit `type`/`mode`.
pub fn default_server_mode() -> String {
    MCP_SERVER_MODE_PASSTHROUGH_LISTENER.to_string()
}

/// True for the modes that generate tools from an OpenAPI document.
pub fn is_conversion_server_mode(mode: &str) -> bool {
    matches!(
        mode,
        MCP_SERVER_MODE_CONVERSION_LISTENER | MCP_SERVER_MODE_CONVERSION_ONLY
    )
}

/// Validates an MCP server mode.
pub fn validate_server_mode(mode: &str) -> Result<(), String> {
    if MCP_SERVER_MODES.contains(&mode) {
        return Ok(());
    }
    Err(format!(
        "invalid MCP server mode '{mode}'; expected one of {}",
        MCP_SERVER_MODES.join(", ")
    ))
}

/// True when a server of this mode may be listed in another server's
/// `sources`. Listeners never source each other in Octra. A `conversion-only`
/// server is a valid source but has no directly routable endpoint.
pub fn is_source_server_mode(mode: &str) -> bool {
    matches!(
        mode,
        MCP_SERVER_MODE_UPSTREAM_SERVER
            | MCP_SERVER_MODE_PASSTHROUGH_LISTENER
            | MCP_SERVER_MODE_CONVERSION_ONLY
    )
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct McpServer {
    pub id: String,
    pub name: String,
    pub transport: Transport,
    /// Kong calls this field `type`; `mode` is accepted as an alias on input.
    #[serde(rename = "type", alias = "mode", default = "default_server_mode")]
    pub mode: String,
    /// Server names this listener aggregates tools from. Empty on a listener
    /// means every enabled `upstream-server` and `passthrough-listener`.
    #[serde(default)]
    pub sources: Vec<String>,
    /// Explicit public path. `None` means the effective `/mcp/<name>` default.
    pub route_path: Option<String>,
    /// CEL expressions that may grant access when non-empty; any deny wins.
    #[serde(default)]
    pub acl_allow_when: Vec<String>,
    /// CEL expressions that deny access when any evaluates to true.
    #[serde(default)]
    pub acl_deny_when: Vec<String>,
    /// Per-server auth strategy. `None` uses the default strategy, then MCP
    /// API keys.
    #[serde(default)]
    pub auth_strategy_id: Option<String>,
    pub command: Option<String>,
    pub url: Option<String>,
    pub enabled: bool,
    pub config: Value,
    pub headers: Map<String, Value>,
    pub protocol_version: Option<String>,
    pub last_seen_at: Option<String>,
    pub created_at: String,
    pub updated_at: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct NewMcpServer {
    pub name: String,
    pub transport: Transport,
    #[serde(rename = "type", alias = "mode", default = "default_server_mode")]
    pub mode: String,
    #[serde(default)]
    pub sources: Vec<String>,
    #[serde(default)]
    pub route_path: Option<String>,
    #[serde(default)]
    pub acl_allow_when: Vec<String>,
    #[serde(default)]
    pub acl_deny_when: Vec<String>,
    #[serde(default)]
    pub auth_strategy_id: Option<String>,
    pub command: Option<String>,
    pub url: Option<String>,
    pub enabled: bool,
    pub config: Value,
    /// Header values may be `<scheme>:<reference>` secret references; they are
    /// never stored in plaintext.
    pub headers: Map<String, Value>,
}

impl NewMcpServer {
    /// Ensures the transport has the fields it needs and no extra ones.
    pub fn validate(&self) -> Result<(), String> {
        crate::protocol::is_valid_server_name(&self.name)
            .then_some(())
            .ok_or_else(|| format!("invalid server name: {}", self.name))?;
        validate_server_mode(&self.mode)?;

        // Conversion servers call the HTTP API described by their OpenAPI
        // document, so they need neither an MCP command nor an MCP url.
        if !is_conversion_server_mode(&self.mode) {
            match self.transport {
                Transport::Stdio => {
                    let command = self.command.as_deref().unwrap_or_default().trim();
                    if command.is_empty() {
                        return Err("stdio servers require a command".into());
                    }
                    if self
                        .url
                        .as_deref()
                        .is_some_and(|url| !url.trim().is_empty())
                    {
                        return Err("stdio servers must not set a url".into());
                    }
                }
                Transport::Http => {
                    let url = self.url.as_deref().unwrap_or_default().trim();
                    if !is_http_url(url) {
                        return Err(format!("http servers require an http(s) url, got: {url}"));
                    }
                    if self
                        .command
                        .as_deref()
                        .is_some_and(|c| !c.trim().is_empty())
                    {
                        return Err("http servers must not set a command".into());
                    }
                }
            }
        }

        if !self.config.is_object() {
            return Err("config must be a JSON object".into());
        }

        let has_openapi = self.config.get("openapi").is_some();
        if is_conversion_server_mode(&self.mode) && !has_openapi {
            return Err(format!(
                "MCP server mode '{}' requires config.openapi",
                self.mode
            ));
        }
        if !is_conversion_server_mode(&self.mode) && has_openapi {
            return Err(format!(
                "config.openapi is only supported for conversion modes, not '{}'",
                self.mode
            ));
        }
        for (name, value) in &self.headers {
            if name.trim().is_empty() {
                return Err("header names must not be empty".into());
            }
            if !value.is_string() {
                return Err(format!("header {name} must be a string"));
            }
        }
        Ok(())
    }
}

/// Auth strategy kinds Octra implements today.
pub const MCP_AUTH_STRATEGY_KINDS: &[&str] = &["oidc"];
pub const MCP_AUTH_STRATEGY_KIND_OIDC: &str = "oidc";

/// Signature algorithms accepted for OIDC strategies.
pub const MCP_OIDC_ALGORITHMS: &[&str] = &["RS256", "RS384", "RS512", "ES256", "ES384"];

/// Token validation modes for OIDC strategies.
pub const MCP_OIDC_TOKEN_VALIDATIONS: &[&str] = &["auto", "jwt", "introspection"];
pub const MCP_OIDC_TOKEN_VALIDATION_AUTO: &str = "auto";
pub const MCP_OIDC_TOKEN_VALIDATION_JWT: &str = "jwt";
pub const MCP_OIDC_TOKEN_VALIDATION_INTROSPECTION: &str = "introspection";

/// A persisted data-plane authentication strategy.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct McpAuthStrategy {
    pub id: String,
    pub name: String,
    /// `oidc` is the only implemented kind.
    pub kind: String,
    pub config: Value,
    pub enabled: bool,
    pub is_default: bool,
    pub created_at: String,
    pub updated_at: String,
}

/// Input for creating or replacing an auth strategy.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct NewMcpAuthStrategy {
    pub name: String,
    pub kind: String,
    pub config: Value,
    pub enabled: bool,
    pub is_default: bool,
}

impl NewMcpAuthStrategy {
    /// Validates the kind and the kind-specific configuration.
    pub fn validate(&self) -> Result<(), String> {
        if self.name.trim().is_empty() {
            return Err("auth strategy name must not be empty".into());
        }
        if !MCP_AUTH_STRATEGY_KINDS.contains(&self.kind.as_str()) {
            return Err(format!(
                "invalid auth strategy kind '{}'; expected one of {}",
                self.kind,
                MCP_AUTH_STRATEGY_KINDS.join(", ")
            ));
        }
        self.oidc_config().map(|_| ())
    }

    /// The OIDC configuration of this strategy.
    pub fn oidc_config(&self) -> Result<McpOidcConfig, String> {
        McpOidcConfig::from_value(&self.config)
    }
}

impl McpAuthStrategy {
    /// The OIDC configuration of a stored strategy.
    pub fn oidc_config(&self) -> Result<McpOidcConfig, String> {
        McpOidcConfig::from_value(&self.config)
    }
}

/// OIDC/JWKS settings stored in `mcp_auth_strategies.config_json`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct McpOidcConfig {
    /// Expected `iss` claim and the identity provider identifier.
    pub issuer: String,
    /// Absolute URL of the JWKS document with signing keys. Optional when a
    /// discovery document supplies it.
    #[serde(default)]
    pub jwks_uri: Option<String>,
    /// Absolute URL of the OpenID Connect discovery document. Defaults to
    /// `{issuer}/.well-known/openid-configuration` when omitted.
    #[serde(default)]
    pub discovery_url: Option<String>,
    /// RFC 7662 introspection settings for opaque access tokens.
    #[serde(default)]
    pub introspection: Option<McpOidcIntrospection>,
    /// How bearer tokens are validated: `auto`, `jwt`, or `introspection`.
    #[serde(default = "default_token_validation")]
    pub token_validation: String,
    /// Expected `aud` claim; when unset, audiences are not validated.
    #[serde(default)]
    pub audience: Option<String>,
    /// Every scope must be present in `scope` or `scp`.
    #[serde(default)]
    pub required_scopes: Vec<String>,
    /// Claim that identifies the consumer. Defaults to `sub`.
    #[serde(default = "default_consumer_claim")]
    pub consumer_claim: String,
    /// Preferred display-name claim; falls back to `preferred_username`,
    /// `email`, then the consumer id.
    #[serde(default)]
    pub username_claim: Option<String>,
    /// Claim with group names. Defaults to `groups` when unset.
    #[serde(default = "default_groups_claim")]
    pub groups_claim: Option<String>,
    /// JWT algorithms allowed for this strategy.
    #[serde(default = "default_oidc_algorithms")]
    pub algorithms: Vec<String>,
    /// In-process JWKS and discovery cache lifetime in seconds.
    #[serde(default = "default_jwks_cache_secs")]
    pub jwks_cache_secs: u64,
}

/// RFC 7662 token introspection settings.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct McpOidcIntrospection {
    /// Absolute introspection endpoint. When empty, the discovery document
    /// supplies `introspection_endpoint`.
    #[serde(default)]
    pub endpoint: String,
    /// OAuth client id used for HTTP Basic or form authentication.
    pub client_id: String,
    /// `env:` or `file:` reference resolved at request time.
    pub client_secret_ref: String,
    /// In-process result cache lifetime in seconds.
    #[serde(default = "default_introspection_cache_secs")]
    pub cache_secs: u64,
}

impl Default for McpOidcIntrospection {
    fn default() -> Self {
        Self {
            endpoint: String::new(),
            client_id: String::new(),
            client_secret_ref: String::new(),
            cache_secs: default_introspection_cache_secs(),
        }
    }
}

impl Default for McpOidcConfig {
    fn default() -> Self {
        Self {
            issuer: String::new(),
            jwks_uri: None,
            discovery_url: None,
            introspection: None,
            token_validation: default_token_validation(),
            audience: None,
            required_scopes: Vec::new(),
            consumer_claim: default_consumer_claim(),
            username_claim: None,
            groups_claim: default_groups_claim(),
            algorithms: default_oidc_algorithms(),
            jwks_cache_secs: default_jwks_cache_secs(),
        }
    }
}

fn default_consumer_claim() -> String {
    "sub".to_string()
}

fn default_groups_claim() -> Option<String> {
    Some("groups".to_string())
}

fn default_oidc_algorithms() -> Vec<String> {
    vec!["RS256".to_string(), "ES256".to_string()]
}

fn default_jwks_cache_secs() -> u64 {
    300
}

fn default_token_validation() -> String {
    MCP_OIDC_TOKEN_VALIDATION_AUTO.to_string()
}

fn default_introspection_cache_secs() -> u64 {
    60
}

impl McpOidcConfig {
    /// Parses and validates a strategy config object.
    pub fn from_value(value: &Value) -> Result<Self, String> {
        let mut config: Self = serde_json::from_value(value.clone())
            .map_err(|err| format!("invalid OIDC config: {err}"))?;
        config.normalize();
        config.validate()?;
        Ok(config)
    }

    /// Treats blank optional URLs and endpoints as absent.
    fn normalize(&mut self) {
        self.jwks_uri = self
            .jwks_uri
            .take()
            .map(|value| value.trim().to_string())
            .filter(|value| !value.is_empty());
        self.discovery_url = self
            .discovery_url
            .take()
            .map(|value| value.trim().to_string())
            .filter(|value| !value.is_empty());
        if let Some(introspection) = self.introspection.as_mut() {
            introspection.endpoint = introspection.endpoint.trim().to_string();
        }
    }

    /// Validates URLs, claim names, algorithms, scopes, and cache lifetime.
    pub fn validate(&self) -> Result<(), String> {
        validate_absolute_http_url("issuer", &self.issuer)?;
        if let Some(jwks_uri) = self.jwks_uri.as_deref() {
            validate_absolute_http_url("jwks_uri", jwks_uri)?;
        }
        if let Some(discovery_url) = self.discovery_url.as_deref() {
            validate_absolute_http_url("discovery_url", discovery_url)?;
        }
        if !MCP_OIDC_TOKEN_VALIDATIONS.contains(&self.token_validation.as_str()) {
            return Err(format!(
                "invalid token_validation '{}'; expected one of {}",
                self.token_validation,
                MCP_OIDC_TOKEN_VALIDATIONS.join(", ")
            ));
        }
        if let Some(introspection) = self.introspection.as_ref() {
            if !introspection.endpoint.is_empty() {
                validate_absolute_http_url("introspection.endpoint", &introspection.endpoint)?;
            }
            if introspection.client_id.trim().is_empty() {
                return Err("introspection.client_id must not be empty".into());
            }
            validate_client_secret_reference(
                "introspection.client_secret_ref",
                &introspection.client_secret_ref,
            )?;
            if !(1..=3_600).contains(&introspection.cache_secs) {
                return Err(format!(
                    "introspection.cache_secs must be between 1 and 3600, got {}",
                    introspection.cache_secs
                ));
            }
        }
        match self.token_validation.as_str() {
            MCP_OIDC_TOKEN_VALIDATION_JWT => {
                if self.jwks_uri.is_none() && self.discovery_url.is_none() {
                    return Err("token_validation 'jwt' requires jwks_uri or discovery_url".into());
                }
            }
            MCP_OIDC_TOKEN_VALIDATION_INTROSPECTION => {
                if self.introspection.is_none() {
                    return Err(
                        "token_validation 'introspection' requires introspection settings".into(),
                    );
                }
            }
            MCP_OIDC_TOKEN_VALIDATION_AUTO
                if self.jwks_uri.is_none()
                    && self.discovery_url.is_none()
                    && self.introspection.is_none() =>
            {
                return Err(
                    "token_validation 'auto' requires jwks_uri, discovery_url, or introspection"
                        .into(),
                );
            }
            _ => {}
        }
        if let Some(audience) = self.audience.as_deref()
            && audience.trim().is_empty()
        {
            return Err("audience must not be empty when set".into());
        }
        validate_claim_name("consumer_claim", &self.consumer_claim)?;
        if let Some(claim) = self.username_claim.as_deref() {
            validate_claim_name("username_claim", claim)?;
        }
        if let Some(claim) = self.groups_claim.as_deref() {
            validate_claim_name("groups_claim", claim)?;
        }
        if self.algorithms.is_empty() {
            return Err("algorithms must contain at least one algorithm".into());
        }
        for algorithm in &self.algorithms {
            if !MCP_OIDC_ALGORITHMS.contains(&algorithm.as_str()) {
                return Err(format!(
                    "unsupported OIDC algorithm '{algorithm}'; expected one of {}",
                    MCP_OIDC_ALGORITHMS.join(", ")
                ));
            }
        }
        for scope in &self.required_scopes {
            if scope.trim().is_empty() {
                return Err("required_scopes entries must be non-empty strings".into());
            }
        }
        if !(1..=86_400).contains(&self.jwks_cache_secs) {
            return Err(format!(
                "jwks_cache_secs must be between 1 and 86400, got {}",
                self.jwks_cache_secs
            ));
        }
        Ok(())
    }
}

fn validate_client_secret_reference(field: &str, value: &str) -> Result<(), String> {
    let Some((scheme, name)) = value.split_once(':') else {
        return Err(format!("{field} must be an env: or file: reference"));
    };
    if !matches!(scheme, "env" | "file") || name.trim().is_empty() {
        return Err(format!("{field} must be an env: or file: reference"));
    }
    Ok(())
}

fn validate_claim_name(field: &str, claim: &str) -> Result<(), String> {
    if claim.trim().is_empty() {
        return Err(format!("{field} must be a non-empty claim name"));
    }
    Ok(())
}

/// Accepts only absolute `http://` or `https://` URLs with a non-empty host.
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

fn is_http_url(url: &str) -> bool {
    let lower = url.to_ascii_lowercase();
    lower.starts_with("http://") || lower.starts_with("https://")
}

/// Default public route of a server: `/mcp/<name>`.
pub fn default_route_path(name: &str) -> String {
    format!("/mcp/{name}")
}

/// Effective route path, using the stored explicit value when present.
pub fn effective_route_path(route_path: Option<&str>, name: &str) -> String {
    route_path
        .map(str::to_string)
        .unwrap_or_else(|| default_route_path(name))
}

/// Normalizes a route path to the wildcard tail matched by `/mcp/{*rest}`.
///
/// Both `/mcp/files` and `/files` address the same scoped endpoint, so their
/// keys are equal. This keeps uniqueness checks aligned with routing.
pub fn route_path_key(route_path: &str) -> String {
    let trimmed = route_path.trim_matches('/');
    trimmed.strip_prefix("mcp/").unwrap_or(trimmed).to_string()
}

/// Validates an explicitly configured route path.
///
/// Paths live below the `/mcp/` wildcard, must be absolute, and may not carry
/// a query or fragment. `/mcp` and `/mcp/` stay reserved for the global
/// aggregate endpoint.
pub fn validate_route_path(route_path: &str) -> Result<(), String> {
    if !route_path.starts_with('/') {
        return Err(format!("route_path must start with '/', got: {route_path}"));
    }
    if route_path.contains('?') || route_path.contains('#') {
        return Err("route_path must not contain a query string or fragment".into());
    }
    if route_path == "/mcp" || route_path == "/mcp/" {
        return Err("route_path '/mcp' is reserved for the global aggregate endpoint".into());
    }
    if route_path_key(route_path).is_empty() {
        return Err("route_path must name a path below /mcp".into());
    }
    Ok(())
}

impl McpServer {
    /// Explicit route path when stored, otherwise the `/mcp/<name>` default.
    pub fn effective_route_path(&self) -> String {
        effective_route_path(self.route_path.as_deref(), &self.name)
    }
}

/// Regular tool discovered from an MCP server.
pub const MCP_TOOL_KIND_MCP: &str = "mcp";
/// Tool generated from an OpenAPI operation and executed over HTTP.
pub const MCP_TOOL_KIND_HTTP: &str = "http";

fn default_tool_kind() -> String {
    MCP_TOOL_KIND_MCP.to_string()
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct McpTool {
    pub id: String,
    pub server_id: String,
    pub name: String,
    pub description: String,
    /// The MCP `inputSchema` for both regular and generated HTTP tools.
    pub schema: Value,
    #[serde(default = "default_tool_kind")]
    pub kind: String,
    #[serde(default)]
    pub http_method: Option<String>,
    #[serde(default)]
    pub http_path: Option<String>,
    #[serde(default = "empty_object")]
    pub http_config: Value,
    pub enabled: bool,
    pub created_at: String,
}

impl McpTool {
    /// True when this tool is executed as an HTTP request instead of an MCP
    /// `tools/call` forwarded to an upstream server.
    pub fn is_http(&self) -> bool {
        self.kind == MCP_TOOL_KIND_HTTP
    }
}

impl McpTool {
    /// Name advertised to clients through `tools/list`.
    pub fn qualified_name(&self, server_name: &str) -> String {
        crate::protocol::qualify_tool_name(server_name, &self.name)
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct McpSession {
    pub id: String,
    pub server_id: String,
    pub status: String,
    pub created_at: String,
    pub updated_at: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct McpInvocation {
    pub id: i64,
    pub server_id: String,
    pub tool_id: String,
    pub consumer_id: Option<String>,
    pub request_id: String,
    pub status: String,
    pub duration_ms: i64,
    pub started_at: String,
    pub finished_at: Option<String>,
    pub error: Option<String>,
}

/// One ACL rule for a tool. Exactly one scope is set: a consumer, a consumer
/// group, or neither for the tool default.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct McpToolAcl {
    pub id: String,
    pub tool_id: String,
    pub consumer_id: Option<String>,
    pub consumer_group_id: Option<String>,
    pub allowed: bool,
    pub created_at: String,
    pub updated_at: String,
}

/// One ACL rule for a server. Exactly one scope is set: a consumer, a consumer
/// group, or neither for the server default. A server rule applies to every
/// tool of the server that has no rows of its own.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct McpServerAcl {
    pub id: String,
    pub server_id: String,
    pub consumer_id: Option<String>,
    pub consumer_group_id: Option<String>,
    pub allowed: bool,
    pub created_at: String,
    pub updated_at: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct McpApiKey {
    pub id: String,
    pub name: String,
    pub key_prefix: String,
    pub consumer_id: Option<String>,
    pub enabled: bool,
    pub expires_at: Option<String>,
    pub created_at: String,
}

/// A newly minted API key. The plaintext is returned to the caller exactly
/// once and never persisted.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct NewApiKey {
    pub key: McpApiKey,
    pub plaintext: String,
}

/// Pure decision function for a tool invocation.
///
/// This is the legacy consumer-only view built from bare `allowed` flags and a
/// single "is a consumer known" bit. Scoped decisions (consumer, consumer
/// group, and tool default) use [`acl_allows_scoped`].
pub fn acl_decision(rows: impl IntoIterator<Item = bool>, consumer_known: bool) -> bool {
    let mut saw_row = false;
    for allowed in rows {
        saw_row = true;
        if allowed && consumer_known {
            return true;
        }
    }
    if saw_row { false } else { ACL_DEFAULT_ALLOW }
}

/// The scope columns shared by tool and server ACL rows.
struct ScopedAcl<'a> {
    consumer_id: Option<&'a str>,
    consumer_group_id: Option<&'a str>,
    allowed: bool,
}

impl ScopedAcl<'_> {
    fn is_default(&self) -> bool {
        self.consumer_id.is_none() && self.consumer_group_id.is_none()
    }
}

/// Scoped ACL decision over a generic row set.
///
/// Precedence, highest first:
/// 1. No rows -> the open-local default ([`ACL_DEFAULT_ALLOW`]).
/// 2. An explicit consumer row -> its `allowed` value.
/// 3. Group rows matching the caller's groups -> any explicit deny wins;
///    otherwise any allow grants.
/// 4. The default row (both scope ids null) -> its `allowed` value.
/// 5. Rows exist but none apply -> deny.
///
/// Anonymous callers (`consumer_id` is `None`) only match the default row.
fn acl_allows_rows<'a>(
    rows: impl IntoIterator<Item = ScopedAcl<'a>>,
    consumer_id: Option<&str>,
    group_ids: &[String],
) -> bool {
    let rows: Vec<ScopedAcl<'a>> = rows.into_iter().collect();
    if rows.is_empty() {
        return ACL_DEFAULT_ALLOW;
    }

    if let Some(consumer_id) = consumer_id {
        if let Some(row) = rows.iter().find(|row| row.consumer_id == Some(consumer_id)) {
            return row.allowed;
        }

        let mut saw_group = false;
        let mut group_allows = false;
        for row in &rows {
            let Some(group_id) = row.consumer_group_id else {
                continue;
            };
            if !group_ids.iter().any(|id| id == group_id) {
                continue;
            }
            saw_group = true;
            if !row.allowed {
                return false;
            }
            group_allows = true;
        }
        if saw_group {
            return group_allows;
        }
    }

    rows.iter()
        .find(|row| row.is_default())
        .is_some_and(|row| row.allowed)
}

/// Scoped ACL decision for one tool. See [`acl_allows_rows`] for precedence.
pub fn acl_allows_scoped(
    rows: &[McpToolAcl],
    consumer_id: Option<&str>,
    group_ids: &[String],
) -> bool {
    acl_allows_rows(rows.iter().map(tool_acl_scope), consumer_id, group_ids)
}

/// Decision for a tool that may inherit its server's default ACL.
///
/// Non-empty tool rows override the server rows entirely, even when none of
/// them match the caller. Otherwise non-empty server rows apply, and when both
/// row sets are empty the open-local default allows.
pub fn acl_allows_default_then_tool(
    tool_rows: &[McpToolAcl],
    server_rows: &[McpServerAcl],
    consumer_id: Option<&str>,
    group_ids: &[String],
) -> bool {
    if !tool_rows.is_empty() {
        return acl_allows_scoped(tool_rows, consumer_id, group_ids);
    }
    acl_allows_rows(
        server_rows.iter().map(server_acl_scope),
        consumer_id,
        group_ids,
    )
}

fn tool_acl_scope(row: &McpToolAcl) -> ScopedAcl<'_> {
    ScopedAcl {
        consumer_id: row.consumer_id.as_deref(),
        consumer_group_id: row.consumer_group_id.as_deref(),
        allowed: row.allowed,
    }
}

fn server_acl_scope(row: &McpServerAcl) -> ScopedAcl<'_> {
    ScopedAcl {
        consumer_id: row.consumer_id.as_deref(),
        consumer_group_id: row.consumer_group_id.as_deref(),
        allowed: row.allowed,
    }
}

/// Secrets live in references (`env:NAME`, `keyring:NAME`, or `inline:` is
/// rejected), never in the `headers_json` column.
pub fn validate_secret_reference(value: &str) -> Result<(), String> {
    if value.starts_with("inline:") {
        return Err("inline secrets are not allowed; use env: or keyring: references".into());
    }
    let scheme = value.split_once(':').map(|(scheme, _)| scheme);
    match scheme {
        Some("env") | Some("keyring") | Some("file") => Ok(()),
        _ => Err(format!(
            "header secret must be a reference such as env:NAME, got: {value}"
        )),
    }
}

/// Returns the reference string when the value is a `scheme:name` reference.
pub fn secret_reference(value: &Value) -> Option<&str> {
    value
        .as_str()
        .filter(|raw| validate_secret_reference(raw).is_ok())
}

pub fn empty_object() -> Value {
    json!({})
}

/// JSON passed to clients in `tools/list` items.
pub fn tool_descriptor(qualified: &str, description: &str, schema: &Value) -> Value {
    json!({
        "name": qualified,
        "description": description,
        "inputSchema": schema,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stdio(name: &str) -> NewMcpServer {
        NewMcpServer {
            name: name.into(),
            transport: Transport::Stdio,
            mode: default_server_mode(),
            sources: Vec::new(),
            route_path: None,
            acl_allow_when: Vec::new(),
            acl_deny_when: Vec::new(),
            auth_strategy_id: None,
            command: Some("node".into()),
            url: None,
            enabled: true,
            config: empty_object(),
            headers: Map::new(),
        }
    }

    #[test]
    fn validates_supported_server_modes() {
        for mode in MCP_SERVER_MODES {
            assert!(
                validate_server_mode(mode).is_ok(),
                "supported mode rejected: {mode}"
            );
            let mut server = stdio("files");
            server.mode = (*mode).to_string();
            if is_conversion_server_mode(mode) {
                server.config = json!({"openapi": {"document": "{}"}});
            }
            assert!(server.validate().is_ok(), "mode rejected: {mode}");
        }
        assert_eq!(default_server_mode(), "passthrough-listener");
    }

    #[test]
    fn conversion_modes_require_openapi_and_other_modes_reject_it() {
        let error = validate_server_mode("proxy").unwrap_err();
        assert!(error.contains("invalid MCP server mode"), "{error}");

        let mut server = stdio("files");
        server.mode = "conversion-only".into();
        let error = server.validate().unwrap_err();
        assert!(error.contains("requires config.openapi"), "{error}");

        server.config = json!({"openapi": {"document": "{}"}});
        assert!(server.validate().is_ok());

        let mut plain = stdio("files");
        plain.config = json!({"openapi": {"document": "{}"}});
        let error = plain.validate().unwrap_err();
        assert!(error.contains("only supported for conversion"), "{error}");
    }

    #[test]
    fn source_eligible_modes_include_conversion_only() {
        assert!(is_source_server_mode("upstream-server"));
        assert!(is_source_server_mode("passthrough-listener"));
        assert!(is_source_server_mode("conversion-only"));
        assert!(!is_source_server_mode("listener"));
        assert!(!is_source_server_mode("conversion-listener"));
    }

    #[test]
    fn validates_stdio_and_http_servers() {
        assert!(stdio("files").validate().is_ok());

        let mut bad = stdio("files");
        bad.command = None;
        assert!(bad.validate().is_err());

        let mut http = stdio("remote");
        http.transport = Transport::Http;
        http.command = None;
        http.url = Some("https://example.test/mcp".into());
        assert!(http.validate().is_ok());

        http.url = Some("ftp://example.test".into());
        assert!(http.validate().is_err());

        assert!(stdio("bad__name").validate().is_err());
    }

    #[test]
    fn acl_decision_follows_rows_or_default() {
        assert!(acl_decision(Vec::<bool>::new(), false));
        assert!(acl_decision([true], true));
        assert!(acl_decision([false, true], true));
        assert!(!acl_decision([false], true));
        assert!(!acl_decision([true], false));
    }

    fn acl(
        consumer_id: Option<&str>,
        consumer_group_id: Option<&str>,
        allowed: bool,
    ) -> McpToolAcl {
        McpToolAcl {
            id: format!("acl-{consumer_id:?}-{consumer_group_id:?}-{allowed}"),
            tool_id: "tool-1".into(),
            consumer_id: consumer_id.map(str::to_string),
            consumer_group_id: consumer_group_id.map(str::to_string),
            allowed,
            created_at: "2026-01-01T00:00:00.000Z".into(),
            updated_at: "2026-01-01T00:00:00.000Z".into(),
        }
    }

    #[test]
    fn scoped_acl_without_rows_follows_open_default() {
        assert!(acl_allows_scoped(&[], Some("c1"), &["g1".into()]));
        assert!(acl_allows_scoped(&[], None, &[]));
    }

    #[test]
    fn scoped_acl_consumer_row_wins_over_group_and_default() {
        let rows = [
            acl(None, None, false),
            acl(None, Some("g1"), false),
            acl(Some("c1"), None, true),
        ];
        assert!(acl_allows_scoped(&rows, Some("c1"), &["g1".into()]));

        let rows = [
            acl(None, None, true),
            acl(None, Some("g1"), true),
            acl(Some("c1"), None, false),
        ];
        assert!(!acl_allows_scoped(&rows, Some("c1"), &["g1".into()]));
    }

    #[test]
    fn scoped_acl_group_deny_overrides_group_allow() {
        let rows = [acl(None, Some("g1"), true), acl(None, Some("g2"), false)];
        let groups = ["g1".to_string(), "g2".to_string()];
        assert!(!acl_allows_scoped(&rows, Some("c1"), &groups));

        // A group the caller is not in does not deny a matching allow.
        assert!(acl_allows_scoped(
            &[acl(None, Some("g1"), true), acl(None, Some("g2"), false)],
            Some("c1"),
            &["g1".into()]
        ));
    }

    #[test]
    fn scoped_acl_default_row_applies_when_no_scope_matches() {
        let rows = [acl(None, None, false), acl(None, Some("other"), true)];
        assert!(!acl_allows_scoped(&rows, Some("c1"), &["g1".into()]));

        let rows = [acl(None, None, true)];
        assert!(acl_allows_scoped(&rows, Some("c1"), &["g1".into()]));
    }

    #[test]
    fn scoped_acl_anonymous_only_matches_default_row() {
        let rows = [acl(None, Some("g1"), true)];
        assert!(!acl_allows_scoped(&rows, None, &[]));
        // Even if group ids were supplied, an anonymous caller ignores them.
        assert!(!acl_allows_scoped(&rows, None, &["g1".into()]));

        let rows = [acl(None, None, true)];
        assert!(acl_allows_scoped(&rows, None, &[]));
    }

    #[test]
    fn scoped_acl_denies_when_no_row_applies() {
        let rows = [acl(Some("c2"), None, true), acl(None, Some("g2"), true)];
        assert!(!acl_allows_scoped(&rows, Some("c1"), &["g1".into()]));
        assert!(!acl_allows_scoped(&rows, None, &[]));
    }

    fn server_acl(
        consumer_id: Option<&str>,
        consumer_group_id: Option<&str>,
        allowed: bool,
    ) -> McpServerAcl {
        McpServerAcl {
            id: format!("server-acl-{consumer_id:?}-{consumer_group_id:?}-{allowed}"),
            server_id: "server-1".into(),
            consumer_id: consumer_id.map(str::to_string),
            consumer_group_id: consumer_group_id.map(str::to_string),
            allowed,
            created_at: "2026-01-01T00:00:00.000Z".into(),
            updated_at: "2026-01-01T00:00:00.000Z".into(),
        }
    }

    #[test]
    fn default_acl_falls_back_from_tool_to_server_to_open() {
        // Neither scope has rows: open-local default.
        assert!(acl_allows_default_then_tool(&[], &[], Some("c1"), &[]));

        // Server rows apply when the tool has none.
        let server_rows = [server_acl(None, None, false)];
        assert!(!acl_allows_default_then_tool(
            &[],
            &server_rows,
            Some("c1"),
            &[]
        ));

        // Non-empty tool rows override server rows, even when they deny.
        let tool_rows = [acl(Some("c1"), None, true)];
        let server_rows = [server_acl(None, None, false)];
        assert!(acl_allows_default_then_tool(
            &tool_rows,
            &server_rows,
            Some("c1"),
            &[]
        ));

        // A local deny wins over an open server default.
        let tool_rows = [acl(None, None, false)];
        let server_rows = [server_acl(None, None, true)];
        assert!(!acl_allows_default_then_tool(
            &tool_rows,
            &server_rows,
            Some("c1"),
            &[]
        ));
    }

    #[test]
    fn server_defaults_follow_the_same_scope_precedence() {
        let server_rows = [
            server_acl(None, None, false),
            server_acl(None, Some("g1"), false),
            server_acl(Some("c1"), None, true),
        ];
        assert!(acl_allows_default_then_tool(
            &[],
            &server_rows,
            Some("c1"),
            &["g1".into()]
        ));

        // Anonymous callers only match the default row.
        assert!(!acl_allows_default_then_tool(&[], &server_rows, None, &[]));

        // A matching group allow grants over the default deny.
        let server_rows = [
            server_acl(None, None, false),
            server_acl(None, Some("g1"), true),
        ];
        assert!(acl_allows_default_then_tool(
            &[],
            &server_rows,
            Some("c2"),
            &["g1".into()]
        ));
    }

    #[test]
    fn rejects_inline_secrets() {
        assert!(validate_secret_reference("env:TOKEN").is_ok());
        assert!(validate_secret_reference("keyring:svc").is_ok());
        assert!(validate_secret_reference("inline:abc").is_err());
        assert!(validate_secret_reference("plain-value").is_err());
    }

    #[test]
    fn tool_descriptor_uses_namespaced_name() {
        let descriptor = tool_descriptor("files__read", "Read", &json!({"type": "object"}));
        assert_eq!(descriptor["name"], "files__read");
        assert_eq!(descriptor["inputSchema"]["type"], "object");
    }

    #[test]
    fn effective_route_path_falls_back_to_the_server_name() {
        assert_eq!(default_route_path("files"), "/mcp/files");
        assert_eq!(effective_route_path(None, "files"), "/mcp/files");
        assert_eq!(
            effective_route_path(Some("/mcp/team/files"), "files"),
            "/mcp/team/files"
        );
        assert_eq!(route_path_key("/mcp/files"), "files");
        assert_eq!(route_path_key("/files"), "files");
        assert_eq!(route_path_key("/mcp/team/files"), "team/files");
    }

    #[test]
    fn validates_explicit_route_paths() {
        assert!(validate_route_path("/mcp/files").is_ok());
        assert!(validate_route_path("/team/files").is_ok());

        for path in [
            "mcp/files",
            "",
            "/mcp",
            "/mcp/",
            "/",
            "/mcp/files?token=x",
            "/mcp/files#fragment",
        ] {
            assert!(
                validate_route_path(path).is_err(),
                "route path should be rejected: {path}"
            );
        }
    }

    #[test]
    fn oidc_config_defaults_and_validates_its_fields() {
        let config = McpOidcConfig::from_value(&json!({
            "issuer": "https://issuer.example.test",
            "jwks_uri": "https://issuer.example.test/.well-known/jwks.json",
        }))
        .unwrap();
        assert_eq!(config.consumer_claim, "sub");
        assert_eq!(config.groups_claim.as_deref(), Some("groups"));
        assert_eq!(config.algorithms, vec!["RS256", "ES256"]);
        assert_eq!(config.jwks_cache_secs, 300);
        assert_eq!(config.token_validation, "auto");
        assert!(config.discovery_url.is_none());
        assert!(config.introspection.is_none());

        for bad in [
            json!({"issuer": "issuer.example.test", "jwks_uri": "https://example.test/jwks"}),
            json!({"issuer": "https://example.test", "jwks_uri": "http://"}),
            json!({
                "issuer": "https://example.test",
                "jwks_uri": "https://example.test/jwks",
                "consumer_claim": " ",
            }),
            json!({
                "issuer": "https://example.test",
                "jwks_uri": "https://example.test/jwks",
                "algorithms": ["HS256"],
            }),
            json!({
                "issuer": "https://example.test",
                "jwks_uri": "https://example.test/jwks",
                "required_scopes": [""],
            }),
            json!({
                "issuer": "https://example.test",
                "jwks_uri": "https://example.test/jwks",
                "jwks_cache_secs": 0,
            }),
            json!({
                "issuer": "https://example.test",
                "jwks_uri": "https://example.test/jwks",
                "jwks_cache_secs": 86_401,
            }),
        ] {
            assert!(
                McpOidcConfig::from_value(&bad).is_err(),
                "config should be rejected: {bad}"
            );
        }

        let strategy = NewMcpAuthStrategy {
            name: "sso".into(),
            kind: "jwt".into(),
            config: json!({}),
            enabled: true,
            is_default: false,
        };
        assert!(strategy.validate().is_err());
    }

    #[test]
    fn oidc_config_supports_discovery_and_introspection() {
        let config = McpOidcConfig::from_value(&json!({
            "issuer": "https://issuer.example.test",
            "discovery_url": "https://issuer.example.test/.well-known/openid-configuration",
            "token_validation": "introspection",
            "introspection": {
                "endpoint": "https://issuer.example.test/introspect",
                "client_id": "octra",
                "client_secret_ref": "env:OIDC_CLIENT_SECRET",
            },
        }))
        .unwrap();
        assert!(config.jwks_uri.is_none());
        assert_eq!(config.token_validation, "introspection");
        let introspection = config.introspection.expect("introspection settings");
        assert_eq!(introspection.cache_secs, 60);
        assert_eq!(introspection.client_id, "octra");

        // The discovery document may provide the introspection endpoint.
        let config = McpOidcConfig::from_value(&json!({
            "issuer": "https://issuer.example.test",
            "discovery_url": "https://issuer.example.test/.well-known/openid-configuration",
            "token_validation": "introspection",
            "introspection": {
                "client_id": "octra",
                "client_secret_ref": "file:/run/secrets/oidc-client",
            },
        }))
        .unwrap();
        assert_eq!(config.introspection.unwrap().endpoint, "");

        // Explicit discovery is a JWKS source for jwt-only mode.
        let config = McpOidcConfig::from_value(&json!({
            "issuer": "https://issuer.example.test",
            "discovery_url": "https://issuer.example.test/.well-known/openid-configuration",
            "token_validation": "jwt",
        }))
        .unwrap();
        assert!(config.jwks_uri.is_none());

        for bad in [
            json!({
                "issuer": "https://issuer.example.test",
                "token_validation": "jwt",
            }),
            json!({
                "issuer": "https://issuer.example.test",
                "token_validation": "introspection",
            }),
            json!({
                "issuer": "https://issuer.example.test",
                "token_validation": "auto",
            }),
            json!({
                "issuer": "https://issuer.example.test",
                "discovery_url": "relative",
                "token_validation": "jwt",
            }),
            json!({
                "issuer": "https://issuer.example.test",
                "token_validation": "introspection",
                "introspection": {
                    "endpoint": "relative",
                    "client_id": "octra",
                    "client_secret_ref": "env:OIDC_CLIENT_SECRET",
                },
            }),
            json!({
                "issuer": "https://issuer.example.test",
                "token_validation": "introspection",
                "introspection": {
                    "client_id": "",
                    "client_secret_ref": "env:OIDC_CLIENT_SECRET",
                },
            }),
            json!({
                "issuer": "https://issuer.example.test",
                "token_validation": "introspection",
                "introspection": {
                    "client_id": "octra",
                    "client_secret_ref": "inline:secret",
                },
            }),
            json!({
                "issuer": "https://issuer.example.test",
                "token_validation": "introspection",
                "introspection": {
                    "client_id": "octra",
                    "client_secret_ref": "env:OIDC_CLIENT_SECRET",
                    "cache_secs": 0,
                },
            }),
            json!({
                "issuer": "https://issuer.example.test",
                "token_validation": "introspection",
                "introspection": {
                    "client_id": "octra",
                    "client_secret_ref": "env:OIDC_CLIENT_SECRET",
                    "cache_secs": 3601,
                },
            }),
            json!({
                "issuer": "https://issuer.example.test",
                "jwks_uri": "https://issuer.example.test/jwks",
                "token_validation": "password",
            }),
        ] {
            assert!(
                McpOidcConfig::from_value(&bad).is_err(),
                "config should be rejected: {bad}"
            );
        }
    }
}
