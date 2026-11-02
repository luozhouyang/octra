//! LLM gateway persistence: providers, models, routing, virtual keys and usage.

use chrono::{DateTime, SecondsFormat, Utc};
use octra_api_core::Policy;
use octra_core::{Error, Result};
use sqlx::{Row, SqlitePool};

#[derive(Clone)]
pub struct LlmStore {
    pub(crate) pool: SqlitePool,
}

impl LlmStore {
    pub(crate) fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }

    pub(crate) fn pool(&self) -> &SqlitePool {
        &self.pool
    }
}

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
    pub account_id: Option<String>,
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
    pub account_id: Option<String>,
    pub weight: i64,
    pub priority: i64,
    pub position: i64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LlmProviderAccountRecord {
    pub id: String,
    pub provider_id: String,
    pub name: String,
    pub secret_ref: Option<String>,
    pub weight: i64,
    pub priority: i64,
    pub enabled: bool,
    pub quota_limit: Option<i64>,
    pub quota_unit: String,
    pub quota_period: String,
    pub quota_used: i64,
    pub quota_reset_at: Option<String>,
    pub cooldown_until: Option<String>,
    pub last_error: Option<String>,
    pub config_json: String,
    pub created_at: String,
    pub updated_at: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LlmProviderAccountInput {
    pub name: String,
    pub secret_ref: Option<String>,
    pub weight: i64,
    pub priority: i64,
    pub enabled: bool,
    pub quota_limit: Option<i64>,
    pub quota_unit: String,
    pub quota_period: String,
    pub config_json: String,
}

/// Aggregated usage for one account over a quota period.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LlmAccountUsage {
    pub requests: i64,
    pub tokens: i64,
    pub cost_micros: i64,
}

/// Aggregated usage for one virtual key over a limit window.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LlmVirtualKeyUsage {
    pub tokens: i64,
    pub cost_micros: i64,
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
    pub account_id: Option<String>,
    pub account_name: Option<String>,
    pub api_key_id: Option<String>,
    pub protocol: Option<String>,
    pub status_code: Option<i64>,
    pub latency_ms: Option<i64>,
    pub input_tokens: i64,
    pub output_tokens: i64,
    pub cache_read_tokens: Option<i64>,
    pub cache_write_tokens: Option<i64>,
    pub reasoning_tokens: Option<i64>,
    pub cost_micros: Option<i64>,
    pub cost_currency: Option<String>,
    pub finish_reason: Option<String>,
    pub error: Option<String>,
    pub created_at: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LlmUsageInput {
    pub request_id: String,
    pub virtual_model_id: String,
    pub provider_id: Option<String>,
    pub model_id: Option<String>,
    pub account_id: Option<String>,
    pub account_name: Option<String>,
    pub api_key_id: Option<String>,
    pub protocol: Option<String>,
    pub status_code: Option<i64>,
    pub latency_ms: Option<i64>,
    pub input_tokens: i64,
    pub output_tokens: i64,
    pub cache_read_tokens: Option<i64>,
    pub cache_write_tokens: Option<i64>,
    pub reasoning_tokens: Option<i64>,
    pub cost_micros: Option<i64>,
    pub cost_currency: Option<String>,
    pub finish_reason: Option<String>,
    pub error: Option<String>,
    /// Request receipt time. `None` stamps the row when it is written.
    pub created_at: Option<String>,
}

/// Canonical usage timestamp; matches SQLite's
/// `strftime('%Y-%m-%dT%H:%M:%fZ', 'now')`.
pub fn canonical_timestamp(at: DateTime<Utc>) -> String {
    at.to_rfc3339_opts(SecondsFormat::Millis, true)
}

fn hour_bucket(timestamp: &str) -> LlmResult<String> {
    DateTime::parse_from_rfc3339(timestamp)
        .map(|at| {
            at.with_timezone(&Utc)
                .format("%Y-%m-%dT%H:00:00.000Z")
                .to_string()
        })
        .map_err(|error| {
            LlmStorageError::Backend(format!("invalid usage timestamp '{timestamp}': {error}"))
        })
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
    pub policies: Vec<Policy>,
    pub models: Vec<LlmModelRecord>,
    pub groups: Vec<LlmRoutingGroupRecord>,
    pub routes: Vec<LlmRouteRecord>,
    pub targets: Vec<LlmRoutingTargetRecord>,
    pub accounts: Vec<LlmProviderAccountRecord>,
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

fn validate_account_input(input: &LlmProviderAccountInput) -> LlmResult<()> {
    if input.name.trim().is_empty() {
        return Err(LlmStorageError::Backend(
            "account name must not be empty".to_string(),
        ));
    }
    if !(1..=1000).contains(&input.weight) {
        return Err(LlmStorageError::Backend(
            "account weight must be between 1 and 1000".to_string(),
        ));
    }
    if !matches!(
        input.quota_unit.as_str(),
        "requests" | "tokens" | "cost_micros"
    ) {
        return Err(LlmStorageError::Backend(format!(
            "unknown quota unit '{}'",
            input.quota_unit
        )));
    }
    if !matches!(input.quota_period.as_str(), "none" | "day" | "month") {
        return Err(LlmStorageError::Backend(format!(
            "unknown quota period '{}'",
            input.quota_period
        )));
    }
    if input.quota_limit.is_some_and(|limit| limit < 0) {
        return Err(LlmStorageError::Backend(
            "quota limit must not be negative".to_string(),
        ));
    }
    Ok(())
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
        account_id: row.get("account_id"),
        weight: row.get("weight"),
        priority: row.get("priority"),
        position: row.get("position"),
        created_at: row.get("created_at"),
        updated_at: row.get("updated_at"),
    }
}

