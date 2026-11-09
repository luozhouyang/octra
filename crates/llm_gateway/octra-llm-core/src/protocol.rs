use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Protocol {
    Openai,
    OpenaiResponses,
    Anthropic,
    Gemini,
    Vertex,
    Bedrock,
}

impl Protocol {
    pub fn as_str(&self) -> &'static str {
        match self {
            Protocol::Openai => "openai",
            Protocol::OpenaiResponses => "openai_responses",
            Protocol::Anthropic => "anthropic",
            Protocol::Gemini => "gemini",
            Protocol::Vertex => "vertex",
            Protocol::Bedrock => "bedrock",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "openai" | "openai_chat" | "chat_completions" => Some(Protocol::Openai),
            "openai_responses" | "responses" => Some(Protocol::OpenaiResponses),
            "anthropic" | "messages" | "anthropic_messages" => Some(Protocol::Anthropic),
            "gemini" | "google" | "generate_content" => Some(Protocol::Gemini),
            "vertex" | "google-vertex" | "vertex-ai" | "vertex_ai" => Some(Protocol::Vertex),
            "bedrock" | "aws-bedrock" | "bedrock-runtime" | "bedrock_converse" => {
                Some(Protocol::Bedrock)
            }
            _ => None,
        }
    }
}

impl std::fmt::Display for Protocol {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorKind {
    /// The client request could not be understood or fails validation.
    InvalidRequest,
    /// The virtual key is missing, unknown, disabled or expired.
    Unauthorized,
    /// The authenticated consumer is not allowed to use this model or provider.
    Forbidden,
    /// The requested virtual model has no enabled route.
    NotFound,
    /// A gateway-owned limit (body size) was exceeded.
    PayloadTooLarge,
    /// A gateway-owned rate limit was exceeded.
    RateLimited,
    /// The upstream provider rejected or failed the request.
    Upstream,
    /// Configuration is wrong: unresolved secret, invalid base URL.
    Config,
    /// The upstream took too long.
    Timeout,
    /// Anything else.
    Internal,
}

impl ErrorKind {
    /// Errors caused by the upstream or by local configuration are worth
    /// trying the next routing target for; client errors are not.
    pub fn is_retryable(&self) -> bool {
        matches!(
            self,
            ErrorKind::Upstream | ErrorKind::Config | ErrorKind::Timeout
        )
    }
}

/// A protocol-neutral error. Every adapter renders this in its own error
/// shape (OpenAI error envelope or Anthropic error envelope).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[allow(clippy::result_large_err)]
pub struct GatewayError {
    pub status: u16,
    pub kind: ErrorKind,
    pub code: Option<String>,
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub upstream_status: Option<u16>,
    /// Raw upstream error body, if it was JSON.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub upstream_body: Option<Value>,
    #[serde(default, skip_serializing_if = "Map::is_empty")]
    pub extra: Map<String, Value>,
}

impl GatewayError {
    pub fn new(status: u16, kind: ErrorKind, message: impl Into<String>) -> Self {
        Self {
            status,
            kind,
            code: None,
            message: message.into(),
            provider: None,
            upstream_status: None,
            upstream_body: None,
            extra: Map::new(),
        }
    }

    pub fn invalid_request(message: impl Into<String>) -> Self {
        Self::new(400, ErrorKind::InvalidRequest, message)
    }

    pub fn unauthorized(message: impl Into<String>) -> Self {
        Self::new(401, ErrorKind::Unauthorized, message)
    }

    pub fn forbidden(message: impl Into<String>) -> Self {
        Self::new(403, ErrorKind::Forbidden, message)
    }

    pub fn not_found(message: impl Into<String>) -> Self {
        Self::new(404, ErrorKind::NotFound, message)
    }

    pub fn internal(message: impl Into<String>) -> Self {
        Self::new(500, ErrorKind::Internal, message)
    }

    pub fn upstream(status: u16, message: impl Into<String>) -> Self {
        Self::new(status, ErrorKind::Upstream, message)
    }

    /// Upstream configuration problems (unresolved secret, bad base URL)
    /// surface as a 502: the client request is fine, the gateway is not.
    pub fn config(message: impl Into<String>) -> Self {
        Self::new(502, ErrorKind::Config, message)
    }

    /// Bad credentials for the upstream provider.
    pub fn upstream_unauthorized(message: impl Into<String>) -> Self {
        Self::new(401, ErrorKind::Upstream, message)
    }

