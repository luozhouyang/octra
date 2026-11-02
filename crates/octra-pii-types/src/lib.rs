//! PII detection contracts shared by the redaction plugin and the engine host.
//!
//! Rule detectors run in-process and are synchronous. Model-backed detectors
//! live behind a sidecar and implement [`PiiDetectorClient`]; the host owns
//! the process lifecycle and resolves clients by configured engine name.

use std::fmt;
use std::sync::Arc;

use async_trait::async_trait;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PiiKind {
    Email,
    Phone,
    IdCard,
    BankCard,
    ApiKey,
    Ip,
    Custom(String),
}

impl PiiKind {
    pub fn label(&self) -> &str {
        match self {
            Self::Email => "email",
            Self::Phone => "phone",
            Self::IdCard => "id_card",
            Self::BankCard => "bank_card",
            Self::ApiKey => "api_key",
            Self::Ip => "ip",
            Self::Custom(name) => name,
        }
    }
}

#[derive(Debug, Clone)]
pub struct Detection {
    pub kind: PiiKind,
    pub start: usize,
    pub end: usize,
    pub confidence: f32,
    pub detector: &'static str,
}

/// In-process detector used by the redaction plugin for rule matches.
pub trait PiiDetector: Send + Sync {
    fn detect(&self, text: &str) -> Vec<Detection>;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EngineReadiness {
    Ready,
    Starting,
    Degraded,
    Failed,
}

#[derive(Debug, Clone, Copy)]
pub struct DetectRequest<'a> {
    pub text: &'a str,
    pub locale: Option<&'a str>,
    /// Empty means every kind the engine supports.
    pub kinds: &'a [PiiKind],
    pub max_bytes: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PiiEngineErrorKind {
    Unavailable,
    Timeout,
    Protocol,
    Rejected,
}

#[derive(Debug, Clone)]
pub struct PiiEngineError {
    pub kind: PiiEngineErrorKind,
    pub message: String,
}

impl PiiEngineError {
    pub fn new(kind: PiiEngineErrorKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
        }
    }
}

impl fmt::Display for PiiEngineError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for PiiEngineError {}

/// Client for one configured PII engine. Implementations are built by the
/// host; plugins never own the underlying process or socket.
#[async_trait]
pub trait PiiDetectorClient: Send + Sync {
    fn readiness(&self) -> EngineReadiness;

    async fn detect(&self, request: DetectRequest<'_>) -> Result<Vec<Detection>, PiiEngineError>;
}

/// Resolves engine names from plugin configuration to host-managed clients.
pub trait PiiEngineResolver: Send + Sync {
    fn resolve(&self, name: &str) -> Option<Arc<dyn PiiDetectorClient>>;
}