fn account_from_row(row: &sqlx::sqlite::SqliteRow) -> LlmProviderAccountRecord {
    LlmProviderAccountRecord {
        id: row.get("id"),
        provider_id: row.get("provider_id"),
        name: row.get("name"),
        secret_ref: row.get("secret_ref"),
        weight: row.get("weight"),
        priority: row.get("priority"),
        enabled: row.get::<i64, _>("enabled") != 0,
        quota_limit: row.get("quota_limit"),
        quota_unit: row.get("quota_unit"),
        quota_period: row.get("quota_period"),
        quota_used: row.get("quota_used"),
        quota_reset_at: row.get("quota_reset_at"),
        cooldown_until: row.get("cooldown_until"),
        last_error: row.get("last_error"),
        config_json: row.get("config_json"),
        created_at: row.get("created_at"),
        updated_at: row.get("updated_at"),
    }
}

const ACCOUNT_COLUMNS: &str = "id, provider_id, name, secret_ref, weight, priority, enabled, \
     quota_limit, quota_unit, quota_period, quota_used, quota_reset_at, cooldown_until, \
     last_error, config_json, created_at, updated_at";

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
        account_id: row.get("account_id"),
        account_name: row.get("account_name"),
        api_key_id: row.get("api_key_id"),
        protocol: row.get("protocol"),
        status_code: row.get("status_code"),
        latency_ms: row.get("latency_ms"),
        input_tokens: row.get("input_tokens"),
        output_tokens: row.get("output_tokens"),
        cache_read_tokens: row.get("cache_read_tokens"),
        cache_write_tokens: row.get("cache_write_tokens"),
        reasoning_tokens: row.get("reasoning_tokens"),
        cost_micros: row.get("cost_micros"),
        cost_currency: row.get("cost_currency"),
        finish_reason: row.get("finish_reason"),
        error: row.get("error"),
        created_at: row.get("created_at"),
    }
}

impl LlmStore {
    pub async fn load_runtime_data(&self) -> Result<LlmRuntimeData> {
        let providers = self
            .list_providers()
            .await
            .map_err(|err| err.into_octra())?;
        let models = self.list_models().await.map_err(|err| err.into_octra())?;
        let groups = self
            .list_routing_groups()
            .await
            .map_err(|err| err.into_octra())?;
        let routes = self.list_routes().await.map_err(|err| err.into_octra())?;
        let targets = self
            .list_routing_targets()
            .await
            .map_err(|err| err.into_octra())?;
        let accounts = self.list_accounts().await.map_err(|err| err.into_octra())?;
        let virtual_keys = self
            .list_virtual_keys()
            .await
            .map_err(|err| err.into_octra())?;
        let policy_rows = sqlx::query(
            "SELECT id, name, scope, consumer_id, document, created_at, updated_at \
             FROM policies ORDER BY name ASC, id ASC",
        )
        .fetch_all(self.pool())
        .await
        .map_err(|err| Error::Storage(format!("cannot load policies: {err}")))?;
        let policies = policy_rows
            .iter()
            .map(crate::api::map_policy)
            .collect::<Result<Vec<Policy>>>()?;
        Ok(LlmRuntimeData {
            providers,
            policies,
            models,
            groups,
            routes,
            targets,
            accounts,
            virtual_keys,
        })
    }

    // Providers

    pub async fn list_providers(&self) -> LlmResult<Vec<LlmProviderRecord>> {
        let sql =
            format!("SELECT {PROVIDER_COLUMNS} FROM llm_providers ORDER BY priority ASC, name ASC");
        let rows = sqlx::query(&sql)
            .fetch_all(self.pool())
            .await
            .map_err(backend)?;
        Ok(rows.iter().map(provider_from_row).collect())
    }

    pub async fn get_provider(&self, id: &str) -> LlmResult<Option<LlmProviderRecord>> {
        let sql = format!("SELECT {PROVIDER_COLUMNS} FROM llm_providers WHERE id = ?");
        let row = sqlx::query(&sql)
            .bind(id)
            .fetch_optional(self.pool())
            .await
            .map_err(backend)?;
        Ok(row.as_ref().map(provider_from_row))
    }

    pub async fn create_provider(&self, input: &LlmProviderInput) -> LlmResult<LlmProviderRecord> {
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
        self.get_provider(&id)
            .await?
            .ok_or_else(|| backend(sqlx::Error::RowNotFound))
    }

    pub async fn update_provider(
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
        self.get_provider(id)
            .await?
            .ok_or_else(|| not_found("provider", id))
    }

    pub async fn delete_provider(&self, id: &str) -> LlmResult<bool> {
        let affected = sqlx::query("DELETE FROM llm_providers WHERE id = ?")
            .bind(id)
            .execute(self.pool())
            .await
            .map_err(backend)?
            .rows_affected();
        Ok(affected > 0)
    }

    pub async fn provider_exists(&self, id: &str) -> LlmResult<bool> {
        let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM llm_providers WHERE id = ?")
            .bind(id)
            .fetch_one(self.pool())
            .await
            .map_err(backend)?;
        Ok(count > 0)
    }

    // Provider accounts

    pub async fn list_accounts(&self) -> LlmResult<Vec<LlmProviderAccountRecord>> {
        let sql = format!(
            "SELECT {ACCOUNT_COLUMNS} FROM llm_provider_accounts \
             ORDER BY provider_id ASC, priority ASC, name ASC"
        );
        let rows = sqlx::query(&sql)
            .fetch_all(self.pool())
            .await
            .map_err(backend)?;
        Ok(rows.iter().map(account_from_row).collect())
    }

    pub async fn list_provider_accounts(
        &self,
        provider_id: &str,
    ) -> LlmResult<Vec<LlmProviderAccountRecord>> {
        let sql = format!(
            "SELECT {ACCOUNT_COLUMNS} FROM llm_provider_accounts \
             WHERE provider_id = ? ORDER BY priority ASC, name ASC"
        );
        let rows = sqlx::query(&sql)
            .bind(provider_id)
            .fetch_all(self.pool())
            .await
            .map_err(backend)?;
        Ok(rows.iter().map(account_from_row).collect())
    }

