//! Data plane authentication: MCP API keys and OIDC bearer tokens.
//!
//! When a server selects an OIDC strategy (or an enabled default strategy
//! exists), every request must carry a valid token in `Authorization: Bearer`.
//! JWTs validate signatures against a JWKS document, which may come from an
//! OpenID Connect discovery document. Opaque tokens can be validated with
//! RFC 7662 introspection. Otherwise the gateway keeps the API-key behaviour:
//! when at least one enabled key exists, every `/mcp` request must carry a
//! valid key in `Authorization: Bearer <key>` or `X-API-Key: <key>`. With no
//! keys the gateway is open to local clients, which is the default for a
//! loopback-only installation. Keys are never stored: the database keeps a
//! display prefix and the SHA-256 hex digest.
//!
//! OIDC strategies validate `exp`, `nbf`, issuer, audience, and required
//! scopes. Claims map to the same consumer identity and group model used by
//! API keys. Tokens, full claims, JWKS key material, client secrets, and
//! introspection responses are never logged; errors may name the issuer or
//! endpoint host only.

use std::collections::{HashMap, HashSet};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use axum::http::{HeaderMap, StatusCode};
use jsonwebtoken::errors::ErrorKind;
use jsonwebtoken::jwk::{Jwk, JwkSet};
use jsonwebtoken::{Algorithm, DecodingKey, Validation, decode, decode_header};
use octra_mcp_core::McpServer;
use octra_mcp_core::domain::{
    MCP_OIDC_TOKEN_VALIDATION_AUTO, MCP_OIDC_TOKEN_VALIDATION_INTROSPECTION,
    MCP_OIDC_TOKEN_VALIDATION_JWT, McpAuthStrategy, McpOidcConfig,
};
use octra_storage::Storage;
use rand::{RngCore, rngs::OsRng};
use serde_json::Value;
use sha2::{Digest, Sha256};

pub const KEY_PREFIX: &str = "octra_";
const KEY_SECRET_BYTES: usize = 32;
const DISPLAY_PREFIX_CHARS: usize = 12;
/// SHA-256 is unsalted by design (keys are high-entropy); this is only the
/// per-row hash compared with the presented key.
pub const API_KEY_HEADER: &str = "x-api-key";

/// Who a request acts as on the data plane.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Caller {
    pub consumer_id: Option<String>,
    /// Display name of the consumer, when one is resolved.
    pub consumer_name: Option<String>,
    pub key_id: Option<String>,
    /// Groups the consumer belongs to, resolved once at authentication time.
    pub consumer_group_ids: Vec<String>,
    /// Group names used by CEL expressions (`consumer_group.names`).
    pub consumer_group_names: Vec<String>,
}

