//! Provider configuration parsing: protocols, auth, secret refs, base URLs.

use octra_llm_core::Protocol;
use octra_storage::{LlmProviderRecord, LlmRuntimeData};
use serde_json::{Map, Value};

/// How the upstream authenticates the gateway.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthType {
    /// `Authorization: Bearer <key>` (OpenAI and most compatible servers).
    Bearer,
    /// `x-api-key: <key>` plus `anthropic-version`.
    AnthropicApiKey,
    /// Custom header named in `config_json.auth_header`.
    Header,
    /// No credential (local servers).
    None,
}

impl AuthType {
    pub fn parse(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "bearer" => Some(AuthType::Bearer),
            "anthropic" | "x_api_key" | "api_key" | "x-api-key" | "anthropic_api_key" => {
                Some(AuthType::AnthropicApiKey)
            }
            "header" | "custom" => Some(AuthType::Header),
            "none" => Some(AuthType::None),
            _ => None,
        }
    }

    pub fn default_for(protocol: Protocol) -> Self {
        match protocol {
            Protocol::Openai => AuthType::Bearer,
            Protocol::Anthropic => AuthType::AnthropicApiKey,
        }
    }
}

pub const ANTHROPIC_VERSION: &str = "2023-06-01";

/// Fully resolved provider settings used by the request path.
#[derive(Debug, Clone)]
pub struct ProviderConfig {
    pub id: String,
    pub name: String,
    pub protocol: Protocol,
    pub auth: AuthType,
    /// Normalized base URL without a trailing slash.
    pub base_url: String,
    pub secret_ref: Option<String>,
    pub auth_header: Option<String>,
    /// Extra headers from `config_json.headers`.
    pub headers: Vec<(String, String)>,
    pub anthropic_version: String,
}

#[derive(Debug)]
pub enum ProviderConfigError {
    Invalid(String),
}

impl ProviderConfig {
    pub fn from_record(record: &LlmProviderRecord) -> Result<Self, ProviderConfigError> {
        let config: Map<String, Value> = match record.config_json.trim() {
            "" | "{}" => Map::new(),
            raw => serde_json::from_str(raw).map_err(|error| {
                ProviderConfigError::Invalid(format!(
                    "provider {}: invalid config_json: {error}",
                    record.name
                ))
            })?,
        };

        // `config_json.wire` lets a local OpenAI-compatible server be driven
        // with the Anthropic wire format (and vice versa).
        let protocol_name = match config.get("wire").and_then(Value::as_str) {
            Some("anthropic") => "anthropic",
            Some("openai") => "openai",
            _ => record.protocol.as_str(),
        };
        let protocol = Protocol::parse(protocol_name).ok_or_else(|| {
            ProviderConfigError::Invalid(format!(
                "provider {}: unknown protocol '{}'",
                record.name, record.protocol
            ))
        })?;

        let mut auth = AuthType::default_for(protocol);
        if !record.auth_type.trim().is_empty() {
            auth = AuthType::parse(&record.auth_type).ok_or_else(|| {
                ProviderConfigError::Invalid(format!(
                    "provider {}: unknown auth_type '{}'",
                    record.name, record.auth_type
                ))
            })?;
        }
        let mut auth_header = None;
        if let Some(value) = config.get("auth_header").and_then(Value::as_str)
            && !value.trim().is_empty()
        {
            auth = AuthType::Header;
            auth_header = Some(value.trim().to_string());
        }

        let base_url = match record.base_url.as_deref().map(str::trim) {
            Some(url) if !url.is_empty() => normalize_base_url(url),
            _ => default_base_url(protocol).to_string(),
        };

        let mut headers = Vec::new();
        if let Some(map) = config.get("headers").and_then(Value::as_object) {
            for (name, value) in map {
                if let Some(value) = value.as_str() {
                    headers.push((name.clone(), value.to_string()));
                }
            }
        }

        Ok(Self {
            id: record.id.clone(),
            name: record.name.clone(),
            protocol,
            auth,
            base_url,
            secret_ref: record
                .secret_ref
                .clone()
                .filter(|value| !value.trim().is_empty()),
            auth_header,
            headers,
            anthropic_version: config
                .get("anthropic_version")
                .and_then(Value::as_str)
                .unwrap_or(ANTHROPIC_VERSION)
                .to_string(),
        })
    }

    pub fn requires_secret(&self) -> bool {
        self.auth != AuthType::None
    }

    pub fn url(&self, path: &str) -> String {
        format!("{}{}", self.base_url, path)
    }
}

