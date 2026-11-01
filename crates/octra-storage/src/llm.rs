//! LLM gateway persistence: providers, models, routing, virtual keys and usage.

use chrono::{DateTime, Utc};
use octra_core::{Error, Result};
use sqlx::Row;

use crate::Storage;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LlmProviderRecord {
    pub id: String,
    pub name: String,
    pub kind: String,
    pub protocol: String,
    pub auth_type: String,
    pub base_url: Option<String>,
    pub secret_ref: Option<String>,
    pub priority: i64,
    pub config_json: String,
    pub enabled: bool,
    pub created_at: String,
    pub updated_at: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LlmProviderInput {
    pub name: String,
    pub kind: String,
    pub protocol: String,
    pub auth_type: String,
    pub base_url: Option<String>,
    pub secret_ref: Option<String>,
    pub priority: i64,
    pub config_json: String,
    pub enabled: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LlmModelRecord {
    pub id: String,
    pub provider_id: String,
    pub model_id: String,
    pub display_name: Option<String>,
    pub protocol: Option<String>,
    pub context_window: Option<i64>,
    pub max_output_tokens: Option<i64>,
    pub capabilities: String,
    pub pricing_json: String,
    pub enabled: bool,
    pub created_at: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LlmModelInput {
    pub provider_id: String,
    pub model_id: String,
    pub display_name: Option<String>,
    pub protocol: Option<String>,
    pub context_window: Option<i64>,
    pub max_output_tokens: Option<i64>,
    pub capabilities: String,
    pub pricing_json: String,
    pub enabled: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LlmRoutingGroupRecord {
    pub id: String,
    pub name: String,
    pub strategy: String,
    pub created_at: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LlmRouteRecord {
    pub id: String,
    pub virtual_model_id: String,
    pub routing_group_id: Option<String>,
    pub created_at: String,
    pub updated_at: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LlmRoutingTargetRecord {
    pub id: String,
    pub group_id: String,
    pub provider_id: String,
    pub model_id: String,
    pub weight: i64,
    pub priority: i64,
    pub position: i64,
    pub created_at: String,
    pub updated_at: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LlmRoutingTargetInput {
    pub provider_id: String,
    pub model_id: String,
    pub weight: i64,
    pub priority: i64,
    pub position: i64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LlmVirtualKeyRecord {
    pub id: String,
    pub name: String,
    pub key_prefix: String,
    pub key_hash: String,
    pub consumer_id: Option<String>,
    pub enabled: bool,
    pub expires_at: Option<DateTime<Utc>>,
    pub last_used_at: Option<String>,
    pub permissions_json: String,
    pub created_at: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LlmVirtualKeyInput {
    pub name: String,
    pub key_prefix: String,
    pub key_hash: String,
    pub consumer_id: Option<String>,
    pub enabled: bool,
    pub expires_at: Option<DateTime<Utc>>,
    pub permissions_json: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LlmUsageRecord {
    pub id: i64,
    pub request_id: String,
    pub virtual_model_id: String,
    pub provider_id: Option<String>,
    pub model_id: Option<String>,
    pub api_key_id: Option<String>,
    pub protocol: Option<String>,
    pub status_code: Option<i64>,
    pub latency_ms: Option<i64>,
    pub input_tokens: i64,
    pub output_tokens: i64,
    pub error: Option<String>,
    pub created_at: String,
}

#[derive(Debug, Clone)]
pub struct LlmUsageSummaryRow {
    pub virtual_model_id: String,
    pub requests: i64,
    pub errors: i64,
    pub input_tokens: i64,
    pub output_tokens: i64,
}

/// Snapshot of the routing configuration used by the LLM data plane.
#[derive(Debug, Clone, Default)]
pub struct LlmRuntimeData {
    pub providers: Vec<LlmProviderRecord>,
    pub models: Vec<LlmModelRecord>,
    pub groups: Vec<LlmRoutingGroupRecord>,
    pub routes: Vec<LlmRouteRecord>,
    pub targets: Vec<LlmRoutingTargetRecord>,
    pub virtual_keys: Vec<LlmVirtualKeyRecord>,
}

#[derive(Debug)]
pub enum LlmStorageError {
    Conflict(String),
    Missing(String),
    Backend(String),
}

impl LlmStorageError {
    pub fn into_octra(self) -> Error {
        match self {
            LlmStorageError::Conflict(message) => Error::Storage(format!("conflict: {message}")),
            LlmStorageError::Missing(message) => Error::Storage(format!("not found: {message}")),
            LlmStorageError::Backend(message) => Error::Storage(message),
        }
    }
}

pub type LlmResult<T> = std::result::Result<T, LlmStorageError>;

fn backend(err: sqlx::Error) -> LlmStorageError {
    LlmStorageError::Backend(format!("sqlite: {err}"))
}

fn conflict(err: sqlx::Error) -> LlmStorageError {
    if let sqlx::Error::Database(db) = &err
        && db.is_unique_violation()
    {
        return LlmStorageError::Conflict(db.message().to_string());
    }
    backend(err)
}

fn not_found(kind: &str, id: &str) -> LlmStorageError {
    LlmStorageError::Missing(format!("{kind} {id} does not exist"))
}

fn missing_fk(err: &sqlx::Error) -> bool {
    match err {
        sqlx::Error::Database(db) => {
            let code = db.code().map(|code| code.into_owned()).unwrap_or_default();
            code.starts_with("787") || db.message().contains("FOREIGN KEY")
        }
        _ => false,
    }
}

fn provider_from_row(row: &sqlx::sqlite::SqliteRow) -> LlmProviderRecord {
    LlmProviderRecord {
        id: row.get("id"),
        name: row.get("name"),
        kind: row.get("kind"),
        protocol: row.get("protocol"),
        auth_type: row.get("auth_type"),
        base_url: row.get("base_url"),
        secret_ref: row.get("secret_ref"),
        priority: row.get("priority"),
        config_json: row.get("config_json"),
        enabled: row.get::<i64, _>("enabled") != 0,
        created_at: row.get("created_at"),
        updated_at: row.get("updated_at"),
    }
}

const PROVIDER_COLUMNS: &str = "id, name, kind, protocol, auth_type, base_url, secret_ref, \
     priority, config_json, enabled, created_at, updated_at";

fn model_from_row(row: &sqlx::sqlite::SqliteRow) -> LlmModelRecord {
    LlmModelRecord {
        id: row.get("id"),
        provider_id: row.get("provider_id"),
        model_id: row.get("model_id"),
        display_name: row.get("display_name"),
        protocol: row.get("protocol"),
        context_window: row.get("context_window"),
        max_output_tokens: row.get("max_output_tokens"),
        capabilities: row.get("capabilities"),
        pricing_json: row.get("pricing_json"),
        enabled: row.get::<i64, _>("enabled") != 0,
        created_at: row.get("created_at"),
    }
}

const MODEL_COLUMNS: &str = "id, provider_id, model_id, display_name, protocol, context_window, \
     max_output_tokens, capabilities, pricing_json, enabled, created_at";

fn group_from_row(row: &sqlx::sqlite::SqliteRow) -> LlmRoutingGroupRecord {
    LlmRoutingGroupRecord {
        id: row.get("id"),
        name: row.get("name"),
        strategy: row.get("strategy"),
        created_at: row.get("created_at"),
    }
}

fn route_from_row(row: &sqlx::sqlite::SqliteRow) -> LlmRouteRecord {
    LlmRouteRecord {
        id: row.get("id"),
        virtual_model_id: row.get("virtual_model_id"),
        routing_group_id: row.get("routing_group_id"),
        created_at: row.get("created_at"),
        updated_at: row.get("updated_at"),
    }
}

fn target_from_row(row: &sqlx::sqlite::SqliteRow) -> LlmRoutingTargetRecord {
    LlmRoutingTargetRecord {
        id: row.get("id"),
        group_id: row.get("group_id"),
        provider_id: row.get("provider_id"),
        model_id: row.get("model_id"),
        weight: row.get("weight"),
        priority: row.get("priority"),
        position: row.get("position"),
        created_at: row.get("created_at"),
        updated_at: row.get("updated_at"),
    }
}

fn virtual_key_from_row(row: &sqlx::sqlite::SqliteRow) -> LlmVirtualKeyRecord {
    LlmVirtualKeyRecord {
        id: row.get("id"),
        name: row.get("name"),
        key_prefix: row.get("key_prefix"),
        key_hash: row.get("key_hash"),
        consumer_id: row.get("consumer_id"),
        enabled: row.get::<i64, _>("enabled") != 0,
        expires_at: row
            .get::<Option<String>, _>("expires_at")
            .and_then(|value| DateTime::parse_from_rfc3339(&value).ok())
            .map(|value| value.with_timezone(&Utc)),
        last_used_at: row.get("last_used_at"),
        permissions_json: row.get("permissions_json"),
        created_at: row.get("created_at"),
    }
}

const VIRTUAL_KEY_COLUMNS: &str = "id, name, key_prefix, key_hash, consumer_id, enabled, \
     expires_at, last_used_at, permissions_json, created_at";

fn usage_from_row(row: &sqlx::sqlite::SqliteRow) -> LlmUsageRecord {
    LlmUsageRecord {
        id: row.get("id"),
        request_id: row.get("request_id"),
        virtual_model_id: row.get("virtual_model_id"),
        provider_id: row.get("provider_id"),
        model_id: row.get("model_id"),
        api_key_id: row.get("api_key_id"),
        protocol: row.get("protocol"),
        status_code: row.get("status_code"),
        latency_ms: row.get("latency_ms"),
        input_tokens: row.get("input_tokens"),
        output_tokens: row.get("output_tokens"),
        error: row.get("error"),
        created_at: row.get("created_at"),
    }
}

impl Storage {
    pub async fn llm_load_runtime_data(&self) -> Result<LlmRuntimeData> {
        let providers = self
            .llm_list_providers()
            .await
            .map_err(|err| err.into_octra())?;
        let models = self
            .llm_list_models()
            .await
            .map_err(|err| err.into_octra())?;
        let groups = self
            .llm_list_routing_groups()
            .await
            .map_err(|err| err.into_octra())?;
        let routes = self
            .llm_list_routes()
            .await
            .map_err(|err| err.into_octra())?;
        let targets = self
            .llm_list_routing_targets()
            .await
            .map_err(|err| err.into_octra())?;
        let virtual_keys = self
            .llm_list_virtual_keys()
            .await
            .map_err(|err| err.into_octra())?;
        Ok(LlmRuntimeData {
            providers,
            models,
            groups,
            routes,
            targets,
            virtual_keys,
        })
    }

    // Providers

    pub async fn llm_list_providers(&self) -> LlmResult<Vec<LlmProviderRecord>> {
        let sql =
            format!("SELECT {PROVIDER_COLUMNS} FROM llm_providers ORDER BY priority ASC, name ASC");
        let rows = sqlx::query(&sql)
            .fetch_all(self.pool())
            .await
            .map_err(backend)?;
        Ok(rows.iter().map(provider_from_row).collect())
    }

    pub async fn llm_get_provider(&self, id: &str) -> LlmResult<Option<LlmProviderRecord>> {
        let sql = format!("SELECT {PROVIDER_COLUMNS} FROM llm_providers WHERE id = ?");
        let row = sqlx::query(&sql)
            .bind(id)
            .fetch_optional(self.pool())
            .await
            .map_err(backend)?;
        Ok(row.as_ref().map(provider_from_row))
    }

    pub async fn llm_create_provider(
        &self,
        input: &LlmProviderInput,
    ) -> LlmResult<LlmProviderRecord> {
        let id = uuid::Uuid::new_v4().to_string();
        sqlx::query(
            "INSERT INTO llm_providers (id, name, kind, protocol, auth_type, base_url, secret_ref, priority, config_json, enabled)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(&id)
        .bind(&input.name)
        .bind(&input.kind)
        .bind(&input.protocol)
        .bind(&input.auth_type)
        .bind(&input.base_url)
        .bind(&input.secret_ref)
        .bind(input.priority)
        .bind(&input.config_json)
        .bind(input.enabled as i64)
        .execute(self.pool())
        .await
        .map_err(|err| {
            if missing_fk(&err) {
                LlmStorageError::Missing("referenced entity does not exist".into())
            } else {
                conflict(err)
            }
        })?;
        self.llm_get_provider(&id)
            .await?
            .ok_or_else(|| backend(sqlx::Error::RowNotFound))
    }

    pub async fn llm_update_provider(
        &self,
        id: &str,
        input: &LlmProviderInput,
    ) -> LlmResult<LlmProviderRecord> {
        let affected = sqlx::query(
            "UPDATE llm_providers SET name = ?, kind = ?, protocol = ?, auth_type = ?, base_url = ?,
                 secret_ref = ?, priority = ?, config_json = ?, enabled = ?,
                 updated_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now')
             WHERE id = ?",
        )
        .bind(&input.name)
        .bind(&input.kind)
        .bind(&input.protocol)
        .bind(&input.auth_type)
        .bind(&input.base_url)
        .bind(&input.secret_ref)
        .bind(input.priority)
        .bind(&input.config_json)
        .bind(input.enabled as i64)
        .bind(id)
        .execute(self.pool())
        .await
        .map_err(conflict)?
        .rows_affected();
        if affected == 0 {
            return Err(not_found("provider", id));
        }
        self.llm_get_provider(id)
            .await?
            .ok_or_else(|| not_found("provider", id))
    }

    pub async fn llm_delete_provider(&self, id: &str) -> LlmResult<bool> {
        let affected = sqlx::query("DELETE FROM llm_providers WHERE id = ?")
            .bind(id)
            .execute(self.pool())
            .await
            .map_err(backend)?
            .rows_affected();
        Ok(affected > 0)
    }

    pub async fn llm_provider_exists(&self, id: &str) -> LlmResult<bool> {
        let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM llm_providers WHERE id = ?")
            .bind(id)
            .fetch_one(self.pool())
            .await
            .map_err(backend)?;
        Ok(count > 0)
    }

    // Models

    pub async fn llm_list_models(&self) -> LlmResult<Vec<LlmModelRecord>> {
        let sql = format!(
            "SELECT {MODEL_COLUMNS} FROM llm_models ORDER BY provider_id ASC, model_id ASC"
        );
        let rows = sqlx::query(&sql)
            .fetch_all(self.pool())
            .await
            .map_err(backend)?;
        Ok(rows.iter().map(model_from_row).collect())
    }

    pub async fn llm_get_model(&self, id: &str) -> LlmResult<Option<LlmModelRecord>> {
        let sql = format!("SELECT {MODEL_COLUMNS} FROM llm_models WHERE id = ?");
        let row = sqlx::query(&sql)
            .bind(id)
            .fetch_optional(self.pool())
            .await
            .map_err(backend)?;
        Ok(row.as_ref().map(model_from_row))
    }

    pub async fn llm_create_model(&self, input: &LlmModelInput) -> LlmResult<LlmModelRecord> {
        if !self.llm_provider_exists(&input.provider_id).await? {
            return Err(not_found("provider", &input.provider_id));
        }
        let id = uuid::Uuid::new_v4().to_string();
        sqlx::query(
            "INSERT INTO llm_models (id, provider_id, model_id, display_name, protocol, context_window,
                 max_output_tokens, capabilities, pricing_json, enabled)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(&id)
        .bind(&input.provider_id)
        .bind(&input.model_id)
        .bind(&input.display_name)
        .bind(&input.protocol)
        .bind(input.context_window)
        .bind(input.max_output_tokens)
        .bind(&input.capabilities)
        .bind(&input.pricing_json)
        .bind(input.enabled as i64)
        .execute(self.pool())
        .await
        .map_err(conflict)?;
        self.llm_get_model(&id)
            .await?
            .ok_or_else(|| backend(sqlx::Error::RowNotFound))
    }

    pub async fn llm_update_model(
        &self,
        id: &str,
        input: &LlmModelInput,
    ) -> LlmResult<LlmModelRecord> {
        if !self.llm_provider_exists(&input.provider_id).await? {
            return Err(not_found("provider", &input.provider_id));
        }
        let affected = sqlx::query(
            "UPDATE llm_models SET provider_id = ?, model_id = ?, display_name = ?, protocol = ?,
                 context_window = ?, max_output_tokens = ?, capabilities = ?, pricing_json = ?, enabled = ?
             WHERE id = ?",
        )
        .bind(&input.provider_id)
        .bind(&input.model_id)
        .bind(&input.display_name)
        .bind(&input.protocol)
        .bind(input.context_window)
        .bind(input.max_output_tokens)
        .bind(&input.capabilities)
        .bind(&input.pricing_json)
        .bind(input.enabled as i64)
        .bind(id)
        .execute(self.pool())
        .await
        .map_err(conflict)?
        .rows_affected();
        if affected == 0 {
            return Err(not_found("model", id));
        }
        self.llm_get_model(id)
            .await?
            .ok_or_else(|| not_found("model", id))
    }

    pub async fn llm_delete_model(&self, id: &str) -> LlmResult<bool> {
        let affected = sqlx::query("DELETE FROM llm_models WHERE id = ?")
            .bind(id)
            .execute(self.pool())
            .await
            .map_err(backend)?
            .rows_affected();
        Ok(affected > 0)
    }

    // Routing groups

    pub async fn llm_list_routing_groups(&self) -> LlmResult<Vec<LlmRoutingGroupRecord>> {
        let rows = sqlx::query(
            "SELECT id, name, strategy, created_at FROM llm_routing_groups ORDER BY name ASC",
        )
        .fetch_all(self.pool())
        .await
        .map_err(backend)?;
        Ok(rows.iter().map(group_from_row).collect())
    }

    pub async fn llm_get_routing_group(
        &self,
        id: &str,
    ) -> LlmResult<Option<LlmRoutingGroupRecord>> {
        let row = sqlx::query(
            "SELECT id, name, strategy, created_at FROM llm_routing_groups WHERE id = ?",
        )
        .bind(id)
        .fetch_optional(self.pool())
        .await
        .map_err(backend)?;
        Ok(row.as_ref().map(group_from_row))
    }

    pub async fn llm_create_routing_group(
        &self,
        name: &str,
        strategy: &str,
    ) -> LlmResult<LlmRoutingGroupRecord> {
        let id = uuid::Uuid::new_v4().to_string();
        sqlx::query("INSERT INTO llm_routing_groups (id, name, strategy) VALUES (?, ?, ?)")
            .bind(&id)
            .bind(name)
            .bind(strategy)
            .execute(self.pool())
            .await
            .map_err(conflict)?;
        self.llm_get_routing_group(&id)
            .await?
            .ok_or_else(|| backend(sqlx::Error::RowNotFound))
    }

    pub async fn llm_update_routing_group(
        &self,
        id: &str,
        name: &str,
        strategy: &str,
    ) -> LlmResult<LlmRoutingGroupRecord> {
        let affected =
            sqlx::query("UPDATE llm_routing_groups SET name = ?, strategy = ? WHERE id = ?")
                .bind(name)
                .bind(strategy)
                .bind(id)
                .execute(self.pool())
                .await
                .map_err(conflict)?
                .rows_affected();
        if affected == 0 {
            return Err(not_found("routing group", id));
        }
        self.llm_get_routing_group(id)
            .await?
            .ok_or_else(|| not_found("routing group", id))
    }

    pub async fn llm_delete_routing_group(&self, id: &str) -> LlmResult<bool> {
        let affected = sqlx::query("DELETE FROM llm_routing_groups WHERE id = ?")
            .bind(id)
            .execute(self.pool())
            .await
            .map_err(backend)?
            .rows_affected();
        Ok(affected > 0)
    }

    pub async fn llm_group_exists(&self, id: &str) -> LlmResult<bool> {
        let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM llm_routing_groups WHERE id = ?")
            .bind(id)
            .fetch_one(self.pool())
            .await
            .map_err(backend)?;
        Ok(count > 0)
    }

    // Routing targets

    pub async fn llm_list_routing_targets(&self) -> LlmResult<Vec<LlmRoutingTargetRecord>> {
        let rows = sqlx::query(
            "SELECT id, group_id, provider_id, model_id, weight, priority, position, created_at, updated_at
             FROM llm_routing_targets ORDER BY priority ASC, position ASC",
        )
        .fetch_all(self.pool())
        .await
        .map_err(backend)?;
        Ok(rows.iter().map(target_from_row).collect())
    }

    pub async fn llm_get_routing_target(
        &self,
        id: &str,
    ) -> LlmResult<Option<LlmRoutingTargetRecord>> {
        let row = sqlx::query(
            "SELECT id, group_id, provider_id, model_id, weight, priority, position, created_at, updated_at
             FROM llm_routing_targets WHERE id = ?",
        )
        .bind(id)
        .fetch_optional(self.pool())
        .await
        .map_err(backend)?;
        Ok(row.as_ref().map(target_from_row))
    }

    pub async fn llm_create_routing_target(
        &self,
        group_id: &str,
        input: &LlmRoutingTargetInput,
    ) -> LlmResult<LlmRoutingTargetRecord> {
        if !self.llm_group_exists(group_id).await? {
            return Err(not_found("routing group", group_id));
        }
        if !self.llm_provider_exists(&input.provider_id).await? {
            return Err(not_found("provider", &input.provider_id));
        }
        let id = uuid::Uuid::new_v4().to_string();
        sqlx::query(
            "INSERT INTO llm_routing_targets (id, group_id, provider_id, model_id, weight, priority, position)
             VALUES (?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(&id)
        .bind(group_id)
        .bind(&input.provider_id)
        .bind(&input.model_id)
        .bind(input.weight)
        .bind(input.priority)
        .bind(input.position)
        .execute(self.pool())
        .await
        .map_err(conflict)?;
        self.llm_get_routing_target(&id)
            .await?
            .ok_or_else(|| backend(sqlx::Error::RowNotFound))
    }

    pub async fn llm_update_routing_target(
        &self,
        id: &str,
        input: &LlmRoutingTargetInput,
    ) -> LlmResult<LlmRoutingTargetRecord> {
        if !self.llm_provider_exists(&input.provider_id).await? {
            return Err(not_found("provider", &input.provider_id));
        }
        let affected = sqlx::query(
            "UPDATE llm_routing_targets SET provider_id = ?, model_id = ?, weight = ?, priority = ?, position = ?,
                 updated_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now')
             WHERE id = ?",
        )
        .bind(&input.provider_id)
        .bind(&input.model_id)
        .bind(input.weight)
        .bind(input.priority)
        .bind(input.position)
        .bind(id)
        .execute(self.pool())
        .await
        .map_err(conflict)?
        .rows_affected();
        if affected == 0 {
            return Err(not_found("routing target", id));
        }
        self.llm_get_routing_target(id)
            .await?
            .ok_or_else(|| not_found("routing target", id))
    }

    pub async fn llm_delete_routing_target(&self, id: &str) -> LlmResult<bool> {
        let affected = sqlx::query("DELETE FROM llm_routing_targets WHERE id = ?")
            .bind(id)
            .execute(self.pool())
            .await
            .map_err(backend)?
            .rows_affected();
        Ok(affected > 0)
    }

    // Routes

    pub async fn llm_list_routes(&self) -> LlmResult<Vec<LlmRouteRecord>> {
        let rows = sqlx::query(
            "SELECT id, virtual_model_id, routing_group_id, created_at, updated_at FROM llm_routes ORDER BY virtual_model_id ASC",
        )
        .fetch_all(self.pool())
        .await
        .map_err(backend)?;
        Ok(rows.iter().map(route_from_row).collect())
    }

    pub async fn llm_get_route(&self, id: &str) -> LlmResult<Option<LlmRouteRecord>> {
        let row = sqlx::query(
            "SELECT id, virtual_model_id, routing_group_id, created_at, updated_at FROM llm_routes WHERE id = ?",
        )
        .bind(id)
        .fetch_optional(self.pool())
        .await
        .map_err(backend)?;
        Ok(row.as_ref().map(route_from_row))
    }

    pub async fn llm_create_route(
        &self,
        virtual_model_id: &str,
        routing_group_id: Option<&str>,
    ) -> LlmResult<LlmRouteRecord> {
        if let Some(group_id) = routing_group_id
            && !self.llm_group_exists(group_id).await?
        {
            return Err(not_found("routing group", group_id));
        }
        let id = uuid::Uuid::new_v4().to_string();
        sqlx::query(
            "INSERT INTO llm_routes (id, virtual_model_id, routing_group_id) VALUES (?, ?, ?)",
        )
        .bind(&id)
        .bind(virtual_model_id)
        .bind(routing_group_id)
        .execute(self.pool())
        .await
        .map_err(conflict)?;
        self.llm_get_route(&id)
            .await?
            .ok_or_else(|| backend(sqlx::Error::RowNotFound))
    }

    pub async fn llm_update_route(
        &self,
        id: &str,
        virtual_model_id: &str,
        routing_group_id: Option<&str>,
    ) -> LlmResult<LlmRouteRecord> {
        if let Some(group_id) = routing_group_id
            && !self.llm_group_exists(group_id).await?
        {
            return Err(not_found("routing group", group_id));
        }
        let affected = sqlx::query(
            "UPDATE llm_routes SET virtual_model_id = ?, routing_group_id = ?,
                 updated_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now') WHERE id = ?",
        )
        .bind(virtual_model_id)
        .bind(routing_group_id)
        .bind(id)
        .execute(self.pool())
        .await
        .map_err(conflict)?
        .rows_affected();
        if affected == 0 {
            return Err(not_found("route", id));
        }
        self.llm_get_route(id)
            .await?
            .ok_or_else(|| not_found("route", id))
    }

    pub async fn llm_delete_route(&self, id: &str) -> LlmResult<bool> {
        let affected = sqlx::query("DELETE FROM llm_routes WHERE id = ?")
            .bind(id)
            .execute(self.pool())
            .await
            .map_err(backend)?
            .rows_affected();
        Ok(affected > 0)
    }

    // Virtual keys

    pub async fn llm_list_virtual_keys(&self) -> LlmResult<Vec<LlmVirtualKeyRecord>> {
        let sql = format!(
            "SELECT {VIRTUAL_KEY_COLUMNS} FROM llm_virtual_keys ORDER BY created_at DESC, rowid DESC"
        );
        let rows = sqlx::query(&sql)
            .fetch_all(self.pool())
            .await
            .map_err(backend)?;
        Ok(rows.iter().map(virtual_key_from_row).collect())
    }

    pub async fn llm_get_virtual_key(&self, id: &str) -> LlmResult<Option<LlmVirtualKeyRecord>> {
        let sql = format!("SELECT {VIRTUAL_KEY_COLUMNS} FROM llm_virtual_keys WHERE id = ?");
        let row = sqlx::query(&sql)
            .bind(id)
            .fetch_optional(self.pool())
            .await
            .map_err(backend)?;
        Ok(row.as_ref().map(virtual_key_from_row))
    }

    pub async fn llm_find_virtual_key_by_hash(
        &self,
        key_hash: &str,
    ) -> LlmResult<Option<LlmVirtualKeyRecord>> {
        let sql = format!("SELECT {VIRTUAL_KEY_COLUMNS} FROM llm_virtual_keys WHERE key_hash = ?");
        let row = sqlx::query(&sql)
            .bind(key_hash)
            .fetch_optional(self.pool())
            .await
            .map_err(backend)?;
        Ok(row.as_ref().map(virtual_key_from_row))
    }

    pub async fn llm_create_virtual_key(
        &self,
        input: &LlmVirtualKeyInput,
    ) -> LlmResult<LlmVirtualKeyRecord> {
        if let Some(consumer_id) = &input.consumer_id {
            let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM consumers WHERE id = ?")
                .bind(consumer_id)
                .fetch_one(self.pool())
                .await
                .map_err(backend)?;
            if count == 0 {
                return Err(not_found("consumer", consumer_id));
            }
        }
        let id = uuid::Uuid::new_v4().to_string();
        sqlx::query(
            "INSERT INTO llm_virtual_keys (id, name, key_prefix, key_hash, consumer_id, enabled, expires_at, permissions_json)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(&id)
        .bind(&input.name)
        .bind(&input.key_prefix)
        .bind(&input.key_hash)
        .bind(&input.consumer_id)
        .bind(input.enabled as i64)
        .bind(input.expires_at.map(|value| value.to_rfc3339()))
        .bind(&input.permissions_json)
        .execute(self.pool())
        .await
        .map_err(conflict)?;
        self.llm_get_virtual_key(&id)
            .await?
            .ok_or_else(|| backend(sqlx::Error::RowNotFound))
    }

    pub async fn llm_update_virtual_key(
        &self,
        id: &str,
        input: &LlmVirtualKeyInput,
    ) -> LlmResult<LlmVirtualKeyRecord> {
        let affected = sqlx::query(
            "UPDATE llm_virtual_keys SET name = ?, consumer_id = ?, enabled = ?, expires_at = ?, permissions_json = ?
             WHERE id = ?",
        )
        .bind(&input.name)
        .bind(&input.consumer_id)
        .bind(input.enabled as i64)
        .bind(input.expires_at.map(|value| value.to_rfc3339()))
        .bind(&input.permissions_json)
        .bind(id)
        .execute(self.pool())
        .await
        .map_err(conflict)?
        .rows_affected();
        if affected == 0 {
            return Err(not_found("virtual key", id));
        }
        self.llm_get_virtual_key(id)
            .await?
            .ok_or_else(|| not_found("virtual key", id))
    }

    pub async fn llm_delete_virtual_key(&self, id: &str) -> LlmResult<bool> {
        let affected = sqlx::query("DELETE FROM llm_virtual_keys WHERE id = ?")
            .bind(id)
            .execute(self.pool())
            .await
            .map_err(backend)?
            .rows_affected();
        Ok(affected > 0)
    }

    pub async fn llm_touch_virtual_key(&self, id: &str) -> LlmResult<()> {
        sqlx::query("UPDATE llm_virtual_keys SET last_used_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now') WHERE id = ?")
            .bind(id)
            .execute(self.pool())
            .await
            .map_err(backend)?;
        Ok(())
    }

    // Usage

    #[allow(clippy::too_many_arguments)]
    pub async fn llm_insert_usage(
        &self,
        request_id: &str,
        virtual_model_id: &str,
        provider_id: Option<&str>,
        model_id: Option<&str>,
        api_key_id: Option<&str>,
        protocol: Option<&str>,
        status_code: Option<i64>,
        latency_ms: Option<i64>,
        input_tokens: i64,
        output_tokens: i64,
        error: Option<&str>,
    ) -> LlmResult<()> {
        sqlx::query(
            "INSERT INTO llm_usage (request_id, virtual_model_id, provider_id, model_id, api_key_id,
                 protocol, status_code, latency_ms, input_tokens, output_tokens, error)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(request_id)
        .bind(virtual_model_id)
        .bind(provider_id)
        .bind(model_id)
        .bind(api_key_id)
        .bind(protocol)
        .bind(status_code)
        .bind(latency_ms)
        .bind(input_tokens)
        .bind(output_tokens)
        .bind(error)
        .execute(self.pool())
        .await
        .map_err(backend)?;
        Ok(())
    }

    pub async fn llm_list_usage(&self, limit: i64) -> LlmResult<Vec<LlmUsageRecord>> {
        let rows = sqlx::query(
            "SELECT id, request_id, virtual_model_id, provider_id, model_id, api_key_id, protocol,
                 status_code, latency_ms, input_tokens, output_tokens, error, created_at
             FROM llm_usage ORDER BY id DESC LIMIT ?",
        )
        .bind(limit.clamp(1, 1000))
        .fetch_all(self.pool())
        .await
        .map_err(backend)?;
        Ok(rows.iter().map(usage_from_row).collect())
    }

    pub async fn llm_usage_summary(&self) -> LlmResult<Vec<LlmUsageSummaryRow>> {
        let rows = sqlx::query(
            "SELECT virtual_model_id, COUNT(*) AS requests,
                 SUM(CASE WHEN error IS NOT NULL OR status_code >= 400 THEN 1 ELSE 0 END) AS errors,
                 SUM(input_tokens) AS input_tokens, SUM(output_tokens) AS output_tokens
             FROM llm_usage GROUP BY virtual_model_id ORDER BY requests DESC",
        )
        .fetch_all(self.pool())
        .await
        .map_err(backend)?;
        Ok(rows
            .iter()
            .map(|row| LlmUsageSummaryRow {
                virtual_model_id: row.get("virtual_model_id"),
                requests: row.get("requests"),
                errors: row.get::<Option<i64>, _>("errors").unwrap_or(0),
                input_tokens: row.get::<Option<i64>, _>("input_tokens").unwrap_or(0),
                output_tokens: row.get::<Option<i64>, _>("output_tokens").unwrap_or(0),
            })
            .collect())
    }

    pub async fn llm_usage_totals(&self) -> LlmResult<LlmUsageSummaryRow> {
        let row = sqlx::query(
            "SELECT COUNT(*) AS requests,
                 SUM(CASE WHEN error IS NOT NULL OR status_code >= 400 THEN 1 ELSE 0 END) AS errors,
                 SUM(input_tokens) AS input_tokens, SUM(output_tokens) AS output_tokens
             FROM llm_usage",
        )
        .fetch_one(self.pool())
        .await
        .map_err(backend)?;
        Ok(LlmUsageSummaryRow {
            virtual_model_id: String::new(),
            requests: row.get("requests"),
            errors: row.get::<Option<i64>, _>("errors").unwrap_or(0),
            input_tokens: row.get::<Option<i64>, _>("input_tokens").unwrap_or(0),
            output_tokens: row.get::<Option<i64>, _>("output_tokens").unwrap_or(0),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn storage() -> Storage {
        Storage::connect_in_memory().await.unwrap()
    }

    fn provider(name: &str) -> LlmProviderInput {
        LlmProviderInput {
            name: name.to_string(),
            kind: "openai".to_string(),
            protocol: "openai".to_string(),
            auth_type: "bearer".to_string(),
            base_url: Some("https://api.openai.com/v1".to_string()),
            secret_ref: Some("env:OPENAI_API_KEY".to_string()),
            priority: 0,
            config_json: "{}".to_string(),
            enabled: true,
        }
    }

    #[tokio::test]
    async fn provider_crud_round_trip() {
        let storage = storage().await;
        let created = storage
            .llm_create_provider(&provider("openai"))
            .await
            .unwrap();
        assert_eq!(created.protocol, "openai");
        assert!(created.enabled);

        assert_eq!(storage.llm_list_providers().await.unwrap().len(), 1);

        // Duplicate names conflict.
        assert!(matches!(
            storage.llm_create_provider(&provider("openai")).await,
            Err(LlmStorageError::Conflict(_))
        ));

        let mut update = provider("openai-main");
        update.priority = 5;
        update.enabled = false;
        let updated = storage
            .llm_update_provider(&created.id, &update)
            .await
            .unwrap();
        assert_eq!(updated.name, "openai-main");
        assert_eq!(updated.priority, 5);
        assert!(!updated.enabled);

        assert!(matches!(
            storage.llm_update_provider("missing", &update).await,
            Err(LlmStorageError::Missing(_))
        ));

        assert!(storage.llm_delete_provider(&created.id).await.unwrap());
        assert!(!storage.llm_delete_provider(&created.id).await.unwrap());
    }

    #[tokio::test]
    async fn model_requires_existing_provider() {
        let storage = storage().await;
        let mut model = LlmModelInput {
            provider_id: "ghost".to_string(),
            model_id: "gpt-4o".to_string(),
            display_name: None,
            protocol: None,
            context_window: None,
            max_output_tokens: None,
            capabilities: "{}".to_string(),
            pricing_json: "{}".to_string(),
            enabled: true,
        };
        assert!(matches!(
            storage.llm_create_model(&model).await,
            Err(LlmStorageError::Missing(_))
        ));

        let created_provider = storage
            .llm_create_provider(&provider("openai"))
            .await
            .unwrap();
        model.provider_id = created_provider.id.clone();
        let created = storage.llm_create_model(&model).await.unwrap();
        assert_eq!(created.model_id, "gpt-4o");
        assert!(matches!(
            storage.llm_create_model(&model).await,
            Err(LlmStorageError::Conflict(_))
        ));

        assert_eq!(storage.llm_list_models().await.unwrap().len(), 1);
        assert!(storage.llm_delete_model(&created.id).await.unwrap());

        // Deleting the provider cascades to its models.
        let model2 = storage.llm_create_model(&model).await.unwrap();
        storage
            .llm_delete_provider(&created_provider.id)
            .await
            .unwrap();
        assert!(storage.llm_get_model(&model2.id).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn routing_group_targets_and_routes() {
        let storage = storage().await;
        let provider = storage
            .llm_create_provider(&provider("openai"))
            .await
            .unwrap();
        let group = storage
            .llm_create_routing_group("primary", "priority")
            .await
            .unwrap();

        let target = storage
            .llm_create_routing_target(
                &group.id,
                &LlmRoutingTargetInput {
                    provider_id: provider.id.clone(),
                    model_id: "gpt-4o".to_string(),
                    weight: 2,
                    priority: 1,
                    position: 0,
                },
            )
            .await
            .unwrap();
        assert_eq!(target.weight, 2);
        assert!(target.group_id == group.id);

        assert!(matches!(
            storage
                .llm_create_routing_target(
                    "ghost-group",
                    &LlmRoutingTargetInput {
                        provider_id: provider.id.clone(),
                        model_id: "gpt-4o".to_string(),
                        weight: 1,
                        priority: 0,
                        position: 0,
                    }
                )
                .await,
            Err(LlmStorageError::Missing(_))
        ));

        let route = storage
            .llm_create_route("octra-default", Some(&group.id))
            .await
            .unwrap();
        assert_eq!(route.virtual_model_id, "octra-default");
        assert!(matches!(
            storage.llm_create_route("octra-default", None).await,
            Err(LlmStorageError::Conflict(_))
        ));
        assert!(matches!(
            storage.llm_create_route("other", Some("ghost")).await,
            Err(LlmStorageError::Missing(_))
        ));

        let updated = storage
            .llm_update_route(&route.id, "octra-default", None)
            .await
            .unwrap();
        assert!(updated.routing_group_id.is_none());

        let data = storage.llm_load_runtime_data().await.unwrap();
        assert_eq!(data.providers.len(), 1);
        assert_eq!(data.targets.len(), 1);
        assert_eq!(data.routes.len(), 1);

        // Deleting the group cascades to targets and nulls the route reference.
        storage.llm_delete_routing_group(&group.id).await.unwrap();
        let data = storage.llm_load_runtime_data().await.unwrap();
        assert!(data.groups.is_empty());
        assert!(data.targets.is_empty());
    }

    #[tokio::test]
    async fn virtual_keys_and_usage() {
        let storage = storage().await;
        let key = storage
            .llm_create_virtual_key(&LlmVirtualKeyInput {
                name: "default".to_string(),
                key_prefix: "octra-abc".to_string(),
                key_hash: "deadbeef".to_string(),
                consumer_id: None,
                enabled: true,
                expires_at: None,
                permissions_json: "{}".to_string(),
            })
            .await
            .unwrap();
        assert_eq!(key.key_prefix, "octra-abc");
        assert!(key.expires_at.is_none());

        let found = storage
            .llm_find_virtual_key_by_hash("deadbeef")
            .await
            .unwrap();
        assert_eq!(found.unwrap().id, key.id);

        storage.llm_touch_virtual_key(&key.id).await.unwrap();
        let touched = storage.llm_get_virtual_key(&key.id).await.unwrap().unwrap();
        assert!(touched.last_used_at.is_some());

        storage
            .llm_insert_usage(
                "req-1",
                "octra-default",
                Some("prov-1"),
                Some("gpt-4o"),
                Some(&key.id),
                Some("openai"),
                Some(200),
                Some(12),
                100,
                50,
                None,
            )
            .await
            .unwrap();
        storage
            .llm_insert_usage(
                "req-2",
                "octra-default",
                None,
                None,
                None,
                Some("anthropic"),
                Some(502),
                Some(3),
                0,
                0,
                Some("upstream failed"),
            )
            .await
            .unwrap();

        let usage = storage.llm_list_usage(10).await.unwrap();
        assert_eq!(usage.len(), 2);
        assert_eq!(usage[0].request_id, "req-2");
        assert_eq!(usage[1].input_tokens, 100);

        let summary = storage.llm_usage_summary().await.unwrap();
        assert_eq!(summary.len(), 1);
        assert_eq!(summary[0].requests, 2);
        assert_eq!(summary[0].errors, 1);
        assert_eq!(summary[0].input_tokens, 100);
        assert_eq!(summary[0].output_tokens, 50);

        let totals = storage.llm_usage_totals().await.unwrap();
        assert_eq!(totals.requests, 2);

        assert!(storage.llm_delete_virtual_key(&key.id).await.unwrap());
        assert!(
            storage
                .llm_find_virtual_key_by_hash("deadbeef")
                .await
                .unwrap()
                .is_none()
        );
    }
}