impl Caller {
    pub fn anonymous() -> Self {
        Self {
            consumer_id: None,
            consumer_name: None,
            key_id: None,
            consumer_group_ids: Vec::new(),
            consumer_group_names: Vec::new(),
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum AuthError {
    #[error("missing MCP API key")]
    Missing,
    #[error("missing bearer token")]
    MissingBearer,
    #[error("invalid MCP API key")]
    Invalid,
    #[error("MCP API key is disabled or expired")]
    Inactive,
    #[error("invalid bearer token: {0}")]
    InvalidToken(String),
    #[error("bearer token is missing required scope: {0}")]
    InsufficientScope(String),
    #[error("JWKS unavailable: {0}")]
    Jwks(String),
    #[error("storage error: {0}")]
    Storage(String),
}

impl From<octra_core::Error> for AuthError {
    fn from(err: octra_core::Error) -> Self {
        Self::Storage(err.to_string())
    }
}

#[derive(Clone)]
struct CachedJwks {
    keys: Vec<Jwk>,
    fetched_at: Instant,
}

const MAX_JWKS_ENTRIES: usize = 256;
const MAX_DISCOVERY_ENTRIES: usize = 256;
const MAX_INTROSPECTION_ENTRIES: usize = 4_096;

static JWKS_CACHE: OnceLock<Mutex<HashMap<String, CachedJwks>>> = OnceLock::new();

fn jwks_cache() -> &'static Mutex<HashMap<String, CachedJwks>> {
    JWKS_CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

/// The subset of the OpenID Connect discovery document the gateway uses.
#[derive(Debug, Clone, serde::Deserialize)]
struct DiscoveryDocument {
    #[serde(default)]
    jwks_uri: Option<String>,
    #[serde(default)]
    introspection_endpoint: Option<String>,
}

#[derive(Clone)]
struct CachedDiscovery {
    document: DiscoveryDocument,
    fetched_at: Instant,
}

static DISCOVERY_CACHE: OnceLock<Mutex<HashMap<String, CachedDiscovery>>> = OnceLock::new();

fn discovery_cache() -> &'static Mutex<HashMap<String, CachedDiscovery>> {
    DISCOVERY_CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

#[derive(Clone)]
enum IntrospectionOutcome {
    Active(Value),
    Inactive,
}

#[derive(Clone)]
struct CachedIntrospection {
    outcome: IntrospectionOutcome,
    expires_at: Instant,
}

/// Introspection results are cached by endpoint and SHA-256 of the token.
static INTROSPECTION_CACHE: OnceLock<Mutex<HashMap<(String, String), CachedIntrospection>>> =
    OnceLock::new();

fn introspection_cache() -> &'static Mutex<HashMap<(String, String), CachedIntrospection>> {
    INTROSPECTION_CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Extracts the API key or bearer token from the headers.
pub fn presented_key(headers: &HeaderMap) -> Option<String> {
    if let Some(value) = presented_bearer(headers) {
        return Some(value);
    }

    headers
        .get(API_KEY_HEADER)
        .and_then(|value| value.to_str().ok())
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
}

/// Extracts only an `Authorization: Bearer` value.
fn presented_bearer(headers: &HeaderMap) -> Option<String> {
    headers
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "))
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
}

/// Resolves the auth strategy for a request.
///
/// A scoped server's strategy wins when enabled; otherwise the enabled default
/// strategy applies. `None` means the caller falls back to MCP API keys.
pub async fn resolve_strategy(
    storage: &Storage,
    server: Option<&McpServer>,
) -> Result<Option<McpAuthStrategy>, AuthError> {
    if let Some(id) = server.and_then(|server| server.auth_strategy_id.as_deref())
        && let Some(strategy) = storage.mcp.get_auth_strategy(id).await?
        && strategy.enabled
    {
        return Ok(Some(strategy));
    }
    Ok(storage.mcp.get_default_auth_strategy().await?)
}

/// Authenticates a data plane request. Returns the anonymous caller when the
/// installation has no enabled keys and no OIDC strategy applies.
pub async fn authenticate(
    storage: &Storage,
    http: Option<&reqwest::Client>,
    headers: &HeaderMap,
    strategy: Option<&McpAuthStrategy>,
    require_key: bool,
) -> Result<Caller, AuthError> {
    if let Some(strategy) = strategy
        && strategy.enabled
        && strategy.kind == octra_mcp_core::domain::MCP_AUTH_STRATEGY_KIND_OIDC
    {
        return authenticate_oidc(storage, http, headers, strategy).await;
    }
    authenticate_api_key(storage, headers, require_key).await
}

/// API-key authentication. Open local mode applies only when no key has ever
/// been configured; disabling or expiring the last key fails closed.
async fn authenticate_api_key(
    storage: &Storage,
    headers: &HeaderMap,
    require_key: bool,
) -> Result<Caller, AuthError> {
    // Resolution does not depend on the key-gating flag for correctness: an
    // unknown or inactive key is rejected whenever one is presented, and an
    // absent key is only tolerated when no enabled keys exist.
    let presented = presented_key(headers);
    if !storage.mcp.has_enabled_api_keys().await? {
        if let Some(key) = presented {
            let record = storage
                .mcp
                .find_active_api_key(&hash_key(&key))
                .await?
                .ok_or(AuthError::Invalid)?;
            return caller_from_key(storage, record).await;
        }
        if require_key || storage.mcp.has_any_api_keys().await? {
            return Err(AuthError::Missing);
        }
        return Ok(Caller::anonymous());
    }

    let presented = presented.ok_or(AuthError::Missing)?;
    let hash = hash_key(&presented);
    let record = storage
        .mcp
        .find_active_api_key(&hash)
        .await?
        .ok_or(AuthError::Invalid)?;

    caller_from_key(storage, record).await
}

/// Authenticates a bearer token with the strategy's configured validation
/// mode: JWKS-backed JWT validation, RFC 7662 introspection, or both.
async fn authenticate_oidc(
    storage: &Storage,
    http: Option<&reqwest::Client>,
    headers: &HeaderMap,
    strategy: &McpAuthStrategy,
) -> Result<Caller, AuthError> {
    let config = strategy
        .oidc_config()
        .map_err(|err| AuthError::Jwks(format!("invalid OIDC strategy config: {err}")))?;
    let token = presented_bearer(headers).ok_or(AuthError::MissingBearer)?;

    match config.token_validation.as_str() {
        MCP_OIDC_TOKEN_VALIDATION_JWT => authenticate_jwt(storage, http, &token, &config).await,
        MCP_OIDC_TOKEN_VALIDATION_INTROSPECTION => {
            authenticate_introspection(storage, http, &token, &config).await
        }
        MCP_OIDC_TOKEN_VALIDATION_AUTO => authenticate_auto(storage, http, &token, &config).await,
        other => Err(AuthError::Jwks(format!(
            "unsupported token_validation mode '{other}'"
        ))),
    }
}

/// Auto mode validates JWT-shaped tokens against JWKS and falls back to
/// introspection for opaque tokens or unsupported JWTs.
async fn authenticate_auto(
    storage: &Storage,
    http: Option<&reqwest::Client>,
    token: &str,
    config: &McpOidcConfig,
) -> Result<Caller, AuthError> {
    if token.split('.').count() == 3 {
        match authenticate_jwt(storage, http, token, config).await {
            Ok(caller) => return Ok(caller),
            Err(err @ (AuthError::InsufficientScope(_) | AuthError::Jwks(_))) => {
                return Err(err);
            }
            Err(AuthError::InvalidToken(message)) if config.introspection.is_some() => {
                if is_unsupported_jwt_error(&message) {
                    return authenticate_introspection(storage, http, token, config).await;
                }
                return Err(AuthError::InvalidToken(message));
            }
            Err(err) => return Err(err),
        }
    }

    if config.introspection.is_some() {
        authenticate_introspection(storage, http, token, config).await
    } else {
        Err(AuthError::InvalidToken(
            "token is not a JWT and introspection is not configured".to_string(),
        ))
    }
}

/// Only unsupported or otherwise opaque JWTs fall back to introspection.
fn is_unsupported_jwt_error(message: &str) -> bool {
    message == "malformed token header"
        || message.starts_with("algorithm '")
        || message.starts_with("no JWKS key for kid")
}

/// Authenticates a bearer JWT against the strategy's JWKS.
async fn authenticate_jwt(
    storage: &Storage,
    http: Option<&reqwest::Client>,
    token: &str,
    config: &McpOidcConfig,
) -> Result<Caller, AuthError> {
    let header = decode_header(token)
        .map_err(|_| AuthError::InvalidToken("malformed token header".to_string()))?;
    let algorithm_name = algorithm_name(header.alg);
    if !config
        .algorithms
        .iter()
        .any(|allowed| allowed == algorithm_name)
    {
        return Err(AuthError::InvalidToken(format!(
            "algorithm '{algorithm_name}' is not allowed"
        )));
    }

    let kid = header.kid.as_deref();
    let jwks_uri = resolve_jwks_uri(http, config, false).await?;
    let mut keys = fetch_jwks(http, &jwks_uri, config.jwks_cache_secs, false).await?;
    let mut candidates = candidate_keys(&keys, kid);
    if candidates.is_empty() && kid.is_some() {
        // A key rotation may have happened since the cached documents were
        // fetched; refresh discovery and JWKS once before rejecting the token.
        let jwks_uri = resolve_jwks_uri(http, config, true).await?;
        keys = fetch_jwks(http, &jwks_uri, config.jwks_cache_secs, true).await?;
        candidates = candidate_keys(&keys, kid);
    }
    if candidates.is_empty() {
        return Err(AuthError::InvalidToken(format!(
            "no JWKS key for kid '{}'",
            kid.unwrap_or_default()
        )));
    }

    let claims = validate_token(token, config, &candidates)?;
    caller_from_claims(storage, config, &claims).await
}

/// Maps validated claims to the caller identity used by the data plane.
async fn caller_from_claims(
    storage: &Storage,
    config: &McpOidcConfig,
    claims: &Value,
) -> Result<Caller, AuthError> {
    let consumer_id = claim_string(claims, &config.consumer_claim)
        .ok_or_else(|| {
            AuthError::InvalidToken(format!(
                "claim '{}' must be a non-empty string",
                config.consumer_claim
            ))
        })?
        .to_string();

    let username = config
        .username_claim
        .as_deref()
        .and_then(|claim| claim_string(claims, claim))
        .or_else(|| claim_string(claims, "preferred_username"))
        .or_else(|| claim_string(claims, "email"))
        .unwrap_or(&consumer_id)
        .to_string();

    let groups_claim = config.groups_claim.as_deref().unwrap_or("groups");
    let consumer_group_names = group_names(claims, groups_claim);
    let mut consumer_group_ids = Vec::new();
    for name in &consumer_group_names {
        if let Some(group) = storage.api.get_consumer_group_by_name(name).await?
            && !consumer_group_ids.contains(&group.id)
        {
            consumer_group_ids.push(group.id);
        }
    }

    Ok(Caller {
        consumer_id: Some(consumer_id),
        consumer_name: Some(username),
        key_id: None,
        consumer_group_ids,
        consumer_group_names,
    })
}

/// Validates the JWT and every configured claim requirement.
fn validate_token(
    token: &str,
    config: &McpOidcConfig,
    candidates: &[&Jwk],
) -> Result<Value, AuthError> {
    let mut first_non_signature_error = None;
    let mut saw_usable_key = false;

    for jwk in candidates {
        let key = match DecodingKey::from_jwk(jwk) {
            Ok(key) => key,
            Err(_) => continue,
        };
        saw_usable_key = true;

        let mut validation = Validation::new(
            decode_header(token)
                .map(|header| header.alg)
                .map_err(|_| AuthError::InvalidToken("malformed token header".to_string()))?,
        );
        validation.validate_exp = true;
        validation.validate_nbf = true;
        validation.set_issuer(std::slice::from_ref(&config.issuer));
        validation.set_required_spec_claims(&["exp", "iss"]);
        if let Some(audience) = config.audience.as_deref() {
            validation.set_audience(&[audience]);
            validation.required_spec_claims.insert("aud".to_string());
        } else {
            // A token may legitimately carry an audience the gateway does not
            // constrain; only validate it when the strategy configures one.
            validation.validate_aud = false;
        }

        match decode::<Value>(token, &key, &validation) {
            Ok(data) => {
                require_scopes(&data.claims, &config.required_scopes)?;
                return Ok(data.claims);
            }
            Err(err) => {
                if matches!(err.kind(), ErrorKind::InvalidSignature) {
                    continue;
                }
                first_non_signature_error = Some(token_error(&err));
            }
        }
    }

    if !saw_usable_key {
        return Err(AuthError::Jwks(
            "JWKS contains no usable signing key".to_string(),
        ));
    }
    Err(AuthError::InvalidToken(
        first_non_signature_error.unwrap_or_else(|| "token signature is invalid".to_string()),
    ))
}

/// Requires every configured scope in `scope` or `scp`.
fn require_scopes(claims: &Value, required: &[String]) -> Result<(), AuthError> {
    if required.is_empty() {
        return Ok(());
    }

    let mut present = HashSet::new();
    if let Some(scope) = claims.get("scope").and_then(Value::as_str) {
        present.extend(scope.split_whitespace().map(str::to_string));
    }
    match claims.get("scp") {
        Some(Value::Array(items)) => {
            present.extend(
                items
                    .iter()
                    .filter_map(Value::as_str)
                    .flat_map(str::split_whitespace)
                    .map(str::to_string),
            );
        }
        Some(Value::String(value)) => {
            present.extend(value.split_whitespace().map(str::to_string));
        }
        _ => {}
    }

    for scope in required {
        if !present.contains(scope) {
            return Err(AuthError::InsufficientScope(scope.clone()));
        }
    }
    Ok(())
}

/// Looks up candidate keys by `kid`. Without a `kid`, every key is a candidate.
fn candidate_keys<'a>(keys: &'a [Jwk], kid: Option<&str>) -> Vec<&'a Jwk> {
    match kid {
        Some(kid) => keys
            .iter()
            .filter(|jwk| jwk.common.key_id.as_deref() == Some(kid))
            .collect(),
        None => keys.iter().collect(),
    }
}

/// Authenticates an opaque token with RFC 7662 introspection.
async fn authenticate_introspection(
    storage: &Storage,
    http: Option<&reqwest::Client>,
    token: &str,
    config: &McpOidcConfig,
) -> Result<Caller, AuthError> {
    let introspection = config.introspection.as_ref().ok_or_else(|| {
        AuthError::Jwks("introspection is not configured for this strategy".to_string())
    })?;
    let endpoint = resolve_introspection_endpoint(http, config, false).await?;
    let cache_key = (endpoint.clone(), hash_key(token));

    if let Some(outcome) = cached_introspection(&cache_key) {
        return introspection_outcome_to_caller(storage, config, outcome).await;
    }

    let client_secret = crate::conversion::resolve_secret_reference(
        &introspection.client_secret_ref,
    )
    .map_err(|_| AuthError::Jwks("cannot resolve the introspection client secret".to_string()))?;

    let response = introspection_request(
        http,
        &endpoint,
        token,
        &introspection.client_id,
        &client_secret,
        true,
    )
    .await?;
    let status = response.status();
    let response = if matches!(
        status,
        StatusCode::BAD_REQUEST | StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN
    ) {
        // Some authorization servers reject HTTP Basic; retry once with the
        // client credentials in the form body.
        introspection_request(
            http,
            &endpoint,
            token,
            &introspection.client_id,
            &client_secret,
            false,
        )
        .await?
    } else {
        response
    };

    let status = response.status();
    if !status.is_success() {
        return Err(AuthError::Jwks(format!(
            "introspection endpoint returned HTTP {status}"
        )));
    }
    let body: Value = response
        .json()
        .await
        .map_err(|err| AuthError::Jwks(format!("invalid introspection response: {err}")))?;
    if !body.is_object() {
        return Err(AuthError::Jwks(
            "introspection response must be a JSON object".to_string(),
        ));
    }
    let active = body.get("active").and_then(Value::as_bool).ok_or_else(|| {
        AuthError::Jwks("introspection response is missing a boolean 'active' field".to_string())
    })?;

    if !active {
        insert_introspection(
            cache_key,
            IntrospectionOutcome::Inactive,
            Duration::from_secs(introspection.cache_secs),
        );
        return Err(AuthError::InvalidToken(
            "access token is not active".to_string(),
        ));
    }

    let ttl = validate_introspection_claims(&body, config, introspection.cache_secs)?;
    let caller = caller_from_claims(storage, config, &body).await?;
    insert_introspection(cache_key, IntrospectionOutcome::Active(body), ttl);
    Ok(caller)
}

async fn introspection_outcome_to_caller(
    storage: &Storage,
    config: &McpOidcConfig,
    outcome: IntrospectionOutcome,
) -> Result<Caller, AuthError> {
    match outcome {
        IntrospectionOutcome::Active(claims) => caller_from_claims(storage, config, &claims).await,
        IntrospectionOutcome::Inactive => Err(AuthError::InvalidToken(
            "access token is not active".to_string(),
        )),
    }
}

/// POSTs an introspection request with either HTTP Basic or form credentials.
async fn introspection_request(
    http: Option<&reqwest::Client>,
    endpoint: &str,
    token: &str,
    client_id: &str,
    client_secret: &str,
    basic: bool,
) -> Result<reqwest::Response, AuthError> {
    let client = match http {
        Some(client) => client.clone(),
        None => reqwest::Client::new(),
    };
    crate::net_guard::ensure_safe_url(endpoint)
        .await
        .map_err(AuthError::Jwks)?;
    let mut request = client
        .post(endpoint)
        .form(&[("token", token), ("token_type_hint", "access_token")]);
    if basic {
        request = request.basic_auth(client_id, Some(client_secret));
    } else {
        request = request.form(&[
            ("token", token),
            ("token_type_hint", "access_token"),
            ("client_id", client_id),
            ("client_secret", client_secret),
        ]);
    }
    request.send().await.map_err(|err| {
        AuthError::Jwks(format!(
            "introspection request failed: {}",
            request_error(&err)
        ))
    })
}

/// Validates introspection `exp`, audience, and scopes. Returns the result
/// cache lifetime bounded by the token expiry.
fn validate_introspection_claims(
    claims: &Value,
    config: &McpOidcConfig,
    cache_secs: u64,
) -> Result<Duration, AuthError> {
    let now = epoch_secs();
    let ttl = match claims.get("exp") {
        Some(value) => {
            let exp = value.as_u64().ok_or_else(|| {
                AuthError::Jwks("introspection 'exp' must be a number".to_string())
            })?;
            if exp <= now {
                return Err(AuthError::InvalidToken("token is expired".to_string()));
            }
            (exp - now).min(cache_secs)
        }
        None => cache_secs,
    };
    if let Some(expected) = config.audience.as_deref()
        && !audience_matches(claims.get("aud"), expected)
    {
        return Err(AuthError::InvalidToken(
            "token audience does not match".to_string(),
        ));
    }
    require_scopes(claims, &config.required_scopes)?;
    Ok(Duration::from_secs(ttl))
}

fn audience_matches(value: Option<&Value>, expected: &str) -> bool {
    match value {
        Some(Value::String(value)) => value == expected,
        Some(Value::Array(values)) => values.iter().any(|value| value.as_str() == Some(expected)),
        _ => false,
    }
}

fn cached_introspection(key: &(String, String)) -> Option<IntrospectionOutcome> {
    let Ok(mut cache) = introspection_cache().lock() else {
        return None;
    };
    match cache.get(key) {
        Some(entry) if entry.expires_at > Instant::now() => Some(entry.outcome.clone()),
        Some(_) => {
            cache.remove(key);
            None
        }
        None => None,
    }
}

fn insert_introspection(key: (String, String), outcome: IntrospectionOutcome, ttl: Duration) {
    if ttl.is_zero() {
        return;
    }
    if let Ok(mut cache) = introspection_cache().lock() {
        if cache.len() >= MAX_INTROSPECTION_ENTRIES {
            let now = Instant::now();
            cache.retain(|_, entry| entry.expires_at > now);
            if cache.len() >= MAX_INTROSPECTION_ENTRIES {
                cache.clear();
            }
        }
        cache.insert(
            key,
            CachedIntrospection {
                outcome,
                expires_at: Instant::now() + ttl,
            },
        );
    }
}

/// Resolves the JWKS URI from config, or from the discovery document when the
/// config omits it.
async fn resolve_jwks_uri(
    http: Option<&reqwest::Client>,
    config: &McpOidcConfig,
    force: bool,
) -> Result<String, AuthError> {
    if let Some(uri) = config.jwks_uri.as_deref() {
        return Ok(uri.to_string());
    }
    let document = fetch_discovery(http, config, force).await?;
    document
        .jwks_uri
        .ok_or_else(|| AuthError::Jwks("discovery document does not declare jwks_uri".to_string()))
}

/// Resolves the introspection endpoint from config, or from the discovery
/// document when the config omits it.
async fn resolve_introspection_endpoint(
    http: Option<&reqwest::Client>,
    config: &McpOidcConfig,
    force: bool,
) -> Result<String, AuthError> {
    if let Some(introspection) = config.introspection.as_ref()
        && !introspection.endpoint.is_empty()
    {
        return Ok(introspection.endpoint.clone());
    }
    let document = fetch_discovery(http, config, force).await?;
    document.introspection_endpoint.ok_or_else(|| {
        AuthError::Jwks("discovery document does not declare introspection_endpoint".to_string())
    })
}

/// Explicit discovery URL, or the issuer-derived well-known URL.
fn discovery_url(config: &McpOidcConfig) -> String {
    config.discovery_url.clone().unwrap_or_else(|| {
        format!(
            "{}/.well-known/openid-configuration",
            config.issuer.trim_end_matches('/')
        )
    })
}

/// Fetches and caches an OpenID Connect discovery document.
async fn fetch_discovery(
    http: Option<&reqwest::Client>,
    config: &McpOidcConfig,
    force: bool,
) -> Result<DiscoveryDocument, AuthError> {
    let url = discovery_url(config);
    if !force {
        let Ok(cache) = discovery_cache().lock() else {
            return Err(AuthError::Jwks(
                "discovery cache lock is poisoned".to_string(),
            ));
        };
        if let Some(entry) = cache.get(&url)
            && entry.fetched_at.elapsed() < Duration::from_secs(config.jwks_cache_secs)
        {
            return Ok(entry.document.clone());
        }
    }

    crate::net_guard::ensure_safe_url(&url)
        .await
        .map_err(AuthError::Jwks)?;
    let client = match http {
        Some(client) => client.clone(),
        None => reqwest::Client::new(),
    };
    let response = client.get(&url).send().await.map_err(|err| {
        AuthError::Jwks(format!("discovery request failed: {}", request_error(&err)))
    })?;
    let status = response.status();
    if !status.is_success() {
        return Err(AuthError::Jwks(format!(
            "discovery endpoint returned HTTP {status}"
        )));
    }
    let value: Value = response
        .json()
        .await
        .map_err(|err| AuthError::Jwks(format!("invalid discovery document: {err}")))?;
    if !value.is_object() {
        return Err(AuthError::Jwks(
            "discovery document must be a JSON object".to_string(),
        ));
    }
    let document: DiscoveryDocument = serde_json::from_value(value)
        .map_err(|err| AuthError::Jwks(format!("invalid discovery document: {err}")))?;
    for (field, value) in [
        ("jwks_uri", document.jwks_uri.as_deref()),
        (
            "introspection_endpoint",
            document.introspection_endpoint.as_deref(),
        ),
    ] {
        if let Some(value) = value
            && !is_absolute_http_url(value)
        {
            return Err(AuthError::Jwks(format!(
                "discovery {field} must be an absolute http(s) URL"
            )));
        }
    }

    if let Ok(mut cache) = discovery_cache().lock() {
        if cache.len() >= MAX_DISCOVERY_ENTRIES {
            cache.clear();
        }
        cache.insert(
            url,
            CachedDiscovery {
                document: document.clone(),
                fetched_at: Instant::now(),
            },
        );
    }
    Ok(document)
}

fn is_absolute_http_url(value: &str) -> bool {
    let lower = value.to_ascii_lowercase();
    let Some(rest) = lower
        .strip_prefix("http://")
        .or_else(|| lower.strip_prefix("https://"))
    else {
        return false;
    };
    !rest
        .split(['/', '?', '#'])
        .next()
        .unwrap_or_default()
        .is_empty()
}

fn epoch_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

/// Fetches a JWKS document, using the process-wide cache unless forced.
async fn fetch_jwks(
    http: Option<&reqwest::Client>,
    uri: &str,
    cache_secs: u64,
    force: bool,
) -> Result<Vec<Jwk>, AuthError> {
    if !force {
        let Ok(cache) = jwks_cache().lock() else {
            return Err(AuthError::Jwks("JWKS cache lock is poisoned".to_string()));
        };
        if let Some(entry) = cache.get(uri)
            && entry.fetched_at.elapsed() < Duration::from_secs(cache_secs)
        {
            return Ok(entry.keys.clone());
        }
    }

    crate::net_guard::ensure_safe_url(uri)
        .await
        .map_err(AuthError::Jwks)?;
    let client = match http {
        Some(client) => client.clone(),
        None => reqwest::Client::new(),
    };
    let response =
        client.get(uri).send().await.map_err(|err| {
            AuthError::Jwks(format!("JWKS request failed: {}", request_error(&err)))
        })?;
    let status = response.status();
    if !status.is_success() {
        return Err(AuthError::Jwks(format!(
            "JWKS endpoint returned HTTP {status}"
        )));
    }
    let set: JwkSet = response
        .json()
        .await
        .map_err(|err| AuthError::Jwks(format!("invalid JWKS document: {err}")))?;
    if set.keys.is_empty() {
        return Err(AuthError::Jwks(
            "JWKS document contains no keys".to_string(),
        ));
    }

    let keys = set.keys;
    let Ok(mut cache) = jwks_cache().lock() else {
        return Err(AuthError::Jwks("JWKS cache lock is poisoned".to_string()));
    };
    if cache.len() >= MAX_JWKS_ENTRIES {
        cache.clear();
    }
    cache.insert(
        uri.to_string(),
        CachedJwks {
            keys: keys.clone(),
            fetched_at: Instant::now(),
        },
    );
    Ok(keys)
}

/// Describes a JWKS transport failure without echoing the URL.
fn request_error(err: &reqwest::Error) -> &'static str {
    if err.is_timeout() {
        "request timed out"
    } else if err.is_connect() {
        "connection failed"
    } else {
        "request failed"
    }
}