pub fn default_base_url(protocol: Protocol) -> &'static str {
    match protocol {
        Protocol::Openai => "https://api.openai.com/v1",
        Protocol::Anthropic => "https://api.anthropic.com/v1",
    }
}

pub fn is_local_url(url: &str) -> bool {
    let lower = url.trim().to_ascii_lowercase();
    [
        "http://127.0.0.1",
        "http://localhost",
        "http://[::1]",
        "http://0.0.0.0",
    ]
    .iter()
    .any(|prefix| lower.starts_with(prefix))
}

/// Strips trailing slashes and a pasted endpoint suffix so the gateway can
/// append `/chat/completions` or `/messages` itself.
pub fn normalize_base_url(raw: &str) -> String {
    let mut url = raw.trim().trim_end_matches('/').to_string();
    for suffix in ["/chat/completions", "/completions", "/messages"] {
        if let Some(stripped) = url.strip_suffix(suffix) {
            url = stripped.trim_end_matches('/').to_string();
            break;
        }
    }
    if url.is_empty() {
        raw.trim().to_string()
    } else {
        url
    }
}

/// Resolves the protocol of a model: the model's own override, else the
/// provider's protocol.
pub fn model_protocol(
    data: &LlmRuntimeData,
    provider_id: &str,
    model_id: &str,
) -> Option<Protocol> {
    data.models
        .iter()
        .find(|model| model.provider_id == provider_id && model.model_id == model_id)
        .and_then(|model| model.protocol.as_deref())
        .and_then(Protocol::parse)
        .or_else(|| {
            data.providers
                .iter()
                .find(|provider| provider.id == provider_id)
                .and_then(|provider| Protocol::parse(&provider.protocol))
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(protocol: &str, auth: &str, base: Option<&str>, config: &str) -> LlmProviderRecord {
        LlmProviderRecord {
            id: "p1".into(),
            name: "p".into(),
            kind: "openai".into(),
            protocol: protocol.into(),
            auth_type: auth.into(),
            base_url: base.map(str::to_string),
            secret_ref: None,
            priority: 0,
            config_json: config.into(),
            enabled: true,
            created_at: String::new(),
            updated_at: String::new(),
        }
    }

    #[test]
    fn normalizes_base_urls() {
        assert_eq!(
            normalize_base_url("https://api.openai.com/v1/"),
            "https://api.openai.com/v1"
        );
        assert_eq!(
            normalize_base_url("https://openrouter.ai/api/v1/chat/completions"),
            "https://openrouter.ai/api/v1"
        );
        assert_eq!(
            normalize_base_url("http://127.0.0.1:1234"),
            "http://127.0.0.1:1234"
        );
    }

    #[test]
    fn defaults_follow_protocol() {
        let config = ProviderConfig::from_record(&record("openai", "", None, "")).unwrap();
        assert_eq!(config.base_url, "https://api.openai.com/v1");
        assert_eq!(config.auth, AuthType::Bearer);

        let config = ProviderConfig::from_record(&record("anthropic", "", None, "")).unwrap();
        assert_eq!(config.base_url, "https://api.anthropic.com/v1");
        assert_eq!(config.auth, AuthType::AnthropicApiKey);
    }

    #[test]
    fn unknown_protocol_is_rejected() {
        assert!(ProviderConfig::from_record(&record("grpc", "bearer", None, "")).is_err());
    }

    #[test]
    fn config_json_selects_auth_and_wire() {
        let config = ProviderConfig::from_record(&record(
            "openai",
            "bearer",
            Some("http://127.0.0.1:8080"),
            r#"{"auth_header":"x-litellm-key","headers":{"x-extra":"1"},"wire":"anthropic"}"#,
        ))
        .unwrap();
        assert_eq!(config.auth, AuthType::Header);
        assert_eq!(config.auth_header.as_deref(), Some("x-litellm-key"));
        assert_eq!(
            config.headers,
            vec![("x-extra".to_string(), "1".to_string())]
        );
        assert_eq!(config.protocol, Protocol::Anthropic);
    }

    #[test]
    fn url_joins_paths() {
        let config = ProviderConfig::from_record(&record(
            "openai",
            "bearer",
            Some("https://example.test/v1"),
            "",
        ))
        .unwrap();
        assert_eq!(config.url("/models"), "https://example.test/v1/models");
    }

    #[test]
    fn local_detection() {
        assert!(is_local_url("http://127.0.0.1:1234/v1"));
        assert!(is_local_url("HTTP://LOCALHOST:8080"));
        assert!(!is_local_url("https://api.openai.com/v1"));
    }
}
