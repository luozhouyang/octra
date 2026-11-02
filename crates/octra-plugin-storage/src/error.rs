use octra_api_plugin_types::StorageError;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum PluginStorageError {
    #[error("invalid plugin name: {0}")]
    InvalidPluginName(String),
    #[error("invalid artifact version: {0}")]
    InvalidVersion(String),
    #[error("filesystem error: {0}")]
    Io(#[from] std::io::Error),
    #[error("sqlite error: {0}")]
    Sqlite(#[from] rusqlite::Error),
    #[error("invalid manifest: {0}")]
    InvalidManifest(String),
    #[error("migration '{version}' checksum mismatch")]
    ChecksumMismatch { version: String },
    #[error("migration '{version}' is destructive; set allow_destructive")]
    DestructiveMigration { version: String },
    #[error("migration '{version}' touches reserved host tables")]
    ReservedTable { version: String },
    #[error("migration '{version}' failed: {source}")]
    MigrationFailed {
        version: String,
        source: rusqlite::Error,
    },
    #[error("entity '{entity}' is not declared")]
    EntityNotDeclared { entity: String },
    #[error("unknown field '{field}' on entity '{entity}'")]
    UnknownField { entity: String, field: String },
    #[error("entity '{entity}' is missing declared fields: {fields:?}")]
    EntityMismatch { entity: String, fields: Vec<String> },
    #[error("entity '{entity}' primary key mismatch: {fields:?}")]
    PrimaryKeyMismatch { entity: String, fields: Vec<String> },
    #[error("invalid storage request: {0}")]
    InvalidRequest(String),
}

impl From<PluginStorageError> for StorageError {
    fn from(error: PluginStorageError) -> Self {
        match error {
            PluginStorageError::EntityNotDeclared { entity } => StorageError::NotDeclared(entity),
            PluginStorageError::UnknownField { entity, field } => StorageError::InvalidRequest(
                format!("unknown field '{field}' on entity '{entity}'"),
            ),
            PluginStorageError::InvalidRequest(message) => StorageError::InvalidRequest(message),
            other => StorageError::Backend(other.to_string()),
        }
    }
}