fn algorithm_name(algorithm: Algorithm) -> &'static str {
    match algorithm {
        Algorithm::RS256 => "RS256",
        Algorithm::RS384 => "RS384",
        Algorithm::RS512 => "RS512",
        Algorithm::ES256 => "ES256",
        Algorithm::ES384 => "ES384",
        _ => "unsupported",
    }
}

fn claim_string<'a>(claims: &'a Value, claim: &str) -> Option<&'a str> {
    claims
        .get(claim)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
}

/// Group names are either a single string or an array of strings.
fn group_names(claims: &Value, claim: &str) -> Vec<String> {
    let mut names = Vec::new();
    match claims.get(claim) {
        Some(Value::Array(items)) => {
            for item in items {
                if let Some(name) = item.as_str().map(str::trim).filter(|name| !name.is_empty())
                    && !names.iter().any(|existing| existing == name)
                {
                    names.push(name.to_string());
                }
            }
        }
        Some(Value::String(name)) => {
            let name = name.trim();
            if !name.is_empty() {
                names.push(name.to_string());
            }
        }
        _ => {}
    }
    names
}

/// Maps a JWT error to a message that never includes token contents.
fn token_error(err: &jsonwebtoken::errors::Error) -> String {
    match err.kind() {
        ErrorKind::ExpiredSignature => "token is expired".to_string(),
        ErrorKind::ImmatureSignature => "token is not valid yet".to_string(),
        ErrorKind::InvalidIssuer => "token issuer does not match".to_string(),
        ErrorKind::InvalidAudience => "token audience does not match".to_string(),
        ErrorKind::MissingRequiredClaim(claim) => {
            format!("token is missing required claim '{claim}'")
        }
        ErrorKind::InvalidSignature => "token signature is invalid".to_string(),
        ErrorKind::InvalidAlgorithm | ErrorKind::InvalidAlgorithmName => {
            "token algorithm is invalid".to_string()
        }
        _ => "token validation failed".to_string(),
    }
}

