//! OAuth credential resolution and refresh for provider accounts.
//!
//! Tokens are stored in `llm_provider_oauth_tokens` (plaintext for now). The
//! in-memory cache is per process and survives runtime reloads; refreshes are
//! single-flight per account.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, RwLock};
use std::time::Duration;

use base64::Engine as _;
use chrono::{DateTime, SecondsFormat, Utc};
use octra_llm_core::GatewayError;
use octra_storage::{LlmOAuthTokenRecord, Storage};
use reqwest::Client;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::config::{OAuthProfile, OAuthTokenAuth, ProviderConfig};
use crate::upstream::resolve_secret_ref;

const REFRESH_MARGIN_SECS: i64 = 60;
const FLOW_TTL_SECS: i64 = 600;

#[derive(Debug, Clone)]
struct CachedToken {
    access_token: String,
    refresh_token: Option<String>,
    token_type: String,
    scope: String,
    expires_at: Option<DateTime<Utc>>,
    extra_json: String,
}

impl CachedToken {
    fn from_record(record: LlmOAuthTokenRecord) -> Self {
        Self {
            access_token: record.access_token,
            refresh_token: record.refresh_token,
            token_type: record.token_type,
            scope: record.scope,
            expires_at: parse_timestamp(record.expires_at.as_deref()),
            extra_json: record.extra_json,
        }
    }

    fn needs_refresh(&self) -> bool {
        match self.expires_at {
            Some(expires_at) => {
                expires_at <= Utc::now() + chrono::Duration::seconds(REFRESH_MARGIN_SECS)
            }
            None => false,
        }
    }
}

fn parse_timestamp(value: Option<&str>) -> Option<DateTime<Utc>> {
    value
        .and_then(|value| DateTime::parse_from_rfc3339(value).ok())
        .map(|value| value.with_timezone(&Utc))
}

pub struct OAuthTokenCache {
    storage: Arc<Storage>,
    http: Client,
    tokens: RwLock<HashMap<String, CachedToken>>,
    locks: Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>,
}

impl std::fmt::Debug for OAuthTokenCache {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("OAuthTokenCache")
    }
}

impl OAuthTokenCache {
    pub fn new(storage: Arc<Storage>) -> Self {
        Self {
            storage,
            http: Client::builder()
                .timeout(Duration::from_secs(30))
                .no_proxy()
                .build()
                .unwrap_or_default(),
            tokens: RwLock::new(HashMap::new()),
            locks: Mutex::new(HashMap::new()),
        }
    }

    fn account_lock(&self, account_id: &str) -> Arc<tokio::sync::Mutex<()>> {
        let mut locks = self.locks.lock().unwrap_or_else(|error| error.into_inner());
        locks.entry(account_id.to_string()).or_default().clone()
    }

    /// Returns a usable access token, refreshing it when it is about to
    /// expire. Refreshes are serialized per account.
    pub async fn access_token(
        &self,
        provider: &ProviderConfig,
        account_id: &str,
    ) -> Result<String, GatewayError> {
        let profile = oauth_profile(provider)?;
        let lock = self.account_lock(account_id);
        let _guard = lock.lock().await;

        if let Some(token) = self.cached(account_id)
            && !token.needs_refresh()
        {
            return Ok(token.access_token);
        }

        let stored = self
            .storage
            .llm
            .get_oauth_token(account_id)
            .await
            .map_err(storage_secret_error)?;
        let Some(mut token) = stored.map(CachedToken::from_record) else {
            return Err(secret_error("account has no OAuth token"));
        };
        if !token.needs_refresh() {
            self.remember(account_id, token.clone());
            return Ok(token.access_token);
        }
        let Some(refresh_token) = token.refresh_token.clone() else {
            self.storage
                .llm
                .mark_oauth_reauth(account_id, "access token expired without a refresh token")
                .await
                .map_err(storage_secret_error)?;
            return Err(secret_error(
                "OAuth token expired; reauthorization required",
            ));
        };
        let refreshed = self
            .refresh_with(profile, account_id, &refresh_token)
            .await?;
        token = refreshed;
        self.remember(account_id, token.clone());
        Ok(token.access_token)
    }

