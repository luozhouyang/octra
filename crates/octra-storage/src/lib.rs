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
use octra_secrets::{MasterKey, MasterKeySet, SecretBox};
use sha2::{Digest, Sha256};
use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions};
use sqlx::{Row, SqlitePool};

#[derive(Debug, Clone, Default)]
pub struct RuntimeData {
    pub services: Vec<ServiceSummary>,
    pub routes: Vec<RouteSummary>,
    pub counts: SnapshotCounts,
}

/// Summary of a master-key rotation and the secret rows re-encrypted with it.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SecretRotationReport {
    /// Number of non-empty secret columns re-encrypted with the new key.
    pub rotated: u64,
}

/// Summary of a master-key retirement pass.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SecretRetirementReport {
    /// Number of keys removed from the key set.
    pub retired: usize,
    /// Number of keys still retained, including the active key.
    pub remaining: usize,
}

/// Stable AES-GCM associated data for one stored secret column.
pub(crate) fn secret_context(table: &str, row_id: &str, column: &str) -> String {
    format!("{table}|{row_id}|{column}")
}

/// Deterministic KeyAuth lookup/dedup hash. This is not encryption: weak keys
/// remain guessable offline, so KeyAuth keys must be high-entropy.
pub(crate) fn key_lookup(key: &str) -> String {
    hex::encode(Sha256::digest(key.as_bytes()))
}