/// Builds a keyed caller and resolves the consumer's group memberships.
async fn caller_from_key(
    storage: &Storage,
    record: octra_mcp_core::McpApiKey,
) -> Result<Caller, AuthError> {
    let (consumer_name, consumer_group_ids, consumer_group_names) =
        match record.consumer_id.as_deref() {
            Some(consumer_id) => {
                let name = storage
                    .api
                    .get_consumer(consumer_id)
                    .await?
                    .and_then(|consumer| consumer.username);
                let groups = storage
                    .api
                    .list_consumer_groups_for_consumer(consumer_id)
                    .await?;
                (
                    name,
                    groups.iter().map(|group| group.id.clone()).collect(),
                    groups.into_iter().map(|group| group.name).collect(),
                )
            }
            None => (None, Vec::new(), Vec::new()),
        };

    Ok(Caller {
        consumer_id: record.consumer_id,
        consumer_name,
        key_id: Some(record.id),
        consumer_group_ids,
        consumer_group_names,
    })
}

/// SHA-256 of the key, lowercase hex.
pub fn hash_key(key: &str) -> String {
    let digest = Sha256::digest(key.as_bytes());
    hex::encode(digest)
}

/// Generates a new key: `octra_` plus 64 hex characters from the OS RNG.
pub fn generate_key() -> String {
    let mut bytes = [0u8; KEY_SECRET_BYTES];
    OsRng.fill_bytes(&mut bytes);
    format!("{KEY_PREFIX}{}", hex::encode(bytes))
}