    /// Forces a refresh, used after an upstream 401.
    pub async fn force_refresh(
        &self,
        provider: &ProviderConfig,
        account_id: &str,
    ) -> Result<(), GatewayError> {
        let profile = oauth_profile(provider)?;
        let lock = self.account_lock(account_id);
        let _guard = lock.lock().await;
        let stored = self
            .storage
            .llm
            .get_oauth_token(account_id)
            .await
            .map_err(storage_secret_error)?;
        let Some(refresh_token) = stored.and_then(|token| token.refresh_token) else {
            self.storage
                .llm
                .mark_oauth_reauth(account_id, "no refresh token")
                .await
                .map_err(storage_secret_error)?;
            return Err(secret_error("OAuth reauthorization required"));
        };
        let token = self
            .refresh_with(profile, account_id, &refresh_token)
            .await?;
        self.remember(account_id, token);
        Ok(())
    }

    /// Exchanges an authorization code for tokens and persists the result.
    pub async fn exchange_code(
        &self,
        provider: &ProviderConfig,
        account_id: &str,
        code: &str,
        verifier: &str,
        redirect_uri: &str,
    ) -> Result<(), GatewayError> {
        let profile = oauth_profile(provider)?;
        let mut form = vec![
            ("grant_type".to_string(), "authorization_code".to_string()),
            ("code".to_string(), code.to_string()),
            ("code_verifier".to_string(), verifier.to_string()),
            ("redirect_uri".to_string(), redirect_uri.to_string()),
        ];
        let token = self.request_token(profile, &mut form).await?;
        self.persist(account_id, token).await
    }

    /// Refreshes one account explicitly (control plane).
    pub async fn refresh_account(
        &self,
        provider: &ProviderConfig,
        account_id: &str,
    ) -> Result<(), GatewayError> {
        self.force_refresh(provider, account_id).await
    }

    pub fn invalidate(&self, account_id: &str) {
        self.tokens
            .write()
            .unwrap_or_else(|error| error.into_inner())
            .remove(account_id);
    }

    fn cached(&self, account_id: &str) -> Option<CachedToken> {
        self.tokens
            .read()
            .unwrap_or_else(|error| error.into_inner())
            .get(account_id)
            .cloned()
    }

    fn remember(&self, account_id: &str, token: CachedToken) {
        self.tokens
            .write()
            .unwrap_or_else(|error| error.into_inner())
            .insert(account_id.to_string(), token);
    }

    async fn refresh_with(
        &self,
        profile: &OAuthProfile,
        account_id: &str,
        refresh_token: &str,
    ) -> Result<CachedToken, GatewayError> {
        let mut form = vec![
            ("grant_type".to_string(), "refresh_token".to_string()),
            ("refresh_token".to_string(), refresh_token.to_string()),
        ];
        match self.request_token(profile, &mut form).await {
            Ok(token) => {
                self.persist(account_id, token.clone()).await?;
                Ok(token)
            }
            Err(error) => {
                if error.code.as_deref() == Some("invalid_grant") {
                    self.storage
                        .llm
                        .mark_oauth_reauth(account_id, "invalid_grant")
                        .await
                        .map_err(storage_secret_error)?;
                    self.invalidate(account_id);
                    return Err(
                        secret_error("OAuth reauthorization required").with_code("reauth_required")
                    );
                }
                Err(error)
            }
        }
    }

