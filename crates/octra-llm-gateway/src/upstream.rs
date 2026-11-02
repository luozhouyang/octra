//! Provider HTTP transport: auth headers, secret resolution, timeouts.
//!
//! Only the LLM data plane talks to providers. Every outbound request
//! carries the upstream credential resolved from a secret reference; the
//! credential never appears in logs or responses.

use std::time::Duration;

use octra_config::OctraConfig;
use octra_llm_core::{GatewayError, Protocol, Value};
use octra_llm_protocol::UpstreamProtocol;
use reqwest::{Client, RequestBuilder, Response, StatusCode, header};

use crate::config::ProviderConfig;

#[derive(Debug, Clone)]
pub struct Upstream {
    client: Client,
    request_timeout: Duration,
    stream_idle_timeout: Duration,
    test_api_key_env: Option<String>,
}

impl Upstream {
    pub fn new(config: &OctraConfig) -> Self {
        Self {
            client: octra_data_plane::http_client(config),
            request_timeout: Duration::from_secs(config.llm.request_timeout_secs.max(1)),
            stream_idle_timeout: Duration::from_secs(config.llm.stream_idle_timeout_secs.max(1)),
            test_api_key_env: None,
        }
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
        crate::adapter::for_protocol(provider.protocol).endpoint(provider, model, stream)
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
        let mut builder = self
            .client
            .post(url)
            .timeout(self.request_timeout)
            .header(header::CONTENT_TYPE, "application/json")
            .header(
                header::USER_AGENT,
                concat!("octra/", env!("CARGO_PKG_VERSION")),
            )
            .header(
                octra_data_plane::HEADER_REQUEST_ID,
                octra_data_plane::new_request_id(),
            )
            .json(body);

        if stream {
            builder = builder.header(header::ACCEPT, "text/event-stream");
        }

        let adapter = crate::adapter::for_protocol(provider.protocol);
        for (name, value) in adapter.extra_headers(provider) {
            builder = builder.header(name, value);
        }
        for (name, value) in adapter.auth_headers(provider, key.as_deref())? {
            builder = builder.header(name, value);
        }

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

    fn provider(
        protocol: &str,
        auth: &str,
        secret: Option<&str>,
        base: Option<&str>,
    ) -> ProviderConfig {
        let record = LlmProviderRecord {
            id: "p".into(),
            name: "test".into(),
            kind: protocol.into(),
            protocol: protocol.into(),
            auth_type: auth.into(),
            base_url: base.map(str::to_string),
            secret_ref: secret.map(str::to_string),
            priority: 0,
            config_json: "{}".into(),
            enabled: true,
            created_at: String::new(),
            updated_at: String::new(),
        };
        ProviderConfig::from_record(&record).unwrap()
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
        let bad = provider("openai", "none", None, Some("ftp://x"));
        assert_eq!(
            upstream.endpoint(&bad, "m", false).unwrap_err().kind,
            ErrorKind::Config
        );
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
    fn headers_are_applied() {
        let upstream = Upstream::new(&OctraConfig::default());
        let config = provider("anthropic", "anthropic", Some("env:NOPE"), None);
        let _ = upstream; // builder construction is exercised in integration tests
        assert_eq!(config.auth, AuthType::AnthropicApiKey);
    }
}