/// Prefix stored for display, enough to identify a key but not reproduce it.
pub fn display_prefix(key: &str) -> String {
    key.chars().take(DISPLAY_PREFIX_CHARS).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderValue;

    #[test]
    fn hashes_are_stable_hex() {
        let hash = hash_key("octra_example");
        assert_eq!(hash.len(), 64);
        assert!(
            hash.chars()
                .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase())
        );
        assert_eq!(hash, hash_key("octra_example"));
        assert_ne!(hash, hash_key("octra_examplf"));
    }

    #[test]
    fn generated_keys_are_unique_and_prefixed() {
        let first = generate_key();
        let second = generate_key();
        assert!(first.starts_with(KEY_PREFIX));
        assert_eq!(first.len(), KEY_PREFIX.len() + KEY_SECRET_BYTES * 2);
        assert_ne!(first, second);
        assert_eq!(display_prefix(&first).len(), DISPLAY_PREFIX_CHARS);
        assert!(!second.starts_with(&display_prefix(&first)));
    }

    #[test]
    fn extracts_bearer_then_api_key_header() {
        let mut headers = HeaderMap::new();
        assert_eq!(presented_key(&headers), None);

        headers.insert(API_KEY_HEADER, HeaderValue::from_static("from-header"));
        assert_eq!(presented_key(&headers).as_deref(), Some("from-header"));

        headers.insert(
            axum::http::header::AUTHORIZATION,
            HeaderValue::from_static("Bearer from-bearer"),
        );
        assert_eq!(presented_key(&headers).as_deref(), Some("from-bearer"));
    }

    #[test]
    fn extracts_scopes_and_group_claims() {
        let claims = serde_json::json!({
            "scope": "read write",
            "scp": ["write", "admin"],
            "groups": ["team", "ops", "team"],
        });
        require_scopes(&claims, &["read".to_string(), "admin".to_string()]).unwrap();
        assert!(matches!(
            require_scopes(&claims, &["missing".to_string()]),
            Err(AuthError::InsufficientScope(scope)) if scope == "missing"
        ));
        assert_eq!(group_names(&claims, "groups"), vec!["team", "ops"]);
        assert_eq!(
            group_names(&serde_json::json!({"groups": "team"}), "groups"),
            vec!["team"]
        );
    }

    #[tokio::test]
    async fn open_mode_accepts_anonymous_and_explicit_keys() {
        let storage = Storage::connect_in_memory().await.unwrap();
        let caller = authenticate(&storage, None, &HeaderMap::new(), None, false)
            .await
            .unwrap();
        assert_eq!(caller, Caller::anonymous());

        let mut headers = HeaderMap::new();
        headers.insert(
            axum::http::header::AUTHORIZATION,
            HeaderValue::from_static("Bearer octra_unknown"),
        );
        assert!(matches!(
            authenticate(&storage, None, &headers, None, false).await,
            Err(AuthError::Invalid)
        ));
    }

    #[tokio::test]
    async fn key_mode_requires_a_valid_key() {
        let storage = Storage::connect_in_memory().await.unwrap();
        storage
            .mcp
            .ensure_consumer("c1", "consumer-one")
            .await
            .unwrap();
        let key = generate_key();
        storage
            .mcp
            .insert_api_key(
                "agent",
                &display_prefix(&key),
                &hash_key(&key),
                Some("c1"),
                None,
            )
            .await
            .unwrap();

        assert!(matches!(
            authenticate(&storage, None, &HeaderMap::new(), None, false).await,
            Err(AuthError::Missing)
        ));

        let mut headers = HeaderMap::new();
        headers.insert(
            axum::http::header::AUTHORIZATION,
            HeaderValue::from_static("Bearer octra_wrong"),
        );
        assert!(matches!(
            authenticate(&storage, None, &headers, None, false).await,
            Err(AuthError::Invalid)
        ));

        let mut headers = HeaderMap::new();
        headers.insert(
            API_KEY_HEADER,
            HeaderValue::from_str(&key).expect("ascii key"),
        );
        let caller = authenticate(&storage, None, &headers, None, false)
            .await
            .unwrap();
        assert_eq!(caller.consumer_id.as_deref(), Some("c1"));
        assert_eq!(caller.consumer_name.as_deref(), Some("consumer-one"));
        assert!(caller.key_id.is_some());
        assert!(caller.consumer_group_ids.is_empty());
        assert!(caller.consumer_group_names.is_empty());
    }

    #[tokio::test]
    async fn keyed_callers_resolve_consumer_groups() {
        let storage = Storage::connect_in_memory().await.unwrap();
        storage
            .mcp
            .ensure_consumer("c1", "consumer-one")
            .await
            .unwrap();
        let group = storage
            .api
            .create_consumer_group(&octra_api_core::CreateConsumerGroup {
                name: "team".into(),
                ..Default::default()
            })
            .await
            .unwrap();
        storage
            .api
            .add_consumer_group_member(&group.id, "c1")
            .await
            .unwrap();

        let key = generate_key();
        storage
            .mcp
            .insert_api_key(
                "agent",
                &display_prefix(&key),
                &hash_key(&key),
                Some("c1"),
                None,
            )
            .await
            .unwrap();
        // A key without a consumer cannot join groups.
        let unscoped = generate_key();
        storage
            .mcp
            .insert_api_key(
                "unscoped",
                &display_prefix(&unscoped),
                &hash_key(&unscoped),
                None,
                None,
            )
            .await
            .unwrap();

        let mut headers = HeaderMap::new();
        headers.insert(API_KEY_HEADER, HeaderValue::from_str(&key).unwrap());
        let caller = authenticate(&storage, None, &headers, None, false)
            .await
            .unwrap();
        assert_eq!(caller.consumer_group_ids, vec![group.id.clone()]);
        assert_eq!(caller.consumer_group_names, vec![group.name.clone()]);

        let mut headers = HeaderMap::new();
        headers.insert(API_KEY_HEADER, HeaderValue::from_str(&unscoped).unwrap());
        let caller = authenticate(&storage, None, &headers, None, false)
            .await
            .unwrap();
        assert!(caller.consumer_id.is_none());
        assert!(caller.consumer_name.is_none());
        assert!(caller.consumer_group_ids.is_empty());
        assert!(caller.consumer_group_names.is_empty());
    }
}