    async fn persist(&self, account_id: &str, token: CachedToken) -> Result<(), GatewayError> {
        let record = LlmOAuthTokenRecord {
            account_id: account_id.to_string(),
            access_token: token.access_token.clone(),
            refresh_token: token.refresh_token.clone(),
            token_type: token.token_type.clone(),
            scope: token.scope.clone(),
            expires_at: token
                .expires_at
                .map(|value| value.to_rfc3339_opts(SecondsFormat::Millis, true)),
            extra_json: token.extra_json.clone(),
        };
        self.storage
            .llm
            .upsert_oauth_token(&record)
            .await
            .map_err(storage_secret_error)?;
        self.remember(account_id, token);
        Ok(())
    }

    async fn request_token(
        &self,
        profile: &OAuthProfile,
        form: &mut Vec<(String, String)>,
    ) -> Result<CachedToken, GatewayError> {
        form.push(("client_id".to_string(), profile.client_id.clone()));
        form.extend(profile.extra_token_params.iter().cloned());
        let mut request = self.http.post(&profile.token_url).form(form);
        if profile.token_auth == OAuthTokenAuth::Basic
            && let Some(secret_ref) = &profile.client_secret_ref
        {
            let secret = resolve_secret_ref(secret_ref).map_err(secret_error)?;
            let encoded = base64::engine::general_purpose::STANDARD
                .encode(format!("{}:{secret}", profile.client_id));
            request = request.header(reqwest::header::AUTHORIZATION, format!("Basic {encoded}"));
        } else if let Some(secret_ref) = &profile.client_secret_ref {
            let secret = resolve_secret_ref(secret_ref).map_err(secret_error)?;
            form.push(("client_secret".to_string(), secret));
        }
        let response = request.send().await.map_err(|error| {
            if error.is_timeout() {
                GatewayError::timeout("OAuth token endpoint timed out")
                    .with_code("secret_unavailable")
            } else {
                secret_error("OAuth token endpoint is unreachable")
            }
        })?;
        let status = response.status();
        let body = response.text().await.unwrap_or_default();
        if !status.is_success() {
            let error = serde_json::from_str::<TokenErrorBody>(&body)
                .ok()
                .and_then(|body| body.error)
                .unwrap_or_else(|| format!("token endpoint returned {status}"));
            if error == "invalid_grant" {
                return Err(
                    GatewayError::config("OAuth refresh was rejected").with_code("invalid_grant")
                );
            }
            return Err(secret_error(format!("OAuth token endpoint error: {error}")));
        }
        let token: TokenResponse = serde_json::from_str(&body).map_err(|error| {
            secret_error(format!(
                "OAuth token endpoint returned invalid JSON: {error}"
            ))
        })?;
        let expires_at = token
            .expires_in
            .map(|seconds| Utc::now() + chrono::Duration::seconds(seconds as i64));
        Ok(CachedToken {
            access_token: token.access_token,
            refresh_token: token.refresh_token,
            token_type: token.token_type.unwrap_or_else(|| "Bearer".to_string()),
            scope: token.scope.unwrap_or_default(),
            expires_at,
            extra_json: serde_json::to_string(&token.extra).unwrap_or_else(|_| "{}".to_string()),
        })
    }
}

fn oauth_profile(provider: &ProviderConfig) -> Result<&OAuthProfile, GatewayError> {
    provider.oauth.as_ref().ok_or_else(|| {
        secret_error(format!(
            "provider {} has no OAuth configuration",
            provider.name
        ))
    })
}

fn secret_error(message: impl Into<String>) -> GatewayError {
    GatewayError::config(message).with_code("secret_unavailable")
}

fn storage_secret_error(error: octra_storage::LlmStorageError) -> GatewayError {
    secret_error(format!("OAuth token storage error: {error}"))
}

#[derive(Deserialize)]
struct TokenResponse {
    access_token: String,
    #[serde(default)]
    refresh_token: Option<String>,
    #[serde(default)]
    token_type: Option<String>,
    #[serde(default)]
    expires_in: Option<u64>,
    #[serde(default)]
    scope: Option<String>,
    #[serde(flatten)]
    extra: serde_json::Map<String, serde_json::Value>,
}

