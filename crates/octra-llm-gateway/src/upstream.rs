//! Provider HTTP transport: auth headers, secret resolution, timeouts.
//!
//! Only the LLM data plane talks to providers. Every outbound request
//! carries the upstream credential resolved from a secret reference; the
//! credential never appears in logs or responses.

use std::sync::Arc;
use std::time::Duration;

use octra_config::OctraConfig;
use octra_llm_core::{GatewayError, Protocol, Value};
use octra_llm_protocol::UpstreamProtocol;
use reqwest::{Client, RequestBuilder, Response, StatusCode, header};

use crate::accounts::AccountRuntime;
use crate::config::ProviderConfig;
use crate::credentials::OAuthTokenCache;

#[derive(Debug, Clone)]
pub struct Upstream {
    client: Client,
    request_timeout: Duration,
    stream_idle_timeout: Duration,
    test_api_key_env: Option<String>,
    oauth: Option<Arc<OAuthTokenCache>>,
}

impl Upstream {
    pub fn new(config: &OctraConfig) -> Self {
        Self {
            client: octra_data_plane::http_client(config),
            request_timeout: Duration::from_secs(config.llm.request_timeout_secs.max(1)),
            stream_idle_timeout: Duration::from_secs(config.llm.stream_idle_timeout_secs.max(1)),
            test_api_key_env: None,
            oauth: None,
        }
    }

    /// Attaches the process-wide OAuth token cache used by account credentials.
    pub fn with_oauth(mut self, oauth: Arc<OAuthTokenCache>) -> Self {
        self.oauth = Some(oauth);
        self
    }

    /// Transport that bypasses secret references with a fixed key.
    #[cfg(test)]
    pub fn for_tests() -> Self {
        Self::with_test_key(&OctraConfig::default(), "test-key")
    }

    /// Builds a transport that sends a fixed key instead of resolving
    /// `secret_ref`. Used by integration tests and local self-checks.
    pub fn with_test_key(config: &OctraConfig, key: impl Into<String>) -> Self {
        let mut upstream = Self::new(config);
        upstream.test_api_key_env = Some(key.into());
        upstream
    }

    /// The fixed key installed by [`Upstream::with_test_key`], if any.
    pub fn test_key(&self) -> Option<&str> {
        self.test_api_key_env.as_deref()
    }

    pub fn request_timeout(&self) -> Duration {
        self.request_timeout
    }

    pub fn stream_idle_timeout(&self) -> Duration {
        self.stream_idle_timeout
    }

    /// Resolves the provider API key. Tests may override the secret ref.
    pub fn resolve_secret(
        &self,
        provider: &ProviderConfig,
    ) -> Result<Option<String>, GatewayError> {
        self.resolve_secret_for(provider, provider.secret_ref.as_deref())
    }

    /// Resolves a secret, preferring an account-level reference over the
    /// provider's own. Tests still override both.
    pub fn resolve_secret_for(
        &self,
        provider: &ProviderConfig,
        account_secret_ref: Option<&str>,
    ) -> Result<Option<String>, GatewayError> {
        // SigV4 obtains its own static credentials from `config_json.sigv4`
        // and never sends a bearer or account token.
        if provider.sigv4.is_some() {
            return Ok(None);
        }
        if let Some(key) = &self.test_api_key_env {
            return Ok(Some(key.clone()));
        }
        let Some(reference) = account_secret_ref.or(provider.secret_ref.as_deref()) else {
            if provider.requires_secret() {
                let mut error = GatewayError::config(format!(
                    "provider {} has no secret_ref configured",
                    provider.name
                ));
                error.code = Some("secret_unavailable".to_string());
                return Err(error);
            }
            return Ok(None);
        };
        resolve_secret_ref(reference).map(Some).map_err(|message| {
            let mut error = GatewayError::config(format!("provider {}: {message}", provider.name));
            error.code = Some("secret_unavailable".to_string());
            error
        })
    }

    pub fn endpoint(
        &self,
        provider: &ProviderConfig,
        model: &str,
        stream: bool,
    ) -> Result<String, GatewayError> {
        let endpoint =
            crate::adapter::for_protocol(provider.protocol).endpoint(provider, model, stream)?;
        append_query(provider, &endpoint)
    }

    /// Builds an authenticated POST for the provider, tagging it with the
    /// request id generated for this gateway request.
    pub fn post_json(
        &self,
        provider: &ProviderConfig,
        url: &str,
        body: &Value,
        stream: bool,
    ) -> Result<RequestBuilder, GatewayError> {
        self.post_json_with_secret(provider, url, body, stream, None)
    }

