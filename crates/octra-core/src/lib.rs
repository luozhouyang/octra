//! Shared domain types and errors used across Octra crates.

pub mod error;
pub mod types;

pub use error::{Error, Result};
pub use types::{RouteSummary, ServiceSummary, SnapshotCounts, SnapshotInfo};

pub const VERSION: &str = env!("CARGO_PKG_VERSION");