#[derive(Deserialize)]
struct TokenErrorBody {
    #[serde(default)]
    error: Option<String>,
}

/// PKCE flow state kept in memory between `start` and `complete`.
#[derive(Debug)]
pub struct PendingFlow {
    pub provider_id: String,
    pub account_id: String,
    pub verifier: String,
    pub redirect_uri: String,
    created_at: i64,
}

#[derive(Default)]
pub struct OAuthFlowStore {
    flows: Mutex<HashMap<String, PendingFlow>>,
}

#[derive(Debug, Serialize)]
pub struct FlowStart {
    pub state: String,
    pub authorize_url: String,
    pub redirect_uri: String,
}

impl OAuthFlowStore {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn begin(&self, provider: &ProviderConfig, account_id: &str) -> Result<FlowStart, String> {
        let profile = provider
            .oauth
            .as_ref()
            .ok_or_else(|| "provider has no OAuth configuration".to_string())?;
        let redirect_uri = profile
            .redirect_uri
            .clone()
            .ok_or_else(|| "oauth.redirect_uri is required for authorization".to_string())?;
        let verifier = generate_verifier();
        let challenge = code_challenge(&verifier);
        let state = uuid::Uuid::new_v4().simple().to_string();

        let mut url = reqwest::Url::parse(&profile.authorize_url)
            .map_err(|error| format!("invalid oauth.authorize_url: {error}"))?;
        {
            let mut query = url.query_pairs_mut();
            query.append_pair("response_type", "code");
            query.append_pair("client_id", &profile.client_id);
            query.append_pair("redirect_uri", &redirect_uri);
            query.append_pair("state", &state);
            query.append_pair("code_challenge", &challenge);
            query.append_pair("code_challenge_method", "S256");
            if !profile.scopes.is_empty() {
                query.append_pair("scope", &profile.scopes.join(" "));
            }
            for (key, value) in &profile.extra_authorize_params {
                query.append_pair(key, value);
            }
        }

        let now = Utc::now().timestamp();
        let mut flows = self.flows.lock().unwrap_or_else(|error| error.into_inner());
        flows.retain(|_, flow| now - flow.created_at < FLOW_TTL_SECS);
        flows.insert(
            state.clone(),
            PendingFlow {
                provider_id: provider.id.clone(),
                account_id: account_id.to_string(),
                verifier,
                redirect_uri: redirect_uri.clone(),
                created_at: now,
            },
        );
        Ok(FlowStart {
            state,
            authorize_url: url.to_string(),
            redirect_uri,
        })
    }

    pub fn take(&self, state: &str) -> Option<PendingFlow> {
        let now = Utc::now().timestamp();
        let mut flows = self.flows.lock().unwrap_or_else(|error| error.into_inner());
        flows.retain(|_, flow| now - flow.created_at < FLOW_TTL_SECS);
        flows.remove(state)
    }
}

fn generate_verifier() -> String {
    format!(
        "{}{}",
        uuid::Uuid::new_v4().simple(),
        uuid::Uuid::new_v4().simple()
    )
}