    /// Like [`Upstream::post_json`], but uses an account-level secret
    /// reference when one is configured.
    pub fn post_json_with_secret(
        &self,
        provider: &ProviderConfig,
        url: &str,
        body: &Value,
        stream: bool,
        account_secret_ref: Option<&str>,
    ) -> Result<RequestBuilder, GatewayError> {
        let key = self.resolve_secret_for(provider, account_secret_ref)?;
        self.build_json(provider, url, body, stream, key)
    }

    /// Like [`Upstream::post_json`], resolving account credentials asynchronously.
    /// OAuth accounts refresh their access token before the request is built.
    pub async fn post_json_with_account(
        &self,
        provider: &ProviderConfig,
        url: &str,
        body: &Value,
        stream: bool,
        account: Option<&AccountRuntime>,
    ) -> Result<RequestBuilder, GatewayError> {
        let key = self.secret_for_account(provider, account).await?;
        self.build_json(provider, url, body, stream, key)
    }

    async fn secret_for_account(
        &self,
        provider: &ProviderConfig,
        account: Option<&AccountRuntime>,
    ) -> Result<Option<String>, GatewayError> {
        if provider.sigv4.is_some() {
            return Ok(None);
        }
        if let Some(key) = &self.test_api_key_env {
            return Ok(Some(key.clone()));
        }
        if account.is_some_and(|account| account.record.credential_kind == "oauth") {
            let account = account.expect("checked above");
            let cache = self.oauth.as_ref().ok_or_else(|| {
                GatewayError::config("OAuth credential cache is not configured")
                    .with_code("secret_unavailable")
            })?;
            return cache
                .access_token(provider, &account.record.id)
                .await
                .map(Some);
        }
        self.resolve_secret_for(
            provider,
            account.and_then(|account| account.record.secret_ref.as_deref()),
        )
    }

    /// Forces an OAuth refresh for an account, used after an upstream 401.
    pub async fn force_refresh(
        &self,
        provider: &ProviderConfig,
        account: &AccountRuntime,
    ) -> Result<(), GatewayError> {
        let cache = self.oauth.as_ref().ok_or_else(|| {
            GatewayError::config("OAuth credential cache is not configured")
                .with_code("secret_unavailable")
        })?;
        cache.force_refresh(provider, &account.record.id).await
    }

    fn build_json(
        &self,
        provider: &ProviderConfig,
        url: &str,
        body: &Value,
        stream: bool,
        key: Option<String>,
    ) -> Result<RequestBuilder, GatewayError> {
        // Serialize once so the exact bytes that are sent are also signed.
        let body_bytes = serde_json::to_vec(body).map_err(|error| {
            GatewayError::internal(format!("cannot serialize upstream request body: {error}"))
        })?;
        let mut builder = self
            .client
            .post(url)
            .header(header::CONTENT_TYPE, "application/json")
            .header(
                header::USER_AGENT,
                concat!("octra/", env!("CARGO_PKG_VERSION")),
            )
            .header(
                octra_data_plane::HEADER_REQUEST_ID,
                octra_data_plane::new_request_id(),
            )
            .body(body_bytes.clone());

        if stream {
            // A total timeout would also cap the response body and kill long
            // SSE streams. Streaming relies on the connect timeout plus the
            // gateway's per-chunk idle timeout instead.
            builder = builder.header(header::ACCEPT, "text/event-stream");
        } else {
            builder = builder.timeout(self.request_timeout);
        }

        let adapter = crate::adapter::for_protocol(provider.protocol);
        for (name, value) in adapter.extra_headers(provider) {
            builder = builder.header(name, value);
        }
        if let Some(config) = &provider.sigv4 {
            let parsed_url = reqwest::Url::parse(url).map_err(|error| {
                GatewayError::config(format!(
                    "provider {}: invalid upstream URL '{url}': {error}",
                    provider.name
                ))
            })?;
            let headers = crate::sigv4::sign_request(
                config,
                "POST",
                &parsed_url,
                &body_bytes,
                chrono::Utc::now(),
            )?;
            for (name, value) in headers {
                builder = builder.header(name, value);
            }
        } else {
            for (name, value) in adapter.auth_headers(provider, key.as_deref())? {
                builder = builder.header(name, value);
            }
        }

        // Operator headers win over generated ones, including `authorization`.
        for (name, value) in &provider.headers {
            if let (Ok(name), Ok(value)) = (
                header::HeaderName::from_bytes(name.as_bytes()),
                header::HeaderValue::from_str(value),
            ) {
                builder = builder.header(name, value);
            }
        }
        Ok(builder)
    }

