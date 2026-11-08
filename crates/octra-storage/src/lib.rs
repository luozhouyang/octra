//! SQLite persistence layer. Secrets are stored as references only.

pub mod agents;
pub mod ai;
pub mod api;
pub mod events;
pub mod llm;
pub mod logs;
pub mod maintenance;
pub mod mcp;
pub mod plugins;

pub use agents::{AgentBackup, AgentProfile, AgentStore, NewAgentBackup, NewAgentProfile};
pub use ai::{AiConfigRevisionRecord, AiStore};
pub use events::{EventDeliveryRecord, EventRecord, JobRecord, NewEvent, NewJob};
pub use llm::{
    LlmAccountUsage, LlmModelRecord, LlmOAuthTokenRecord, LlmPiiEngineInput, LlmPiiEngineRecord,
    LlmProviderAccountRecord, LlmProviderRecord, LlmRouteRecord, LlmRoutingGroupRecord,
    LlmRoutingTargetRecord, LlmRuntimeData, LlmStorageError, LlmUsageInput, LlmUsageProviderRow,
    LlmUsageRecord, LlmUsageSummaryRow, LlmVirtualKeyInput, LlmVirtualKeyRecord,
    LlmVirtualKeyRotation, LlmVirtualKeyRotationResult, LlmVirtualKeyUsage, canonical_timestamp,
};
pub use logs::{AuditLog, NewRequestLog, RequestLog};
pub use maintenance::{PruneReport, RetentionPolicy};
pub use plugins::{
    PluginArtifactInput, PluginArtifactRecord, PluginConfigRecord, PluginData, PluginRecord,
};

use std::path::{Path, PathBuf};
use std::time::Duration;

use octra_core::{Error, Result, RouteSummary, ServiceSummary, SnapshotCounts};
use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions};
use sqlx::{Row, SqlitePool};

#[derive(Debug, Clone, Default)]
pub struct RuntimeData {
    pub services: Vec<ServiceSummary>,
    pub routes: Vec<RouteSummary>,
    pub counts: SnapshotCounts,
}

pub struct Storage {
    pool: SqlitePool,
    pub agents: agents::AgentStore,
    pub ai: ai::AiStore,
    pub api: api::ApiStore,
    pub llm: llm::LlmStore,
    pub mcp: mcp::McpStore,
    pub logs: logs::LogStore,
}

impl Storage {
    pub async fn connect(path: &Path) -> Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|err| {
                Error::Storage(format!(
                    "cannot create database directory {}: {err}",
                    parent.display()
                ))
            })?;
        }

        let options = SqliteConnectOptions::new()
            .filename(path)
            .create_if_missing(true)
            .journal_mode(SqliteJournalMode::Wal)
            .foreign_keys(true)
            .busy_timeout(Duration::from_secs(5));

        let storage = Self::from_options(options, 5).await?;
        storage.migrate().await?;
        storage.ensure_usage_rollup().await?;
        restrict_database_permissions(path)?;
        Ok(storage)
    }

    pub async fn connect_in_memory() -> Result<Self> {
        let options = SqliteConnectOptions::new()
            .filename(":memory:")
            .foreign_keys(true);

        let storage = Self::from_options(options, 1).await?;
        storage.migrate().await?;
        storage.ensure_usage_rollup().await?;
        Ok(storage)
    }

    async fn from_options(options: SqliteConnectOptions, max_connections: u32) -> Result<Self> {
        let pool = SqlitePoolOptions::new()
            .max_connections(max_connections)
            .connect_with(options)
            .await
            .map_err(|err| Error::Storage(format!("cannot open sqlite: {err}")))?;

        Ok(Self {
            agents: agents::AgentStore::new(pool.clone()),
            ai: ai::AiStore::new(pool.clone()),
            api: api::ApiStore::new(pool.clone()),
            llm: llm::LlmStore::new(pool.clone()),
            mcp: mcp::McpStore::new(pool.clone()),
            logs: logs::LogStore::new(pool.clone()),
            pool,
        })
    }

    async fn migrate(&self) -> Result<()> {
        sqlx::migrate!("../../migrations")
            .run(&self.pool)
            .await
            .map_err(|err| Error::Storage(format!("migration failed: {err}")))
    }

    pub fn pool(&self) -> &SqlitePool {
        &self.pool
    }

    pub async fn ping(&self) -> Result<()> {
        sqlx::query_scalar::<_, i64>("SELECT 1")
            .fetch_one(&self.pool)
            .await
            .map_err(|err| Error::Storage(format!("sqlite ping failed: {err}")))?;
        Ok(())
    }

    pub async fn load_runtime_data(&self) -> Result<RuntimeData> {
        let service_rows =
            sqlx::query("SELECT id, name, protocol, enabled FROM services ORDER BY name ASC")
                .fetch_all(&self.pool)
                .await
                .map_err(|err| Error::Storage(format!("cannot load services: {err}")))?;

        let services = service_rows
            .into_iter()
            .map(|row| ServiceSummary {
                id: row.get("id"),
                name: row.get("name"),
                protocol: row.get("protocol"),
                enabled: row.get::<i64, _>("enabled") != 0,
            })
            .collect();

        let route_rows = sqlx::query(
            "SELECT id, service_id, hosts, paths, enabled FROM routes ORDER BY paths ASC",
        )
        .fetch_all(&self.pool)
        .await
        .map_err(|err| Error::Storage(format!("cannot load routes: {err}")))?;

        let routes = route_rows
            .into_iter()
            .map(|row| RouteSummary {
                id: row.get("id"),
                service_id: row
                    .get::<Option<String>, _>("service_id")
                    .unwrap_or_default(),
                host: row
                    .try_get::<Option<String>, _>("hosts")
                    .ok()
                    .flatten()
                    .and_then(|raw| serde_json::from_str::<Vec<String>>(&raw).ok())
                    .and_then(|hosts| hosts.into_iter().next()),
                path_prefix: row
                    .try_get::<Option<String>, _>("paths")
                    .ok()
                    .flatten()
                    .and_then(|raw| serde_json::from_str::<Vec<String>>(&raw).ok())
                    .and_then(|paths| paths.into_iter().next())
                    .unwrap_or_else(|| "/".into()),
                enabled: row.get::<i64, _>("enabled") != 0,
            })
            .collect();

        let counts = sqlx::query(
            "SELECT
                (SELECT COUNT(*) FROM services) AS services,
                (SELECT COUNT(*) FROM routes) AS routes,
                (SELECT COUNT(*) FROM upstreams) AS upstreams,
                (SELECT COUNT(*) FROM targets) AS targets",
        )
        .fetch_one(&self.pool)
        .await
        .map_err(|err| Error::Storage(format!("cannot count runtime entities: {err}")))?;

        Ok(RuntimeData {
            services,
            routes,
            counts: SnapshotCounts {
                services: counts.get("services"),
                routes: counts.get("routes"),
                upstreams: counts.get("upstreams"),
                targets: counts.get("targets"),
            },
        })
    }
}