    pub async fn get_account(&self, id: &str) -> LlmResult<Option<LlmProviderAccountRecord>> {
        let sql = format!("SELECT {ACCOUNT_COLUMNS} FROM llm_provider_accounts WHERE id = ?");
        let row = sqlx::query(&sql)
            .bind(id)
            .fetch_optional(self.pool())
            .await
            .map_err(backend)?;
        Ok(row.as_ref().map(account_from_row))
    }

    pub async fn create_account(
        &self,
        provider_id: &str,
        input: &LlmProviderAccountInput,
    ) -> LlmResult<LlmProviderAccountRecord> {
        if !self.provider_exists(provider_id).await? {
            return Err(not_found("provider", provider_id));
        }
        validate_account_input(input)?;
        let id = uuid::Uuid::new_v4().to_string();
        sqlx::query(
            "INSERT INTO llm_provider_accounts (id, provider_id, name, secret_ref, weight, priority,
                 enabled, quota_limit, quota_unit, quota_period, config_json)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(&id)
        .bind(provider_id)
        .bind(&input.name)
        .bind(&input.secret_ref)
        .bind(input.weight)
        .bind(input.priority)
        .bind(input.enabled as i64)
        .bind(input.quota_limit)
        .bind(&input.quota_unit)
        .bind(&input.quota_period)
        .bind(&input.config_json)
        .execute(self.pool())
        .await
        .map_err(conflict)?;
        self.get_account(&id)
            .await?
            .ok_or_else(|| backend(sqlx::Error::RowNotFound))
    }

    pub async fn update_account(
        &self,
        id: &str,
        input: &LlmProviderAccountInput,
    ) -> LlmResult<LlmProviderAccountRecord> {
        validate_account_input(input)?;
        let affected = sqlx::query(
            "UPDATE llm_provider_accounts SET name = ?, secret_ref = ?, weight = ?, priority = ?,
                 enabled = ?, quota_limit = ?, quota_unit = ?, quota_period = ?, config_json = ?,
                 updated_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now')
             WHERE id = ?",
        )
        .bind(&input.name)
        .bind(&input.secret_ref)
        .bind(input.weight)
        .bind(input.priority)
        .bind(input.enabled as i64)
        .bind(input.quota_limit)
        .bind(&input.quota_unit)
        .bind(&input.quota_period)
        .bind(&input.config_json)
        .bind(id)
        .execute(self.pool())
        .await
        .map_err(conflict)?
        .rows_affected();
        if affected == 0 {
            return Err(not_found("account", id));
        }
        self.get_account(id)
            .await?
            .ok_or_else(|| not_found("account", id))
    }

    pub async fn delete_account(&self, id: &str) -> LlmResult<bool> {
        let affected = sqlx::query("DELETE FROM llm_provider_accounts WHERE id = ?")
            .bind(id)
            .execute(self.pool())
            .await
            .map_err(backend)?
            .rows_affected();
        Ok(affected > 0)
    }

    /// Clears the rolling quota counter and the persisted cooldown.
    pub async fn reset_account_quota(&self, id: &str) -> LlmResult<bool> {
        let affected = sqlx::query(
            "UPDATE llm_provider_accounts SET quota_used = 0, quota_reset_at = NULL,
                 cooldown_until = NULL, last_error = NULL,
                 updated_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now')
             WHERE id = ?",
        )
        .bind(id)
        .execute(self.pool())
        .await
        .map_err(backend)?
        .rows_affected();
        Ok(affected > 0)
    }

    /// Adds one request's consumption to the account quota. The increment is
    /// applied to the counter matching the account's configured unit.
    pub async fn consume_account_quota(
        &self,
        id: &str,
        requests: i64,
        tokens: i64,
        cost_micros: i64,
    ) -> LlmResult<()> {
        sqlx::query(
            "UPDATE llm_provider_accounts SET
                 quota_used = quota_used + CASE quota_unit
                     WHEN 'tokens' THEN ?
                     WHEN 'cost_micros' THEN ?
                     ELSE ?
                 END,
                 updated_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now')
             WHERE id = ?",
        )
        .bind(tokens)
        .bind(cost_micros)
        .bind(requests)
        .bind(id)
        .execute(self.pool())
        .await
        .map_err(backend)?;
        Ok(())
    }

    pub async fn set_account_cooldown(
        &self,
        id: &str,
        until: &str,
        last_error: Option<&str>,
    ) -> LlmResult<()> {
        sqlx::query(
            "UPDATE llm_provider_accounts SET cooldown_until = ?, last_error = ?,
                 updated_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now')
             WHERE id = ?",
        )
        .bind(until)
        .bind(last_error)
        .bind(id)
        .execute(self.pool())
        .await
        .map_err(backend)?;
        Ok(())
    }

    /// Persists a rolled quota period: counter back to zero, next boundary.
    pub async fn roll_account_quota(&self, id: &str, reset_at: &str) -> LlmResult<()> {
        sqlx::query(
            "UPDATE llm_provider_accounts SET quota_used = 0, quota_reset_at = ?,
                 updated_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now')
             WHERE id = ?",
        )
        .bind(reset_at)
        .bind(id)
        .execute(self.pool())
        .await
        .map_err(backend)?;
        Ok(())
    }

    /// Aggregates persisted usage for an account since an ISO timestamp.
    pub async fn account_usage_since(
        &self,
        account_id: &str,
        since: Option<&str>,
    ) -> LlmResult<LlmAccountUsage> {
        let row = sqlx::query(
            "SELECT COUNT(*) AS requests,
                 SUM(input_tokens + output_tokens) AS tokens,
                 SUM(COALESCE(cost_micros, 0)) AS cost_micros
             FROM llm_usage
             WHERE account_id = ? AND (? IS NULL OR created_at >= ?)",
        )
        .bind(account_id)
        .bind(since)
        .bind(since)
        .fetch_one(self.pool())
        .await
        .map_err(backend)?;
        Ok(LlmAccountUsage {
            requests: row.get::<Option<i64>, _>("requests").unwrap_or(0),
            tokens: row.get::<Option<i64>, _>("tokens").unwrap_or(0),
            cost_micros: row.get::<Option<i64>, _>("cost_micros").unwrap_or(0),
        })
    }

