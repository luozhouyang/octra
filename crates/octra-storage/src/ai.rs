//! AI control-plane persistence: providers, accounts, models, targets, policies,
//! policy attachments, and configuration revisions.

use octra_ai_config::{
    AiControlPlaneData, AiModel, AiModelTarget, AiPolicy, AiPolicyAttachment, AiProvider,
    AiProviderAccount,
};
use octra_core::{Error, Result};
use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::Value;
use sqlx::pool::PoolConnection;
use sqlx::sqlite::{SqliteConnection, SqliteRow};
use sqlx::{Row, Sqlite, SqlitePool};

const PROVIDER_COLUMNS: &str = "id, name, protocol, base_url, auth_ref, capabilities_json, \
     config_json, enabled, created_at, updated_at";

const ACCOUNT_COLUMNS: &str = "id, provider_id, name, credential_kind, secret_ref, oauth_ref, \
     weight, priority, quota_limit, quota_unit, quota_period, quota_used, cooldown_until, \
     enabled, oauth_status, oauth_expires_at, config_json, created_at, updated_at";

const MODEL_COLUMNS: &str = "id, name, kind, capabilities_json, formats_json, route_paths_json, \
     balancer_json, enabled, created_at, updated_at";

const TARGET_COLUMNS: &str = "id, model_id, target_model_id, provider_id, upstream_model, \
     account_id, weight, priority, position, config_json, context_window, max_output_tokens, \
     capabilities_json, pricing_json, semantic_description, enabled, created_at, updated_at";

const POLICY_COLUMNS: &str = "id, name, policy_type, config_json, enabled, created_at, updated_at";

const ATTACHMENT_COLUMNS: &str = "id, policy_id, scope, scope_id, order_index, enabled, \
     created_at, updated_at";

const REVISION_COLUMNS: &str =
    "id, created_at, actor, status, diff_json, snapshot_json, applied_at";

fn write_error(context: &str, err: sqlx::Error) -> Error {
    if let sqlx::Error::Database(database) = &err
        && database.is_unique_violation()
    {
        return Error::Storage(format!("{context}: duplicate value"));
    }
    Error::Storage(format!("{context}: {err}"))
}

fn parse_json<T: DeserializeOwned>(column: &str, raw: &str) -> Result<T> {
    serde_json::from_str(raw)
        .map_err(|err| Error::Storage(format!("invalid JSON in {column}: {err}")))
}

fn parse_enum<T: DeserializeOwned>(column: &str, raw: &str) -> Result<T> {
    serde_json::from_value(Value::String(raw.to_string()))
        .map_err(|err| Error::Storage(format!("invalid value '{raw}' in {column}: {err}")))
}

fn encode_json<T: Serialize>(column: &str, value: &T) -> Result<String> {
    serde_json::to_string(value)
        .map_err(|err| Error::Storage(format!("cannot encode {column}: {err}")))
}

/// Encodes a snake_case enum as a bare string, without JSON quotes.
fn encode_enum<T: Serialize>(column: &str, value: &T) -> Result<String> {
    let encoded = serde_json::to_value(value)
        .map_err(|err| Error::Storage(format!("cannot encode {column}: {err}")))?;
    encoded
        .as_str()
        .map(str::to_string)
        .ok_or_else(|| Error::Storage(format!("{column} must encode to a string")))
}

fn provider_from_row(row: &SqliteRow) -> Result<AiProvider> {
    Ok(AiProvider {
        id: row.get("id"),
        name: row.get("name"),
        protocol: row.get("protocol"),
        base_url: row.get("base_url"),
        auth_ref: row.get("auth_ref"),
        capabilities: parse_json(
            "ai_providers.capabilities_json",
            &row.get::<String, _>("capabilities_json"),
        )?,
        config: parse_json(
            "ai_providers.config_json",
            &row.get::<String, _>("config_json"),
        )?,
        enabled: row.get::<i64, _>("enabled") != 0,
        created_at: row.get("created_at"),
        updated_at: row.get("updated_at"),
    })
}

fn account_from_row(row: &SqliteRow) -> Result<AiProviderAccount> {
    Ok(AiProviderAccount {
        id: row.get("id"),
        provider_id: row.get("provider_id"),
        name: row.get("name"),
        credential_kind: row.get("credential_kind"),
        secret_ref: row.get("secret_ref"),
        oauth_ref: row.get("oauth_ref"),
        weight: row.get("weight"),
        priority: row.get("priority"),
        quota_limit: row.get("quota_limit"),
        quota_unit: row.get("quota_unit"),
        quota_period: row.get("quota_period"),
        quota_used: row.get("quota_used"),
        cooldown_until: row.get("cooldown_until"),
        enabled: row.get::<i64, _>("enabled") != 0,
        oauth_status: row.get("oauth_status"),
        oauth_expires_at: row.get("oauth_expires_at"),
        config: parse_json(
            "ai_provider_accounts.config_json",
            &row.get::<String, _>("config_json"),
        )?,
        created_at: row.get("created_at"),
        updated_at: row.get("updated_at"),
    })
}

fn model_from_row(row: &SqliteRow) -> Result<AiModel> {
    Ok(AiModel {
        id: row.get("id"),
        name: row.get("name"),
        kind: parse_enum("ai_models.kind", &row.get::<String, _>("kind"))?,
        capabilities: parse_json(
            "ai_models.capabilities_json",
            &row.get::<String, _>("capabilities_json"),
        )?,
        formats: parse_json(
            "ai_models.formats_json",
            &row.get::<String, _>("formats_json"),
        )?,
        route_paths: parse_json(
            "ai_models.route_paths_json",
            &row.get::<String, _>("route_paths_json"),
        )?,
        balancer: parse_json(
            "ai_models.balancer_json",
            &row.get::<String, _>("balancer_json"),
        )?,
        enabled: row.get::<i64, _>("enabled") != 0,
        created_at: row.get("created_at"),
        updated_at: row.get("updated_at"),
    })
}

fn target_from_row(row: &SqliteRow) -> Result<AiModelTarget> {
    Ok(AiModelTarget {
        id: row.get("id"),
        model_id: row.get("model_id"),
        target_model_id: row.get("target_model_id"),
        provider_id: row.get("provider_id"),
        upstream_model: row.get("upstream_model"),
        account_id: row.get("account_id"),
        weight: row.get("weight"),
        priority: row.get("priority"),
        position: row.get("position"),
        config: parse_json(
            "ai_model_targets.config_json",
            &row.get::<String, _>("config_json"),
        )?,
        context_window: row.get("context_window"),
        max_output_tokens: row.get("max_output_tokens"),
        capabilities: parse_json(
            "ai_model_targets.capabilities_json",
            &row.get::<String, _>("capabilities_json"),
        )?,
        pricing: parse_json(
            "ai_model_targets.pricing_json",
            &row.get::<String, _>("pricing_json"),
        )?,
        semantic_description: row.get("semantic_description"),
        enabled: row.get::<i64, _>("enabled") != 0,
        created_at: row.get("created_at"),
        updated_at: row.get("updated_at"),
    })
}

fn policy_from_row(row: &SqliteRow) -> Result<AiPolicy> {
    Ok(AiPolicy {
        id: row.get("id"),
        name: row.get("name"),
        policy_type: parse_enum(
            "ai_policies.policy_type",
            &row.get::<String, _>("policy_type"),
        )?,
        config: parse_json(
            "ai_policies.config_json",
            &row.get::<String, _>("config_json"),
        )?,
        enabled: row.get::<i64, _>("enabled") != 0,
        created_at: row.get("created_at"),
        updated_at: row.get("updated_at"),
    })
}

