//! LLM gateway persistence: virtual keys, usage, OAuth tokens, PII engines and
//! shared counters. Model and routing configuration lives in the AI-native
//! control plane (`ai.rs`); only the runtime record types are kept here.

use chrono::{DateTime, SecondsFormat, Utc};
use octra_api_core::Policy;
use octra_core::{Error, Result};
use octra_llm_plugin_types::{BudgetWindowView, CounterOutcome, CounterWindow};
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
    pub credential_kind: String,
    pub oauth_status: String,
    pub oauth_expires_at: Option<String>,
    pub config_json: String,
    pub created_at: String,
    pub updated_at: String,
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
    /// Stable identity shared by every key produced by rotation.
    pub key_family: String,
    /// Key this one replaced, when it was created by rotation.
    pub rotated_from: Option<String>,
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
    /// `None` derives the family from the new row id.
    pub key_family: Option<String>,
}

/// Values for one rotation step. The old key is read inside the same
/// transaction so lineage and expiry cannot race.
#[derive(Debug, Clone)]
pub struct LlmVirtualKeyRotation {
    pub name: String,
    pub key_prefix: String,
    pub key_hash: String,
    pub expires_at: Option<DateTime<Utc>>,
    pub permissions_json: Option<String>,
    pub grace_seconds: u64,
}

#[derive(Debug, Clone)]
pub struct LlmVirtualKeyRotationResult {
    pub key: LlmVirtualKeyRecord,
    pub replaces: String,
    pub previous_expires_at: Option<DateTime<Utc>>,
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
    pub cost_micros: i64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LlmUsageProviderRow {
    pub provider_id: String,
    pub requests: i64,
    pub errors: i64,
    pub input_tokens: i64,
    pub output_tokens: i64,
    pub cost_micros: i64,
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
    /// AI-native target descriptions keyed by synthesised runtime target id.
    /// Legacy rows leave this empty; only the AI-native mapping populates it.
    pub target_semantic_descriptions: std::collections::BTreeMap<String, String>,
    pub accounts: Vec<LlmProviderAccountRecord>,
    pub virtual_keys: Vec<LlmVirtualKeyRecord>,
}

#[derive(Debug)]
pub enum LlmStorageError {
    Conflict(String),
    Missing(String),
    Backend(String),
}

impl std::fmt::Display for LlmStorageError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Conflict(message) => write!(formatter, "conflict: {message}"),
            Self::Missing(message) => write!(formatter, "missing: {message}"),
            Self::Backend(message) => write!(formatter, "backend: {message}"),
        }
    }
}

impl std::error::Error for LlmStorageError {}

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