    /// Aggregates persisted usage for a virtual key since an ISO timestamp.
    pub async fn virtual_key_usage_since(
        &self,
        api_key_id: &str,
        since: &str,
    ) -> LlmResult<LlmVirtualKeyUsage> {
        let row = sqlx::query(
            "SELECT COALESCE(SUM(input_tokens + output_tokens + cache_write_tokens), 0) AS tokens,
                 COALESCE(SUM(cost_micros), 0) AS cost_micros
             FROM llm_usage_rollup
             WHERE api_key_id = ? AND bucket_start >= ?",
        )
        .bind(api_key_id)
        .bind(since)
        .fetch_one(self.pool())
        .await
        .map_err(backend)?;
        Ok(LlmVirtualKeyUsage {
            tokens: row.get::<Option<i64>, _>("tokens").unwrap_or(0),
            cost_micros: row.get::<Option<i64>, _>("cost_micros").unwrap_or(0),
        })
    }

    // Models

    pub async fn list_models(&self) -> LlmResult<Vec<LlmModelRecord>> {
        let sql = format!(
            "SELECT {MODEL_COLUMNS} FROM llm_models ORDER BY provider_id ASC, model_id ASC"
        );
        let rows = sqlx::query(&sql)
            .fetch_all(self.pool())
            .await
            .map_err(backend)?;
        Ok(rows.iter().map(model_from_row).collect())
    }

    pub async fn get_model(&self, id: &str) -> LlmResult<Option<LlmModelRecord>> {
        let sql = format!("SELECT {MODEL_COLUMNS} FROM llm_models WHERE id = ?");
        let row = sqlx::query(&sql)
            .bind(id)
            .fetch_optional(self.pool())
            .await
            .map_err(backend)?;
        Ok(row.as_ref().map(model_from_row))
    }