fn attachment_from_row(row: &SqliteRow) -> Result<AiPolicyAttachment> {
    let scope_id = row.get::<String, _>("scope_id");
    Ok(AiPolicyAttachment {
        id: row.get("id"),
        policy_id: row.get("policy_id"),
        scope: parse_enum(
            "ai_policy_attachments.scope",
            &row.get::<String, _>("scope"),
        )?,
        scope_id: if scope_id.is_empty() {
            None
        } else {
            Some(scope_id)
        },
        order: row.get("order_index"),
        enabled: row.get::<i64, _>("enabled") != 0,
        created_at: row.get("created_at"),
        updated_at: row.get("updated_at"),
    })
}

fn revision_from_row(row: &SqliteRow) -> AiConfigRevisionRecord {
    AiConfigRevisionRecord {
        id: row.get("id"),
        created_at: row.get("created_at"),
        actor: row.get("actor"),
        status: row.get("status"),
        diff_json: row.get("diff_json"),
        snapshot_json: row.get("snapshot_json"),
        applied_at: row.get("applied_at"),
    }
}

/// One persisted control-plane revision. `diff_json` and `snapshot_json` are
/// kept as raw JSON text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AiConfigRevisionRecord {
    pub id: String,
    pub created_at: i64,
    pub actor: String,
    pub status: String,
    pub diff_json: String,
    pub snapshot_json: String,
    pub applied_at: Option<i64>,
}

#[derive(Clone)]
pub struct AiStore {
    pool: SqlitePool,
}