fn code_challenge(verifier: &str) -> String {
    let digest = Sha256::digest(verifier.as_bytes());
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(digest)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn code_challenge_is_url_safe_and_deterministic() {
        let challenge = code_challenge("abc");
        assert_eq!(challenge, code_challenge("abc"));
        assert!(!challenge.contains('='));
        assert!(!challenge.contains('+'));
        assert!(!challenge.contains('/'));
    }

    #[test]
    fn verifier_has_pkce_length() {
        let verifier = generate_verifier();
        assert!(verifier.len() >= 43 && verifier.len() <= 128);
    }

    #[tokio::test]
    async fn refresh_is_single_flight_and_invalid_grant_marks_reauth() {
        use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

        use axum::Json;
        use axum::routing::post;
        use octra_ai_config::{AiProvider, AiProviderAccount};
        use octra_storage::{LlmOAuthTokenRecord, LlmProviderRecord, Storage};
        use serde_json::json;

        let hits = Arc::new(AtomicUsize::new(0));
        let invalid = Arc::new(AtomicBool::new(false));
        let server_hits = hits.clone();
        let server_invalid = invalid.clone();
        let app = axum::Router::new().route(
            "/token",
            post(move || {
                let hits = server_hits.clone();
                let invalid = server_invalid.clone();
                async move {
                    hits.fetch_add(1, Ordering::Relaxed);
                    if invalid.load(Ordering::Relaxed) {
                        (
                            axum::http::StatusCode::BAD_REQUEST,
                            Json(json!({"error": "invalid_grant"})),
                        )
                    } else {
                        (
                            axum::http::StatusCode::OK,
                            Json(json!({"access_token": "new", "expires_in": 3600})),
                        )
                    }
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });

        let storage = Arc::new(Storage::connect_in_memory().await.unwrap());
        let now = Utc::now().timestamp();
        let provider_id = "provider-subscription";
        let provider_config = json!({
            "oauth": {
                "authorize_url": "https://example.com/authorize",
                "token_url": format!("http://{address}/token"),
                "client_id": "client"
            }
        });
        let provider_record = LlmProviderRecord {
            id: provider_id.to_string(),
            name: "subscription".into(),
            kind: "openai".into(),
            protocol: "openai".into(),
            auth_type: "bearer".into(),
            base_url: None,
            secret_ref: None,
            priority: 0,
            config_json: provider_config.to_string(),
            enabled: true,
            created_at: String::new(),
            updated_at: String::new(),
        };
        storage
            .ai
            .upsert_provider(&AiProvider {
                id: provider_id.to_string(),
                name: "subscription".into(),
                protocol: "openai".into(),
                base_url: String::new(),
                auth_ref: None,
                capabilities: vec!["chat".into()],
                config: provider_config,
                enabled: true,
                created_at: now,
                updated_at: now,
            })
            .await
            .unwrap();
        let account_id = "account-subscription";
        storage
            .ai
            .upsert_account(&AiProviderAccount {
                id: account_id.to_string(),
                provider_id: provider_id.to_string(),
                name: "sub".into(),
                credential_kind: "static".into(),
                secret_ref: None,
                oauth_ref: None,
                weight: 1,
                priority: 0,
                quota_limit: None,
                quota_unit: "requests".into(),
                quota_period: "none".into(),
                quota_used: 0,
                cooldown_until: None,
                enabled: true,
                config: json!({}),
                created_at: now,
                updated_at: now,
            })
            .await
            .unwrap();
        storage
            .llm
            .upsert_oauth_token(&LlmOAuthTokenRecord {
                account_id: account_id.to_string(),
                access_token: "old".into(),
                refresh_token: Some("refresh".into()),
                token_type: "Bearer".into(),
                scope: String::new(),
                expires_at: Some("2000-01-01T00:00:00.000Z".into()),
                extra_json: "{}".into(),
            })
            .await
            .unwrap();
        let provider = ProviderConfig::from_record(&provider_record).unwrap();
        let cache = Arc::new(OAuthTokenCache::new(storage.clone()));

        let (first, second) = tokio::join!(
            cache.access_token(&provider, account_id),
            cache.access_token(&provider, account_id)
        );
        assert_eq!(first.unwrap(), "new");
        assert_eq!(second.unwrap(), "new");
        assert_eq!(hits.load(Ordering::Relaxed), 1);

        invalid.store(true, Ordering::Relaxed);
        let error = cache
            .force_refresh(&provider, account_id)
            .await
            .unwrap_err();
        assert_eq!(error.code.as_deref(), Some("reauth_required"));
        assert!(
            storage
                .llm
                .get_oauth_token(account_id)
                .await
                .unwrap()
                .is_none()
        );
        let account = storage.ai.get_account(account_id).await.unwrap().unwrap();
        assert_eq!(account.id, account_id);
        server.abort();
    }
}