fn virtual_key_from_row(row: &sqlx::sqlite::SqliteRow) -> LlmVirtualKeyRecord {
    let id: String = row.get("id");
    LlmVirtualKeyRecord {
        key_family: row
            .get::<Option<String>, _>("key_family")
            .filter(|family| !family.trim().is_empty())
            .unwrap_or_else(|| id.clone()),
        rotated_from: row.get("rotated_from"),
        id,
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
     expires_at, last_used_at, permissions_json, key_family, rotated_from, created_at";

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

    /// Aggregates usage for every key in one rotation family. Deleted keys
    /// drop out of the aggregate; rotation only expires keys.
    pub async fn virtual_key_usage_since_family(
        &self,
        key_family: &str,
        since: &str,
    ) -> LlmResult<LlmVirtualKeyUsage> {
        let row = sqlx::query(
            "SELECT COALESCE(SUM(r.input_tokens + r.output_tokens + r.cache_write_tokens), 0) AS tokens,
                 COALESCE(SUM(r.cost_micros), 0) AS cost_micros
             FROM llm_usage_rollup r
             JOIN llm_virtual_keys k ON k.id = r.api_key_id
             WHERE COALESCE(k.key_family, k.id) = ? AND r.bucket_start >= ?",
        )
        .bind(key_family)
        .bind(since)
        .fetch_one(self.pool())
        .await
        .map_err(backend)?;
        Ok(LlmVirtualKeyUsage {
            tokens: row.get::<Option<i64>, _>("tokens").unwrap_or(0),
            cost_micros: row.get::<Option<i64>, _>("cost_micros").unwrap_or(0),
        })
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
        let key_family = input
            .key_family
            .clone()
            .filter(|family| !family.trim().is_empty())
            .unwrap_or_else(|| id.clone());
        sqlx::query(
            "INSERT INTO llm_virtual_keys (id, name, key_prefix, key_hash, consumer_id, enabled, expires_at, permissions_json, key_family)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(&id)
        .bind(&input.name)
        .bind(&input.key_prefix)
        .bind(&input.key_hash)
        .bind(&input.consumer_id)
        .bind(input.enabled as i64)
        .bind(input.expires_at.map(|value| value.to_rfc3339()))
        .bind(&input.permissions_json)
        .bind(&key_family)
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

    /// Creates a replacement key and ages out the old one in one transaction.
    /// The caller supplies the new plaintext hash/prefix; the family and
    /// lineage are derived from the row being replaced.
    pub async fn rotate_virtual_key(
        &self,
        old_id: &str,
        rotation: &LlmVirtualKeyRotation,
    ) -> LlmResult<LlmVirtualKeyRotationResult> {
        let mut tx = self.pool().begin().await.map_err(backend)?;
        let select = format!("SELECT {VIRTUAL_KEY_COLUMNS} FROM llm_virtual_keys WHERE id = ?");
        let row = sqlx::query(&select)
            .bind(old_id)
            .fetch_optional(&mut *tx)
            .await
            .map_err(backend)?
            .ok_or_else(|| not_found("virtual key", old_id))?;
        let old = virtual_key_from_row(&row);
        let previous_expires_at = old.expires_at;
        let now = Utc::now();

        let old_expiry = if rotation.grace_seconds == 0 {
            None
        } else {
            let grace = chrono::Duration::seconds(rotation.grace_seconds as i64);
            let candidate = now + grace;
            Some(
                old.expires_at
                    .map(|expires| expires.min(candidate))
                    .unwrap_or(candidate),
            )
        };

        let id = uuid::Uuid::new_v4().to_string();
        let family = if old.key_family.trim().is_empty() {
            old.id.clone()
        } else {
            old.key_family.clone()
        };
        let permissions = rotation
            .permissions_json
            .clone()
            .unwrap_or_else(|| old.permissions_json.clone());
        sqlx::query(
            "INSERT INTO llm_virtual_keys
                (id, name, key_prefix, key_hash, consumer_id, enabled, expires_at,
                 permissions_json, key_family, rotated_from)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(&id)
        .bind(&rotation.name)
        .bind(&rotation.key_prefix)
        .bind(&rotation.key_hash)
        .bind(&old.consumer_id)
        .bind(1_i64)
        .bind(rotation.expires_at.map(|value| value.to_rfc3339()))
        .bind(&permissions)
        .bind(&family)
        .bind(&old.id)
        .execute(&mut *tx)
        .await
        .map_err(conflict)?;

        if rotation.grace_seconds == 0 {
            sqlx::query("UPDATE llm_virtual_keys SET enabled = 0 WHERE id = ?")
                .bind(&old.id)
                .execute(&mut *tx)
                .await
                .map_err(backend)?;
        } else {
            sqlx::query("UPDATE llm_virtual_keys SET expires_at = ? WHERE id = ?")
                .bind(old_expiry.map(|value| value.to_rfc3339()))
                .bind(&old.id)
                .execute(&mut *tx)
                .await
                .map_err(backend)?;
        }

        tx.commit().await.map_err(backend)?;
        let key = self
            .get_virtual_key(&id)
            .await?
            .ok_or_else(|| not_found("virtual key", &id))?;
        Ok(LlmVirtualKeyRotationResult {
            key,
            replaces: old.id,
            previous_expires_at,
        })
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
                 SUM(input_tokens) AS input_tokens, SUM(output_tokens) AS output_tokens,
                 SUM(cost_micros) AS cost_micros
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
                cost_micros: row.get::<Option<i64>, _>("cost_micros").unwrap_or(0),
            })
            .collect())
    }

    pub async fn usage_by_provider(&self) -> LlmResult<Vec<LlmUsageProviderRow>> {
        let rows = sqlx::query(
            "SELECT provider_id, SUM(requests) AS requests, SUM(errors) AS errors,
                 SUM(input_tokens) AS input_tokens, SUM(output_tokens) AS output_tokens,
                 SUM(cost_micros) AS cost_micros
             FROM llm_usage_rollup GROUP BY provider_id ORDER BY requests DESC",
        )
        .fetch_all(self.pool())
        .await
        .map_err(backend)?;
        Ok(rows
            .iter()
            .map(|row| LlmUsageProviderRow {
                provider_id: row.get("provider_id"),
                requests: row.get::<Option<i64>, _>("requests").unwrap_or(0),
                errors: row.get::<Option<i64>, _>("errors").unwrap_or(0),
                input_tokens: row.get::<Option<i64>, _>("input_tokens").unwrap_or(0),
                output_tokens: row.get::<Option<i64>, _>("output_tokens").unwrap_or(0),
                cost_micros: row.get::<Option<i64>, _>("cost_micros").unwrap_or(0),
            })
            .collect())
    }

    pub async fn usage_totals(&self) -> LlmResult<LlmUsageSummaryRow> {
        let row = sqlx::query(
            "SELECT COALESCE(SUM(requests), 0) AS requests,
                 COALESCE(SUM(errors), 0) AS errors,
                 COALESCE(SUM(input_tokens), 0) AS input_tokens,
                 COALESCE(SUM(output_tokens), 0) AS output_tokens,
                 COALESCE(SUM(cost_micros), 0) AS cost_micros
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
            cost_micros: row.get::<Option<i64>, _>("cost_micros").unwrap_or(0),
        })
    }
}

