//! Provider configuration parsing: protocols, auth, secret refs, base URLs.

use octra_llm_core::Protocol;
use octra_storage::{LlmProviderRecord, LlmRuntimeData};
use serde_json::{Map, Value};

use crate::cli::CliConfig;

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
            Protocol::Openai | Protocol::OpenaiResponses => AuthType::Bearer,
            Protocol::Anthropic => AuthType::AnthropicApiKey,
            Protocol::Gemini => AuthType::Header,
        }
    }
}

pub const ANTHROPIC_VERSION: &str = "2023-06-01";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OAuthTokenAuth {
    Form,
    Basic,
}

/// OAuth 2.0 settings for an account credential. Endpoints are configuration,
/// not hardcoded, because subscription providers do not publish stable APIs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OAuthProfile {
    pub authorize_url: String,
    pub token_url: String,
    pub client_id: String,
    pub client_secret_ref: Option<String>,
    pub scopes: Vec<String>,
    pub redirect_uri: Option<String>,
    pub token_auth: OAuthTokenAuth,
    pub extra_authorize_params: Vec<(String, String)>,
    pub extra_token_params: Vec<(String, String)>,
}

impl OAuthProfile {
    pub fn from_value(value: &Value) -> Result<Self, String> {
        let object = value
            .as_object()
            .ok_or_else(|| "`oauth` must be an object".to_string())?;
        let string = |key: &str| {
            object
                .get(key)
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .map(str::to_string)
        };
        let authorize_url = string("authorize_url")
            .ok_or_else(|| "`oauth.authorize_url` is required".to_string())?;
        let token_url =
            string("token_url").ok_or_else(|| "`oauth.token_url` is required".to_string())?;
        for (name, url) in [("authorize_url", &authorize_url), ("token_url", &token_url)] {
            let parsed = reqwest::Url::parse(url)
                .map_err(|error| format!("invalid oauth.{name}: {error}"))?;
            if !matches!(parsed.scheme(), "http" | "https") {
                return Err(format!("oauth.{name} must be http(s)"));
            }
        }
        let client_id =
            string("client_id").ok_or_else(|| "`oauth.client_id` is required".to_string())?;
        let scopes = object
            .get("scopes")
            .and_then(Value::as_array)
            .map(|items| {
                items
                    .iter()
                    .filter_map(Value::as_str)
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_default();
        let token_auth = match string("token_auth").as_deref() {
            None | Some("form") => OAuthTokenAuth::Form,
            Some("basic") => OAuthTokenAuth::Basic,
            Some(other) => {
                return Err(format!(
                    "oauth.token_auth must be form or basic, got '{other}'"
                ));
            }
        };
        let params = |key: &str| -> Vec<(String, String)> {
            object
                .get(key)
                .and_then(Value::as_object)
                .map(|map| {
                    map.iter()
                        .filter_map(|(key, value)| {
                            value.as_str().map(|value| (key.clone(), value.to_string()))
                        })
                        .collect()
                })
                .unwrap_or_default()
        };
        Ok(Self {
            authorize_url,
            token_url,
            client_id,
            client_secret_ref: string("client_secret_ref"),
            scopes,
            redirect_uri: string("redirect_uri"),
            token_auth,
            extra_authorize_params: params("extra_authorize_params"),
            extra_token_params: params("extra_token_params"),
        })
    }
}

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
    /// Query parameters from `config_json.query`, appended to every upstream
    /// endpoint URL (for example Azure OpenAI `api-version`).
    pub query: Vec<(String, String)>,
    pub anthropic_version: String,
    pub oauth: Option<OAuthProfile>,
    /// Set when the provider is reached by spawning a local CLI instead of
    /// making an HTTP request.
    pub cli: Option<CliConfig>,
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
        // with the Anthropic wire format (and vice versa). Provider names may
        // also be presets such as `ollama`, `groq`, or `claude`.
        let requested_protocol = match config.get("wire").and_then(Value::as_str) {
            Some(wire) => wire,
            None => record.protocol.as_str(),
        };
        let (protocol_name, preset_base_url) =
            protocol_alias(requested_protocol).ok_or_else(|| {
                ProviderConfigError::Invalid(format!(
                    "provider {}: unknown protocol '{}'",
                    record.name, requested_protocol
                ))
            })?;
        let protocol = Protocol::parse(protocol_name).ok_or_else(|| {
            ProviderConfigError::Invalid(format!(
                "provider {}: unknown protocol '{}'",
                record.name, requested_protocol
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
        if auth == AuthType::Header && auth_header.is_none() && protocol == Protocol::Gemini {
            auth_header = Some("x-goog-api-key".to_string());
        }

        let base_url = match record.base_url.as_deref().map(str::trim) {
            Some(url) if !url.is_empty() => normalize_base_url(url),
            _ => preset_base_url
                .map(str::to_string)
                .unwrap_or_else(|| default_base_url(protocol).to_string()),
        };

        let oauth = match config.get("oauth") {
            Some(value) => Some(OAuthProfile::from_value(value).map_err(|message| {
                ProviderConfigError::Invalid(format!("provider {}: {message}", record.name))
            })?),
            None => None,
        };

        let cli = match config.get("cli") {
            Some(value) => Some(CliConfig::from_value(value).map_err(|message| {
                ProviderConfigError::Invalid(format!("provider {}: {message}", record.name))
            })?),
            None => None,
        };

        let mut headers = Vec::new();
        if let Some(map) = config.get("headers").and_then(Value::as_object) {
            for (name, value) in map {
                if let Some(value) = value.as_str() {
                    headers.push((name.clone(), value.to_string()));
                }
            }
        }

        let query = parse_query(&config, &record.name)?;

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
            query,
            anthropic_version: config
                .get("anthropic_version")
                .and_then(Value::as_str)
                .unwrap_or(ANTHROPIC_VERSION)
                .to_string(),
            oauth,
            cli,
        })
    }

    pub fn requires_secret(&self) -> bool {
        self.cli.is_none() && self.auth != AuthType::None
    }

    pub fn url(&self, path: &str) -> String {
        format!("{}{}", self.base_url, path)
    }
}

/// Parses `config_json.query` into ordered name/value pairs. The map keeps
/// whatever order the underlying JSON object stores; values must be strings
/// so a typo cannot silently drop an `api-version`.
fn parse_query(
    config: &Map<String, Value>,
    provider_name: &str,
) -> Result<Vec<(String, String)>, ProviderConfigError> {
    let Some(value) = config.get("query") else {
        return Ok(Vec::new());
    };
    let object = value.as_object().ok_or_else(|| {
        ProviderConfigError::Invalid(format!(
            "provider {provider_name}: config_json.query must be an object"
        ))
    })?;
    let mut query = Vec::with_capacity(object.len());
    for (key, value) in object {
        let key = key.trim();
        if key.is_empty() {
            return Err(ProviderConfigError::Invalid(format!(
                "provider {provider_name}: config_json.query keys must not be empty"
            )));
        }
        let value = value.as_str().ok_or_else(|| {
            ProviderConfigError::Invalid(format!(
                "provider {provider_name}: config_json.query.{key} must be a string"
            ))
        })?;
        query.push((key.to_string(), value.to_string()));
    }
    Ok(query)
}

pub fn default_base_url(protocol: Protocol) -> &'static str {
    match protocol {
        Protocol::Openai | Protocol::OpenaiResponses => "https://api.openai.com/v1",
        Protocol::Anthropic => "https://api.anthropic.com/v1",
        Protocol::Gemini => "https://generativelanguage.googleapis.com/v1beta",
    }
}

/// Maps provider presets and aliases onto the four wire protocols.
///
/// The optional second item is the default base URL for a provider whose
/// record does not set one; aliases without a preset use the protocol default.
fn protocol_alias(name: &str) -> Option<(&'static str, Option<&'static str>)> {
    let lower = name.trim().to_ascii_lowercase().replace('_', "-");
    match lower.as_str() {
        "openai" | "openai-chat" | "openai-compatible" => Some(("openai", None)),
        "openai-responses" | "responses" => Some(("openai_responses", None)),
        "anthropic" | "claude" => Some(("anthropic", None)),
        "gemini" | "google" => Some(("gemini", None)),
        "ollama" => Some(("openai", Some("http://127.0.0.1:11434/v1"))),
        "vllm" => Some(("openai", Some("http://127.0.0.1:8000/v1"))),
        "lmstudio" | "lm-studio" => Some(("openai", Some("http://127.0.0.1:1234/v1"))),
        "groq" => Some(("openai", Some("https://api.groq.com/openai/v1"))),
        "openrouter" => Some(("openai", Some("https://openrouter.ai/api/v1"))),
        "deepseek" => Some(("openai", Some("https://api.deepseek.com/v1"))),
        "together" => Some(("openai", Some("https://api.together.xyz/v1"))),
        "fireworks" => Some(("openai", Some("https://api.fireworks.ai/inference/v1"))),
        _ => None,
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
    fn cli_transport_needs_no_secret() {
        let config = ProviderConfig::from_record(&record(
            "openai",
            "bearer",
            None,
            r#"{"cli":{"kind":"claude_code"}}"#,
        ))
        .unwrap();
        let cli = config.cli.as_ref().expect("cli config is parsed");
        assert_eq!(cli.command, "claude");
        assert!(!config.requires_secret());

        let broken = ProviderConfig::from_record(&record(
            "openai",
            "bearer",
            None,
            r#"{"cli":{"kind":"unknown"}}"#,
        ));
        assert!(broken.is_err());
    }

    #[test]
    fn parses_provider_query_parameters() {
        let config = ProviderConfig::from_record(&record(
            "openai",
            "bearer",
            Some("https://example.test/v1"),
            r#"{"query":{"api-version":"2024-10-21","x-extra":"1"}}"#,
        ))
        .unwrap();
        assert_eq!(
            config.query,
            vec![
                ("api-version".to_string(), "2024-10-21".to_string()),
                ("x-extra".to_string(), "1".to_string()),
            ]
        );

        let no_query =
            ProviderConfig::from_record(&record("openai", "bearer", None, "{}")).unwrap();
        assert!(no_query.query.is_empty());

        let empty_query =
            ProviderConfig::from_record(&record("openai", "bearer", None, r#"{"query":{}}"#))
                .unwrap();
        assert!(empty_query.query.is_empty());
    }

    #[test]
    fn invalid_query_parameters_are_rejected() {
        for config in [
            r#"{"query":"api-version=2024-10-21"}"#,
            r#"{"query":["api-version"]}"#,
            r#"{"query":null}"#,
            r#"{"query":{"api-version":20241021}}"#,
            r#"{"query":{"api-version":true}}"#,
            r#"{"query":{"api-version":null}}"#,
            r#"{"query":{"   ":"value"}}"#,
        ] {
            let error =
                ProviderConfig::from_record(&record("openai", "bearer", None, config)).unwrap_err();
            let ProviderConfigError::Invalid(message) = error;
            assert!(
                message.contains("config_json.query"),
                "unexpected message: {message}"
            );
        }
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
    fn provider_presets_map_to_openai_and_default_base_urls() {
        for (protocol, expected) in [
            ("ollama", "http://127.0.0.1:11434/v1"),
            ("vllm", "http://127.0.0.1:8000/v1"),
            ("lmstudio", "http://127.0.0.1:1234/v1"),
            ("groq", "https://api.groq.com/openai/v1"),
            ("openrouter", "https://openrouter.ai/api/v1"),
            ("deepseek", "https://api.deepseek.com/v1"),
            ("together", "https://api.together.xyz/v1"),
            ("fireworks", "https://api.fireworks.ai/inference/v1"),
        ] {
            let config =
                ProviderConfig::from_record(&record(protocol, "bearer", None, "")).unwrap();
            assert_eq!(config.protocol, Protocol::Openai, "protocol {protocol}");
            assert_eq!(config.base_url, expected, "protocol {protocol}");
        }

        let claude = ProviderConfig::from_record(&record("claude", "", None, "")).unwrap();
        assert_eq!(claude.protocol, Protocol::Anthropic);
        let responses =
            ProviderConfig::from_record(&record("responses", "bearer", None, "")).unwrap();
        assert_eq!(responses.protocol, Protocol::OpenaiResponses);
    }

    #[test]
    fn wire_aliases_override_the_recorded_protocol() {
        let config = ProviderConfig::from_record(&record(
            "ollama",
            "bearer",
            Some("http://127.0.0.1:9000/v1"),
            r#"{"wire":"openai-compatible"}"#,
        ))
        .unwrap();
        assert_eq!(config.protocol, Protocol::Openai);
        assert_eq!(config.base_url, "http://127.0.0.1:9000/v1");

        let error = ProviderConfig::from_record(&record(
            "openai",
            "bearer",
            None,
            r#"{"wire":"not-a-wire"}"#,
        ))
        .unwrap_err();
        let ProviderConfigError::Invalid(message) = error;
        assert!(
            message.contains("not-a-wire"),
            "unexpected message: {message}"
        );
    }

    #[test]
    fn local_detection() {
        assert!(is_local_url("http://127.0.0.1:1234/v1"));
        assert!(is_local_url("HTTP://LOCALHOST:8080"));
        assert!(!is_local_url("https://api.openai.com/v1"));
    }
}