    pub async fn create_model(&self, input: &LlmModelInput) -> LlmResult<LlmModelRecord> {
        if !self.provider_exists(&input.provider_id).await? {
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
        self.get_model(&id)
            .await?
            .ok_or_else(|| backend(sqlx::Error::RowNotFound))
    }

    pub async fn update_model(&self, id: &str, input: &LlmModelInput) -> LlmResult<LlmModelRecord> {
        if !self.provider_exists(&input.provider_id).await? {
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
        self.get_model(id)
            .await?
            .ok_or_else(|| not_found("model", id))
    }

    pub async fn delete_model(&self, id: &str) -> LlmResult<bool> {
        let affected = sqlx::query("DELETE FROM llm_models WHERE id = ?")
            .bind(id)
            .execute(self.pool())
            .await
            .map_err(backend)?
            .rows_affected();
        Ok(affected > 0)
    }

    // Routing groups

    pub async fn list_routing_groups(&self) -> LlmResult<Vec<LlmRoutingGroupRecord>> {
        let rows = sqlx::query(
            "SELECT id, name, strategy, created_at FROM llm_routing_groups ORDER BY name ASC",
        )
        .fetch_all(self.pool())
        .await
        .map_err(backend)?;
        Ok(rows.iter().map(group_from_row).collect())
    }

    pub async fn get_routing_group(&self, id: &str) -> LlmResult<Option<LlmRoutingGroupRecord>> {
        let row = sqlx::query(
            "SELECT id, name, strategy, created_at FROM llm_routing_groups WHERE id = ?",
        )
        .bind(id)
        .fetch_optional(self.pool())
        .await
        .map_err(backend)?;
        Ok(row.as_ref().map(group_from_row))
    }

    pub async fn create_routing_group(
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
        self.get_routing_group(&id)
            .await?
            .ok_or_else(|| backend(sqlx::Error::RowNotFound))
    }

    pub async fn update_routing_group(
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
        self.get_routing_group(id)
            .await?
            .ok_or_else(|| not_found("routing group", id))
    }

    pub async fn delete_routing_group(&self, id: &str) -> LlmResult<bool> {
        let affected = sqlx::query("DELETE FROM llm_routing_groups WHERE id = ?")
            .bind(id)
            .execute(self.pool())
            .await
            .map_err(backend)?
            .rows_affected();
        Ok(affected > 0)
    }

    pub async fn group_exists(&self, id: &str) -> LlmResult<bool> {
        let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM llm_routing_groups WHERE id = ?")
            .bind(id)
            .fetch_one(self.pool())
            .await
            .map_err(backend)?;
        Ok(count > 0)
    }

    // Routing targets

    /// A routing target may only pin an account that belongs to its provider.
    async fn check_target_account(
        &self,
        provider_id: &str,
        account_id: Option<&str>,
    ) -> LlmResult<()> {
        let Some(account_id) = account_id else {
            return Ok(());
        };
        let account = self
            .get_account(account_id)
            .await?
            .ok_or_else(|| not_found("account", account_id))?;
        if account.provider_id != provider_id {
            return Err(LlmStorageError::Conflict(format!(
                "account {account_id} does not belong to provider {provider_id}"
            )));
        }
        Ok(())
    }

    pub async fn list_routing_targets(&self) -> LlmResult<Vec<LlmRoutingTargetRecord>> {
        let rows = sqlx::query(
            "SELECT id, group_id, provider_id, model_id, account_id, weight, priority, position,
                 created_at, updated_at
             FROM llm_routing_targets ORDER BY priority ASC, position ASC",
        )
        .fetch_all(self.pool())
        .await
        .map_err(backend)?;
        Ok(rows.iter().map(target_from_row).collect())
    }

    pub async fn get_routing_target(&self, id: &str) -> LlmResult<Option<LlmRoutingTargetRecord>> {
        let row = sqlx::query(
            "SELECT id, group_id, provider_id, model_id, account_id, weight, priority, position,
                 created_at, updated_at
             FROM llm_routing_targets WHERE id = ?",
        )
        .bind(id)
        .fetch_optional(self.pool())
        .await
        .map_err(backend)?;
        Ok(row.as_ref().map(target_from_row))
    }

    pub async fn create_routing_target(
        &self,
        group_id: &str,
        input: &LlmRoutingTargetInput,
    ) -> LlmResult<LlmRoutingTargetRecord> {
        if !self.group_exists(group_id).await? {
            return Err(not_found("routing group", group_id));
        }
        if !self.provider_exists(&input.provider_id).await? {
            return Err(not_found("provider", &input.provider_id));
        }
        self.check_target_account(&input.provider_id, input.account_id.as_deref())
            .await?;
        let id = uuid::Uuid::new_v4().to_string();
        sqlx::query(
            "INSERT INTO llm_routing_targets (id, group_id, provider_id, model_id, account_id,
                 weight, priority, position)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(&id)
        .bind(group_id)
        .bind(&input.provider_id)
        .bind(&input.model_id)
        .bind(&input.account_id)
        .bind(input.weight)
        .bind(input.priority)
        .bind(input.position)
        .execute(self.pool())
        .await
        .map_err(conflict)?;
        self.get_routing_target(&id)
            .await?
            .ok_or_else(|| backend(sqlx::Error::RowNotFound))
    }

    pub async fn update_routing_target(
        &self,
        id: &str,
        input: &LlmRoutingTargetInput,
    ) -> LlmResult<LlmRoutingTargetRecord> {
        if !self.provider_exists(&input.provider_id).await? {
            return Err(not_found("provider", &input.provider_id));
        }
        self.check_target_account(&input.provider_id, input.account_id.as_deref())
            .await?;
        let affected = sqlx::query(
            "UPDATE llm_routing_targets SET provider_id = ?, model_id = ?, account_id = ?,
                 weight = ?, priority = ?, position = ?,
                 updated_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now')
             WHERE id = ?",
        )
        .bind(&input.provider_id)
        .bind(&input.model_id)
        .bind(&input.account_id)
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
        self.get_routing_target(id)
            .await?
            .ok_or_else(|| not_found("routing target", id))
    }

    pub async fn delete_routing_target(&self, id: &str) -> LlmResult<bool> {
        let affected = sqlx::query("DELETE FROM llm_routing_targets WHERE id = ?")
            .bind(id)
            .execute(self.pool())
            .await
            .map_err(backend)?
            .rows_affected();
        Ok(affected > 0)
    }

    // Routes

    pub async fn list_routes(&self) -> LlmResult<Vec<LlmRouteRecord>> {
        let rows = sqlx::query(
            "SELECT id, virtual_model_id, routing_group_id, created_at, updated_at FROM llm_routes ORDER BY virtual_model_id ASC",
        )
        .fetch_all(self.pool())
        .await
        .map_err(backend)?;
        Ok(rows.iter().map(route_from_row).collect())
    }

    pub async fn get_route(&self, id: &str) -> LlmResult<Option<LlmRouteRecord>> {
        let row = sqlx::query(
            "SELECT id, virtual_model_id, routing_group_id, created_at, updated_at FROM llm_routes WHERE id = ?",
        )
        .bind(id)
        .fetch_optional(self.pool())
        .await
        .map_err(backend)?;
        Ok(row.as_ref().map(route_from_row))
    }

    pub async fn create_route(
        &self,
        virtual_model_id: &str,
        routing_group_id: Option<&str>,
    ) -> LlmResult<LlmRouteRecord> {
        if let Some(group_id) = routing_group_id
            && !self.group_exists(group_id).await?
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
        self.get_route(&id)
            .await?
            .ok_or_else(|| backend(sqlx::Error::RowNotFound))
    }

    pub async fn update_route(
        &self,
        id: &str,
        virtual_model_id: &str,
        routing_group_id: Option<&str>,
    ) -> LlmResult<LlmRouteRecord> {
        if let Some(group_id) = routing_group_id
            && !self.group_exists(group_id).await?
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
        self.get_route(id)
            .await?
            .ok_or_else(|| not_found("route", id))
    }

    pub async fn delete_route(&self, id: &str) -> LlmResult<bool> {
        let affected = sqlx::query("DELETE FROM llm_routes WHERE id = ?")
            .bind(id)
            .execute(self.pool())
            .await
            .map_err(backend)?
            .rows_affected();
        Ok(affected > 0)
    }

    // Virtual keys

    pub async fn list_virtual_keys(&self) -> LlmResult<Vec<LlmVirtualKeyRecord>> {
        let sql = format!(
            "SELECT {VIRTUAL_KEY_COLUMNS} FROM llm_virtual_keys ORDER BY created_at DESC, rowid DESC"
        );
        let rows = sqlx::query(&sql)
            .fetch_all(self.pool())
            .await
            .map_err(backend)?;
        Ok(rows.iter().map(virtual_key_from_row).collect())
    }

    pub async fn get_virtual_key(&self, id: &str) -> LlmResult<Option<LlmVirtualKeyRecord>> {
        let sql = format!("SELECT {VIRTUAL_KEY_COLUMNS} FROM llm_virtual_keys WHERE id = ?");
        let row = sqlx::query(&sql)
            .bind(id)
            .fetch_optional(self.pool())
            .await
            .map_err(backend)?;
        Ok(row.as_ref().map(virtual_key_from_row))
    }

    pub async fn find_virtual_key_by_hash(
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

    pub async fn create_virtual_key(
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
        self.get_virtual_key(&id)
            .await?
            .ok_or_else(|| backend(sqlx::Error::RowNotFound))
    }

    pub async fn update_virtual_key(
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
        self.get_virtual_key(id)
            .await?
            .ok_or_else(|| not_found("virtual key", id))
    }

    pub async fn delete_virtual_key(&self, id: &str) -> LlmResult<bool> {
        let affected = sqlx::query("DELETE FROM llm_virtual_keys WHERE id = ?")
            .bind(id)
            .execute(self.pool())
            .await
            .map_err(backend)?
            .rows_affected();
        Ok(affected > 0)
    }

    pub async fn touch_virtual_key(&self, id: &str) -> LlmResult<()> {
        sqlx::query("UPDATE llm_virtual_keys SET last_used_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now') WHERE id = ?")
            .bind(id)
            .execute(self.pool())
            .await
            .map_err(backend)?;
        Ok(())
    }

    // Usage

    pub async fn insert_usage(&self, input: &LlmUsageInput) -> LlmResult<()> {
        let created_at = input
            .created_at
            .clone()
            .unwrap_or_else(|| canonical_timestamp(Utc::now()));
        let bucket = hour_bucket(&created_at)?;
        let errors = i64::from(
            input.error.is_some() || input.status_code.is_some_and(|status| status >= 400),
        );
        let mut tx = self.pool().begin().await.map_err(backend)?;
        sqlx::query(
            "INSERT INTO llm_usage (request_id, virtual_model_id, provider_id, model_id,
                 account_id, account_name, api_key_id, protocol, status_code, latency_ms,
                 input_tokens, output_tokens, cache_read_tokens, cache_write_tokens,
                 reasoning_tokens, cost_micros, cost_currency, finish_reason, error, created_at)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(&input.request_id)
        .bind(&input.virtual_model_id)
        .bind(&input.provider_id)
        .bind(&input.model_id)
        .bind(&input.account_id)
        .bind(&input.account_name)
        .bind(&input.api_key_id)
        .bind(&input.protocol)
        .bind(input.status_code)
        .bind(input.latency_ms)
        .bind(input.input_tokens)
        .bind(input.output_tokens)
        .bind(input.cache_read_tokens)
        .bind(input.cache_write_tokens)
        .bind(input.reasoning_tokens)
        .bind(input.cost_micros)
        .bind(&input.cost_currency)
        .bind(&input.finish_reason)
        .bind(&input.error)
        .bind(&created_at)
        .execute(&mut *tx)
        .await
        .map_err(backend)?;
        sqlx::query(
            "INSERT INTO llm_usage_rollup (bucket_start, api_key_id, account_id, provider_id,
                 model_id, virtual_model_id, requests, errors, input_tokens, output_tokens,
                 cache_write_tokens, cost_micros)
             VALUES (?, ?, ?, ?, ?, ?, 1, ?, ?, ?, ?, ?)
             ON CONFLICT (bucket_start, api_key_id, account_id, provider_id, model_id, virtual_model_id)
             DO UPDATE SET
                 requests = requests + 1,
                 errors = errors + excluded.errors,
                 input_tokens = input_tokens + excluded.input_tokens,
                 output_tokens = output_tokens + excluded.output_tokens,
                 cache_write_tokens = cache_write_tokens + excluded.cache_write_tokens,
                 cost_micros = cost_micros + excluded.cost_micros",
        )
        .bind(&bucket)
        .bind(input.api_key_id.as_deref().unwrap_or(""))
        .bind(input.account_id.as_deref().unwrap_or(""))
        .bind(input.provider_id.as_deref().unwrap_or(""))
        .bind(input.model_id.as_deref().unwrap_or(""))
        .bind(&input.virtual_model_id)
        .bind(errors)
        .bind(input.input_tokens)
        .bind(input.output_tokens)
        .bind(input.cache_write_tokens.unwrap_or(0))
        .bind(input.cost_micros.unwrap_or(0))
        .execute(&mut *tx)
        .await
        .map_err(backend)?;
        tx.commit().await.map_err(backend)?;
        Ok(())
    }

    pub async fn list_usage(&self, limit: i64) -> LlmResult<Vec<LlmUsageRecord>> {
        let rows = sqlx::query(
            "SELECT id, request_id, virtual_model_id, provider_id, model_id, account_id,
                 account_name, api_key_id, protocol, status_code, latency_ms, input_tokens,
                 output_tokens, cache_read_tokens, cache_write_tokens, reasoning_tokens,
                 cost_micros, cost_currency, finish_reason, error, created_at
             FROM llm_usage ORDER BY id DESC LIMIT ?",
        )
        .bind(limit.clamp(1, 1000))
        .fetch_all(self.pool())
        .await
        .map_err(backend)?;
        Ok(rows.iter().map(usage_from_row).collect())
    }

    pub async fn usage_summary(&self) -> LlmResult<Vec<LlmUsageSummaryRow>> {
        let rows = sqlx::query(
            "SELECT virtual_model_id, SUM(requests) AS requests, SUM(errors) AS errors,
                 SUM(input_tokens) AS input_tokens, SUM(output_tokens) AS output_tokens
             FROM llm_usage_rollup GROUP BY virtual_model_id ORDER BY requests DESC",
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

    pub async fn usage_totals(&self) -> LlmResult<LlmUsageSummaryRow> {
        let row = sqlx::query(
            "SELECT COALESCE(SUM(requests), 0) AS requests,
                 COALESCE(SUM(errors), 0) AS errors,
                 COALESCE(SUM(input_tokens), 0) AS input_tokens,
                 COALESCE(SUM(output_tokens), 0) AS output_tokens
             FROM llm_usage_rollup",
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
    use crate::Storage;
    use sqlx::Row;

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
            .llm
            .create_provider(&provider("openai"))
            .await
            .unwrap();
        assert_eq!(created.protocol, "openai");
        assert!(created.enabled);

        assert_eq!(storage.llm.list_providers().await.unwrap().len(), 1);

        // Duplicate names conflict.
        assert!(matches!(
            storage.llm.create_provider(&provider("openai")).await,
            Err(LlmStorageError::Conflict(_))
        ));

        let mut update = provider("openai-main");
        update.priority = 5;
        update.enabled = false;
        let updated = storage
            .llm
            .update_provider(&created.id, &update)
            .await
            .unwrap();
        assert_eq!(updated.name, "openai-main");
        assert_eq!(updated.priority, 5);
        assert!(!updated.enabled);

        assert!(matches!(
            storage.llm.update_provider("missing", &update).await,
            Err(LlmStorageError::Missing(_))
        ));

        assert!(storage.llm.delete_provider(&created.id).await.unwrap());
        assert!(!storage.llm.delete_provider(&created.id).await.unwrap());
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
            storage.llm.create_model(&model).await,
            Err(LlmStorageError::Missing(_))
        ));

        let created_provider = storage
            .llm
            .create_provider(&provider("openai"))
            .await
            .unwrap();
        model.provider_id = created_provider.id.clone();
        let created = storage.llm.create_model(&model).await.unwrap();
        assert_eq!(created.model_id, "gpt-4o");
        assert!(matches!(
            storage.llm.create_model(&model).await,
            Err(LlmStorageError::Conflict(_))
        ));

        assert_eq!(storage.llm.list_models().await.unwrap().len(), 1);
        assert!(storage.llm.delete_model(&created.id).await.unwrap());

        // Deleting the provider cascades to its models.
        let model2 = storage.llm.create_model(&model).await.unwrap();
        storage
            .llm
            .delete_provider(&created_provider.id)
            .await
            .unwrap();
        assert!(storage.llm.get_model(&model2.id).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn routing_group_targets_and_routes() {
        let storage = storage().await;
        let provider = storage
            .llm
            .create_provider(&provider("openai"))
            .await
            .unwrap();
        let group = storage
            .llm
            .create_routing_group("primary", "priority")
            .await
            .unwrap();

        let target = storage
            .llm
            .create_routing_target(
                &group.id,
                &LlmRoutingTargetInput {
                    provider_id: provider.id.clone(),
                    model_id: "gpt-4o".to_string(),
                    account_id: None,
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
                .llm
                .create_routing_target(
                    "ghost-group",
                    &LlmRoutingTargetInput {
                        provider_id: provider.id.clone(),
                        model_id: "gpt-4o".to_string(),
                        account_id: None,
                        weight: 1,
                        priority: 0,
                        position: 0,
                    }
                )
                .await,
            Err(LlmStorageError::Missing(_))
        ));

        let route = storage
            .llm
            .create_route("octra-default", Some(&group.id))
            .await
            .unwrap();
        assert_eq!(route.virtual_model_id, "octra-default");
        assert!(matches!(
            storage.llm.create_route("octra-default", None).await,
            Err(LlmStorageError::Conflict(_))
        ));
        assert!(matches!(
            storage.llm.create_route("other", Some("ghost")).await,
            Err(LlmStorageError::Missing(_))
        ));

        let updated = storage
            .llm
            .update_route(&route.id, "octra-default", None)
            .await
            .unwrap();
        assert!(updated.routing_group_id.is_none());

        let data = storage.llm.load_runtime_data().await.unwrap();
        assert_eq!(data.providers.len(), 1);
        assert_eq!(data.targets.len(), 1);
        assert_eq!(data.routes.len(), 1);

        // Deleting the group cascades to targets and nulls the route reference.
        storage.llm.delete_routing_group(&group.id).await.unwrap();
        let data = storage.llm.load_runtime_data().await.unwrap();
        assert!(data.groups.is_empty());
        assert!(data.targets.is_empty());
    }

    #[tokio::test]
    async fn virtual_keys_and_usage() {
        let storage = storage().await;
        let key = storage
            .llm
            .create_virtual_key(&LlmVirtualKeyInput {
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
            .llm
            .find_virtual_key_by_hash("deadbeef")
            .await
            .unwrap();
        assert_eq!(found.unwrap().id, key.id);

        storage.llm.touch_virtual_key(&key.id).await.unwrap();
        let touched = storage.llm.get_virtual_key(&key.id).await.unwrap().unwrap();
        assert!(touched.last_used_at.is_some());

        storage
            .llm
            .insert_usage(&LlmUsageInput {
                request_id: "req-1".into(),
                virtual_model_id: "octra-default".into(),
                provider_id: Some("prov-1".into()),
                model_id: Some("gpt-4o".into()),
                api_key_id: Some(key.id.clone()),
                protocol: Some("openai".into()),
                status_code: Some(200),
                latency_ms: Some(12),
                input_tokens: 100,
                output_tokens: 50,
                cost_micros: Some(42),
                ..LlmUsageInput::default()
            })
            .await
            .unwrap();
        storage
            .llm
            .insert_usage(&LlmUsageInput {
                request_id: "req-2".into(),
                virtual_model_id: "octra-default".into(),
                protocol: Some("anthropic".into()),
                status_code: Some(502),
                latency_ms: Some(3),
                error: Some("upstream failed".into()),
                ..LlmUsageInput::default()
            })
            .await
            .unwrap();

        let usage = storage.llm.list_usage(10).await.unwrap();
        assert_eq!(usage.len(), 2);
        assert_eq!(usage[0].request_id, "req-2");
        assert_eq!(usage[1].input_tokens, 100);

        let key_usage = storage
            .llm
            .virtual_key_usage_since(&key.id, "2000-01-01T00:00:00.000Z")
            .await
            .unwrap();
        assert_eq!(key_usage.tokens, 150);
        assert_eq!(key_usage.cost_micros, 42);
        let future_usage = storage
            .llm
            .virtual_key_usage_since(&key.id, "2999-01-01T00:00:00.000Z")
            .await
            .unwrap();
        assert_eq!(future_usage.tokens, 0);
        assert_eq!(future_usage.cost_micros, 0);

        let summary = storage.llm.usage_summary().await.unwrap();
        assert_eq!(summary.len(), 1);
        assert_eq!(summary[0].requests, 2);
        assert_eq!(summary[0].errors, 1);
        assert_eq!(summary[0].input_tokens, 100);
        assert_eq!(summary[0].output_tokens, 50);

        let totals = storage.llm.usage_totals().await.unwrap();
        assert_eq!(totals.requests, 2);

        assert!(storage.llm.delete_virtual_key(&key.id).await.unwrap());
        assert!(
            storage
                .llm
                .find_virtual_key_by_hash("deadbeef")
                .await
                .unwrap()
                .is_none()
        );
    }

    #[tokio::test]
    async fn usage_rollup_buckets_by_hour_and_dimensions() {
        let storage = storage().await;
        for (request_id, created_at, input, output, cache_write, cost, status) in [
            (
                "a",
                "2026-10-01T05:12:00.000Z",
                10,
                5,
                Some(2),
                Some(7),
                200,
            ),
            ("b", "2026-10-01T05:55:00.000Z", 1, 2, None, None, 500),
            ("c", "2026-10-01T06:01:00.000Z", 3, 4, None, None, 200),
        ] {
            storage
                .llm
                .insert_usage(&LlmUsageInput {
                    request_id: request_id.into(),
                    virtual_model_id: "octra-default".into(),
                    provider_id: Some("p1".into()),
                    model_id: Some("m1".into()),
                    account_id: Some("a1".into()),
                    api_key_id: Some("k1".into()),
                    status_code: Some(status),
                    input_tokens: input,
                    output_tokens: output,
                    cache_write_tokens: cache_write,
                    cost_micros: cost,
                    created_at: Some(created_at.into()),
                    ..LlmUsageInput::default()
                })
                .await
                .unwrap();
        }

        let rows = sqlx::query(
            "SELECT bucket_start, requests, errors, input_tokens, output_tokens,
                 cache_write_tokens, cost_micros
             FROM llm_usage_rollup ORDER BY bucket_start",
        )
        .fetch_all(storage.pool())
        .await
        .unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(
            rows[0].get::<String, _>("bucket_start"),
            "2026-10-01T05:00:00.000Z"
        );
        assert_eq!(rows[0].get::<i64, _>("requests"), 2);
        assert_eq!(rows[0].get::<i64, _>("errors"), 1);
        assert_eq!(rows[0].get::<i64, _>("input_tokens"), 11);
        assert_eq!(rows[0].get::<i64, _>("output_tokens"), 7);
        assert_eq!(rows[0].get::<i64, _>("cache_write_tokens"), 2);
        assert_eq!(rows[0].get::<i64, _>("cost_micros"), 7);
        assert_eq!(
            rows[1].get::<String, _>("bucket_start"),
            "2026-10-01T06:00:00.000Z"
        );
        assert_eq!(rows[1].get::<i64, _>("requests"), 1);
        assert_eq!(rows[1].get::<i64, _>("errors"), 0);
        assert_eq!(rows[1].get::<i64, _>("input_tokens"), 3);
        assert_eq!(rows[1].get::<i64, _>("output_tokens"), 4);

        let totals = storage.llm.usage_totals().await.unwrap();
        assert_eq!(totals.requests, 3);
        assert_eq!(totals.errors, 1);
        assert_eq!(totals.input_tokens, 14);
        assert_eq!(totals.output_tokens, 11);
    }

    #[tokio::test]
    async fn virtual_key_usage_since_uses_hour_buckets() {
        let storage = storage().await;
        for (request_id, created_at, input, output, cost) in [
            ("a", "2026-10-01T05:12:00.000Z", 10, 5, Some(3)),
            ("b", "2026-10-01T06:01:00.000Z", 6, 3, Some(4)),
        ] {
            storage
                .llm
                .insert_usage(&LlmUsageInput {
                    request_id: request_id.into(),
                    virtual_model_id: "octra-default".into(),
                    api_key_id: Some("k1".into()),
                    input_tokens: input,
                    output_tokens: output,
                    cost_micros: cost,
                    created_at: Some(created_at.into()),
                    ..LlmUsageInput::default()
                })
                .await
                .unwrap();
        }

        let since_05 = storage
            .llm
            .virtual_key_usage_since("k1", "2026-10-01T05:00:00.000Z")
            .await
            .unwrap();
        assert_eq!(since_05.tokens, 24);
        assert_eq!(since_05.cost_micros, 7);

        let since_06 = storage
            .llm
            .virtual_key_usage_since("k1", "2026-10-01T06:00:00.000Z")
            .await
            .unwrap();
        assert_eq!(since_06.tokens, 9);
        assert_eq!(since_06.cost_micros, 4);

        let since_07 = storage
            .llm
            .virtual_key_usage_since("k1", "2026-10-01T07:00:00.000Z")
            .await
            .unwrap();
        assert_eq!(since_07.tokens, 0);
        assert_eq!(since_07.cost_micros, 0);
    }

    #[tokio::test]
    async fn null_dimensions_share_the_empty_string_bucket() {
        let storage = storage().await;
        for (request_id, api_key) in [("none", None), ("empty", Some(""))] {
            storage
                .llm
                .insert_usage(&LlmUsageInput {
                    request_id: request_id.into(),
                    virtual_model_id: "octra-default".into(),
                    api_key_id: api_key.map(str::to_string),
                    input_tokens: 1,
                    created_at: Some("2026-10-01T05:12:00.000Z".into()),
                    ..LlmUsageInput::default()
                })
                .await
                .unwrap();
        }

        let requests: i64 = sqlx::query_scalar(
            "SELECT requests FROM llm_usage_rollup
             WHERE api_key_id = '' AND bucket_start = '2026-10-01T05:00:00.000Z'",
        )
        .fetch_one(storage.pool())
        .await
        .unwrap();
        assert_eq!(requests, 2);
    }
}
