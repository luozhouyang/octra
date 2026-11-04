//! Plugin-owned SQLite storage.
//!
//! The host applies plugin migrations at install/upgrade time and exposes a
//! narrow entity API at runtime. Plugin databases are separate from the core
//! gateway database and are keyed by plugin name.

mod artifact;
mod entity;
mod error;
mod kv;
mod migrate;
mod store;

pub use artifact::{
    ArtifactLayout, copy_artifact, validate_artifact_dir, validate_artifact_dir_for_kind,
};
pub use error::PluginStorageError;
pub use migrate::MigrationReport;
pub use store::{PluginStorageRoot, PluginStore};

pub use octra_api_plugin_types::{
    EntityFilter, EntityOrder, EntityQuery, EntitySpec, FilterOp, MigrationSpec, PluginStorage,
    StorageError,
};
