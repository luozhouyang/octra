//! Host-managed PII detection engines.

pub mod catalog;
mod client;
mod local;
mod models;
mod registry;

pub use catalog::{MODELS, ModelSpec};
pub use client::validate_endpoint;
pub use local::LocalPiiClient;
pub use models::{DownloadState, LocalModelManager, ModelStatus};
pub use registry::{EngineStatus, PiiEngineRuntime};
