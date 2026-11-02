//! Host-managed PII detection engines.

mod client;
mod registry;

pub use client::validate_endpoint;
pub use registry::{EngineStatus, PiiEngineRuntime};
