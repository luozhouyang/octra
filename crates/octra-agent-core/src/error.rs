//! Errors raised by agent discovery, rendering and file operations.

#[derive(Debug, thiserror::Error)]
pub enum AgentError {
    #[error("unknown agent '{0}'")]
    UnknownAgent(String),

    #[error("agent '{0}' has no profile")]
    MissingProfile(String),

    #[error("invalid configuration: {0}")]
    InvalidConfig(String),

    #[error("configuration conflict: {0}")]
    Conflict(String),

    #[error(transparent)]
    Io(#[from] std::io::Error),

    #[error(transparent)]
    Storage(#[from] octra_core::Error),
}

pub type AgentResult<T> = Result<T, AgentError>;
