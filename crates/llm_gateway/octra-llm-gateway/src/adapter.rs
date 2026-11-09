//! Provider adapters: endpoint shape and auth headers per wire protocol.
//!
//! Built-in adapters cover OpenAI Chat, OpenAI Responses, Anthropic Messages,
//! Gemini, Vertex and AWS Bedrock. A future Provider Plugin implements the
//! same trait and registers itself without gateway changes.

use http::{HeaderName, HeaderValue, header};

use octra_llm_core::{GatewayError, Protocol};

use crate::config::{AuthType, ProviderConfig};

pub trait ProviderAdapter: Send + Sync {
    fn protocol(&self) -> Protocol;

    fn endpoint(
        &self,
        provider: &ProviderConfig,
        model: &str,
        stream: bool,
    ) -> Result<String, GatewayError>;

    /// Headers every request needs regardless of auth (API versions).
    fn extra_headers(&self, _provider: &ProviderConfig) -> Vec<(HeaderName, HeaderValue)> {
        Vec::new()
    }

    /// Auth headers for a resolved secret. `None` means the provider does not
    /// require a credential.
    fn auth_headers(
        &self,
        provider: &ProviderConfig,
        secret: Option<&str>,
    ) -> Result<Vec<(HeaderName, HeaderValue)>, GatewayError> {
        let Some(secret) = secret else {
            return Ok(Vec::new());
        };
        let value = HeaderValue::from_str(secret).map_err(|_| {
            GatewayError::config(format!(
                "provider {}: secret contains invalid header characters",
                provider.name
            ))
        })?;
        match provider.auth {
            AuthType::Bearer => {
                let mut value =
                    HeaderValue::from_str(&format!("Bearer {secret}")).map_err(|_| {
                        GatewayError::config(format!(
                            "provider {}: secret contains invalid header characters",
                            provider.name
                        ))
                    })?;
                value.set_sensitive(true);
                Ok(vec![(header::AUTHORIZATION, value)])
            }
            AuthType::AnthropicApiKey => Ok(vec![(HeaderName::from_static("x-api-key"), value)]),
            AuthType::Header => {
                let name = provider.auth_header.as_deref().unwrap_or("x-api-key");
                let name = HeaderName::from_bytes(name.as_bytes()).map_err(|_| {
                    GatewayError::config(format!(
                        "provider {}: invalid auth header name",
                        provider.name
                    ))
                })?;
                Ok(vec![(name, value)])
            }
            AuthType::None => Ok(Vec::new()),
        }
    }
}

struct OpenAiChat;
struct OpenAiResponses;
struct Anthropic;
struct Gemini;
struct Vertex;
struct Bedrock;

impl ProviderAdapter for OpenAiChat {
    fn protocol(&self) -> Protocol {
        Protocol::Openai
    }

    fn endpoint(
        &self,
        provider: &ProviderConfig,
        _model: &str,
        _stream: bool,
    ) -> Result<String, GatewayError> {
        checked_url(provider, "/chat/completions")
    }
}

impl ProviderAdapter for OpenAiResponses {
    fn protocol(&self) -> Protocol {
        Protocol::OpenaiResponses
    }

    fn endpoint(
        &self,
        provider: &ProviderConfig,
        _model: &str,
        _stream: bool,
    ) -> Result<String, GatewayError> {
        checked_url(provider, "/responses")
    }
}

impl ProviderAdapter for Anthropic {
    fn protocol(&self) -> Protocol {
        Protocol::Anthropic
    }

    fn endpoint(
        &self,
        provider: &ProviderConfig,
        _model: &str,
        _stream: bool,
    ) -> Result<String, GatewayError> {
        checked_url(provider, "/messages")
    }

    fn extra_headers(&self, provider: &ProviderConfig) -> Vec<(HeaderName, HeaderValue)> {
        let mut headers = Vec::new();
        if let Ok(value) = HeaderValue::from_str(&provider.anthropic_version) {
            headers.push((HeaderName::from_static("anthropic-version"), value));
        }
        headers
    }
}

impl ProviderAdapter for Gemini {
    fn protocol(&self) -> Protocol {
        Protocol::Gemini
    }

    fn endpoint(
        &self,
        provider: &ProviderConfig,
        model: &str,
        stream: bool,
    ) -> Result<String, GatewayError> {
        let action = if stream {
            ":streamGenerateContent?alt=sse"
        } else {
            ":generateContent"
        };
        checked_url(provider, &format!("/models/{model}{action}"))
    }
}

/// Vertex reuses the Gemini payload shape; only the endpoint and auth differ.
/// Auth defaults to bearer (OAuth access tokens); operators using an API key
/// set `config_json.auth_header = "x-goog-api-key"`.
impl ProviderAdapter for Vertex {
    fn protocol(&self) -> Protocol {
        Protocol::Vertex
    }

    fn endpoint(
        &self,
        provider: &ProviderConfig,
        model: &str,
        stream: bool,
    ) -> Result<String, GatewayError> {
        let action = if stream {
            ":streamGenerateContent?alt=sse"
        } else {
            ":generateContent"
        };
        checked_url(
            provider,
            &format!("/publishers/google/models/{model}{action}"),
        )
    }
}

/// Bedrock Converse keeps the model in the URL, including its colon and dots.
/// Authentication defaults to a bearer Bedrock API key; SigV4 is a later
/// transport concern and does not change these endpoints.
impl ProviderAdapter for Bedrock {
    fn protocol(&self) -> Protocol {
        Protocol::Bedrock
    }

    fn endpoint(
        &self,
        provider: &ProviderConfig,
        model: &str,
        stream: bool,
    ) -> Result<String, GatewayError> {
        let action = if stream {
            "converse-stream"
        } else {
            "converse"
        };
        checked_url(provider, &format!("/model/{model}/{action}"))
    }
}

fn checked_url(provider: &ProviderConfig, path: &str) -> Result<String, GatewayError> {
    let url = provider.url(path);
    if url.starts_with("http://") || url.starts_with("https://") {
        Ok(url)
    } else {
        Err(GatewayError::config(format!(
            "provider {}: base_url must start with http:// or https://",
            provider.name
        )))
    }
}

pub fn for_protocol(protocol: Protocol) -> &'static dyn ProviderAdapter {
    static OPENAI: OpenAiChat = OpenAiChat;
    static RESPONSES: OpenAiResponses = OpenAiResponses;
    static ANTHROPIC: Anthropic = Anthropic;
    static GEMINI: Gemini = Gemini;
    static VERTEX: Vertex = Vertex;
    static BEDROCK: Bedrock = Bedrock;
    match protocol {
        Protocol::Openai => &OPENAI,
        Protocol::OpenaiResponses => &RESPONSES,
        Protocol::Anthropic => &ANTHROPIC,
        Protocol::Gemini => &GEMINI,
        Protocol::Vertex => &VERTEX,
        Protocol::Bedrock => &BEDROCK,
    }
}