// ---------------------------------------------------------------------------
// OAuth tokens

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LlmOAuthTokenRecord {
    pub account_id: String,
    pub access_token: String,
    pub refresh_token: Option<String>,
    pub token_type: String,
    pub scope: String,
    pub expires_at: Option<String>,
    pub extra_json: String,
}

impl LlmStore {
    pub async fn get_oauth_token(
        &self,
        account_id: &str,
    ) -> LlmResult<Option<LlmOAuthTokenRecord>> {
        let row = sqlx::query(
            "SELECT account_id, access_token, refresh_token, token_type, scope, expires_at,
                    extra_json
             FROM ai_provider_oauth_tokens WHERE account_id = ?",
        )
        .bind(account_id)
        .fetch_optional(self.pool())
        .await
        .map_err(backend)?;
        Ok(row.map(|row| LlmOAuthTokenRecord {
            account_id: row.get("account_id"),
            access_token: row.get("access_token"),
            refresh_token: row.get("refresh_token"),
            token_type: row.get("token_type"),
            scope: row.get("scope"),
            expires_at: row.get("expires_at"),
            extra_json: row.get("extra_json"),
        }))
    }

    pub async fn upsert_oauth_token(&self, token: &LlmOAuthTokenRecord) -> LlmResult<()> {
        let mut tx = self.pool.begin().await.map_err(backend)?;
        sqlx::query(
            "INSERT INTO ai_provider_oauth_tokens
                (account_id, access_token, refresh_token, token_type, scope, expires_at,
                 extra_json, created_at, updated_at)
             VALUES (?, ?, ?, ?, ?, ?, ?, strftime('%Y-%m-%dT%H:%M:%fZ', 'now'),
                     strftime('%Y-%m-%dT%H:%M:%fZ', 'now'))
             ON CONFLICT(account_id) DO UPDATE SET
                access_token = excluded.access_token,
                refresh_token = COALESCE(excluded.refresh_token, refresh_token),
                token_type = excluded.token_type,
                scope = excluded.scope,
                expires_at = excluded.expires_at,
                extra_json = excluded.extra_json,
                updated_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now')",
        )
        .bind(&token.account_id)
        .bind(&token.access_token)
        .bind(&token.refresh_token)
        .bind(&token.token_type)
        .bind(&token.scope)
        .bind(&token.expires_at)
        .bind(&token.extra_json)
        .execute(&mut *tx)
        .await
        .map_err(backend)?;
        sqlx::query(
            "UPDATE ai_provider_accounts
             SET credential_kind = 'oauth', oauth_status = 'authorized',
                 oauth_expires_at = ?, updated_at = CAST(strftime('%s', 'now') AS INTEGER)
             WHERE id = ?",
        )
        .bind(&token.expires_at)
        .bind(&token.account_id)
        .execute(&mut *tx)
        .await
        .map_err(backend)?;
        tx.commit().await.map_err(backend)
    }

    pub async fn delete_oauth_token(&self, account_id: &str) -> LlmResult<bool> {
        let mut tx = self.pool.begin().await.map_err(backend)?;
        let result = sqlx::query("DELETE FROM ai_provider_oauth_tokens WHERE account_id = ?")
            .bind(account_id)
            .execute(&mut *tx)
            .await
            .map_err(backend)?;
        sqlx::query(
            "UPDATE ai_provider_accounts
             SET oauth_status = 'none', oauth_expires_at = NULL,
                 updated_at = CAST(strftime('%s', 'now') AS INTEGER)
             WHERE id = ?",
        )
        .bind(account_id)
        .execute(&mut *tx)
        .await
        .map_err(backend)?;
        tx.commit().await.map_err(backend)?;
        Ok(result.rows_affected() > 0)
    }

    pub async fn mark_oauth_reauth(&self, account_id: &str, _reason: &str) -> LlmResult<()> {
        let mut tx = self.pool.begin().await.map_err(backend)?;
        sqlx::query("DELETE FROM ai_provider_oauth_tokens WHERE account_id = ?")
            .bind(account_id)
            .execute(&mut *tx)
            .await
            .map_err(backend)?;
        sqlx::query(
            "UPDATE ai_provider_accounts
             SET oauth_status = 'reauth_required', oauth_expires_at = NULL,
                 updated_at = CAST(strftime('%s', 'now') AS INTEGER)
             WHERE id = ?",
        )
        .bind(account_id)
        .execute(&mut *tx)
        .await
        .map_err(backend)?;
        tx.commit().await.map_err(backend)
    }
}