impl AiStore {
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }

    fn ai_pool(&self) -> &SqlitePool {
        &self.pool
    }

    async fn acquire_write(&self, context: &str) -> Result<PoolConnection<Sqlite>> {
        self.ai_pool()
            .acquire()
            .await
            .map_err(|err| write_error(context, err))
    }

    async fn query_all_rows(&self, context: &str, sql: &str) -> Result<Vec<SqliteRow>> {
        sqlx::query(sql)
            .fetch_all(self.ai_pool())
            .await
            .map_err(|err| write_error(context, err))
    }

    async fn query_all_rows_filtered(
        &self,
        context: &str,
        sql: &str,
        filter: &str,
    ) -> Result<Vec<SqliteRow>> {
        sqlx::query(sql)
            .bind(filter)
            .fetch_all(self.ai_pool())
            .await
            .map_err(|err| write_error(context, err))
    }

    async fn query_row_by_id(
        &self,
        context: &str,
        sql: &str,
        id: &str,
    ) -> Result<Option<SqliteRow>> {
        sqlx::query(sql)
            .bind(id)
            .fetch_optional(self.ai_pool())
            .await
            .map_err(|err| write_error(context, err))
    }

    async fn delete_row_by_id(&self, context: &str, sql: &str, id: &str) -> Result<bool> {
        let result = sqlx::query(sql)
            .bind(id)
            .execute(self.ai_pool())
            .await
            .map_err(|err| write_error(context, err))?;
        Ok(result.rows_affected() > 0)
    }

    /// Loads the complete control plane. Every JSON column is decoded strictly;
    /// a malformed row fails the whole load with a storage error.
    pub async fn load_control_plane(&self) -> Result<AiControlPlaneData> {
        let provider_rows = sqlx::query(&format!(
            "SELECT {PROVIDER_COLUMNS} FROM ai_providers ORDER BY name ASC, id ASC"
        ))
        .fetch_all(self.ai_pool())
        .await
        .map_err(|err| write_error("cannot list ai providers", err))?;
        let providers = provider_rows
            .iter()
            .map(provider_from_row)
            .collect::<Result<Vec<_>>>()?;

        let account_rows = sqlx::query(&format!(
            "SELECT {ACCOUNT_COLUMNS} FROM ai_provider_accounts \
             ORDER BY provider_id ASC, priority ASC, name ASC, id ASC"
        ))
        .fetch_all(self.ai_pool())
        .await
        .map_err(|err| write_error("cannot list ai provider accounts", err))?;
        let accounts = account_rows
            .iter()
            .map(account_from_row)
            .collect::<Result<Vec<_>>>()?;

        let model_rows = sqlx::query(&format!(
            "SELECT {MODEL_COLUMNS} FROM ai_models ORDER BY name ASC, id ASC"
        ))
        .fetch_all(self.ai_pool())
        .await
        .map_err(|err| write_error("cannot list ai models", err))?;
        let models = model_rows
            .iter()
            .map(model_from_row)
            .collect::<Result<Vec<_>>>()?;

        let target_rows = sqlx::query(&format!(
            "SELECT {TARGET_COLUMNS} FROM ai_model_targets \
             ORDER BY model_id ASC, priority ASC, position ASC, id ASC"
        ))
        .fetch_all(self.ai_pool())
        .await
        .map_err(|err| write_error("cannot list ai model targets", err))?;
        let targets = target_rows
            .iter()
            .map(target_from_row)
            .collect::<Result<Vec<_>>>()?;

        let policy_rows = sqlx::query(&format!(
            "SELECT {POLICY_COLUMNS} FROM ai_policies ORDER BY name ASC, id ASC"
        ))
        .fetch_all(self.ai_pool())
        .await
        .map_err(|err| write_error("cannot list ai policies", err))?;
        let policies = policy_rows
            .iter()
            .map(policy_from_row)
            .collect::<Result<Vec<_>>>()?;

        let attachment_rows = sqlx::query(&format!(
            "SELECT {ATTACHMENT_COLUMNS} FROM ai_policy_attachments \
             ORDER BY scope ASC, scope_id ASC, order_index ASC, id ASC"
        ))
        .fetch_all(self.ai_pool())
        .await
        .map_err(|err| write_error("cannot list ai policy attachments", err))?;
        let attachments = attachment_rows
            .iter()
            .map(attachment_from_row)
            .collect::<Result<Vec<_>>>()?;

        Ok(AiControlPlaneData {
            providers,
            accounts,
            models,
            targets,
            policies,
            attachments,
        })
    }

    pub async fn upsert_provider(&self, provider: &AiProvider) -> Result<()> {
        let mut conn = self.acquire_write("cannot upsert ai provider").await?;
        Self::upsert_provider_in(&mut conn, provider).await
    }

    async fn upsert_provider_in(conn: &mut SqliteConnection, provider: &AiProvider) -> Result<()> {
        sqlx::query(
            "INSERT INTO ai_providers
                 (id, name, protocol, base_url, auth_ref, capabilities_json, config_json,
                  enabled, created_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)
             ON CONFLICT(id) DO UPDATE SET
                 name = excluded.name,
                 protocol = excluded.protocol,
                 base_url = excluded.base_url,
                 auth_ref = excluded.auth_ref,
                 capabilities_json = excluded.capabilities_json,
                 config_json = excluded.config_json,
                 enabled = excluded.enabled,
                 updated_at = excluded.updated_at",
        )
        .bind(&provider.id)
        .bind(&provider.name)
        .bind(&provider.protocol)
        .bind(&provider.base_url)
        .bind(&provider.auth_ref)
        .bind(encode_json(
            "ai_providers.capabilities_json",
            &provider.capabilities,
        )?)
        .bind(encode_json("ai_providers.config_json", &provider.config)?)
        .bind(i64::from(provider.enabled))
        .bind(provider.created_at)
        .bind(provider.updated_at)
        .execute(&mut *conn)
        .await
        .map_err(|err| write_error("cannot upsert ai provider", err))?;
        Ok(())
    }

    pub async fn upsert_account(&self, account: &AiProviderAccount) -> Result<()> {
        let mut conn = self
            .acquire_write("cannot upsert ai provider account")
            .await?;
        Self::upsert_account_in(&mut conn, account).await
    }

    async fn upsert_account_in(
        conn: &mut SqliteConnection,
        account: &AiProviderAccount,
    ) -> Result<()> {
        sqlx::query(
            "INSERT INTO ai_provider_accounts
                 (id, provider_id, name, credential_kind, secret_ref, oauth_ref, weight,
                  priority, quota_limit, quota_unit, quota_period, quota_used, cooldown_until,
                  enabled, oauth_status, oauth_expires_at, config_json, created_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16,
                     ?17, ?18, ?19)
             ON CONFLICT(id) DO UPDATE SET
                 provider_id = excluded.provider_id,
                 name = excluded.name,
                 credential_kind = excluded.credential_kind,
                 secret_ref = excluded.secret_ref,
                 oauth_ref = excluded.oauth_ref,
                 weight = excluded.weight,
                 priority = excluded.priority,
                 quota_limit = excluded.quota_limit,
                 quota_unit = excluded.quota_unit,
                 quota_period = excluded.quota_period,
                 quota_used = excluded.quota_used,
                 cooldown_until = excluded.cooldown_until,
                 enabled = excluded.enabled,
                 config_json = excluded.config_json,
                 updated_at = excluded.updated_at",
        )
        .bind(&account.id)
        .bind(&account.provider_id)
        .bind(&account.name)
        .bind(&account.credential_kind)
        .bind(&account.secret_ref)
        .bind(&account.oauth_ref)
        .bind(account.weight)
        .bind(account.priority)
        .bind(account.quota_limit)
        .bind(&account.quota_unit)
        .bind(&account.quota_period)
        .bind(account.quota_used)
        .bind(&account.cooldown_until)
        .bind(i64::from(account.enabled))
        .bind(&account.oauth_status)
        .bind(&account.oauth_expires_at)
        .bind(encode_json(
            "ai_provider_accounts.config_json",
            &account.config,
        )?)
        .bind(account.created_at)
        .bind(account.updated_at)
        .execute(&mut *conn)
        .await
        .map_err(|err| write_error("cannot upsert ai provider account", err))?;
        Ok(())
    }

    pub async fn upsert_model(&self, model: &AiModel) -> Result<()> {
        let mut conn = self.acquire_write("cannot upsert ai model").await?;
        Self::upsert_model_in(&mut conn, model).await
    }

    async fn upsert_model_in(conn: &mut SqliteConnection, model: &AiModel) -> Result<()> {
        sqlx::query(
            "INSERT INTO ai_models
                 (id, name, kind, capabilities_json, formats_json, route_paths_json,
                  balancer_json, enabled, created_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)
             ON CONFLICT(id) DO UPDATE SET
                 name = excluded.name,
                 kind = excluded.kind,
                 capabilities_json = excluded.capabilities_json,
                 formats_json = excluded.formats_json,
                 route_paths_json = excluded.route_paths_json,
                 balancer_json = excluded.balancer_json,
                 enabled = excluded.enabled,
                 updated_at = excluded.updated_at",
        )
        .bind(&model.id)
        .bind(&model.name)
        .bind(encode_enum("ai_models.kind", &model.kind)?)
        .bind(encode_json(
            "ai_models.capabilities_json",
            &model.capabilities,
        )?)
        .bind(encode_json("ai_models.formats_json", &model.formats)?)
        .bind(encode_json(
            "ai_models.route_paths_json",
            &model.route_paths,
        )?)
        .bind(encode_json("ai_models.balancer_json", &model.balancer)?)
        .bind(i64::from(model.enabled))
        .bind(model.created_at)
        .bind(model.updated_at)
        .execute(&mut *conn)
        .await
        .map_err(|err| write_error("cannot upsert ai model", err))?;
        Ok(())
    }

    pub async fn upsert_target(&self, target: &AiModelTarget) -> Result<()> {
        let mut conn = self.acquire_write("cannot upsert ai model target").await?;
        Self::upsert_target_in(&mut conn, target).await
    }

    async fn upsert_target_in(conn: &mut SqliteConnection, target: &AiModelTarget) -> Result<()> {
        sqlx::query(
            "INSERT INTO ai_model_targets
                 (id, model_id, target_model_id, provider_id, upstream_model, account_id,
                  weight, priority, position, config_json, context_window, max_output_tokens,
                  capabilities_json, pricing_json, semantic_description, enabled, created_at,
                  updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18)
             ON CONFLICT(id) DO UPDATE SET
                 model_id = excluded.model_id,
                 target_model_id = excluded.target_model_id,
                 provider_id = excluded.provider_id,
                 upstream_model = excluded.upstream_model,
                 account_id = excluded.account_id,
                 weight = excluded.weight,
                 priority = excluded.priority,
                 position = excluded.position,
                 config_json = excluded.config_json,
                 context_window = excluded.context_window,
                 max_output_tokens = excluded.max_output_tokens,
                 capabilities_json = excluded.capabilities_json,
                 pricing_json = excluded.pricing_json,
                 semantic_description = excluded.semantic_description,
                 enabled = excluded.enabled,
                 updated_at = excluded.updated_at",
        )
        .bind(&target.id)
        .bind(&target.model_id)
        .bind(&target.target_model_id)
        .bind(&target.provider_id)
        .bind(&target.upstream_model)
        .bind(&target.account_id)
        .bind(target.weight)
        .bind(target.priority)
        .bind(target.position)
        .bind(encode_json("ai_model_targets.config_json", &target.config)?)
        .bind(target.context_window)
        .bind(target.max_output_tokens)
        .bind(encode_json(
            "ai_model_targets.capabilities_json",
            &target.capabilities,
        )?)
        .bind(encode_json(
            "ai_model_targets.pricing_json",
            &target.pricing,
        )?)
        .bind(&target.semantic_description)
        .bind(i64::from(target.enabled))
        .bind(target.created_at)
        .bind(target.updated_at)
        .execute(&mut *conn)
        .await
        .map_err(|err| write_error("cannot upsert ai model target", err))?;
        Ok(())
    }

    pub async fn upsert_policy(&self, policy: &AiPolicy) -> Result<()> {
        let mut conn = self.acquire_write("cannot upsert ai policy").await?;
        Self::upsert_policy_in(&mut conn, policy).await
    }

    async fn upsert_policy_in(conn: &mut SqliteConnection, policy: &AiPolicy) -> Result<()> {
        sqlx::query(
            "INSERT INTO ai_policies
                 (id, name, policy_type, config_json, enabled, created_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
             ON CONFLICT(id) DO UPDATE SET
                 name = excluded.name,
                 policy_type = excluded.policy_type,
                 config_json = excluded.config_json,
                 enabled = excluded.enabled,
                 updated_at = excluded.updated_at",
        )
        .bind(&policy.id)
        .bind(&policy.name)
        .bind(encode_enum("ai_policies.policy_type", &policy.policy_type)?)
        .bind(encode_json("ai_policies.config_json", &policy.config)?)
        .bind(i64::from(policy.enabled))
        .bind(policy.created_at)
        .bind(policy.updated_at)
        .execute(&mut *conn)
        .await
        .map_err(|err| write_error("cannot upsert ai policy", err))?;
        Ok(())
    }

    pub async fn upsert_attachment(&self, attachment: &AiPolicyAttachment) -> Result<()> {
        let mut conn = self
            .acquire_write("cannot upsert ai policy attachment")
            .await?;
        Self::upsert_attachment_in(&mut conn, attachment).await
    }

    async fn upsert_attachment_in(
        conn: &mut SqliteConnection,
        attachment: &AiPolicyAttachment,
    ) -> Result<()> {
        sqlx::query(
            "INSERT INTO ai_policy_attachments
                 (id, policy_id, scope, scope_id, order_index, enabled, created_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
             ON CONFLICT(id) DO UPDATE SET
                 policy_id = excluded.policy_id,
                 scope = excluded.scope,
                 scope_id = excluded.scope_id,
                 order_index = excluded.order_index,
                 enabled = excluded.enabled,
                 updated_at = excluded.updated_at",
        )
        .bind(&attachment.id)
        .bind(&attachment.policy_id)
        .bind(encode_enum(
            "ai_policy_attachments.scope",
            &attachment.scope,
        )?)
        .bind(attachment.scope_id.as_deref().unwrap_or(""))
        .bind(attachment.order)
        .bind(i64::from(attachment.enabled))
        .bind(attachment.created_at)
        .bind(attachment.updated_at)
        .execute(&mut *conn)
        .await
        .map_err(|err| write_error("cannot upsert ai policy attachment", err))?;
        Ok(())
    }

    pub async fn list_providers(&self) -> Result<Vec<AiProvider>> {
        let rows = self
            .query_all_rows(
                "cannot list ai providers",
                &format!("SELECT {PROVIDER_COLUMNS} FROM ai_providers ORDER BY name ASC, id ASC"),
            )
            .await?;
        rows.iter().map(provider_from_row).collect()
    }

    pub async fn get_provider(&self, id: &str) -> Result<Option<AiProvider>> {
        let row = self
            .query_row_by_id(
                "cannot load ai provider",
                &format!("SELECT {PROVIDER_COLUMNS} FROM ai_providers WHERE id = ?1"),
                id,
            )
            .await?;
        row.as_ref().map(provider_from_row).transpose()
    }

    pub async fn delete_provider(&self, id: &str) -> Result<bool> {
        self.delete_row_by_id(
            "cannot delete ai provider",
            "DELETE FROM ai_providers WHERE id = ?1",
            id,
        )
        .await
    }

    /// Lists accounts ordered by provider, priority, name, and id. When
    /// `provider_id` is set, only that provider's accounts are returned.
    pub async fn list_accounts(&self, provider_id: Option<&str>) -> Result<Vec<AiProviderAccount>> {
        let rows = match provider_id {
            Some(provider_id) => {
                self.query_all_rows_filtered(
                    "cannot list ai provider accounts",
                    &format!(
                        "SELECT {ACCOUNT_COLUMNS} FROM ai_provider_accounts WHERE provider_id = ?1 \
                         ORDER BY provider_id ASC, priority ASC, name ASC, id ASC"
                    ),
                    provider_id,
                )
                .await?
            }
            None => {
                self.query_all_rows(
                    "cannot list ai provider accounts",
                    &format!(
                        "SELECT {ACCOUNT_COLUMNS} FROM ai_provider_accounts \
                         ORDER BY provider_id ASC, priority ASC, name ASC, id ASC"
                    ),
                )
                .await?
            }
        };
        rows.iter().map(account_from_row).collect()
    }

    pub async fn get_account(&self, id: &str) -> Result<Option<AiProviderAccount>> {
        let row = self
            .query_row_by_id(
                "cannot load ai provider account",
                &format!("SELECT {ACCOUNT_COLUMNS} FROM ai_provider_accounts WHERE id = ?1"),
                id,
            )
            .await?;
        row.as_ref().map(account_from_row).transpose()
    }

    /// Returns the server-managed OAuth status and expiry for an account.
    pub async fn account_oauth_status(&self, id: &str) -> Result<Option<(String, Option<String>)>> {
        let row = sqlx::query(
            "SELECT oauth_status, oauth_expires_at FROM ai_provider_accounts WHERE id = ?1",
        )
        .bind(id)
        .fetch_optional(self.ai_pool())
        .await
        .map_err(|error| write_error("cannot read ai account OAuth state", error))?;
        Ok(row.map(|row| (row.get("oauth_status"), row.get("oauth_expires_at"))))
    }

    pub async fn delete_account(&self, id: &str) -> Result<bool> {
        self.delete_row_by_id(
            "cannot delete ai provider account",
            "DELETE FROM ai_provider_accounts WHERE id = ?1",
            id,
        )
        .await
    }

    /// Clears the rolling quota counter and the persisted cooldown.
    pub async fn reset_account_quota(&self, id: &str) -> Result<bool> {
        let result = sqlx::query(
            "UPDATE ai_provider_accounts
             SET quota_used = 0, cooldown_until = NULL,
                 updated_at = CAST(strftime('%s', 'now') AS INTEGER)
             WHERE id = ?1",
        )
        .bind(id)
        .execute(self.ai_pool())
        .await
        .map_err(|err| write_error("cannot reset ai account quota", err))?;
        Ok(result.rows_affected() > 0)
    }

    /// Adds one request's consumption to the account quota. The increment is
    /// applied to the counter matching the account's configured unit.
    pub async fn consume_account_quota(
        &self,
        id: &str,
        requests: i64,
        tokens: i64,
        cost_micros: i64,
    ) -> Result<()> {
        sqlx::query(
            "UPDATE ai_provider_accounts SET
                 quota_used = quota_used + CASE quota_unit
                     WHEN 'tokens' THEN ?1
                     WHEN 'cost_micros' THEN ?2
                     ELSE ?3
                 END,
                 updated_at = CAST(strftime('%s', 'now') AS INTEGER)
             WHERE id = ?4",
        )
        .bind(tokens)
        .bind(cost_micros)
        .bind(requests)
        .bind(id)
        .execute(self.ai_pool())
        .await
        .map_err(|err| write_error("cannot consume ai account quota", err))?;
        Ok(())
    }

    /// Persists a cooldown. `last_error` is accepted for call-site
    /// compatibility but not stored: the AI account schema has no error column.
    pub async fn set_account_cooldown(
        &self,
        id: &str,
        until: &str,
        _last_error: Option<&str>,
    ) -> Result<()> {
        sqlx::query(
            "UPDATE ai_provider_accounts SET cooldown_until = ?1,
                 updated_at = CAST(strftime('%s', 'now') AS INTEGER)
             WHERE id = ?2",
        )
        .bind(until)
        .bind(id)
        .execute(self.ai_pool())
        .await
        .map_err(|err| write_error("cannot set ai account cooldown", err))?;
        Ok(())
    }

    /// Persists a rolled quota period by resetting the counter. `reset_at` is
    /// accepted for call-site compatibility but not stored: the AI account
    /// schema has no quota reset timestamp, so period-end persistence is
    /// recomputed at process start.
    pub async fn roll_account_quota(&self, id: &str, _reset_at: &str) -> Result<()> {
        sqlx::query(
            "UPDATE ai_provider_accounts
             SET quota_used = 0,
                 updated_at = CAST(strftime('%s', 'now') AS INTEGER)
             WHERE id = ?1",
        )
        .bind(id)
        .execute(self.ai_pool())
        .await
        .map_err(|err| write_error("cannot roll ai account quota", err))?;
        Ok(())
    }

    pub async fn list_models(&self) -> Result<Vec<AiModel>> {
        let rows = self
            .query_all_rows(
                "cannot list ai models",
                &format!("SELECT {MODEL_COLUMNS} FROM ai_models ORDER BY name ASC, id ASC"),
            )
            .await?;
        rows.iter().map(model_from_row).collect()
    }

    pub async fn get_model(&self, id: &str) -> Result<Option<AiModel>> {
        let row = self
            .query_row_by_id(
                "cannot load ai model",
                &format!("SELECT {MODEL_COLUMNS} FROM ai_models WHERE id = ?1"),
                id,
            )
            .await?;
        row.as_ref().map(model_from_row).transpose()
    }

    pub async fn delete_model(&self, id: &str) -> Result<bool> {
        self.delete_row_by_id(
            "cannot delete ai model",
            "DELETE FROM ai_models WHERE id = ?1",
            id,
        )
        .await
    }

    /// Lists targets ordered by model, priority, position, and id. When
    /// `model_id` is set, only that model's targets are returned.
    pub async fn list_targets(&self, model_id: Option<&str>) -> Result<Vec<AiModelTarget>> {
        let rows = match model_id {
            Some(model_id) => {
                self.query_all_rows_filtered(
                    "cannot list ai model targets",
                    &format!(
                        "SELECT {TARGET_COLUMNS} FROM ai_model_targets WHERE model_id = ?1 \
                         ORDER BY model_id ASC, priority ASC, position ASC, id ASC"
                    ),
                    model_id,
                )
                .await?
            }
            None => {
                self.query_all_rows(
                    "cannot list ai model targets",
                    &format!(
                        "SELECT {TARGET_COLUMNS} FROM ai_model_targets \
                         ORDER BY model_id ASC, priority ASC, position ASC, id ASC"
                    ),
                )
                .await?
            }
        };
        rows.iter().map(target_from_row).collect()
    }

    pub async fn get_target(&self, id: &str) -> Result<Option<AiModelTarget>> {
        let row = self
            .query_row_by_id(
                "cannot load ai model target",
                &format!("SELECT {TARGET_COLUMNS} FROM ai_model_targets WHERE id = ?1"),
                id,
            )
            .await?;
        row.as_ref().map(target_from_row).transpose()
    }

    pub async fn delete_target(&self, id: &str) -> Result<bool> {
        self.delete_row_by_id(
            "cannot delete ai model target",
            "DELETE FROM ai_model_targets WHERE id = ?1",
            id,
        )
        .await
    }

    pub async fn list_policies(&self) -> Result<Vec<AiPolicy>> {
        let rows = self
            .query_all_rows(
                "cannot list ai policies",
                &format!("SELECT {POLICY_COLUMNS} FROM ai_policies ORDER BY name ASC, id ASC"),
            )
            .await?;
        rows.iter().map(policy_from_row).collect()
    }

    pub async fn get_policy(&self, id: &str) -> Result<Option<AiPolicy>> {
        let row = self
            .query_row_by_id(
                "cannot load ai policy",
                &format!("SELECT {POLICY_COLUMNS} FROM ai_policies WHERE id = ?1"),
                id,
            )
            .await?;
        row.as_ref().map(policy_from_row).transpose()
    }

    pub async fn delete_policy(&self, id: &str) -> Result<bool> {
        self.delete_row_by_id(
            "cannot delete ai policy",
            "DELETE FROM ai_policies WHERE id = ?1",
            id,
        )
        .await
    }

    /// Lists attachments ordered by scope, scope id, order, and id. When
    /// `policy_id` is set, only that policy's attachments are returned.
    pub async fn list_attachments(
        &self,
        policy_id: Option<&str>,
    ) -> Result<Vec<AiPolicyAttachment>> {
        let rows = match policy_id {
            Some(policy_id) => {
                self.query_all_rows_filtered(
                    "cannot list ai policy attachments",
                    &format!(
                        "SELECT {ATTACHMENT_COLUMNS} FROM ai_policy_attachments WHERE policy_id = ?1 \
                         ORDER BY scope ASC, scope_id ASC, order_index ASC, id ASC"
                    ),
                    policy_id,
                )
                .await?
            }
            None => {
                self.query_all_rows(
                    "cannot list ai policy attachments",
                    &format!(
                        "SELECT {ATTACHMENT_COLUMNS} FROM ai_policy_attachments \
                         ORDER BY scope ASC, scope_id ASC, order_index ASC, id ASC"
                    ),
                )
                .await?
            }
        };
        rows.iter().map(attachment_from_row).collect()
    }

    pub async fn get_attachment(&self, id: &str) -> Result<Option<AiPolicyAttachment>> {
        let row = self
            .query_row_by_id(
                "cannot load ai policy attachment",
                &format!("SELECT {ATTACHMENT_COLUMNS} FROM ai_policy_attachments WHERE id = ?1"),
                id,
            )
            .await?;
        row.as_ref().map(attachment_from_row).transpose()
    }

    pub async fn delete_attachment(&self, id: &str) -> Result<bool> {
        self.delete_row_by_id(
            "cannot delete ai policy attachment",
            "DELETE FROM ai_policy_attachments WHERE id = ?1",
            id,
        )
        .await
    }

    /// Imports a complete control plane by upserting every record in
    /// dependency order: providers, accounts, models, targets, policies, then
    /// attachments.
    ///
    /// Existing rows are never deleted. All upserts run inside one transaction,
    /// so a failure rolls the whole import back.
    pub async fn import_control_plane(&self, data: &AiControlPlaneData) -> Result<()> {
        let mut tx = self
            .ai_pool()
            .begin()
            .await
            .map_err(|err| write_error("cannot begin ai control plane import", err))?;
        Self::upsert_control_plane(&mut tx, data).await?;
        tx.commit()
            .await
            .map_err(|err| write_error("cannot commit ai control plane import", err))
    }

    /// Replaces the complete control plane in one transaction: existing rows
    /// are deleted in reverse dependency order, then the new snapshot is
    /// inserted in dependency order. Any failure rolls the replacement back.
    pub async fn replace_control_plane(&self, data: &AiControlPlaneData) -> Result<()> {
        let mut tx = self
            .ai_pool()
            .begin()
            .await
            .map_err(|err| write_error("cannot begin ai control plane replacement", err))?;

        let deletions = [
            (
                "ai_policy_attachments",
                "cannot delete ai policy attachments",
            ),
            ("ai_policies", "cannot delete ai policies"),
            ("ai_model_targets", "cannot delete ai model targets"),
            ("ai_models", "cannot delete ai models"),
            ("ai_provider_accounts", "cannot delete ai provider accounts"),
            ("ai_providers", "cannot delete ai providers"),
        ];
        for (table, context) in deletions {
            sqlx::query(&format!("DELETE FROM {table}"))
                .execute(&mut *tx)
                .await
                .map_err(|err| write_error(context, err))?;
        }

        Self::upsert_control_plane(&mut tx, data).await?;
        tx.commit()
            .await
            .map_err(|err| write_error("cannot commit ai control plane replacement", err))
    }

    async fn upsert_control_plane(
        conn: &mut SqliteConnection,
        data: &AiControlPlaneData,
    ) -> Result<()> {
        for provider in &data.providers {
            Self::upsert_provider_in(conn, provider).await?;
        }
        for account in &data.accounts {
            Self::upsert_account_in(conn, account).await?;
        }
        for model in &data.models {
            Self::upsert_model_in(conn, model).await?;
        }
        for target in &data.targets {
            Self::upsert_target_in(conn, target).await?;
        }
        for policy in &data.policies {
            Self::upsert_policy_in(conn, policy).await?;
        }
        for attachment in &data.attachments {
            Self::upsert_attachment_in(conn, attachment).await?;
        }
        Ok(())
    }

    /// Lists revisions newest first.
    pub async fn list_revisions(&self) -> Result<Vec<AiConfigRevisionRecord>> {
        let rows = sqlx::query(&format!(
            "SELECT {REVISION_COLUMNS} FROM ai_config_revisions \
             ORDER BY created_at DESC, id ASC"
        ))
        .fetch_all(self.ai_pool())
        .await
        .map_err(|err| write_error("cannot list ai config revisions", err))?;
        Ok(rows.iter().map(revision_from_row).collect())
    }

    pub async fn get_revision(&self, id: &str) -> Result<Option<AiConfigRevisionRecord>> {
        let row = self
            .query_row_by_id(
                "cannot load ai config revision",
                &format!("SELECT {REVISION_COLUMNS} FROM ai_config_revisions WHERE id = ?1"),
                id,
            )
            .await?;
        Ok(row.as_ref().map(revision_from_row))
    }

    pub async fn insert_revision(&self, revision: &AiConfigRevisionRecord) -> Result<()> {
        sqlx::query(
            "INSERT INTO ai_config_revisions
                 (id, created_at, actor, status, diff_json, snapshot_json, applied_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        )
        .bind(&revision.id)
        .bind(revision.created_at)
        .bind(&revision.actor)
        .bind(&revision.status)
        .bind(&revision.diff_json)
        .bind(&revision.snapshot_json)
        .bind(revision.applied_at)
        .execute(self.ai_pool())
        .await
        .map_err(|err| write_error("cannot insert ai config revision", err))?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use octra_ai_config::{AiBalancer, AiModelKind, AiPolicyScope, AiPolicyType};
    use serde_json::json;

    fn provider() -> AiProvider {
        AiProvider {
            id: "provider-openai".to_string(),
            name: "OpenAI".to_string(),
            protocol: "openai".to_string(),
            base_url: "https://api.example.com".to_string(),
            auth_ref: Some("secret:openai".to_string()),
            capabilities: vec!["chat".to_string()],
            config: json!({"region": "us"}),
            enabled: true,
            created_at: 100,
            updated_at: 100,
        }
    }

    fn account() -> AiProviderAccount {
        AiProviderAccount {
            id: "account-primary".to_string(),
            provider_id: "provider-openai".to_string(),
            name: "primary".to_string(),
            credential_kind: "static".to_string(),
            secret_ref: Some("secret:primary".to_string()),
            oauth_ref: None,
            weight: 1,
            priority: 0,
            quota_limit: Some(1_000),
            quota_unit: "requests".to_string(),
            quota_period: "day".to_string(),
            quota_used: 0,
            cooldown_until: None,
            enabled: true,
            oauth_status: "none".to_string(),
            oauth_expires_at: None,
            config: json!({}),
            created_at: 100,
            updated_at: 100,
        }
    }

    fn group_model() -> AiModel {
        AiModel {
            id: "model-group".to_string(),
            name: "chat-group".to_string(),
            kind: AiModelKind::Group,
            capabilities: vec!["chat".to_string()],
            formats: vec!["openai".to_string()],
            route_paths: Vec::new(),
            balancer: AiBalancer::default(),
            enabled: true,
            created_at: 100,
            updated_at: 100,
        }
    }

    fn route_model() -> AiModel {
        AiModel {
            id: "model-route".to_string(),
            name: "chat-route".to_string(),
            kind: AiModelKind::Model,
            capabilities: vec!["chat".to_string()],
            formats: vec!["openai".to_string()],
            route_paths: vec!["/v1/chat/completions".to_string()],
            balancer: AiBalancer::default(),
            enabled: true,
            created_at: 100,
            updated_at: 100,
        }
    }

    fn group_target() -> AiModelTarget {
        AiModelTarget {
            id: "target-provider".to_string(),
            model_id: "model-group".to_string(),
            target_model_id: None,
            provider_id: Some("provider-openai".to_string()),
            upstream_model: Some("gpt-4o".to_string()),
            account_id: Some("account-primary".to_string()),
            weight: 1,
            priority: 0,
            position: 0,
            config: json!({}),
            context_window: Some(128_000),
            max_output_tokens: Some(16_384),
            capabilities: json!({"vision": true, "tools": true, "reasoning": true}),
            pricing: json!({"input_per_million": 3.0, "output_per_million": 15.0}),
            semantic_description: None,
            enabled: true,
            created_at: 100,
            updated_at: 100,
        }
    }

    fn route_target() -> AiModelTarget {
        AiModelTarget {
            id: "target-group".to_string(),
            model_id: "model-route".to_string(),
            target_model_id: Some("model-group".to_string()),
            provider_id: None,
            upstream_model: None,
            account_id: None,
            weight: 1,
            priority: 0,
            position: 0,
            config: json!({}),
            context_window: None,
            max_output_tokens: None,
            capabilities: json!({}),
            pricing: json!({}),
            semantic_description: None,
            enabled: true,
            created_at: 100,
            updated_at: 100,
        }
    }

    fn policy() -> AiPolicy {
        AiPolicy {
            id: "policy-allow".to_string(),
            name: "allow-chat".to_string(),
            policy_type: AiPolicyType::Allowlist,
            config: json!({"models": ["chat-route"]}),
            enabled: true,
            created_at: 100,
            updated_at: 100,
        }
    }

    fn attachment() -> AiPolicyAttachment {
        AiPolicyAttachment {
            id: "attachment-route".to_string(),
            policy_id: "policy-allow".to_string(),
            scope: AiPolicyScope::Model,
            scope_id: Some("model-route".to_string()),
            order: 0,
            enabled: true,
            created_at: 100,
            updated_at: 100,
        }
    }

    #[tokio::test]
    async fn ai_control_plane_round_trips_and_compiles() {
        let storage = crate::Storage::connect_in_memory().await.unwrap();
        let store = &storage.ai;

        let provider = provider();
        let account = account();
        let group_model = group_model();
        let route_model = route_model();
        let group_target = group_target();
        let route_target = route_target();
        let policy = policy();
        let attachment = attachment();

        store.upsert_provider(&provider).await.unwrap();
        store.upsert_account(&account).await.unwrap();
        store.upsert_model(&group_model).await.unwrap();
        store.upsert_model(&route_model).await.unwrap();
        store.upsert_target(&group_target).await.unwrap();
        store.upsert_target(&route_target).await.unwrap();
        store.upsert_policy(&policy).await.unwrap();
        store.upsert_attachment(&attachment).await.unwrap();

        let data = store.load_control_plane().await.unwrap();
        assert_eq!(data.providers, vec![provider.clone()]);
        assert_eq!(data.accounts, vec![account.clone()]);
        assert_eq!(data.models.len(), 2);
        assert!(data.models.iter().any(|model| model == &group_model));
        assert!(data.models.iter().any(|model| model == &route_model));
        assert_eq!(data.targets.len(), 2);
        assert!(data.targets.iter().any(|target| target == &group_target));
        assert!(data.targets.iter().any(|target| target == &route_target));
        assert_eq!(data.policies, vec![policy.clone()]);
        assert_eq!(data.attachments, vec![attachment]);

        let compiled = octra_ai_config::compile(&data).expect("control plane should compile");
        let compiled_route = compiled
            .models
            .values()
            .find(|model| model.model.id == route_model.id)
            .expect("route model should be compiled");
        assert_eq!(compiled_route.targets.len(), 1);
        let compiled_target = &compiled_route.targets[0];
        assert_eq!(
            compiled_target.nested_model_id.as_deref(),
            Some(group_model.id.as_str())
        );
        assert!(compiled_route.policy_ids.contains(&policy.id));
        assert!(compiled_target.policy_ids.contains(&policy.id));

        let compiled_group = compiled
            .models
            .values()
            .find(|model| model.model.id == group_model.id)
            .expect("group model should be compiled");
        assert_eq!(compiled_group.targets.len(), 1);
        assert_eq!(
            compiled_group.targets[0].provider,
            Some(octra_ai_config::CompiledProviderRef {
                provider_id: provider.id.clone(),
                upstream_model: "gpt-4o".to_string(),
                account_id: Some(account.id.clone()),
            })
        );
    }

    #[tokio::test]
    async fn ai_account_oauth_fields_round_trip_without_upsert_overwrite() {
        let storage = crate::Storage::connect_in_memory().await.unwrap();
        let store = &storage.ai;

        store.upsert_provider(&provider()).await.unwrap();
        let mut account = account();
        account.credential_kind = "oauth".to_string();
        account.oauth_status = "authorized".to_string();
        account.oauth_expires_at = Some("2030-01-01T00:00:00.000Z".to_string());
        store.upsert_account(&account).await.unwrap();

        let loaded = store.get_account(&account.id).await.unwrap().unwrap();
        assert_eq!(loaded.oauth_status, "authorized");
        assert_eq!(
            loaded.oauth_expires_at.as_deref(),
            Some("2030-01-01T00:00:00.000Z")
        );
        assert_eq!(
            store.list_accounts(None).await.unwrap(),
            vec![loaded.clone()]
        );

        // A regular account update must not clobber OAuth state that the OAuth
        // methods manage.
        let mut update = loaded;
        update.name = "renamed".to_string();
        update.oauth_status = "none".to_string();
        update.oauth_expires_at = None;
        store.upsert_account(&update).await.unwrap();

        let reloaded = store.get_account(&account.id).await.unwrap().unwrap();
        assert_eq!(reloaded.name, "renamed");
        assert_eq!(reloaded.oauth_status, "authorized");
        assert_eq!(
            reloaded.oauth_expires_at.as_deref(),
            Some("2030-01-01T00:00:00.000Z")
        );
        assert_eq!(
            store.account_oauth_status(&account.id).await.unwrap(),
            Some((
                "authorized".to_string(),
                Some("2030-01-01T00:00:00.000Z".to_string())
            ))
        );
    }

    #[tokio::test]
    async fn ai_config_revisions_round_trip() {
        let storage = crate::Storage::connect_in_memory().await.unwrap();
        let store = &storage.ai;

        let older = AiConfigRevisionRecord {
            id: "revision-1".to_string(),
            created_at: 100,
            actor: "admin".to_string(),
            status: "applied".to_string(),
            diff_json: "{}".to_string(),
            snapshot_json: "{\"providers\":[]}".to_string(),
            applied_at: Some(101),
        };
        let newer = AiConfigRevisionRecord {
            id: "revision-2".to_string(),
            created_at: 200,
            actor: "admin".to_string(),
            status: "pending".to_string(),
            diff_json: "{\"models\":1}".to_string(),
            snapshot_json: "{\"models\":[{\"id\":\"model-route\"}]}".to_string(),
            applied_at: None,
        };

        store.insert_revision(&older).await.unwrap();
        store.insert_revision(&newer).await.unwrap();

        let revisions = store.list_revisions().await.unwrap();
        assert_eq!(revisions, vec![newer.clone(), older.clone()]);
        assert_eq!(revisions[0].snapshot_json, newer.snapshot_json);
        assert_eq!(revisions[1].snapshot_json, older.snapshot_json);

        assert_eq!(
            store.get_revision(&newer.id).await.unwrap(),
            Some(newer.clone())
        );
        assert_eq!(
            store.get_revision(&older.id).await.unwrap(),
            Some(older.clone())
        );
        assert_eq!(store.get_revision("revision-missing").await.unwrap(), None);
    }

    #[tokio::test]
    async fn ai_providers_and_models_list_get_delete_round_trip() {
        let storage = crate::Storage::connect_in_memory().await.unwrap();
        let store = &storage.ai;

        let provider = provider();
        let model = group_model();
        store.upsert_provider(&provider).await.unwrap();
        store.upsert_model(&model).await.unwrap();

        assert_eq!(
            store.list_providers().await.unwrap(),
            vec![provider.clone()]
        );
        assert_eq!(
            store.get_provider(&provider.id).await.unwrap(),
            Some(provider.clone())
        );
        assert_eq!(store.list_models().await.unwrap(), vec![model.clone()]);
        assert_eq!(
            store.get_model(&model.id).await.unwrap(),
            Some(model.clone())
        );

        assert!(store.delete_provider(&provider.id).await.unwrap());
        assert!(!store.delete_provider(&provider.id).await.unwrap());
        assert_eq!(store.get_provider(&provider.id).await.unwrap(), None);
        assert!(store.list_providers().await.unwrap().is_empty());

        assert!(store.delete_model(&model.id).await.unwrap());
        assert!(!store.delete_model(&model.id).await.unwrap());
        assert_eq!(store.get_model(&model.id).await.unwrap(), None);
        assert!(store.list_models().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn ai_child_entities_filter_and_delete() {
        let storage = crate::Storage::connect_in_memory().await.unwrap();
        let store = &storage.ai;

        let provider = provider();
        let account = account();
        let group_model = group_model();
        let route_model = route_model();
        let group_target = group_target();
        let route_target = route_target();
        let policy = policy();
        let attachment = attachment();

        store.upsert_provider(&provider).await.unwrap();
        store.upsert_account(&account).await.unwrap();
        store.upsert_model(&group_model).await.unwrap();
        store.upsert_model(&route_model).await.unwrap();
        store.upsert_target(&group_target).await.unwrap();
        store.upsert_target(&route_target).await.unwrap();
        store.upsert_policy(&policy).await.unwrap();
        store.upsert_attachment(&attachment).await.unwrap();

        assert_eq!(
            store.list_accounts(None).await.unwrap(),
            vec![account.clone()]
        );
        assert_eq!(
            store.list_accounts(Some(&provider.id)).await.unwrap(),
            vec![account.clone()]
        );
        assert!(
            store
                .list_accounts(Some("provider-missing"))
                .await
                .unwrap()
                .is_empty()
        );
        assert_eq!(
            store.get_account(&account.id).await.unwrap(),
            Some(account.clone())
        );

        assert_eq!(
            store.list_targets(None).await.unwrap(),
            vec![group_target.clone(), route_target.clone()]
        );
        assert_eq!(
            store.list_targets(Some(&group_model.id)).await.unwrap(),
            vec![group_target.clone()]
        );
        assert_eq!(
            store.get_target(&route_target.id).await.unwrap(),
            Some(route_target.clone())
        );

        assert_eq!(store.list_policies().await.unwrap(), vec![policy.clone()]);
        assert_eq!(
            store.get_policy(&policy.id).await.unwrap(),
            Some(policy.clone())
        );

        assert_eq!(
            store.list_attachments(None).await.unwrap(),
            vec![attachment.clone()]
        );
        assert_eq!(
            store.list_attachments(Some(&policy.id)).await.unwrap(),
            vec![attachment.clone()]
        );
        assert_eq!(
            store.get_attachment(&attachment.id).await.unwrap(),
            Some(attachment.clone())
        );

        assert!(store.delete_attachment(&attachment.id).await.unwrap());
        assert!(!store.delete_attachment(&attachment.id).await.unwrap());
        assert!(store.delete_target(&group_target.id).await.unwrap());
        assert!(!store.delete_target(&group_target.id).await.unwrap());
        assert!(store.delete_account(&account.id).await.unwrap());
        assert!(!store.delete_account(&account.id).await.unwrap());
        assert!(store.delete_policy(&policy.id).await.unwrap());
        assert!(!store.delete_policy(&policy.id).await.unwrap());

        assert!(store.list_attachments(None).await.unwrap().is_empty());
        assert_eq!(
            store.list_targets(Some(&group_model.id)).await.unwrap(),
            Vec::new()
        );
        assert!(
            store
                .list_accounts(Some(&provider.id))
                .await
                .unwrap()
                .is_empty()
        );
        assert!(store.list_policies().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn account_quota_methods_cover_units_cooldown_and_rollover() {
        let storage = crate::Storage::connect_in_memory().await.unwrap();
        let store = &storage.ai;

        store.upsert_provider(&provider()).await.unwrap();

        for (id, unit, expected) in [
            ("account-requests", "requests", 2),
            ("account-tokens", "tokens", 100),
            ("account-cost", "cost_micros", 42),
        ] {
            let mut account = account();
            account.id = id.to_string();
            account.name = id.to_string();
            account.quota_unit = unit.to_string();
            store.upsert_account(&account).await.unwrap();

            store.consume_account_quota(id, 2, 100, 42).await.unwrap();
            let loaded = store.get_account(id).await.unwrap().unwrap();
            assert_eq!(loaded.quota_used, expected, "unit {unit}");
        }

        store
            .set_account_cooldown(
                "account-requests",
                "2030-01-01T00:00:00.000Z",
                Some("rate limited"),
            )
            .await
            .unwrap();
        let loaded = store
            .get_account("account-requests")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            loaded.cooldown_until.as_deref(),
            Some("2030-01-01T00:00:00.000Z")
        );

        store
            .roll_account_quota("account-tokens", "2030-02-01T00:00:00.000Z")
            .await
            .unwrap();
        let rolled = store.get_account("account-tokens").await.unwrap().unwrap();
        assert_eq!(rolled.quota_used, 0, "rollover resets the counter");

        assert!(store.reset_account_quota("account-cost").await.unwrap());
        let reset = store.get_account("account-cost").await.unwrap().unwrap();
        assert_eq!(reset.quota_used, 0);
        assert!(reset.cooldown_until.is_none());
        assert!(!store.reset_account_quota("account-missing").await.unwrap());
    }

    #[tokio::test]
    async fn target_model_metadata_round_trips() {
        let storage = crate::Storage::connect_in_memory().await.unwrap();
        let store = &storage.ai;

        let provider = provider();
        let account = account();
        let model = group_model();
        let target = group_target();
        store.upsert_provider(&provider).await.unwrap();
        store.upsert_account(&account).await.unwrap();
        store.upsert_model(&model).await.unwrap();
        store.upsert_target(&target).await.unwrap();

        let loaded = store
            .get_target(&target.id)
            .await
            .unwrap()
            .expect("target should exist");
        assert_eq!(loaded.context_window, Some(128_000));
        assert_eq!(loaded.max_output_tokens, Some(16_384));
        assert_eq!(
            loaded.capabilities,
            json!({"vision": true, "tools": true, "reasoning": true})
        );
        assert_eq!(
            loaded.pricing,
            json!({"input_per_million": 3.0, "output_per_million": 15.0})
        );
        assert_eq!(loaded, target);

        let mut updated = target;
        updated.context_window = None;
        updated.max_output_tokens = None;
        updated.capabilities = json!({});
        updated.pricing = json!({});
        store.upsert_target(&updated).await.unwrap();

        let loaded = store
            .get_target(&updated.id)
            .await
            .unwrap()
            .expect("target should still exist");
        assert_eq!(loaded.context_window, None);
        assert_eq!(loaded.max_output_tokens, None);
        assert_eq!(loaded.capabilities, json!({}));
        assert_eq!(loaded.pricing, json!({}));
    }

    fn control_plane() -> AiControlPlaneData {
        AiControlPlaneData {
            providers: vec![provider()],
            accounts: vec![account()],
            models: vec![group_model(), route_model()],
            targets: vec![group_target(), route_target()],
            policies: vec![policy()],
            attachments: vec![attachment()],
        }
    }

    #[tokio::test]
    async fn import_control_plane_persists_complete_control_plane() {
        let storage = crate::Storage::connect_in_memory().await.unwrap();
        let store = &storage.ai;

        let mut legacy = provider();
        legacy.id = "provider-legacy".to_string();
        legacy.name = "Legacy Provider".to_string();
        store.upsert_provider(&legacy).await.unwrap();

        let data = control_plane();
        store.import_control_plane(&data).await.unwrap();
        assert_eq!(
            store.get_provider(&legacy.id).await.unwrap(),
            Some(legacy.clone())
        );
        assert_eq!(
            store.get_provider(&provider().id).await.unwrap(),
            Some(provider())
        );
        assert_eq!(store.list_accounts(None).await.unwrap(), data.accounts);
        assert_eq!(store.list_models().await.unwrap(), data.models);
        assert_eq!(store.list_targets(None).await.unwrap(), data.targets);
        assert_eq!(store.list_policies().await.unwrap(), data.policies);
        assert_eq!(
            store.list_attachments(None).await.unwrap(),
            data.attachments
        );

        // Import upserts instead of replacing, so repeating it is idempotent and
        // leaves pre-existing rows in place.
        let imported = store.load_control_plane().await.unwrap();
        assert_eq!(imported.providers.len(), 2);
        store.import_control_plane(&data).await.unwrap();
        assert_eq!(store.load_control_plane().await.unwrap(), imported);
    }

    #[tokio::test]
    async fn replace_control_plane_swaps_the_complete_control_plane() {
        let storage = crate::Storage::connect_in_memory().await.unwrap();
        let store = &storage.ai;

        let original = control_plane();
        store.import_control_plane(&original).await.unwrap();
        assert_eq!(store.load_control_plane().await.unwrap(), original);

        let mut replacement_provider = provider();
        replacement_provider.id = "provider-replacement".to_string();
        replacement_provider.name = "Replacement".to_string();
        let replacement = AiControlPlaneData {
            providers: vec![replacement_provider.clone()],
            accounts: Vec::new(),
            models: Vec::new(),
            targets: Vec::new(),
            policies: Vec::new(),
            attachments: Vec::new(),
        };

        store.replace_control_plane(&replacement).await.unwrap();
        assert_eq!(store.load_control_plane().await.unwrap(), replacement);
        assert_eq!(store.get_provider(&provider().id).await.unwrap(), None);
        assert!(store.list_accounts(None).await.unwrap().is_empty());
        assert!(store.list_models().await.unwrap().is_empty());
        assert!(store.list_targets(None).await.unwrap().is_empty());
        assert!(store.list_policies().await.unwrap().is_empty());
        assert!(store.list_attachments(None).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn replace_control_plane_rolls_back_on_failure() {
        let storage = crate::Storage::connect_in_memory().await.unwrap();
        let store = &storage.ai;

        let original = control_plane();
        store.import_control_plane(&original).await.unwrap();

        let mut replacement_provider = provider();
        replacement_provider.id = "provider-replacement".to_string();
        replacement_provider.name = "Replacement".to_string();
        let mut replacement_model = route_model();
        replacement_model.id = "model-replacement".to_string();
        replacement_model.name = "replacement-route".to_string();
        let broken_target = AiModelTarget {
            id: "target-broken".to_string(),
            model_id: "model-replacement".to_string(),
            target_model_id: None,
            provider_id: Some("provider-missing".to_string()),
            upstream_model: Some("gpt-5".to_string()),
            account_id: None,
            weight: 1,
            priority: 0,
            position: 0,
            config: json!({}),
            context_window: None,
            max_output_tokens: None,
            capabilities: json!({}),
            pricing: json!({}),
            semantic_description: None,
            enabled: true,
            created_at: 100,
            updated_at: 100,
        };
        let replacement = AiControlPlaneData {
            providers: vec![replacement_provider],
            accounts: Vec::new(),
            models: vec![replacement_model],
            targets: vec![broken_target],
            policies: Vec::new(),
            attachments: Vec::new(),
        };

        assert!(store.replace_control_plane(&replacement).await.is_err());

        // The failed replacement must leave the previous control plane intact.
        assert_eq!(store.load_control_plane().await.unwrap(), original);
    }
}