const SECRET_TABLES: [(&str, &str, &[&str]); 4] = [
    (
        "ai_provider_oauth_tokens",
        "account_id",
        &["access_token", "refresh_token", "extra_json"],
    ),
    ("keyauth_credentials", "id", &["key"]),
    ("basicauth_credentials", "id", &["password"]),
    ("jwt_secrets", "id", &["secret"]),
];

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
        let key_set = MasterKeySet::load_or_create(&key_file, true, !existed)
            .map_err(|err| Error::Storage(format!("cannot load master key: {err}")))?;
        let secrets = Arc::new(SecretBox::from_key_set(key_set));

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

    /// Proves that the resolved master key set can open every existing sealed
    /// row. v2 rows are validated with their `table|row|column` AAD context;
    /// v1 rows are validated against every retained key.
    ///
    /// This prevents a changed keychain entry or key file from silently
    /// serving a database whose secrets can no longer be decrypted.
    pub async fn verify_secrets(&self) -> Result<()> {
        for (table, id_column, columns) in SECRET_TABLES {
            let selected = columns.join(", ");
            let sql = format!("SELECT {id_column}, {selected} FROM {table}");
            let rows = sqlx::query(&sql)
                .fetch_all(self.pool())
                .await
                .map_err(|err| {
                    Error::Storage(format!("cannot read sealed secrets from {table}: {err}"))
                })?;
            for row in rows {
                let id: String = row.get(id_column);
                for column in columns {
                    let stored: Option<String> = row.try_get(*column).map_err(|err| {
                        Error::Storage(format!(
                            "cannot read stored secret {table}.{column} for row '{id}': {err}"
                        ))
                    })?;
                    let Some(stored) = stored.filter(|value| !value.is_empty()) else {
                        continue;
                    };
                    let context = secret_context(table, &id, column);
                    self.secrets
                        .open_with_context(&stored, &context)
                        .map_err(|error| {
                            Error::Storage(format!(
                                "cannot decrypt stored secrets in {table}.{column} for row '{id}': {error}"
                            ))
                        })?;
                }
            }
        }
        Ok(())
    }

    /// Seals plaintext values and upgrades v1 ciphertext to v2 with AAD. The
    /// operation is idempotent: v2 values are left untouched.
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

        let keyauth_rows = sqlx::query("SELECT id, key, key_lookup FROM keyauth_credentials")
            .fetch_all(&mut *tx)
            .await
            .map_err(|err| Error::Storage(format!("cannot read keyauth credentials: {err}")))?;
        for row in keyauth_rows {
            let id: String = row.get("id");
            let key: String = row.get("key");
            let existing_lookup: Option<String> = row.get("key_lookup");
            let context = secret_context("keyauth_credentials", &id, "key");
            let plaintext = if SecretBox::is_sealed(&key) {
                self.secrets
                    .open_with_context(&key, &context)
                    .map_err(|error| {
                        Error::Storage(format!(
                            "cannot decrypt keyauth_credentials.key for row '{id}': {error}"
                        ))
                    })?
            } else {
                key.as_bytes().to_vec()
            };
            let plaintext = String::from_utf8(plaintext).map_err(|_| {
                Error::Storage(format!(
                    "decrypted keyauth_credentials.key for row '{id}' is not UTF-8"
                ))
            })?;
            let lookup = key_lookup(&plaintext);
            let sealed = if key.starts_with("enc:v2:") {
                None
            } else {
                Some(
                    self.secrets
                        .seal_with_context(plaintext.as_bytes(), &context)
                        .map_err(|_| {
                            Error::Storage(format!(
                                "cannot encrypt keyauth_credentials.key for row '{id}'"
                            ))
                        })?,
                )
            };
            if sealed.is_none() && existing_lookup.as_deref() == Some(lookup.as_str()) {
                continue;
            }
            sqlx::query(
                "UPDATE keyauth_credentials SET key = COALESCE(?, key), key_lookup = ? WHERE id = ?",
            )
            .bind(sealed)
            .bind(&lookup)
            .bind(&id)
            .execute(&mut *tx)
            .await
            .map_err(|err| {
                Error::Storage(format!(
                    "cannot backfill keyauth_credentials for row '{id}': {err}"
                ))
            })?;
            updated += 1;
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

    /// Rotates the active master key and re-encrypts every stored secret with
    /// it and its `table|row|column` AAD context. All tables are updated in one
    /// transaction, and any undecryptable row aborts the whole rotation.
    pub async fn rotate_secrets(&self) -> Result<SecretRotationReport> {
        self.secrets
            .rotate()
            .map_err(|error| Error::Storage(format!("cannot rotate the master key: {error}")))?;

        let mut tx = self
            .pool
            .begin()
            .await
            .map_err(|err| Error::Storage(format!("cannot start secret rotation: {err}")))?;
        let mut rotated = 0u64;
        for (table, id_column, columns) in SECRET_TABLES {
            rotated += self
                .reseal_table(&mut tx, table, id_column, columns)
                .await?;
        }
        tx.commit()
            .await
            .map_err(|err| Error::Storage(format!("cannot commit secret rotation: {err}")))?;
        tracing::info!(rotated, "rotated and re-encrypted stored secrets");
        Ok(SecretRotationReport { rotated })
    }

    /// Retires master keys that are no longer referenced by any stored v2
    /// ciphertext. The active key and every key id found in an `enc:v2:`
    /// prefix are retained.
    ///
    /// Retirement is refused while any `enc:v1:` value remains because those
    /// values carry no key id. Run a rotation or backfill first so every row
    /// records its key id.
    pub async fn retire_secrets(&self) -> Result<SecretRetirementReport> {
        let mut referenced = std::collections::HashSet::new();
        for (table, id_column, columns) in SECRET_TABLES {
            let selected = columns.join(", ");
            let sql = format!("SELECT {id_column}, {selected} FROM {table}");
            let rows = sqlx::query(&sql)
                .fetch_all(self.pool())
                .await
                .map_err(|err| {
                    Error::Storage(format!(
                        "cannot scan {table} for master-key references: {err}"
                    ))
                })?;
            for row in rows {
                let id: String = row.get(id_column);
                for column in columns {
                    let stored: Option<String> = row.try_get(*column).map_err(|err| {
                        Error::Storage(format!(
                            "cannot read stored secret {table}.{column} for row '{id}': {err}"
                        ))
                    })?;
                    let Some(stored) = stored.filter(|value| !value.is_empty()) else {
                        continue;
                    };
                    if stored.starts_with("enc:v1:") {
                        return Err(Error::Storage(format!(
                            "cannot retire master keys: {table}.{column} for row '{id}' contains enc:v1 data with no key id; run secret rotation or backfill first"
                        )));
                    }
                    let Some(rest) = stored.strip_prefix("enc:v2:") else {
                        continue;
                    };
                    let key_id = rest.split_once(':').map(|(key_id, _)| key_id);
                    match key_id.filter(|key_id| !key_id.is_empty()) {
                        Some(key_id) => {
                            referenced.insert(key_id.to_string());
                        }
                        None => {
                            return Err(Error::Storage(format!(
                                "cannot retire master keys: {table}.{column} for row '{id}' has a malformed enc:v2: prefix"
                            )));
                        }
                    }
                }
            }
        }

        referenced.insert(self.secrets.active_key_id().map_err(|error| {
            Error::Storage(format!("cannot read the active master key id: {error}"))
        })?);
        let retired = self
            .secrets
            .retire(&referenced)
            .map_err(|error| Error::Storage(format!("cannot retire master keys: {error}")))?;
        let remaining = self
            .secrets
            .key_ids()
            .map_err(|error| Error::Storage(format!("cannot list master keys: {error}")))?
            .len();
        tracing::info!(retired, remaining, "retired unreferenced master keys");
        Ok(SecretRetirementReport { retired, remaining })
    }

    async fn reseal_table(
        &self,
        tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
        table: &str,
        id_column: &str,
        columns: &[&str],
    ) -> Result<u64> {
        let selected = columns.join(", ");
        let sql = format!("SELECT {id_column}, {selected} FROM {table}");
        let rows = sqlx::query(&sql)
            .fetch_all(&mut **tx)
            .await
            .map_err(|err| Error::Storage(format!("cannot read secrets from {table}: {err}")))?;

        let mut updates = Vec::with_capacity(rows.len());
        let mut rotated = 0u64;
        for row in rows {
            let id: String = row.get(id_column);
            let mut values = Vec::with_capacity(columns.len());
            for column in columns {
                let stored: Option<String> = row.try_get(*column).map_err(|err| {
                    Error::Storage(format!(
                        "cannot read stored secret {table}.{column} for row '{id}': {err}"
                    ))
                })?;
                let sealed = match stored.filter(|value| !value.is_empty()) {
                    Some(value) => {
                        let context = secret_context(table, &id, column);
                        let plaintext =
                            self.secrets
                                .open_with_context(&value, &context)
                                .map_err(|error| {
                                    Error::Storage(format!(
                                        "cannot re-encrypt {table}.{column} for row '{id}': {error}"
                                    ))
                                })?;
                        rotated += 1;
                        Some(
                            self.secrets
                                .seal_with_context(&plaintext, &context)
                                .map_err(|error| {
                                    Error::Storage(format!(
                                        "cannot re-encrypt {table}.{column} for row '{id}': {error}"
                                    ))
                                })?,
                        )
                    }
                    None => None,
                };
                values.push(sealed);
            }
            updates.push((id, values));
        }

        let assignments = columns
            .iter()
            .map(|column| format!("{column} = COALESCE(?, {column})"))
            .collect::<Vec<_>>()
            .join(", ");
        let sql = format!("UPDATE {table} SET {assignments} WHERE {id_column} = ?");
        for (id, values) in updates {
            if values.iter().all(|value| value.is_none()) {
                continue;
            }
            let mut query = sqlx::query(&sql);
            for value in values {
                query = query.bind(value);
            }
            query.bind(id).execute(&mut **tx).await.map_err(|err| {
                Error::Storage(format!(
                    "cannot store re-encrypted secrets in {table}: {err}"
                ))
            })?;
        }
        Ok(rotated)
    }

    fn seal_if_needed(
        &self,
        value: &str,
        table: &str,
        column: &str,
        id: &str,
    ) -> Result<Option<String>> {
        if value.is_empty() || value.starts_with("enc:v2:") {
            return Ok(None);
        }
        let context = secret_context(table, id, column);
        let plaintext = if value.starts_with("enc:v1:") {
            self.secrets
                .open_with_context(value, &context)
                .map_err(|error| {
                    Error::Storage(format!(
                        "cannot decrypt legacy {table}.{column} for row '{id}': {error}"
                    ))
                })?
        } else {
            value.as_bytes().to_vec()
        };
        self.secrets
            .seal_with_context(&plaintext, &context)
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

    async fn seed_keyauth(storage: &Storage, key: &str) -> (String, String) {
        use octra_api_core::{CreateConsumer, CreateKeyauthCredential};

        let consumer = storage
            .api
            .create_consumer(&CreateConsumer {
                username: Some(format!("retirement-{key}")),
                ..Default::default()
            })
            .await
            .unwrap();
        let credential = storage
            .api
            .create_credential(&CreateKeyauthCredential {
                consumer_id: consumer.id.clone(),
                key: Some(key.into()),
                ..Default::default()
            })
            .await
            .unwrap();
        (consumer.id, credential.id)
    }

    async fn insert_keyauth_row(storage: &Storage, consumer_id: &str, id: &str, key: &str) {
        sqlx::query(
            "INSERT INTO keyauth_credentials (id, consumer_id, key, key_lookup, tags, created_at) \
             VALUES (?, ?, ?, ?, '[]', CAST(strftime('%s', 'now') AS INTEGER))",
        )
        .bind(id)
        .bind(consumer_id)
        .bind(key)
        .bind(key_lookup(key))
        .execute(storage.pool())
        .await
        .unwrap();
    }

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

    #[tokio::test]
    async fn rotation_reencrypts_every_secret_and_upgrades_v1() {
        use octra_ai_config::{AiProvider, AiProviderAccount};
        use octra_api_core::{
            CreateBasicauthCredential, CreateConsumer, CreateJwtSecret, CreateKeyauthCredential,
        };
        use serde_json::json;

        let storage = Storage::connect_in_memory().await.unwrap();
        let consumer = storage
            .api
            .create_consumer(&CreateConsumer {
                username: Some("rotation".into()),
                ..Default::default()
            })
            .await
            .unwrap();
        let keyauth = storage
            .api
            .create_credential(&CreateKeyauthCredential {
                consumer_id: consumer.id.clone(),
                key: Some("rotation-key".into()),
                ..Default::default()
            })
            .await
            .unwrap();
        let basicauth = storage
            .api
            .create_basicauth(&CreateBasicauthCredential {
                consumer_id: consumer.id.clone(),
                username: "rotator".into(),
                password: "rotation-password".into(),
                ..Default::default()
            })
            .await
            .unwrap();
        let jwt = storage
            .api
            .create_jwt(&CreateJwtSecret {
                consumer_id: consumer.id.clone(),
                key: "rotation-jwt-key".into(),
                secret: "rotation-jwt-secret".into(),
                ..Default::default()
            })
            .await
            .unwrap();

        let provider = AiProvider {
            id: "provider-rotation".into(),
            name: "rotation".into(),
            protocol: "openai".into(),
            base_url: "https://api.example.com".into(),
            auth_ref: None,
            capabilities: Vec::new(),
            config: json!({}),
            enabled: true,
            created_at: 100,
            updated_at: 100,
        };
        storage.ai.upsert_provider(&provider).await.unwrap();
        let account = AiProviderAccount {
            id: "account-rotation".into(),
            provider_id: provider.id.clone(),
            name: "rotation".into(),
            credential_kind: "oauth".into(),
            secret_ref: None,
            oauth_ref: None,
            weight: 1,
            priority: 0,
            quota_limit: None,
            quota_unit: "requests".into(),
            quota_period: "none".into(),
            quota_used: 0,
            cooldown_until: None,
            enabled: true,
            oauth_status: "authorized".into(),
            oauth_expires_at: None,
            config: json!({}),
            created_at: 100,
            updated_at: 100,
        };
        storage.ai.upsert_account(&account).await.unwrap();
        storage
            .llm
            .upsert_oauth_token(&LlmOAuthTokenRecord {
                account_id: account.id.clone(),
                access_token: "rotation-access".into(),
                refresh_token: Some("rotation-refresh".into()),
                token_type: "Bearer".into(),
                scope: "chat".into(),
                expires_at: None,
                extra_json: "{\"region\":\"eu\"}".into(),
            })
            .await
            .unwrap();

        let legacy = storage.secrets.seal_legacy(b"legacy-key").unwrap();
        assert!(legacy.starts_with("enc:v1:"));
        sqlx::query(
            "INSERT INTO keyauth_credentials (id, consumer_id, key, key_lookup, tags, created_at) \
             VALUES (?, ?, ?, ?, '[]', CAST(strftime('%s', 'now') AS INTEGER))",
        )
        .bind("cred_legacy")
        .bind(&consumer.id)
        .bind(&legacy)
        .bind(key_lookup("legacy-key"))
        .execute(storage.pool())
        .await
        .unwrap();

        let report = storage.rotate_secrets().await.unwrap();
        assert_eq!(report.rotated, 7);

        let raw_key: String =
            sqlx::query_scalar("SELECT key FROM keyauth_credentials WHERE id = ?")
                .bind(&keyauth.id)
                .fetch_one(storage.pool())
                .await
                .unwrap();
        assert!(raw_key.starts_with("enc:v2:"));
        let raw_legacy: String =
            sqlx::query_scalar("SELECT key FROM keyauth_credentials WHERE id = 'cred_legacy'")
                .fetch_one(storage.pool())
                .await
                .unwrap();
        assert!(raw_legacy.starts_with("enc:v2:"));
        let raw_password: String =
            sqlx::query_scalar("SELECT password FROM basicauth_credentials WHERE id = ?")
                .bind(&basicauth.id)
                .fetch_one(storage.pool())
                .await
                .unwrap();
        assert!(raw_password.starts_with("enc:v2:"));
        let raw_jwt: String = sqlx::query_scalar("SELECT secret FROM jwt_secrets WHERE id = ?")
            .bind(&jwt.id)
            .fetch_one(storage.pool())
            .await
            .unwrap();
        assert!(raw_jwt.starts_with("enc:v2:"));
        let raw_oauth: (String, Option<String>, String) = sqlx::query_as(
            "SELECT access_token, refresh_token, extra_json
             FROM ai_provider_oauth_tokens WHERE account_id = ?",
        )
        .bind(&account.id)
        .fetch_one(storage.pool())
        .await
        .unwrap();
        assert!(raw_oauth.0.starts_with("enc:v2:"));
        assert!(raw_oauth.1.as_deref().unwrap().starts_with("enc:v2:"));
        assert!(raw_oauth.2.starts_with("enc:v2:"));

        assert_eq!(
            storage
                .api
                .get_credential(&keyauth.id)
                .await
                .unwrap()
                .unwrap()
                .key,
            "rotation-key"
        );
        assert_eq!(
            storage
                .api
                .get_credential("cred_legacy")
                .await
                .unwrap()
                .unwrap()
                .key,
            "legacy-key"
        );
        assert_eq!(
            storage
                .api
                .get_basicauth(&basicauth.id)
                .await
                .unwrap()
                .unwrap()
                .password,
            "rotation-password"
        );
        assert_eq!(
            storage.api.get_jwt(&jwt.id).await.unwrap().unwrap().secret,
            "rotation-jwt-secret"
        );
        let oauth = storage
            .llm
            .get_oauth_token(&account.id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(oauth.access_token, "rotation-access");
        assert_eq!(oauth.refresh_token.as_deref(), Some("rotation-refresh"));
        assert_eq!(oauth.extra_json, "{\"region\":\"eu\"}");

        let second = storage.rotate_secrets().await.unwrap();
        assert_eq!(second.rotated, 7);
        assert_eq!(
            storage
                .api
                .get_credential(&keyauth.id)
                .await
                .unwrap()
                .unwrap()
                .key,
            "rotation-key"
        );
    }

    #[tokio::test]
    async fn retirement_removes_unreferenced_keys_and_keeps_the_active_key() {
        let storage = Storage::connect_in_memory().await.unwrap();
        let (_, credential_id) = seed_keyauth(&storage, "retirement-key").await;
        let old_id = storage.secrets.active_key_id().unwrap();

        let rotation = storage.rotate_secrets().await.unwrap();
        assert_eq!(rotation.rotated, 1);
        let new_id = storage.secrets.active_key_id().unwrap();
        assert_ne!(old_id, new_id);

        let report = storage.retire_secrets().await.unwrap();
        assert_eq!(
            report,
            SecretRetirementReport {
                retired: 1,
                remaining: 1,
            }
        );
        assert_eq!(storage.secrets.key_ids().unwrap(), vec![new_id]);

        let credential = storage
            .api
            .get_credential(&credential_id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(credential.key, "retirement-key");

        let repeated = storage.retire_secrets().await.unwrap();
        assert_eq!(
            repeated,
            SecretRetirementReport {
                retired: 0,
                remaining: 1,
            }
        );
    }

    #[tokio::test]
    async fn retirement_keeps_a_key_referenced_by_a_manual_v2_row() {
        let storage = Storage::connect_in_memory().await.unwrap();
        let (consumer_id, _) = seed_keyauth(&storage, "active-key").await;
        let old_id = storage.secrets.active_key_id().unwrap();
        let context = secret_context("keyauth_credentials", "cred_manual", "key");
        let pinned = storage
            .secrets
            .seal_with_context(b"pinned-value", &context)
            .unwrap();
        assert!(pinned.starts_with(&format!("enc:v2:{old_id}:")));

        storage.rotate_secrets().await.unwrap();
        let new_id = storage.secrets.active_key_id().unwrap();
        assert_ne!(old_id, new_id);
        insert_keyauth_row(&storage, &consumer_id, "cred_manual", &pinned).await;

        let report = storage.retire_secrets().await.unwrap();
        assert_eq!(
            report,
            SecretRetirementReport {
                retired: 0,
                remaining: 2,
            }
        );
        let ids = storage.secrets.key_ids().unwrap();
        assert!(ids.contains(&old_id));
        assert!(ids.contains(&new_id));
        let credential = storage
            .api
            .get_credential("cred_manual")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(credential.key, "pinned-value");
    }

    #[tokio::test]
    async fn retirement_is_blocked_by_legacy_v1_rows() {
        let storage = Storage::connect_in_memory().await.unwrap();
        let (consumer_id, _) = seed_keyauth(&storage, "legacy-key").await;
        let legacy = storage.secrets.seal_legacy(b"legacy-retire").unwrap();
        assert!(legacy.starts_with("enc:v1:"));
        insert_keyauth_row(&storage, &consumer_id, "cred_legacy_retire", &legacy).await;
        let before = storage.secrets.key_ids().unwrap();

        let error = storage.retire_secrets().await.unwrap_err();
        let message = error.to_string();
        assert!(message.contains("enc:v1"), "{message}");
        assert!(
            message.contains("rotation") || message.contains("backfill"),
            "{message}"
        );
        assert_eq!(storage.secrets.key_ids().unwrap(), before);
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