// ---------------------------------------------------------------------------
// PII engines

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LlmPiiEngineRecord {
    pub id: String,
    pub name: String,
    pub mode: String,
    pub transport: String,
    pub endpoint: Option<String>,
    pub command: Option<String>,
    pub args: Vec<String>,
    pub env_allow: Vec<String>,
    pub model_dir: Option<String>,
    pub config_json: String,
    pub enabled: bool,
    pub created_at: String,
    pub updated_at: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LlmPiiEngineInput {
    pub name: String,
    pub mode: String,
    pub transport: String,
    pub endpoint: Option<String>,
    pub command: Option<String>,
    pub args: Vec<String>,
    pub env_allow: Vec<String>,
    pub model_dir: Option<String>,
    pub config_json: String,
    pub enabled: bool,
}

const PII_ENGINE_COLUMNS: &str = "id, name, mode, transport, endpoint, command, args_json, \
     env_allow_json, model_dir, config_json, enabled, created_at, updated_at";

fn pii_engine_from_row(row: &sqlx::sqlite::SqliteRow) -> LlmPiiEngineRecord {
    let args_json: String = row.get("args_json");
    let env_allow_json: String = row.get("env_allow_json");
    LlmPiiEngineRecord {
        id: row.get("id"),
        name: row.get("name"),
        mode: row.get("mode"),
        transport: row.get("transport"),
        endpoint: row.get("endpoint"),
        command: row.get("command"),
        args: serde_json::from_str(&args_json).unwrap_or_default(),
        env_allow: serde_json::from_str(&env_allow_json).unwrap_or_default(),
        model_dir: row.get("model_dir"),
        config_json: row.get("config_json"),
        enabled: row.get::<i64, _>("enabled") != 0,
        created_at: row.get("created_at"),
        updated_at: row.get("updated_at"),
    }
}

impl LlmStore {
    pub async fn list_pii_engines(&self) -> LlmResult<Vec<LlmPiiEngineRecord>> {
        let sql =
            format!("SELECT {PII_ENGINE_COLUMNS} FROM llm_pii_engines ORDER BY name ASC, id ASC");
        let rows = sqlx::query(&sql)
            .fetch_all(self.pool())
            .await
            .map_err(backend)?;
        Ok(rows.iter().map(pii_engine_from_row).collect())
    }

    pub async fn get_pii_engine(&self, id: &str) -> LlmResult<Option<LlmPiiEngineRecord>> {
        let sql = format!("SELECT {PII_ENGINE_COLUMNS} FROM llm_pii_engines WHERE id = ?");
        let row = sqlx::query(&sql)
            .bind(id)
            .fetch_optional(self.pool())
            .await
            .map_err(backend)?;
        Ok(row.as_ref().map(pii_engine_from_row))
    }

    pub async fn get_pii_engine_by_name(
        &self,
        name: &str,
    ) -> LlmResult<Option<LlmPiiEngineRecord>> {
        let sql = format!("SELECT {PII_ENGINE_COLUMNS} FROM llm_pii_engines WHERE name = ?");
        let row = sqlx::query(&sql)
            .bind(name)
            .fetch_optional(self.pool())
            .await
            .map_err(backend)?;
        Ok(row.as_ref().map(pii_engine_from_row))
    }

    pub async fn create_pii_engine(
        &self,
        input: &LlmPiiEngineInput,
    ) -> LlmResult<LlmPiiEngineRecord> {
        let id = uuid::Uuid::new_v4().to_string();
        sqlx::query(
            "INSERT INTO llm_pii_engines
                (id, name, mode, transport, endpoint, command, args_json, env_allow_json,
                 model_dir, config_json, enabled)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(&id)
        .bind(&input.name)
        .bind(&input.mode)
        .bind(&input.transport)
        .bind(&input.endpoint)
        .bind(&input.command)
        .bind(
            serde_json::to_string(&input.args).map_err(|error| {
                LlmStorageError::Backend(format!("cannot encode args: {error}"))
            })?,
        )
        .bind(serde_json::to_string(&input.env_allow).map_err(|error| {
            LlmStorageError::Backend(format!("cannot encode env_allow: {error}"))
        })?)
        .bind(&input.model_dir)
        .bind(&input.config_json)
        .bind(i64::from(input.enabled))
        .execute(self.pool())
        .await
        .map_err(conflict)?;
        self.get_pii_engine(&id)
            .await?
            .ok_or_else(|| LlmStorageError::Backend("PII engine disappeared after insert".into()))
    }

    pub async fn update_pii_engine(
        &self,
        id: &str,
        input: &LlmPiiEngineInput,
    ) -> LlmResult<LlmPiiEngineRecord> {
        let result =
            sqlx::query(
                "UPDATE llm_pii_engines SET
                name = ?, mode = ?, transport = ?, endpoint = ?, command = ?,
                args_json = ?, env_allow_json = ?, model_dir = ?, config_json = ?,
                enabled = ?, updated_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now')
             WHERE id = ?",
            )
            .bind(&input.name)
            .bind(&input.mode)
            .bind(&input.transport)
            .bind(&input.endpoint)
            .bind(&input.command)
            .bind(serde_json::to_string(&input.args).map_err(|error| {
                LlmStorageError::Backend(format!("cannot encode args: {error}"))
            })?)
            .bind(serde_json::to_string(&input.env_allow).map_err(|error| {
                LlmStorageError::Backend(format!("cannot encode env_allow: {error}"))
            })?)
            .bind(&input.model_dir)
            .bind(&input.config_json)
            .bind(i64::from(input.enabled))
            .bind(id)
            .execute(self.pool())
            .await
            .map_err(conflict)?;
        if result.rows_affected() == 0 {
            return Err(not_found("pii engine", id));
        }
        self.get_pii_engine(id)
            .await?
            .ok_or_else(|| not_found("pii engine", id))
    }

    pub async fn delete_pii_engine(&self, id: &str) -> LlmResult<()> {
        let result = sqlx::query("DELETE FROM llm_pii_engines WHERE id = ?")
            .bind(id)
            .execute(self.pool())
            .await
            .map_err(backend)?;
        if result.rows_affected() == 0 {
            return Err(not_found("pii engine", id));
        }
        Ok(())
    }
}

/// Runs `body` inside a `BEGIN IMMEDIATE` transaction. Taking the write lock
/// before reading serialises counter updates across processes.
macro_rules! immediate_transaction {
    ($pool:expr, |$connection:ident| $body:expr) => {{
        let mut $connection = $pool.acquire().await.map_err(|error| {
            Error::Storage(format!("cannot open a counter transaction: {error}"))
        })?;
        sqlx::query("BEGIN IMMEDIATE")
            .execute(&mut *$connection)
            .await
            .map_err(|error| {
                Error::Storage(format!("cannot begin a counter transaction: {error}"))
            })?;
        let result: std::result::Result<_, sqlx::Error> = async { $body }.await;
        match result {
            Ok(value) => {
                sqlx::query("COMMIT")
                    .execute(&mut *$connection)
                    .await
                    .map_err(|error| {
                        Error::Storage(format!("cannot commit a counter transaction: {error}"))
                    })?;
                Ok(value)
            }
            Err(error) => {
                let _ = sqlx::query("ROLLBACK").execute(&mut *$connection).await;
                Err(Error::Storage(format!(
                    "counter transaction failed: {error}"
                )))
            }
        }
    }};
}

/// Shared LLM counters used by rate limiting and budget guards.
impl LlmStore {
    /// Atomically checks every window and increments all of them when none is
    /// exhausted. A rejected admission leaves every counter unchanged.
    pub async fn admit_counter_windows(
        &self,
        windows: &[CounterWindow<'_>],
    ) -> Result<CounterOutcome> {
        if windows.is_empty() {
            return Ok(CounterOutcome {
                counts: Vec::new(),
                accepted: true,
            });
        }
        immediate_transaction!(self.pool(), |connection| {
            admit_windows_in(&mut connection, windows).await
        })
    }

    pub async fn admit_budget_window(
        &self,
        key: &str,
        now_ms: i64,
        period_ms: i64,
        max_requests: Option<u64>,
        max_tokens: Option<u64>,
        estimate: u64,
    ) -> Result<BudgetWindowView> {
        immediate_transaction!(self.pool(), |connection| {
            admit_budget_in(
                &mut connection,
                key,
                now_ms,
                period_ms,
                max_requests,
                max_tokens,
                estimate,
            )
            .await
        })
    }

    pub async fn settle_budget_window(
        &self,
        key: &str,
        now_ms: i64,
        period_ms: i64,
        reserved: u64,
        actual: u64,
    ) -> Result<BudgetWindowView> {
        immediate_transaction!(self.pool(), |connection| {
            settle_budget_in(&mut connection, key, now_ms, period_ms, reserved, actual).await
        })
    }

    /// Removes expired calendar-window counters. Rolling budget windows are
    /// reused in place and need no pruning.
    pub async fn prune_llm_counters(&self, now_unix: i64) -> Result<u64> {
        let result = sqlx::query("DELETE FROM llm_counters WHERE window_end < ?1")
            .bind(now_unix)
            .execute(self.pool())
            .await
            .map_err(|error| Error::Storage(format!("cannot prune LLM counters: {error}")))?;
        Ok(result.rows_affected())
    }
}

async fn admit_windows_in(
    connection: &mut sqlx::SqliteConnection,
    windows: &[CounterWindow<'_>],
) -> std::result::Result<CounterOutcome, sqlx::Error> {
    let mut counts = Vec::with_capacity(windows.len());
    let mut accepted = true;
    for window in windows {
        let current: Option<i64> = sqlx::query_scalar(
            "SELECT value FROM llm_counters WHERE key = ?1 AND window_start = ?2",
        )
        .bind(window.key)
        .bind(window.window_start)
        .fetch_optional(&mut *connection)
        .await?;
        let current = u64::try_from(current.unwrap_or(0)).unwrap_or(0);
        if current >= window.limit {
            accepted = false;
        }
        counts.push(current);
    }
    if accepted {
        for (window, count) in windows.iter().zip(counts.iter_mut()) {
            *count = count.saturating_add(1);
            let value = i64::try_from(*count).unwrap_or(i64::MAX);
            sqlx::query(
                "INSERT INTO llm_counters (key, window_start, value, window_end)
                 VALUES (?1, ?2, ?3, ?4)
                 ON CONFLICT(key, window_start) DO UPDATE SET
                    value = excluded.value,
                    window_end = excluded.window_end",
            )
            .bind(window.key)
            .bind(window.window_start)
            .bind(value)
            .bind(window.window_end)
            .execute(&mut *connection)
            .await?;
        }
    }
    Ok(CounterOutcome { counts, accepted })
}

async fn admit_budget_in(
    connection: &mut sqlx::SqliteConnection,
    key: &str,
    now_ms: i64,
    period_ms: i64,
    max_requests: Option<u64>,
    max_tokens: Option<u64>,
    estimate: u64,
) -> std::result::Result<BudgetWindowView, sqlx::Error> {
    let (ends_at_ms, requests, tokens) = load_budget(connection, key, now_ms, period_ms).await?;
    let requests_exceeded = max_requests.is_some_and(|limit| requests >= limit);
    let tokens_exceeded = max_tokens.is_some_and(|limit| tokens.saturating_add(estimate) > limit);
    let accepted = !(requests_exceeded || tokens_exceeded);
    let requests = if accepted {
        requests.saturating_add(1)
    } else {
        requests
    };
    let tokens = if accepted {
        tokens.saturating_add(estimate)
    } else {
        tokens
    };
    store_budget(connection, key, ends_at_ms, requests, tokens).await?;
    Ok(BudgetWindowView {
        accepted,
        ends_at_ms,
        requests,
        tokens,
    })
}

async fn settle_budget_in(
    connection: &mut sqlx::SqliteConnection,
    key: &str,
    now_ms: i64,
    period_ms: i64,
    reserved: u64,
    actual: u64,
) -> std::result::Result<BudgetWindowView, sqlx::Error> {
    let (ends_at_ms, requests, tokens) = load_budget(connection, key, now_ms, period_ms).await?;
    let tokens = tokens.saturating_sub(reserved).saturating_add(actual);
    store_budget(connection, key, ends_at_ms, requests, tokens).await?;
    Ok(BudgetWindowView {
        accepted: true,
        ends_at_ms,
        requests,
        tokens,
    })
}

async fn load_budget(
    connection: &mut sqlx::SqliteConnection,
    key: &str,
    now_ms: i64,
    period_ms: i64,
) -> std::result::Result<(i64, u64, u64), sqlx::Error> {
    let row: Option<(i64, i64, i64)> = sqlx::query_as(
        "SELECT ends_at_ms, requests, tokens FROM llm_budget_windows WHERE key = ?1",
    )
    .bind(key)
    .fetch_optional(&mut *connection)
    .await?;
    let (mut ends_at_ms, mut requests, mut tokens) = row
        .map(|(ends, requests, tokens)| {
            (
                ends,
                u64::try_from(requests).unwrap_or(0),
                u64::try_from(tokens).unwrap_or(0),
            )
        })
        .unwrap_or((0, 0, 0));
    if ends_at_ms == 0 {
        ends_at_ms = now_ms.saturating_add(period_ms);
    } else if now_ms >= ends_at_ms {
        ends_at_ms = now_ms.saturating_add(period_ms);
        requests = 0;
        tokens = 0;
    }
    Ok((ends_at_ms, requests, tokens))
}

async fn store_budget(
    connection: &mut sqlx::SqliteConnection,
    key: &str,
    ends_at_ms: i64,
    requests: u64,
    tokens: u64,
) -> std::result::Result<(), sqlx::Error> {
    sqlx::query(
        "INSERT INTO llm_budget_windows (key, ends_at_ms, requests, tokens)
         VALUES (?1, ?2, ?3, ?4)
         ON CONFLICT(key) DO UPDATE SET
            ends_at_ms = excluded.ends_at_ms,
            requests = excluded.requests,
            tokens = excluded.tokens",
    )
    .bind(key)
    .bind(ends_at_ms)
    .bind(i64::try_from(requests).unwrap_or(i64::MAX))
    .bind(i64::try_from(tokens).unwrap_or(i64::MAX))
    .execute(&mut *connection)
    .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Storage;
    use sqlx::Row;

    async fn storage() -> Storage {
        Storage::connect_in_memory().await.unwrap()
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
                key_family: None,
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
    async fn virtual_key_usage_since_family_sums_rotated_keys() {
        let storage = storage().await;
        let family = "family-1".to_string();
        let mut ids = Vec::new();
        for (name, hash) in [("a", "hash-a"), ("b", "hash-b")] {
            let key = storage
                .llm
                .create_virtual_key(&LlmVirtualKeyInput {
                    name: name.into(),
                    key_prefix: "octra-abc".into(),
                    key_hash: hash.into(),
                    consumer_id: None,
                    enabled: true,
                    expires_at: None,
                    permissions_json: "{}".into(),
                    key_family: Some(family.clone()),
                })
                .await
                .unwrap();
            ids.push(key.id);
        }
        for (request_id, key_id, tokens, cost) in [("a", &ids[0], 10, 3), ("b", &ids[1], 5, 2)] {
            storage
                .llm
                .insert_usage(&LlmUsageInput {
                    request_id: request_id.into(),
                    virtual_model_id: "octra-default".into(),
                    api_key_id: Some(key_id.clone()),
                    input_tokens: tokens,
                    cost_micros: Some(cost),
                    created_at: Some("2026-10-01T05:12:00.000Z".into()),
                    ..LlmUsageInput::default()
                })
                .await
                .unwrap();
        }

        let usage = storage
            .llm
            .virtual_key_usage_since_family(&family, "2026-10-01T05:00:00.000Z")
            .await
            .unwrap();
        assert_eq!(usage.tokens, 15);
        assert_eq!(usage.cost_micros, 5);
    }

    #[tokio::test]
    async fn null_key_family_falls_back_to_the_key_id() {
        let storage = storage().await;
        let key = storage
            .llm
            .create_virtual_key(&LlmVirtualKeyInput {
                name: "legacy".into(),
                key_prefix: "octra-abc".into(),
                key_hash: "legacy-hash".into(),
                consumer_id: None,
                enabled: true,
                expires_at: None,
                permissions_json: "{}".into(),
                key_family: None,
            })
            .await
            .unwrap();
        sqlx::query("UPDATE llm_virtual_keys SET key_family = NULL WHERE id = ?")
            .bind(&key.id)
            .execute(storage.pool())
            .await
            .unwrap();
        let fetched = storage.llm.get_virtual_key(&key.id).await.unwrap().unwrap();
        assert_eq!(fetched.key_family, key.id);
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

    #[tokio::test]
    async fn oauth_tokens_round_trip_and_cascade() {
        use octra_ai_config::{AiProvider, AiProviderAccount};
        use serde_json::json;

        let storage = storage().await;
        let provider = AiProvider {
            id: "provider-oauth".into(),
            name: "oauth".into(),
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
            id: "account-oauth".into(),
            provider_id: provider.id.clone(),
            name: "subscription".into(),
            credential_kind: "static".into(),
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
            config: json!({}),
            created_at: 100,
            updated_at: 100,
        };
        storage.ai.upsert_account(&account).await.unwrap();

        let initial: (String, String) = sqlx::query_as(
            "SELECT credential_kind, oauth_status FROM ai_provider_accounts WHERE id = ?",
        )
        .bind(&account.id)
        .fetch_one(storage.pool())
        .await
        .unwrap();
        assert_eq!(initial, ("static".into(), "none".into()));

        let token = LlmOAuthTokenRecord {
            account_id: account.id.clone(),
            access_token: "access".into(),
            refresh_token: Some("refresh".into()),
            token_type: "Bearer".into(),
            scope: "chat".into(),
            expires_at: Some("2030-01-01T00:00:00.000Z".into()),
            extra_json: "{}".into(),
        };
        storage.llm.upsert_oauth_token(&token).await.unwrap();
        let loaded = storage
            .llm
            .get_oauth_token(&account.id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(loaded.access_token, "access");
        assert_eq!(loaded.refresh_token.as_deref(), Some("refresh"));
        let updated: (String, String, Option<String>) = sqlx::query_as(
            "SELECT credential_kind, oauth_status, oauth_expires_at
             FROM ai_provider_accounts WHERE id = ?",
        )
        .bind(&account.id)
        .fetch_one(storage.pool())
        .await
        .unwrap();
        assert_eq!(updated.0, "oauth");
        assert_eq!(updated.1, "authorized");
        assert_eq!(updated.2.as_deref(), Some("2030-01-01T00:00:00.000Z"));

        storage
            .llm
            .mark_oauth_reauth(&account.id, "invalid_grant")
            .await
            .unwrap();
        assert!(
            storage
                .llm
                .get_oauth_token(&account.id)
                .await
                .unwrap()
                .is_none()
        );
        let updated: (String, Option<String>) = sqlx::query_as(
            "SELECT oauth_status, oauth_expires_at FROM ai_provider_accounts WHERE id = ?",
        )
        .bind(&account.id)
        .fetch_one(storage.pool())
        .await
        .unwrap();
        assert_eq!(updated.0, "reauth_required");
        assert!(updated.1.is_none());

        storage.llm.upsert_oauth_token(&token).await.unwrap();
        assert!(storage.ai.delete_account(&account.id).await.unwrap());
        let orphaned: Option<String> = sqlx::query_scalar(
            "SELECT account_id FROM ai_provider_oauth_tokens WHERE account_id = ?",
        )
        .bind(&account.id)
        .fetch_optional(storage.pool())
        .await
        .unwrap();
        assert!(orphaned.is_none());
    }

    #[tokio::test]
    async fn counter_windows_are_shared_and_all_or_nothing() {
        let storage = storage().await;
        let windows = [
            CounterWindow {
                key: "rate:a",
                window_start: 10,
                window_end: 20,
                limit: 2,
            },
            CounterWindow {
                key: "rate:b",
                window_start: 10,
                window_end: 20,
                limit: 1,
            },
        ];
        let first = storage.llm.admit_counter_windows(&windows).await.unwrap();
        assert!(first.accepted);
        assert_eq!(first.counts, vec![1, 1]);

        let second = storage.llm.admit_counter_windows(&windows).await.unwrap();
        assert!(!second.accepted, "b is exhausted");
        assert_eq!(
            second.counts,
            vec![1, 1],
            "rejected admits do not increment"
        );

        // A second store handle on the same database sees the same counters.
        let other = storage.llm.clone();
        let third = other
            .admit_counter_windows(&[CounterWindow {
                key: "rate:a",
                window_start: 10,
                window_end: 20,
                limit: 2,
            }])
            .await
            .unwrap();
        assert!(third.accepted);
        assert_eq!(third.counts, vec![2]);
    }

    #[tokio::test]
    async fn budget_windows_roll_and_settle() {
        let storage = storage().await;
        let admitted = storage
            .llm
            .admit_budget_window("cfg-1", 1_000, 1_000, Some(1), None, 10)
            .await
            .unwrap();
        assert!(admitted.accepted);
        assert_eq!(admitted.requests, 1);
        assert_eq!(admitted.tokens, 10);

        let rejected = storage
            .llm
            .admit_budget_window("cfg-1", 1_000, 1_000, Some(1), None, 10)
            .await
            .unwrap();
        assert!(!rejected.accepted);
        assert_eq!(rejected.requests, 1);

        let settled = storage
            .llm
            .settle_budget_window("cfg-1", 1_000, 1_000, 10, 30)
            .await
            .unwrap();
        assert_eq!(settled.tokens, 30);

        let rolled = storage
            .llm
            .admit_budget_window("cfg-1", 2_000, 1_000, Some(1), None, 5)
            .await
            .unwrap();
        assert!(rolled.accepted);
        assert_eq!(rolled.ends_at_ms, 3_000);
        assert_eq!(rolled.requests, 1);
        assert_eq!(rolled.tokens, 5);
    }

    #[tokio::test]
    async fn prune_removes_only_expired_counters() {
        let storage = storage().await;
        storage
            .llm
            .admit_counter_windows(&[
                CounterWindow {
                    key: "expired",
                    window_start: 0,
                    window_end: 10,
                    limit: 1,
                },
                CounterWindow {
                    key: "live",
                    window_start: 0,
                    window_end: 1_000,
                    limit: 1,
                },
            ])
            .await
            .unwrap();
        assert_eq!(storage.llm.prune_llm_counters(100).await.unwrap(), 1);
        assert_eq!(storage.llm.prune_llm_counters(100).await.unwrap(), 0);
    }
}