/// Keeps the database and its WAL sidecars private. They may contain agent
/// configuration backups, which can include credentials.
fn restrict_database_permissions(path: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;

        let mut candidates: Vec<PathBuf> = vec![path.to_path_buf()];
        for suffix in ["-wal", "-shm"] {
            let mut name = path.as_os_str().to_os_string();
            name.push(suffix);
            candidates.push(PathBuf::from(name));
        }

        for candidate in candidates {
            if candidate.exists() {
                std::fs::set_permissions(&candidate, std::fs::Permissions::from_mode(0o600))
                    .map_err(|error| {
                        Error::Storage(format!(
                            "cannot restrict permissions on {}: {error}",
                            candidate.display()
                        ))
                    })?;
            }
        }
    }
    #[cfg(not(unix))]
    let _ = path;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn migrations_create_the_initial_schema() {
        let storage = Storage::connect_in_memory().await.unwrap();
        storage.ping().await.unwrap();

        let data = storage.load_runtime_data().await.unwrap();
        assert!(data.services.is_empty());
        assert!(data.routes.is_empty());
        assert_eq!(data.counts.services, 0);

        let removed_tables: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name IN (
                 'llm_providers', 'llm_provider_accounts', 'llm_models', 'llm_routing_groups',
                 'llm_routes', 'llm_routing_targets', 'llm_provider_oauth_tokens'
             )",
        )
        .fetch_one(storage.pool())
        .await
        .unwrap();
        assert_eq!(removed_tables, 0, "legacy model tables must be dropped");

        let ai_tables: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name IN (
                 'ai_providers', 'ai_provider_accounts', 'ai_provider_oauth_tokens'
             )",
        )
        .fetch_one(storage.pool())
        .await
        .unwrap();
        assert_eq!(ai_tables, 3, "AI-native tables must exist");

        let rollup: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = 'llm_usage_rollup'",
        )
        .fetch_one(storage.pool())
        .await
        .unwrap();
        assert_eq!(rollup, 1);

        let indexes: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM sqlite_master WHERE type = 'index' AND name IN (
                 'idx_llm_usage_api_key_created', 'idx_llm_usage_account_created',
                 'idx_llm_usage_vmodel_created', 'idx_rollup_api_key'
             )",
        )
        .fetch_one(storage.pool())
        .await
        .unwrap();
        assert_eq!(indexes, 4);

        let event_index: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM sqlite_master
              WHERE type = 'index' AND name = 'idx_events_status_created'",
        )
        .fetch_one(storage.pool())
        .await
        .unwrap();
        assert_eq!(event_index, 1);

        let dropped: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM sqlite_master WHERE type = 'index' AND name = 'idx_llm_usage_api_key'",
        )
        .fetch_one(storage.pool())
        .await
        .unwrap();
        assert_eq!(dropped, 0);

        let kept: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM sqlite_master WHERE type = 'index' AND name = 'idx_llm_usage_created'",
        )
        .fetch_one(storage.pool())
        .await
        .unwrap();
        assert_eq!(kept, 1);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn database_files_are_private() {
        use std::os::unix::fs::PermissionsExt;

        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("octra.db");
        let storage = Storage::connect(&path).await.unwrap();
        storage.ping().await.unwrap();

        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);

        for suffix in ["-wal", "-shm"] {
            let mut name = path.as_os_str().to_os_string();
            name.push(suffix);
            let sidecar = PathBuf::from(name);
            if sidecar.exists() {
                let mode = std::fs::metadata(&sidecar).unwrap().permissions().mode() & 0o777;
                assert_eq!(mode, 0o600, "{}", sidecar.display());
            }
        }
    }
}
