use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use octra_api_plugin_types::{EntityQuery, EntitySpec, MigrationSpec, PluginStorage, StorageError};
use parking_lot::Mutex;
use rusqlite::Connection;
use serde_json::Value;

use crate::entity;
use crate::error::PluginStorageError;
use crate::kv;
use crate::migrate::{self, AppliedMigration, MigrationReport};

const HOST_SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS _octra_plugin_migrations (
    version    TEXT PRIMARY KEY,
    checksum   TEXT NOT NULL,
    applied_at INTEGER NOT NULL
);

CREATE TABLE IF NOT EXISTS _octra_kv (
    key        TEXT PRIMARY KEY,
    value      BLOB NOT NULL,
    expires_at INTEGER
);

CREATE INDEX IF NOT EXISTS idx_octra_kv_expiry ON _octra_kv(expires_at);
"#;

pub struct PluginStorageRoot {
    root: PathBuf,
    max_db_bytes: Option<u64>,
}

impl PluginStorageRoot {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self {
            root: root.into(),
            max_db_bytes: None,
        }
    }

    /// Caps every plugin database; zero leaves the databases unbounded.
    pub fn with_max_bytes(mut self, max_bytes: u64) -> Self {
        self.max_db_bytes = (max_bytes > 0).then_some(max_bytes);
        self
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn plugin_dir(&self, name: &str) -> Result<PathBuf, PluginStorageError> {
        validate_plugin_name(name)?;
        Ok(self.root.join(name))
    }

    pub fn open(
        &self,
        name: &str,
        entities: Vec<EntitySpec>,
    ) -> Result<PluginStore, PluginStorageError> {
        let directory = self.plugin_dir(name)?;
        std::fs::create_dir_all(&directory)?;
        PluginStore::open(name, directory.join("data.db"), entities, self.max_db_bytes)
    }
}

pub struct PluginStore {
    name: String,
    db_path: PathBuf,
    connection: Mutex<Connection>,
    entities: BTreeMap<String, EntitySpec>,
}

impl PluginStore {
    pub fn open(
        name: &str,
        db_path: impl Into<PathBuf>,
        entities: Vec<EntitySpec>,
        max_db_bytes: Option<u64>,
    ) -> Result<Self, PluginStorageError> {
        validate_plugin_name(name)?;
        let db_path = db_path.into();
        if let Some(parent) = db_path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let connection = Connection::open(&db_path)?;
        configure(&connection, true)?;
        if let Some(max_bytes) = max_db_bytes {
            apply_quota(&connection, max_bytes)?;
        }
        connection.execute_batch(HOST_SCHEMA)?;
        let entities = index_entities(entities)?;
        Ok(Self {
            name: name.to_string(),
            db_path,
            connection: Mutex::new(connection),
            entities,
        })
    }

    pub fn open_in_memory(
        name: &str,
        entities: Vec<EntitySpec>,
    ) -> Result<Self, PluginStorageError> {
        validate_plugin_name(name)?;
        let connection = Connection::open_in_memory()?;
        configure(&connection, false)?;
        connection.execute_batch(HOST_SCHEMA)?;
        let entities = index_entities(entities)?;
        Ok(Self {
            name: name.to_string(),
            db_path: PathBuf::new(),
            connection: Mutex::new(connection),
            entities,
        })
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn db_path(&self) -> &Path {
        &self.db_path
    }

    pub fn entity_specs(&self) -> impl Iterator<Item = &EntitySpec> {
        self.entities.values()
    }

    pub fn size_bytes(&self) -> Result<u64, PluginStorageError> {
        let connection = self.connection.lock();
        let pages: i64 = connection.query_row("PRAGMA page_count", [], |row| row.get(0))?;
        let page_size: i64 = connection.query_row("PRAGMA page_size", [], |row| row.get(0))?;
        Ok(u64::try_from(pages.max(0)).unwrap_or(0) * u64::try_from(page_size.max(0)).unwrap_or(0))
    }

    pub fn apply_migrations(
        &self,
        artifact_dir: &Path,
        migrations: &[MigrationSpec],
        allow_destructive: bool,
    ) -> Result<MigrationReport, PluginStorageError> {
        let mut connection = self.connection.lock();
        let report = migrate::apply(&mut connection, artifact_dir, migrations, allow_destructive)?;
        migrate::verify_entities(&connection, self.entities.values())?;
        Ok(report)
    }

    pub fn ledger(&self) -> Result<Vec<AppliedMigration>, PluginStorageError> {
        let connection = self.connection.lock();
        migrate::ledger(&connection)
    }

    fn spec(&self, entity: &str) -> Result<&EntitySpec, StorageError> {
        self.entities
            .get(entity)
            .ok_or_else(|| StorageError::NotDeclared(entity.to_string()))
    }
}

impl PluginStorage for PluginStore {
    fn kv_get(&self, key: &str) -> Result<Option<Vec<u8>>, StorageError> {
        let connection = self.connection.lock();
        kv::get(&connection, key).map_err(Into::into)
    }