    pub fn timeout(message: impl Into<String>) -> Self {
        Self::new(504, ErrorKind::Timeout, message)
    }

    pub fn with_code(mut self, code: impl Into<String>) -> Self {
        self.code = Some(code.into());
        self
    }

    pub fn with_provider(mut self, provider: impl Into<String>) -> Self {
        self.provider = Some(provider.into());
        self
    }

    pub fn with_upstream(mut self, status: u16, body: Option<Value>) -> Self {
        self.upstream_status = Some(status);
        self.upstream_body = body;
        self
    }
}

impl std::fmt::Display for GatewayError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{} ({}): {}",
            self.kind_code(),
            self.status,
            self.message
        )
    }
}

impl std::error::Error for GatewayError {}

impl GatewayError {
    pub fn kind_code(&self) -> &'static str {
        match self.kind {
            ErrorKind::InvalidRequest => "invalid_request",
            ErrorKind::Unauthorized => "unauthorized",
            ErrorKind::Forbidden => "forbidden",
            ErrorKind::NotFound => "not_found",
            ErrorKind::PayloadTooLarge => "payload_too_large",
            ErrorKind::RateLimited => "rate_limited",
            ErrorKind::Upstream => "upstream",
            ErrorKind::Config => "config",
            ErrorKind::Timeout => "timeout",
            ErrorKind::Internal => "internal",
        }
    }
}

/// Adapter-level failure: a payload that could not be parsed or serialized.
#[derive(Debug, Clone, PartialEq)]
pub struct ProtocolError {
    pub message: String,
}

impl ProtocolError {
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }

    pub fn into_gateway(self) -> GatewayError {
        GatewayError::invalid_request(format!("invalid request: {}", self.message))
    }
}

impl std::fmt::Display for ProtocolError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for ProtocolError {}

impl From<serde_json::Error> for ProtocolError {
    fn from(err: serde_json::Error) -> Self {
        ProtocolError::new(err.to_string())
    }
}

pub type ProtocolResult<T> = std::result::Result<T, ProtocolError>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn retryable_kinds() {
        assert!(ErrorKind::Upstream.is_retryable());
        assert!(ErrorKind::Timeout.is_retryable());
        assert!(ErrorKind::Config.is_retryable());
        assert!(!ErrorKind::InvalidRequest.is_retryable());
        assert!(!ErrorKind::Unauthorized.is_retryable());
        assert!(!ErrorKind::NotFound.is_retryable());
    }

    #[test]
    fn protocol_parse_accepts_aliases() {
        assert_eq!(Protocol::parse("OpenAI"), Some(Protocol::Openai));
        assert_eq!(Protocol::parse("chat_completions"), Some(Protocol::Openai));
        assert_eq!(Protocol::parse("anthropic"), Some(Protocol::Anthropic));
        assert_eq!(Protocol::parse("messages"), Some(Protocol::Anthropic));
        assert_eq!(Protocol::parse("vertex"), Some(Protocol::Vertex));
        assert_eq!(Protocol::parse("google-vertex"), Some(Protocol::Vertex));
        assert_eq!(Protocol::parse("vertex-ai"), Some(Protocol::Vertex));
        assert_eq!(Protocol::parse("vertex_ai"), Some(Protocol::Vertex));
        assert_eq!(Protocol::parse("bedrock"), Some(Protocol::Bedrock));
        assert_eq!(Protocol::parse("aws-bedrock"), Some(Protocol::Bedrock));
        assert_eq!(Protocol::parse("bedrock-runtime"), Some(Protocol::Bedrock));
        assert_eq!(Protocol::parse("bedrock_converse"), Some(Protocol::Bedrock));
        assert_eq!(Protocol::parse("grpc"), None);
        assert_eq!(Protocol::Anthropic.to_string(), "anthropic");
        assert_eq!(Protocol::Vertex.to_string(), "vertex");
        assert_eq!(Protocol::Bedrock.to_string(), "bedrock");
    }

    #[test]
    fn protocol_error_converts_to_invalid_request() {
        let err = ProtocolError::new("missing model").into_gateway();
        assert_eq!(err.status, 400);
        assert_eq!(err.kind, ErrorKind::InvalidRequest);
        assert!(err.message.contains("missing model"));
    }
}