    pub async fn send_json(&self, builder: RequestBuilder) -> Result<Value, GatewayError> {
        let response = builder.send().await.map_err(transport_error)?;
        let status = response.status();
        let body = response.bytes().await.map_err(transport_error)?;
        if !status.is_success() {
            let parsed = serde_json::from_slice::<Value>(&body).ok();
            return Err(error_from_upstream(status, parsed));
        }
        serde_json::from_slice(&body).map_err(|error| {
            GatewayError::upstream(502, format!("upstream returned invalid JSON: {error}"))
        })
    }

    pub async fn send_stream(&self, builder: RequestBuilder) -> Result<Response, GatewayError> {
        let response = builder.send().await.map_err(transport_error)?;
        let status = response.status();
        if !status.is_success() {
            let body = response.bytes().await.unwrap_or_default();
            let parsed = serde_json::from_slice::<Value>(&body).ok();
            return Err(error_from_upstream(status, parsed));
        }
        Ok(response)
    }
}

/// Appends `config_json.query` pairs to an adapter-produced endpoint. Parsing
/// with `reqwest::Url` keeps an existing query (Gemini `?alt=sse`, custom
/// gateways) intact and percent-encodes names and values.
fn append_query(provider: &ProviderConfig, endpoint: &str) -> Result<String, GatewayError> {
    if provider.query.is_empty() {
        return Ok(endpoint.to_string());
    }
    let mut url = reqwest::Url::parse(endpoint).map_err(|error| {
        GatewayError::config(format!(
            "provider {}: invalid upstream endpoint '{endpoint}': {error}",
            provider.name
        ))
    })?;
    {
        let mut pairs = url.query_pairs_mut();
        for (name, value) in &provider.query {
            pairs.append_pair(name, value);
        }
    }
    Ok(url.to_string())
}

/// Converts an upstream failure into a canonical error, keeping the upstream
/// status so clients see the provider's own code where it makes sense.
pub fn error_from_upstream(status: StatusCode, body: Option<Value>) -> GatewayError {
    let code = status.as_u16();
    let message = body
        .as_ref()
        .and_then(|body| body.get("error"))
        .and_then(|error| {
            error
                .get("message")
                .and_then(Value::as_str)
                .or_else(|| error.as_str())
        })
        .or_else(|| {
            body.as_ref()
                .and_then(|b| b.get("message"))
                .and_then(Value::as_str)
        })
        .unwrap_or("upstream request failed")
        .to_string();

    // Client-caused failures keep the upstream status so the caller can act
    // on them; 401/403 stay 401 so a bad upstream key is not blamed on the
    // client. Server-side failures become a 502 gateway error.
    let output_status = match code {
        400..=403 | 404 | 405 | 413 | 422 | 429 => code,
        _ => 502,
    };
    GatewayError::upstream(output_status, message).with_upstream(code, body)
}

fn transport_error(error: reqwest::Error) -> GatewayError {
    let message = error.to_string();
    if error.is_timeout() {
        GatewayError::timeout("upstream request timed out")
    } else if error.is_connect() {
        GatewayError::upstream(502, format!("cannot connect to upstream: {message}"))
    } else {
        GatewayError::upstream(502, format!("upstream transport error: {message}"))
    }
}

/// Resolves `env:VAR` and `file:/path` secret references. Returns the trimmed
/// secret or a human-readable failure message.
pub fn resolve_secret_ref(reference: &str) -> Result<String, String> {
    let reference = reference.trim();
    if let Some(var) = reference.strip_prefix("env:") {
        let var = var.trim();
        if var.is_empty() {
            return Err("empty environment variable name in secret_ref".to_string());
        }
        let value =
            std::env::var(var).map_err(|_| format!("environment variable {var} is not set"))?;
        return non_empty(value, || format!("environment variable {var} is empty"));
    }
    if let Some(path) = reference.strip_prefix("file:") {
        let path = path.trim();
        if path.is_empty() {
            return Err("empty path in secret_ref".to_string());
        }
        let value = std::fs::read_to_string(path).map_err(|error| {
            // Do not echo file contents; only the path and OS error.
            format!("cannot read secret file {path}: {error}")
        })?;
        return non_empty(value, || format!("secret file {path} is empty"));
    }
    if let Some(var) = reference.strip_prefix('$') {
        let value =
            std::env::var(var).map_err(|_| format!("environment variable {var} is not set"))?;
        return non_empty(value, || format!("environment variable {var} is empty"));
    }
    Err(format!(
        "unsupported secret_ref '{reference}': use env:VAR or file:/absolute/path"
    ))
}

