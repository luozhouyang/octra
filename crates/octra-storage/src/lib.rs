//! SQLite persistence layer. Sensitive credential values are sealed with
//! `octra-secrets`; configuration references remain plaintext identifiers.

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
use std::sync::Arc;
use std::time::Duration;

use octra_core::{Error, Result, RouteSummary, ServiceSummary, SnapshotCounts};
use octra_secrets::{MasterKey, SecretBox};
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
    secrets: Arc<SecretBox>,
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

        let key_file = path
            .parent()
            .map(|parent| parent.join("master.key"))
            .unwrap_or_else(|| PathBuf::from("master.key"));
        let existed = path.exists();
        let key = MasterKey::load_or_create(&key_file, true, !existed)
            .map_err(|err| Error::Storage(format!("cannot load master key: {err}")))?;
        let secrets = Arc::new(SecretBox::new(key));

        let storage = Self::from_options(options, 5, secrets).await?;
        storage.migrate().await?;
        storage.ensure_usage_rollup().await?;
        storage.verify_secrets().await?;
        storage.backfill_secrets().await?;
        restrict_database_permissions(path)?;
        Ok(storage)
    }

    pub async fn connect_in_memory() -> Result<Self> {
        let options = SqliteConnectOptions::new()
            .filename(":memory:")
            .foreign_keys(true);

        let secrets = Arc::new(SecretBox::new(MasterKey::ephemeral()));
        let storage = Self::from_options(options, 1, secrets).await?;
        storage.migrate().await?;
        storage.ensure_usage_rollup().await?;
        storage.backfill_secrets().await?;
        Ok(storage)
    }

    async fn from_options(
        options: SqliteConnectOptions,
        max_connections: u32,
        secrets: Arc<SecretBox>,
    ) -> Result<Self> {
        let pool = SqlitePoolOptions::new()
            .max_connections(max_connections)
            .connect_with(options)
            .await
            .map_err(|err| Error::Storage(format!("cannot open sqlite: {err}")))?;

        Ok(Self {
            agents: agents::AgentStore::new(pool.clone()),
            ai: ai::AiStore::new(pool.clone()),
            api: api::ApiStore::new(pool.clone(), secrets.clone()),
            llm: llm::LlmStore::new(pool.clone(), secrets.clone()),
            mcp: mcp::McpStore::new(pool.clone()),
            logs: logs::LogStore::new(pool.clone()),
            secrets,
            pool,
        })
    }

    /// Seals legacy plaintext credential values in place. The operation is
    /// idempotent: values that already carry the sealed prefix are skipped.
    /// Proves that the resolved master key can open the existing sealed rows.
    ///
    /// This prevents a changed keychain entry or key file from silently
    /// serving a database whose secrets can no longer be decrypted.
    pub async fn verify_secrets(&self) -> Result<()> {
        for (table, column) in [
            ("ai_provider_oauth_tokens", "access_token"),
            ("keyauth_credentials", "key"),
            ("basicauth_credentials", "password"),
            ("jwt_secrets", "secret"),
        ] {
            let sql =
                format!("SELECT {column} FROM {table} WHERE {column} LIKE 'enc:v1:%' LIMIT 1");
            let stored = sqlx::query_scalar::<_, String>(&sql)
                .fetch_optional(self.pool())
                .await
                .map_err(|err| {
                    Error::Storage(format!("cannot read sealed secrets from {table}: {err}"))
                })?;
            if let Some(stored) = stored {
                self.secrets.open(&stored).map_err(|_| {
                    Error::Storage(format!(
                        "cannot decrypt stored secrets in {table}; the master key does not match the database"
                    ))
                })?;
                return Ok(());
            }
        }
        Ok(())
    }

    pub async fn backfill_secrets(&self) -> Result<()> {
        let mut tx = self
            .pool
            .begin()
            .await
            .map_err(|err| Error::Storage(format!("cannot start secret backfill: {err}")))?;
        let mut updated = 0u64;

        let oauth_rows = sqlx::query(
            "SELECT account_id, access_token, refresh_token, extra_json
             FROM ai_provider_oauth_tokens",
        )
        .fetch_all(&mut *tx)
        .await
        .map_err(|err| Error::Storage(format!("cannot read oauth tokens: {err}")))?;
        for row in oauth_rows {
            let account_id: String = row.get("account_id");
            let access_token: String = row.get("access_token");
            let refresh_token: Option<String> = row.get("refresh_token");
            let extra_json: String = row.get("extra_json");
            let sealed_access = self.seal_if_needed(
                &access_token,
                "ai_provider_oauth_tokens",
                "access_token",
                &account_id,
            )?;
            let sealed_refresh = match refresh_token.as_deref() {
                Some(value) => self.seal_if_needed(
                    value,
                    "ai_provider_oauth_tokens",
                    "refresh_token",
                    &account_id,
                )?,
                None => None,
            };
            let sealed_extra = self.seal_if_needed(
                &extra_json,
                "ai_provider_oauth_tokens",
                "extra_json",
                &account_id,
            )?;
            if sealed_access.is_none() && sealed_refresh.is_none() && sealed_extra.is_none() {
                continue;
            }
            sqlx::query(
                "UPDATE ai_provider_oauth_tokens
                 SET access_token = COALESCE(?, access_token),
                     refresh_token = COALESCE(?, refresh_token),
                     extra_json = COALESCE(?, extra_json),
                     updated_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now')
                 WHERE account_id = ?",
            )
            .bind(sealed_access)
            .bind(sealed_refresh)
            .bind(sealed_extra)
            .bind(&account_id)
            .execute(&mut *tx)
            .await
            .map_err(|err| {
                Error::Storage(format!(
                    "cannot seal ai_provider_oauth_tokens for row '{account_id}': {err}"
                ))
            })?;
            updated += 1;
        }

        let keyauth_rows = sqlx::query("SELECT id, key FROM keyauth_credentials")
            .fetch_all(&mut *tx)
            .await
            .map_err(|err| Error::Storage(format!("cannot read keyauth credentials: {err}")))?;
        for row in keyauth_rows {
            let id: String = row.get("id");
            let key: String = row.get("key");
            if let Some(sealed) = self.seal_if_needed(&key, "keyauth_credentials", "key", &id)? {
                sqlx::query("UPDATE keyauth_credentials SET key = ? WHERE id = ?")
                    .bind(sealed)
                    .bind(&id)
                    .execute(&mut *tx)
                    .await
                    .map_err(|err| {
                        Error::Storage(format!(
                            "cannot seal keyauth_credentials for row '{id}': {err}"
                        ))
                    })?;
                updated += 1;
            }
        }

        let basicauth_rows = sqlx::query("SELECT id, password FROM basicauth_credentials")
            .fetch_all(&mut *tx)
            .await
            .map_err(|err| Error::Storage(format!("cannot read basicauth credentials: {err}")))?;
        for row in basicauth_rows {
            let id: String = row.get("id");
            let password: String = row.get("password");
            if let Some(sealed) =
                self.seal_if_needed(&password, "basicauth_credentials", "password", &id)?
            {
                sqlx::query("UPDATE basicauth_credentials SET password = ? WHERE id = ?")
                    .bind(sealed)
                    .bind(&id)
                    .execute(&mut *tx)
                    .await
                    .map_err(|err| {
                        Error::Storage(format!(
                            "cannot seal basicauth_credentials for row '{id}': {err}"
                        ))
                    })?;
                updated += 1;
            }
        }

        let jwt_rows = sqlx::query("SELECT id, secret FROM jwt_secrets")
            .fetch_all(&mut *tx)
            .await
            .map_err(|err| Error::Storage(format!("cannot read JWT secrets: {err}")))?;
        for row in jwt_rows {
            let id: String = row.get("id");
            let secret: String = row.get("secret");
            if let Some(sealed) = self.seal_if_needed(&secret, "jwt_secrets", "secret", &id)? {
                sqlx::query("UPDATE jwt_secrets SET secret = ? WHERE id = ?")
                    .bind(sealed)
                    .bind(&id)
                    .execute(&mut *tx)
                    .await
                    .map_err(|err| {
                        Error::Storage(format!("cannot seal jwt_secrets for row '{id}': {err}"))
                    })?;
                updated += 1;
            }
        }

        tx.commit()
            .await
            .map_err(|err| Error::Storage(format!("cannot commit secret backfill: {err}")))?;
        if updated > 0 {
            tracing::info!(rows = updated, "sealed legacy stored secrets");
        }
        Ok(())
    }

    fn seal_if_needed(
        &self,
        value: &str,
        table: &str,
        column: &str,
        id: &str,
    ) -> Result<Option<String>> {
        if value.is_empty() || SecretBox::is_sealed(value) {
            return Ok(None);
        }
        self.secrets
            .seal(value.as_bytes())
            .map(Some)
            .map_err(|_| Error::Storage(format!("cannot encrypt {table}.{column} for row '{id}'")))
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
            "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = 'ai_usage_rollup'",
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

    #[tokio::test]
    async fn in_memory_storage_uses_an_ephemeral_key() {
        let storage = Storage::connect_in_memory().await.unwrap();
        let sealed = storage.secrets.seal(b"ephemeral").unwrap();
        assert!(SecretBox::is_sealed(&sealed));
        assert_eq!(storage.secrets.open(&sealed).unwrap(), b"ephemeral");
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