    fn kv_put(&self, key: &str, value: &[u8], ttl: Option<Duration>) -> Result<(), StorageError> {
        let connection = self.connection.lock();
        kv::put(&connection, key, value, ttl).map_err(Into::into)
    }

    fn kv_delete(&self, key: &str) -> Result<bool, StorageError> {
        let connection = self.connection.lock();
        kv::delete(&connection, key).map_err(Into::into)
    }

    fn kv_incr(&self, key: &str, delta: i64, ttl: Option<Duration>) -> Result<i64, StorageError> {
        let connection = self.connection.lock();
        kv::incr(&connection, key, delta, ttl).map_err(Into::into)
    }

    fn get(&self, entity: &str, key: &Value) -> Result<Option<Value>, StorageError> {
        let spec = self.spec(entity)?;
        let connection = self.connection.lock();
        entity::get(&connection, spec, key).map_err(Into::into)
    }

    fn upsert(&self, entity: &str, values: &Value) -> Result<(), StorageError> {
        let spec = self.spec(entity)?;
        let connection = self.connection.lock();
        entity::upsert(&connection, spec, values).map_err(Into::into)
    }

    fn increment(
        &self,
        entity: &str,
        key: &Value,
        field: &str,
        delta: i64,
        extra: Option<&Value>,
    ) -> Result<i64, StorageError> {
        let spec = self.spec(entity)?;
        let connection = self.connection.lock();
        entity::increment(&connection, spec, key, field, delta, extra).map_err(Into::into)
    }

    fn delete(&self, entity: &str, key: &Value) -> Result<bool, StorageError> {
        let spec = self.spec(entity)?;
        let connection = self.connection.lock();
        entity::delete(&connection, spec, key).map_err(Into::into)
    }

    fn query(&self, entity: &str, query: &EntityQuery) -> Result<Vec<Value>, StorageError> {
        let spec = self.spec(entity)?;
        let connection = self.connection.lock();
        entity::query(&connection, spec, query).map_err(Into::into)
    }
}

/// Lets SQLite itself reject writes past the quota with `SQLITE_FULL`.
fn apply_quota(connection: &Connection, max_bytes: u64) -> Result<(), PluginStorageError> {
    let page_size: i64 = connection.query_row("PRAGMA page_size", [], |row| row.get(0))?;
    let page_size = u64::try_from(page_size).unwrap_or(0).max(1);
    let max_pages = (max_bytes / page_size).max(1);
    let max_pages = i64::try_from(max_pages).unwrap_or(i64::MAX);
    connection.pragma_update(None, "max_page_count", max_pages)?;
    Ok(())
}

fn configure(connection: &Connection, file_backed: bool) -> Result<(), PluginStorageError> {
    connection.busy_timeout(Duration::from_secs(5))?;
    connection.pragma_update(None, "foreign_keys", "ON")?;
    if file_backed {
        connection.pragma_update(None, "journal_mode", "WAL")?;
    }
    Ok(())
}

fn index_entities(
    entities: Vec<EntitySpec>,
) -> Result<BTreeMap<String, EntitySpec>, PluginStorageError> {
    let mut indexed = BTreeMap::new();
    for spec in entities {
        entity::validate_spec(&spec)?;
        let name = spec.name.clone();
        if indexed.insert(name.clone(), spec).is_some() {
            return Err(PluginStorageError::InvalidManifest(format!(
                "entity '{name}' is declared twice"
            )));
        }
    }
    Ok(indexed)
}

pub(crate) fn validate_plugin_name(name: &str) -> Result<(), PluginStorageError> {
    let mut chars = name.chars();
    let Some(first) = chars.next() else {
        return Err(PluginStorageError::InvalidPluginName(name.to_string()));
    };
    let valid = name.len() <= 64
        && (first.is_ascii_lowercase() || first.is_ascii_digit())
        && chars.all(|character| {
            character.is_ascii_lowercase()
                || character.is_ascii_digit()
                || character == '-'
                || character == '_'
        });
    if valid {
        Ok(())
    } else {
        Err(PluginStorageError::InvalidPluginName(name.to_string()))
    }
}

pub(crate) fn validate_version(version: &str) -> Result<(), PluginStorageError> {
    let mut chars = version.chars();
    let Some(first) = chars.next() else {
        return Err(PluginStorageError::InvalidVersion(version.to_string()));
    };
    let valid = version.len() <= 64
        && !version.contains("..")
        && (first.is_ascii_alphanumeric())
        && chars.all(|character| {
            character.is_ascii_alphanumeric() || matches!(character, '.' | '_' | '+' | '-')
        });
    if valid {
        Ok(())
    } else {
        Err(PluginStorageError::InvalidVersion(version.to_string()))
    }
}