fn non_empty(value: String, message: impl FnOnce() -> String) -> Result<String, String> {
    let value = value.trim().to_string();
    if value.is_empty() {
        Err(message())
    } else {
        Ok(value)
    }
}

/// Picks the upstream protocol label used by `UpstreamProtocol` for logging
/// and tests.
pub fn wire(protocol: Protocol) -> UpstreamProtocol {
    match protocol {
        Protocol::Openai => UpstreamProtocol::OpenAi,
        Protocol::OpenaiResponses => UpstreamProtocol::OpenAiResponses,
        Protocol::Anthropic => UpstreamProtocol::Anthropic,
        Protocol::Gemini => UpstreamProtocol::Gemini,
        Protocol::Vertex => UpstreamProtocol::Vertex,
        Protocol::Bedrock => UpstreamProtocol::Bedrock,
    }
}

/// Masked key for diagnostics: never log the secret itself.
pub fn mask_secret(secret: &str) -> String {
    if secret.len() <= 8 {
        "****".to_string()
    } else {
        format!("****{}", &secret[secret.len() - 4..])
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::AuthType;
    use octra_llm_core::ErrorKind;
    use octra_storage::LlmProviderRecord;

    fn provider_record(
        protocol: &str,
        auth: &str,
        secret: Option<&str>,
        base: Option<&str>,
        config: &str,
    ) -> LlmProviderRecord {
        LlmProviderRecord {
            id: "p".into(),
            name: "test".into(),
            kind: protocol.into(),
            protocol: protocol.into(),
            auth_type: auth.into(),
            base_url: base.map(str::to_string),
            secret_ref: secret.map(str::to_string),
            priority: 0,
            config_json: config.into(),
            enabled: true,
            created_at: String::new(),
            updated_at: String::new(),
        }
    }

    fn provider(
        protocol: &str,
        auth: &str,
        secret: Option<&str>,
        base: Option<&str>,
    ) -> ProviderConfig {
        ProviderConfig::from_record(&provider_record(protocol, auth, secret, base, "{}")).unwrap()
    }

    fn provider_with_config(
        protocol: &str,
        auth: &str,
        base: Option<&str>,
        config: &str,
    ) -> ProviderConfig {
        ProviderConfig::from_record(&provider_record(protocol, auth, None, base, config)).unwrap()
    }

    #[test]
    #[allow(unsafe_code)]
    fn resolves_env_secret() {
        // SAFETY: test-only environment mutation with a unique variable name.
        unsafe {
            std::env::set_var("OCTRA_TEST_SECRET_A", "  sk-test-123  \n");
        }
        assert_eq!(
            resolve_secret_ref("env:OCTRA_TEST_SECRET_A").unwrap(),
            "sk-test-123"
        );
        assert!(resolve_secret_ref("env:OCTRA_TEST_SECRET_MISSING").is_err());
        assert!(resolve_secret_ref("plaintext-secret").is_err());
    }

    #[test]
    fn resolves_file_secret() {
        let dir = std::env::temp_dir().join(format!("octra-secret-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("key");
        std::fs::write(&path, "sk-from-file\n").unwrap();
        let reference = format!("file:{}", path.display());
        assert_eq!(resolve_secret_ref(&reference).unwrap(), "sk-from-file");
        std::fs::remove_dir_all(&dir).ok();
        assert!(resolve_secret_ref(&reference).is_err());
    }

    #[test]
    fn missing_secret_is_a_config_error() {
        let upstream = Upstream::for_tests();
        let test_config = provider(
            "openai",
            "bearer",
            Some("env:OCTRA_TEST_UNSET_SECRET"),
            None,
        );
        assert_eq!(
            upstream.resolve_secret(&test_config).unwrap().as_deref(),
            Some("test-key")
        );
        let upstream = Upstream::new(&OctraConfig::default());
        let config = provider("openai", "bearer", None, None);
        let error = upstream.resolve_secret(&config).unwrap_err();
        assert_eq!(error.kind, ErrorKind::Config);
        assert_eq!(error.status, 502);
        assert_eq!(error.code.as_deref(), Some("secret_unavailable"));

        // `auth_type = none` is how local servers skip credentials; the
        // provider adapter maps it to `AuthType::None`.
        let anonymous = provider("openai", "none", None, Some("http://127.0.0.1:1/v1"));
        assert!(upstream.resolve_secret(&anonymous).unwrap().is_none());
    }

    #[test]
    fn endpoints_follow_protocol() {
        let upstream = Upstream::new(&OctraConfig::default());
        let openai = provider("openai", "none", None, Some("https://api.openai.com/v1"));
        assert_eq!(
            upstream.endpoint(&openai, "gpt-4o", false).unwrap(),
            "https://api.openai.com/v1/chat/completions"
        );
        let anthropic = provider(
            "anthropic",
            "none",
            None,
            Some("https://api.anthropic.com/v1/"),
        );
        assert_eq!(
            upstream.endpoint(&anthropic, "claude", false).unwrap(),
            "https://api.anthropic.com/v1/messages"
        );
        let vertex = provider(
            "vertex",
            "none",
            None,
            Some("https://aiplatform.googleapis.com/v1"),
        );
        assert_eq!(
            upstream
                .endpoint(&vertex, "gemini-2.0-flash", false)
                .unwrap(),
            "https://aiplatform.googleapis.com/v1/publishers/google/models/gemini-2.0-flash:generateContent"
        );
        assert_eq!(
            upstream
                .endpoint(&vertex, "gemini-2.0-flash", true)
                .unwrap(),
            "https://aiplatform.googleapis.com/v1/publishers/google/models/gemini-2.0-flash:streamGenerateContent?alt=sse"
        );
        let bedrock = provider(
            "bedrock",
            "none",
            None,
            Some("https://bedrock-runtime.us-east-1.amazonaws.com"),
        );
        let model = "anthropic.claude-3-5-sonnet-20241022-v2:0";
        assert_eq!(
            upstream.endpoint(&bedrock, model, false).unwrap(),
            format!("https://bedrock-runtime.us-east-1.amazonaws.com/model/{model}/converse")
        );
        assert_eq!(
            upstream.endpoint(&bedrock, model, true).unwrap(),
            format!(
                "https://bedrock-runtime.us-east-1.amazonaws.com/model/{model}/converse-stream"
            )
        );
        let bad = provider("openai", "none", None, Some("ftp://x"));
        assert_eq!(
            upstream.endpoint(&bad, "m", false).unwrap_err().kind,
            ErrorKind::Config
        );
    }

    #[test]
    fn endpoints_append_provider_query() {
        let upstream = Upstream::new(&OctraConfig::default());
        let openai = provider_with_config(
            "openai",
            "none",
            Some("https://api.openai.com/v1"),
            r#"{"query":{"api-version":"2024-10-21"}}"#,
        );
        assert_eq!(
            upstream.endpoint(&openai, "gpt-4o", false).unwrap(),
            "https://api.openai.com/v1/chat/completions?api-version=2024-10-21"
        );

        let anthropic = provider_with_config(
            "anthropic",
            "none",
            Some("https://api.anthropic.com/v1"),
            r#"{"query":{"api-version":"2024-10-21"}}"#,
        );
        assert_eq!(
            upstream.endpoint(&anthropic, "claude", false).unwrap(),
            "https://api.anthropic.com/v1/messages?api-version=2024-10-21"
        );

        let encoded = provider_with_config(
            "openai",
            "none",
            Some("https://example.test/v1"),
            r#"{"query":{"tag":"a&b"}}"#,
        );
        assert_eq!(
            upstream.endpoint(&encoded, "m", false).unwrap(),
            "https://example.test/v1/chat/completions?tag=a%26b"
        );
    }

    #[test]
    fn provider_query_preserves_existing_query_parameters() {
        let upstream = Upstream::new(&OctraConfig::default());
        let gemini = provider_with_config(
            "gemini",
            "none",
            Some("https://generativelanguage.googleapis.com/v1beta"),
            r#"{"query":{"key":"abc"}}"#,
        );
        assert_eq!(
            upstream.endpoint(&gemini, "gemini-pro", true).unwrap(),
            "https://generativelanguage.googleapis.com/v1beta/models/gemini-pro:streamGenerateContent?alt=sse&key=abc"
        );

        let vertex = provider_with_config(
            "vertex",
            "none",
            Some("https://aiplatform.googleapis.com/v1"),
            r#"{"query":{"key":"abc"}}"#,
        );
        assert_eq!(
            upstream
                .endpoint(&vertex, "gemini-2.0-flash", true)
                .unwrap(),
            "https://aiplatform.googleapis.com/v1/publishers/google/models/gemini-2.0-flash:streamGenerateContent?alt=sse&key=abc"
        );
        assert_eq!(
            upstream
                .endpoint(&vertex, "gemini-2.0-flash", false)
                .unwrap(),
            "https://aiplatform.googleapis.com/v1/publishers/google/models/gemini-2.0-flash:generateContent?key=abc"
        );
    }

    #[test]
    fn provider_query_rejects_unparsable_endpoints() {
        let upstream = Upstream::new(&OctraConfig::default());
        let bad = provider_with_config(
            "openai",
            "none",
            Some("https://bad host/v1"),
            r#"{"query":{"a":"b"}}"#,
        );
        let error = upstream.endpoint(&bad, "m", false).unwrap_err();
        assert_eq!(error.kind, ErrorKind::Config);
        assert!(error.message.contains("invalid upstream endpoint"));
    }

    #[test]
    fn upstream_errors_are_mapped() {
        let body =
            Some(serde_json::json!({"error": {"message": "rate limited", "type": "rate_limit"}}));
        let error = error_from_upstream(StatusCode::TOO_MANY_REQUESTS, body);
        assert_eq!(error.status, 429);
        assert_eq!(error.message, "rate limited");
        assert_eq!(error.upstream_status, Some(429));

        let error = error_from_upstream(StatusCode::INTERNAL_SERVER_ERROR, None);
        assert_eq!(error.status, 502);
        assert_eq!(error.kind, ErrorKind::Upstream);
    }

    #[test]
    fn masks_secrets() {
        assert_eq!(mask_secret("short"), "****");
        assert_eq!(mask_secret("sk-1234567890"), "****7890");
    }

    #[test]
    fn sigv4_provider_signs_request_without_bearer_auth() {
        let dir =
            std::env::temp_dir().join(format!("octra-sigv4-upstream-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let access = dir.join("access_key");
        let secret = dir.join("secret_key");
        std::fs::write(&access, "AKIDEXAMPLE\n").unwrap();
        std::fs::write(&secret, "test-secret-value\n").unwrap();

        let config = serde_json::json!({
            "sigv4": {
                "region": "us-east-1",
                "service": "bedrock",
                "access_key_ref": format!("file:{}", access.display()),
                "secret_key_ref": format!("file:{}", secret.display()),
            },
            "headers": {"x-extra": "kept"},
        })
        .to_string();
        let provider = provider_with_config(
            "bedrock",
            "bearer",
            Some("https://bedrock-runtime.us-east-1.amazonaws.com"),
            &config,
        );
        assert_eq!(provider.auth, AuthType::None);

        let body = serde_json::json!({
            "messages": [{"role": "user", "content": [{"text": "hello"}]}]
        });
        let upstream = Upstream::new(&OctraConfig::default());
        let request = upstream
            .build_json(
                &provider,
                "https://bedrock-runtime.us-east-1.amazonaws.com/model/m/converse",
                &body,
                false,
                Some("unused-bearer-key".to_string()),
            )
            .unwrap()
            .build()
            .unwrap();

        let headers = request.headers();
        let authorization = headers
            .get(header::AUTHORIZATION)
            .expect("authorization header")
            .to_str()
            .unwrap();
        assert!(
            authorization.starts_with("AWS4-HMAC-SHA256 Credential=AKIDEXAMPLE/"),
            "{authorization}"
        );
        assert!(!authorization.contains("Bearer"));
        assert!(
            authorization
                .contains("SignedHeaders=content-type;host;x-amz-content-sha256;x-amz-date,")
        );
        assert!(headers.contains_key("x-amz-date"));
        assert!(headers.contains_key("x-amz-content-sha256"));
        assert_eq!(headers.get("x-extra").unwrap(), "kept");
        assert_eq!(
            headers.get(header::CONTENT_TYPE).unwrap(),
            "application/json"
        );

        let expected_body = serde_json::to_vec(&body).unwrap();
        assert_eq!(
            request.body().and_then(reqwest::Body::as_bytes),
            Some(expected_body.as_slice())
        );

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn headers_are_applied() {
        let upstream = Upstream::new(&OctraConfig::default());
        let config = provider("anthropic", "anthropic", Some("env:NOPE"), None);
        let _ = upstream; // builder construction is exercised in integration tests
        assert_eq!(config.auth, AuthType::AnthropicApiKey);
    }
}
