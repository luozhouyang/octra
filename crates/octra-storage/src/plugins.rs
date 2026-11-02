//! Plugin catalog and per-scope configuration repositories.

use octra_api_plugin_types::{PluginManifest, Scope};
use octra_core::{Error, Result};
use sqlx::Row;

use super::llm::LlmStore;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PluginRecord {
    pub id: String,
    pub name: String,
    pub version: String,
    pub kind: String,
    pub runtime_type: String,
    pub enabled: bool,
    pub priority: i64,
    pub description: String,
    pub manifest_json: String,
    pub config_schema_json: String,
    pub artifact_path: Option<String>,
    pub artifact_hash: Option<String>,
    pub updated_at: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PluginConfigRecord {
    pub id: String,
    pub plugin_id: String,
    pub scope: String,
    pub scope_id: String,
    pub enabled: bool,
    pub config_json: String,
    pub created_at: String,
    pub updated_at: String,
}

#[derive(Debug, Clone, Default)]
pub struct PluginData {
    pub plugins: Vec<PluginRecord>,
    pub configs: Vec<PluginConfigRecord>,
}

const PLUGIN_COLUMNS: &str = "id, name, version, kind, runtime_type, enabled, priority, description, \
     manifest_json, config_schema_json, artifact_path, artifact_hash, updated_at";

const CONFIG_COLUMNS: &str =
    "id, plugin_id, scope, scope_id, enabled, config, created_at, updated_at";

fn plugin_from_row(row: &sqlx::sqlite::SqliteRow) -> PluginRecord {
    PluginRecord {
        id: row.get("id"),
        name: row.get("name"),
        version: row.get("version"),
        kind: row.get("kind"),
        runtime_type: row.get("runtime_type"),
        enabled: row.get::<i64, _>("enabled") != 0,
        priority: row.get("priority"),
        description: row.get("description"),
        manifest_json: row.get("manifest_json"),
        config_schema_json: row.get("config_schema_json"),
        artifact_path: row.get("artifact_path"),
        artifact_hash: row.get("artifact_hash"),
        updated_at: row.get("updated_at"),
    }
}

fn config_from_row(row: &sqlx::sqlite::SqliteRow) -> PluginConfigRecord {
    PluginConfigRecord {
        id: row.get("id"),
        plugin_id: row.get("plugin_id"),
        scope: row.get("scope"),
        scope_id: row.get("scope_id"),
        enabled: row.get::<i64, _>("enabled") != 0,
        config_json: row.get("config"),
        created_at: row.get("created_at"),
        updated_at: row.get("updated_at"),
    }
}

fn map_error(context: &str, error: sqlx::Error) -> Error {
    if let sqlx::Error::Database(database) = &error
        && database.is_unique_violation()
    {
        return Error::Storage(format!("conflict: {context}"));
    }
    Error::Storage(format!("{context}: {error}"))
}

impl LlmStore {
    /// Inserts or refreshes a built-in plugin catalog row. `enabled` is left
    /// untouched on conflict so an administrator can keep a plugin disabled
    /// across restarts.
    pub async fn upsert_plugin(&self, manifest: &PluginManifest) -> Result<PluginRecord> {
        let id = format!("plg_{}", manifest.name);
        let manifest_json = serde_json::to_string(manifest).map_err(|error| {
            Error::Storage(format!("cannot serialize plugin manifest: {error}"))
        })?;
        let schema_json = manifest
            .config_schema
            .as_ref()
            .map(serde_json::to_string)
            .transpose()
            .map_err(|error| Error::Storage(format!("cannot serialize plugin schema: {error}")))?
            .unwrap_or_else(|| "{}".to_string());

        sqlx::query(
            "INSERT INTO plugins
                (id, name, version, kind, enabled, runtime_type, priority, description,
                 manifest_json, config_schema_json)
             VALUES (?1, ?2, ?3, ?4, 1, ?5, ?6, ?7, ?8, ?9)
             ON CONFLICT(name) DO UPDATE SET
                version = excluded.version,
                kind = excluded.kind,
                runtime_type = excluded.runtime_type,
                priority = excluded.priority,
                description = excluded.description,
                manifest_json = excluded.manifest_json,
                config_schema_json = excluded.config_schema_json,
                updated_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now')",
        )
        .bind(&id)
        .bind(&manifest.name)
        .bind(&manifest.version)
        .bind(manifest.kind.as_str())
        .bind(manifest.runtime.as_str())
        .bind(i64::from(manifest.priority))
        .bind(&manifest.description)
        .bind(manifest_json)
        .bind(schema_json)
        .execute(self.pool())
        .await
        .map_err(|error| map_error("cannot upsert plugin", error))?;

        self.get_plugin(&id)
            .await?
            .ok_or_else(|| Error::Storage("plugin vanished after upsert".to_string()))
    }

    pub async fn list_plugins(&self) -> Result<Vec<PluginRecord>> {
        let rows = sqlx::query(&format!(
            "SELECT {PLUGIN_COLUMNS} FROM plugins ORDER BY name ASC"
        ))
        .fetch_all(self.pool())
        .await
        .map_err(|error| map_error("cannot list plugins", error))?;
        Ok(rows.iter().map(plugin_from_row).collect())
    }

    pub async fn get_plugin(&self, id: &str) -> Result<Option<PluginRecord>> {
        let row = sqlx::query(&format!(
            "SELECT {PLUGIN_COLUMNS} FROM plugins WHERE id = ?1"
        ))
        .bind(id)
        .fetch_optional(self.pool())
        .await
        .map_err(|error| map_error("cannot load plugin", error))?;
        Ok(row.as_ref().map(plugin_from_row))
    }

    pub async fn get_plugin_by_name(&self, name: &str) -> Result<Option<PluginRecord>> {
        let row = sqlx::query(&format!(
            "SELECT {PLUGIN_COLUMNS} FROM plugins WHERE name = ?1"
        ))
        .bind(name)
        .fetch_optional(self.pool())
        .await
        .map_err(|error| map_error("cannot load plugin", error))?;
        Ok(row.as_ref().map(plugin_from_row))
    }

    pub async fn update_plugin_flags(
        &self,
        id: &str,
        enabled: Option<bool>,
        priority: Option<i64>,
    ) -> Result<Option<PluginRecord>> {
        let result = sqlx::query(
            "UPDATE plugins
                SET enabled = COALESCE(?2, enabled),
                    priority = COALESCE(?3, priority),
                    updated_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now')
              WHERE id = ?1",
        )
        .bind(id)
        .bind(enabled.map(i64::from))
        .bind(priority)
        .execute(self.pool())
        .await
        .map_err(|error| map_error("cannot update plugin", error))?;

        if result.rows_affected() == 0 {
            return Ok(None);
        }
        self.get_plugin(id).await
    }

    pub async fn list_plugin_configs(&self, plugin_id: &str) -> Result<Vec<PluginConfigRecord>> {
        let rows = sqlx::query(&format!(
            "SELECT {CONFIG_COLUMNS} FROM plugin_configs
              WHERE plugin_id = ?1
              ORDER BY scope ASC, scope_id ASC"
        ))
        .bind(plugin_id)
        .fetch_all(self.pool())
        .await
        .map_err(|error| map_error("cannot list plugin configs", error))?;
        Ok(rows.iter().map(config_from_row).collect())
    }

    pub async fn list_all_plugin_configs(&self) -> Result<Vec<PluginConfigRecord>> {
        let rows = sqlx::query(&format!(
            "SELECT {CONFIG_COLUMNS} FROM plugin_configs ORDER BY plugin_id ASC, scope ASC"
        ))
        .fetch_all(self.pool())
        .await
        .map_err(|error| map_error("cannot list plugin configs", error))?;
        Ok(rows.iter().map(config_from_row).collect())
    }

    pub async fn get_plugin_config(&self, id: &str) -> Result<Option<PluginConfigRecord>> {
        let row = sqlx::query(&format!(
            "SELECT {CONFIG_COLUMNS} FROM plugin_configs WHERE id = ?1"
        ))
        .bind(id)
        .fetch_optional(self.pool())
        .await
        .map_err(|error| map_error("cannot load plugin config", error))?;
        Ok(row.as_ref().map(config_from_row))
    }

    pub async fn create_plugin_config(
        &self,
        plugin_id: &str,
        scope: Scope,
        scope_id: Option<&str>,
        enabled: bool,
        config_json: &str,
    ) -> Result<PluginConfigRecord> {
        let normalized_scope_id = scope
            .normalize_id(scope_id)
            .ok_or_else(|| Error::Storage("not_found: plugin scope id is required".to_string()))?;
        let id = format!("pcfg_{}", uuid::Uuid::new_v4().simple());

        sqlx::query(
            "INSERT INTO plugin_configs
                (id, plugin_id, scope, scope_id, enabled, config)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        )
        .bind(&id)
        .bind(plugin_id)
        .bind(scope.as_str())
        .bind(normalized_scope_id)
        .bind(i64::from(enabled))
        .bind(config_json)
        .execute(self.pool())
        .await
        .map_err(|error| map_error("plugin config already exists for this scope", error))?;

        self.get_plugin_config(&id)
            .await?
            .ok_or_else(|| Error::Storage("plugin config vanished after insert".to_string()))
    }

    pub async fn update_plugin_config(
        &self,
        id: &str,
        enabled: Option<bool>,
        config_json: Option<&str>,
    ) -> Result<Option<PluginConfigRecord>> {
        let result = sqlx::query(
            "UPDATE plugin_configs
                SET enabled = COALESCE(?2, enabled),
                    config = COALESCE(?3, config),
                    updated_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now')
              WHERE id = ?1",
        )
        .bind(id)
        .bind(enabled.map(i64::from))
        .bind(config_json)
        .execute(self.pool())
        .await
        .map_err(|error| map_error("cannot update plugin config", error))?;

        if result.rows_affected() == 0 {
            return Ok(None);
        }
        self.get_plugin_config(id).await
    }

    pub async fn delete_plugin_config(&self, id: &str) -> Result<bool> {
        let result = sqlx::query("DELETE FROM plugin_configs WHERE id = ?1")
            .bind(id)
            .execute(self.pool())
            .await
            .map_err(|error| map_error("cannot delete plugin config", error))?;
        Ok(result.rows_affected() > 0)
    }

    /// Creates a scoped config with a raw scope name. LLM plugins use scopes
    /// that the API Gateway does not (`virtual_key`, `model`, `provider`), so
    /// the caller validates the scope vocabulary before calling this.
    pub async fn create_plugin_config_raw(
        &self,
        plugin_id: &str,
        scope: &str,
        scope_id: Option<&str>,
        enabled: bool,
        config_json: &str,
    ) -> Result<PluginConfigRecord> {
        let scope_id = if scope == "global" {
            String::new()
        } else {
            scope_id
                .map(str::trim)
                .filter(|id| !id.is_empty())
                .ok_or_else(|| Error::Storage(format!("not_found: {scope} scope id is required")))?
                .to_string()
        };
        let id = format!("pcfg_{}", uuid::Uuid::new_v4().simple());
        sqlx::query(
            "INSERT INTO plugin_configs
                (id, plugin_id, scope, scope_id, enabled, config)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        )
        .bind(&id)
        .bind(plugin_id)
        .bind(scope)
        .bind(scope_id)
        .bind(i64::from(enabled))
        .bind(config_json)
        .execute(self.pool())
        .await
        .map_err(|error| map_error("plugin config already exists for this scope", error))?;
        self.get_plugin_config(&id)
            .await?
            .ok_or_else(|| Error::Storage("plugin config vanished after insert".to_string()))
    }

    pub async fn load_plugin_data(&self) -> Result<PluginData> {
        Ok(PluginData {
            plugins: self.list_plugins().await?,
            configs: self.list_all_plugin_configs().await?,
        })
    }

    /// Loads host-maintained plugin state (for example budget windows).
    /// Rows with invalid JSON are skipped so one bad value cannot block
    /// bootstrap.
    pub async fn load_plugin_state(&self) -> Result<Vec<(String, serde_json::Value)>> {
        let rows = sqlx::query("SELECT key, value FROM llm_plugin_state")
            .fetch_all(self.pool())
            .await
            .map_err(|error| Error::Storage(format!("cannot load plugin state: {error}")))?;
        Ok(rows
            .into_iter()
            .filter_map(|row| {
                let key: String = row.get("key");
                let raw: String = row.get("value");
                match serde_json::from_str(&raw) {
                    Ok(value) => Some((key, value)),
                    Err(error) => {
                        tracing::warn!(key, %error, "skipping invalid plugin state row");
                        None
                    }
                }
            })
            .collect())
    }

    pub async fn upsert_plugin_state(&self, key: &str, value: &str) -> Result<()> {
        sqlx::query(
            "INSERT INTO llm_plugin_state (key, value, updated_at)
             VALUES (?1, ?2, strftime('%Y-%m-%dT%H:%M:%fZ', 'now'))
             ON CONFLICT(key) DO UPDATE SET
                value = excluded.value,
                updated_at = excluded.updated_at",
        )
        .bind(key)
        .bind(value)
        .execute(self.pool())
        .await
        .map_err(|error| Error::Storage(format!("cannot persist plugin state: {error}")))?;
        Ok(())
    }
}
