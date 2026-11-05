//! Local agent profiles and configuration backups.

use octra_core::{Error, Result};
use serde::Serialize;
use sqlx::Row;
use sqlx::sqlite::SqliteRow;

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct AgentProfile {
    pub id: String,
    pub agent_id: String,
    pub name: String,
    pub base_url: String,
    pub virtual_model_id: String,
    pub virtual_key_id: Option<String>,
    pub config_path: Option<String>,
    pub created_at: String,
    pub updated_at: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewAgentProfile {
    pub agent_id: String,
    pub name: String,
    pub base_url: String,
    pub virtual_model_id: String,
    pub virtual_key_id: Option<String>,
    pub config_path: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct AgentBackup {
    pub id: String,
    pub agent_id: String,
    pub profile_id: Option<String>,
    pub path: String,
    /// Original file content. Never returned by the control plane.
    #[serde(skip)]
    pub original_content: Option<String>,
    pub original_sha256: String,
    pub applied_sha256: String,
    pub file_existed: bool,
    pub created_at: String,
    pub restored_at: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewAgentBackup {
    pub agent_id: String,
    pub profile_id: Option<String>,
    pub path: String,
    pub original_content: Option<String>,
    pub original_sha256: String,
    pub applied_sha256: String,
    pub file_existed: bool,
}

#[derive(Clone)]
pub struct AgentStore {
    pool: sqlx::SqlitePool,
}

impl AgentStore {
    pub(crate) fn new(pool: sqlx::SqlitePool) -> Self {
        Self { pool }
    }

    fn pool(&self) -> &sqlx::SqlitePool {
        &self.pool
    }

    pub async fn list_profiles(&self, agent_id: Option<&str>) -> Result<Vec<AgentProfile>> {
        let rows = sqlx::query(
            "SELECT id, agent_id, name, endpoint AS base_url,
                    model_id AS virtual_model_id, virtual_key_id,
                    config_path, created_at, updated_at
               FROM agent_profiles
              WHERE (?1 IS NULL OR agent_id = ?1)
              ORDER BY agent_id ASC, updated_at DESC",
        )
        .bind(agent_id)
        .fetch_all(self.pool())
        .await
        .map_err(backend)?;
        rows.iter().map(profile_from_row).collect()
    }

    pub async fn get_profile(&self, id: &str) -> Result<Option<AgentProfile>> {
        let row = sqlx::query(
            "SELECT id, agent_id, name, endpoint AS base_url,
                    model_id AS virtual_model_id, virtual_key_id,
                    config_path, created_at, updated_at
               FROM agent_profiles WHERE id = ?1",
        )
        .bind(id)
        .fetch_optional(self.pool())
        .await
        .map_err(backend)?;
        row.as_ref().map(profile_from_row).transpose()
    }

    pub async fn active_profile(&self, agent_id: &str) -> Result<Option<AgentProfile>> {
        let row = sqlx::query(
            "SELECT id, agent_id, name, endpoint AS base_url,
                    model_id AS virtual_model_id, virtual_key_id,
                    config_path, created_at, updated_at
               FROM agent_profiles
              WHERE agent_id = ?1
              ORDER BY updated_at DESC
              LIMIT 1",
        )
        .bind(agent_id)
        .fetch_optional(self.pool())
        .await
        .map_err(backend)?;
        row.as_ref().map(profile_from_row).transpose()
    }

    pub async fn create_profile(&self, input: &NewAgentProfile) -> Result<AgentProfile> {
        let id = uuid::Uuid::new_v4().to_string();
        sqlx::query(
            "INSERT INTO agent_profiles
                (id, agent_id, name, endpoint, model_id, virtual_key_id, config_path)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        )
        .bind(&id)
        .bind(&input.agent_id)
        .bind(&input.name)
        .bind(&input.base_url)
        .bind(&input.virtual_model_id)
        .bind(&input.virtual_key_id)
        .bind(&input.config_path)
        .execute(self.pool())
        .await
        .map_err(backend)?;

        self.get_profile(&id)
            .await?
            .ok_or_else(|| Error::Storage("agent profile disappeared after insert".to_string()))
    }

    pub async fn update_profile(
        &self,
        id: &str,
        input: &NewAgentProfile,
    ) -> Result<Option<AgentProfile>> {
        let affected = sqlx::query(
            "UPDATE agent_profiles
                SET agent_id = ?2,
                    name = ?3,
                    endpoint = ?4,
                    model_id = ?5,
                    virtual_key_id = ?6,
                    config_path = ?7,
                    updated_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now')
              WHERE id = ?1",
        )
        .bind(id)
        .bind(&input.agent_id)
        .bind(&input.name)
        .bind(&input.base_url)
        .bind(&input.virtual_model_id)
        .bind(&input.virtual_key_id)
        .bind(&input.config_path)
        .execute(self.pool())
        .await
        .map_err(backend)?;

        if affected.rows_affected() == 0 {
            return Ok(None);
        }
        self.get_profile(id).await
    }

    pub async fn delete_profile(&self, id: &str) -> Result<bool> {
        let affected = sqlx::query("DELETE FROM agent_profiles WHERE id = ?1")
            .bind(id)
            .execute(self.pool())
            .await
            .map_err(backend)?;
        Ok(affected.rows_affected() > 0)
    }

    pub async fn insert_backup(&self, input: &NewAgentBackup) -> Result<AgentBackup> {
        let id = uuid::Uuid::new_v4().to_string();
        sqlx::query(
            "INSERT INTO agent_config_backups
                (id, agent_id, profile_id, path, content, original_sha256,
                 applied_sha256, file_existed)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
        )
        .bind(&id)
        .bind(&input.agent_id)
        .bind(&input.profile_id)
        .bind(&input.path)
        .bind(input.original_content.as_deref().unwrap_or_default())
        .bind(&input.original_sha256)
        .bind(&input.applied_sha256)
        .bind(input.file_existed)
        .execute(self.pool())
        .await
        .map_err(backend)?;

        self.get_backup(&id)
            .await?
            .ok_or_else(|| Error::Storage("agent backup disappeared after insert".to_string()))
    }

    pub async fn get_backup(&self, id: &str) -> Result<Option<AgentBackup>> {
        let row = sqlx::query(
            "SELECT id, agent_id, profile_id, path, content AS original_content,
                    original_sha256, applied_sha256, file_existed, created_at, restored_at
               FROM agent_config_backups WHERE id = ?1",
        )
        .bind(id)
        .fetch_optional(self.pool())
        .await
        .map_err(backend)?;
        row.as_ref().map(backup_from_row).transpose()
    }

    pub async fn list_backups(&self, agent_id: &str, limit: u32) -> Result<Vec<AgentBackup>> {
        let rows = sqlx::query(
            "SELECT id, agent_id, profile_id, path, content AS original_content,
                    original_sha256, applied_sha256, file_existed, created_at, restored_at
               FROM agent_config_backups
              WHERE agent_id = ?1
              ORDER BY created_at DESC
              LIMIT ?2",
        )
        .bind(agent_id)
        .bind(i64::from(limit.clamp(1, 100)))
        .fetch_all(self.pool())
        .await
        .map_err(backend)?;
        rows.iter().map(backup_from_row).collect()
    }

    pub async fn latest_backup(&self, agent_id: &str) -> Result<Option<AgentBackup>> {
        let row = sqlx::query(
            "SELECT id, agent_id, profile_id, path, content AS original_content,
                    original_sha256, applied_sha256, file_existed, created_at, restored_at
               FROM agent_config_backups
              WHERE agent_id = ?1 AND restored_at IS NULL
              ORDER BY created_at DESC
              LIMIT 1",
        )
        .bind(agent_id)
        .fetch_optional(self.pool())
        .await
        .map_err(backend)?;
        row.as_ref().map(backup_from_row).transpose()
    }

    pub async fn mark_restored(&self, id: &str) -> Result<()> {
        sqlx::query(
            "UPDATE agent_config_backups
                SET restored_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now')
              WHERE id = ?1 AND restored_at IS NULL",
        )
        .bind(id)
        .execute(self.pool())
        .await
        .map_err(backend)?;
        Ok(())
    }
}

fn profile_from_row(row: &SqliteRow) -> Result<AgentProfile> {
    Ok(AgentProfile {
        id: row.get("id"),
        agent_id: row.get("agent_id"),
        name: row.get("name"),
        base_url: row.get("base_url"),
        virtual_model_id: row.get("virtual_model_id"),
        virtual_key_id: row.get("virtual_key_id"),
        config_path: row.get("config_path"),
        created_at: row.get("created_at"),
        updated_at: row.get("updated_at"),
    })
}

fn backup_from_row(row: &SqliteRow) -> Result<AgentBackup> {
    let file_existed = row.get::<i64, _>("file_existed") != 0;
    let content: Option<String> = row.get("original_content");
    Ok(AgentBackup {
        id: row.get("id"),
        agent_id: row.get("agent_id"),
        profile_id: row.get("profile_id"),
        path: row.get("path"),
        original_content: if file_existed { content } else { None },
        original_sha256: row.get("original_sha256"),
        applied_sha256: row.get("applied_sha256"),
        file_existed,
        created_at: row.get("created_at"),
        restored_at: row.get("restored_at"),
    })
}

fn backend(error: sqlx::Error) -> Error {
    Error::Storage(format!("agent storage error: {error}"))
}

#[cfg(test)]
mod tests {
    use octra_core::Result;

    use super::{NewAgentBackup, NewAgentProfile};
    use crate::Storage;

    async fn storage() -> Result<Storage> {
        Storage::connect_in_memory().await
    }

    fn profile() -> NewAgentProfile {
        NewAgentProfile {
            agent_id: "claude-code".to_string(),
            name: "default".to_string(),
            base_url: "http://127.0.0.1:8080".to_string(),
            virtual_model_id: "octra-default".to_string(),
            virtual_key_id: Some("key-1".to_string()),
            config_path: None,
        }
    }

    #[tokio::test]
    async fn profiles_round_trip_and_delete() {
        let storage = storage().await.unwrap();
        let created = storage.agents.create_profile(&profile()).await.unwrap();
        assert_eq!(created.agent_id, "claude-code");

        let active = storage
            .agents
            .active_profile("claude-code")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(active.id, created.id);

        let mut update = profile();
        update.name = "work".to_string();
        let updated = storage
            .agents
            .update_profile(&created.id, &update)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(updated.name, "work");

        assert!(storage.agents.delete_profile(&created.id).await.unwrap());
        assert!(
            storage
                .agents
                .get_profile(&created.id)
                .await
                .unwrap()
                .is_none()
        );
    }

    #[tokio::test]
    async fn backups_keep_content_and_track_restoration() {
        let storage = storage().await.unwrap();
        let backup = storage
            .agents
            .insert_backup(&NewAgentBackup {
                agent_id: "codex".to_string(),
                profile_id: None,
                path: "/tmp/config.toml".to_string(),
                original_content: Some("model = 'old'".to_string()),
                original_sha256: "old-hash".to_string(),
                applied_sha256: "new-hash".to_string(),
                file_existed: true,
            })
            .await
            .unwrap();

        let latest = storage
            .agents
            .latest_backup("codex")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(latest.id, backup.id);
        assert_eq!(latest.original_content.as_deref(), Some("model = 'old'"));

        storage.agents.mark_restored(&backup.id).await.unwrap();
        assert!(
            storage
                .agents
                .latest_backup("codex")
                .await
                .unwrap()
                .is_none()
        );
        assert!(
            storage
                .agents
                .get_backup(&backup.id)
                .await
                .unwrap()
                .unwrap()
                .restored_at
                .is_some()
        );
    }
}
