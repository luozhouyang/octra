//! AI-native control-plane router.
//!
//! Exposes CRUD for the `ai_*` entities, compiler validation and preview, and
//! the apply/revision workflow. Records are stored flat; the compiler resolves
//! them into the data-plane view.
//!
//! The router carries no authentication. The daemon mounts it under the same
//! bearer and audit layers as the rest of the control plane.

use std::sync::Arc;

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use chrono::Utc;
use octra_ai_config::{
    AiBalancer, AiControlPlaneData, AiModel, AiModelKind, AiModelTarget, AiPolicy,
    AiPolicyAttachment, AiPolicyScope, AiPolicyType, AiProvider, AiProviderAccount,
};
use octra_storage::{AiConfigRevisionRecord, Storage};
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::sync::RwLock;
use uuid::Uuid;

/// Shared state for the AI control-plane router.
#[derive(Clone)]
pub struct AiControlState {
    storage: Arc<Storage>,
    last_compile: Arc<RwLock<Option<AiCompileReport>>>,
    runtime: Option<Arc<crate::runtime::LlmRuntime>>,
    plugin_runtime: Option<Arc<octra_llm_plugin_runtime::LlmPluginRuntime>>,
}

impl AiControlState {
    pub fn new(storage: Arc<Storage>) -> Self {
        Self {
            storage,
            last_compile: Arc::new(RwLock::new(None)),
            runtime: None,
            plugin_runtime: None,
        }
    }

    /// Attaches the live LLM runtime so the control state can reload it after
    /// an activation endpoint changes the control plane.
    pub fn with_runtime(mut self, runtime: Arc<crate::runtime::LlmRuntime>) -> Self {
        self.runtime = Some(runtime);
        self
    }

    /// Attaches the LLM plugin runtime so the control state can recompile
    /// plugin chains with the AI policies as synthetic plugin configurations.
    pub fn with_plugin_runtime(
        mut self,
        runtime: Arc<octra_llm_plugin_runtime::LlmPluginRuntime>,
    ) -> Self {
        self.plugin_runtime = Some(runtime);
        self
    }

    /// Reloads the live LLM runtime and plugin chains after an activation
    /// point.
    ///
    /// Runtime reloads are skipped when the corresponding runtime is not
    /// attached.
    pub async fn refresh(&self) -> octra_core::Result<()> {
        if let Some(runtime) = &self.runtime {
            runtime.reload().await?;
        }
        if let Some(plugin_runtime) = &self.plugin_runtime {
            let data = self.storage.ai.load_control_plane().await?;
            let extra =
                crate::ai_policy_plugins::plugin_data_from_ai(&data, plugin_runtime.registry());
            plugin_runtime.reload_with_extra(extra).await?;
        }
        Ok(())
    }

    async fn reload_runtime(&self) -> AiResult<()> {
        self.refresh().await.map_err(AiError::internal)
    }

    /// Loads and compiles the current control plane and records the report in
    /// `last_compile`.
    pub async fn compile_current(&self) -> octra_core::Result<AiCompileReport> {
        let data = self.storage.ai.load_control_plane().await?;
        let report = compile_report(&data);
        *self.last_compile.write().await = Some(report.clone());
        Ok(report)
    }

    async fn last_compile(&self) -> Option<AiCompileReport> {
        self.last_compile.read().await.clone()
    }
}

// ---------------------------------------------------------------------------
// Views

#[derive(Debug, Clone, serde::Serialize)]
pub struct AiCompileErrorView {
    pub code: String,
    pub message: String,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct AiEntityCounts {
    pub providers: usize,
    pub accounts: usize,
    pub models: usize,
    pub targets: usize,
    pub policies: usize,
    pub attachments: usize,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct AiCompileReport {
    pub ok: bool,
    pub errors: Vec<AiCompileErrorView>,
    pub counts: AiEntityCounts,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct AiTargetPreview {
    pub id: String,
    pub provider_id: Option<String>,
    pub upstream_model: Option<String>,
    pub nested_model_id: Option<String>,
    pub account_id: Option<String>,
    pub policy_ids: Vec<String>,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct AiModelPreview {
    pub id: String,
    pub name: String,
    pub kind: String,
    pub targets: Vec<AiTargetPreview>,
    pub policy_ids: Vec<String>,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct AiPreview {
    pub report: AiCompileReport,
    pub models: Vec<AiModelPreview>,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct AiStatus {
    pub counts: AiEntityCounts,
    pub last_compile: Option<AiCompileReport>,
    pub revisions: usize,
}

/// A persisted revision with its `diff_json` decoded for the client.
///
/// The full configuration snapshot is not returned in listings; clients only
/// learn whether one is available for rollback.
#[derive(Debug, Clone, serde::Serialize)]
pub struct AiRevisionView {
    pub id: String,
    pub created_at: i64,
    pub actor: String,
    pub status: String,
    pub diff: Value,
    pub applied_at: Option<i64>,
    pub has_snapshot: bool,
}

impl From<AiConfigRevisionRecord> for AiRevisionView {
    fn from(record: AiConfigRevisionRecord) -> Self {
        Self {
            id: record.id,
            created_at: record.created_at,
            actor: record.actor,
            status: record.status,
            diff: serde_json::from_str(&record.diff_json).unwrap_or(Value::Null),
            applied_at: record.applied_at,
            has_snapshot: snapshot_present(&record.snapshot_json),
        }
    }
}

/// A snapshot is usable when it is not empty and not the default `{}`.
fn snapshot_present(snapshot_json: &str) -> bool {
    let snapshot_json = snapshot_json.trim();
    !snapshot_json.is_empty() && snapshot_json != "{}"
}

// ---------------------------------------------------------------------------
// Router

pub fn router(state: AiControlState) -> Router {
    Router::new()
        .route("/api/v1/ai/status", get(get_status))
        .route(
            "/api/v1/ai/providers",
            get(list_providers).post(create_provider),
        )
        .route(
            "/api/v1/ai/providers/{id}",
            get(get_provider)
                .put(update_provider)
                .delete(delete_provider),
        )
        .route(
            "/api/v1/ai/providers/{provider_id}/accounts",
            get(list_accounts).post(create_account),
        )
        .route(
            "/api/v1/ai/accounts/{id}",
            get(get_account).put(update_account).delete(delete_account),
        )
        .route("/api/v1/ai/models", get(list_models).post(create_model))
        .route(
            "/api/v1/ai/models/{id}",
            get(get_model).put(update_model).delete(delete_model),
        )
        .route(
            "/api/v1/ai/models/{model_id}/targets",
            get(list_targets).post(create_target),
        )
        .route(
            "/api/v1/ai/targets/{id}",
            get(get_target).put(update_target).delete(delete_target),
        )
        .route(
            "/api/v1/ai/policies",
            get(list_policies).post(create_policy),
        )
        .route(
            "/api/v1/ai/policies/{id}",
            get(get_policy).put(update_policy).delete(delete_policy),
        )
        .route(
            "/api/v1/ai/policies/{policy_id}/attachments",
            get(list_attachments).post(create_attachment),
        )
        .route(
            "/api/v1/ai/attachments/{id}",
            get(get_attachment)
                .put(update_attachment)
                .delete(delete_attachment),
        )
        .route("/api/v1/ai/validate", post(validate))
        .route("/api/v1/ai/preview", post(preview))
        .route("/api/v1/ai/apply", post(apply))
        .route("/api/v1/ai/revisions", get(list_revisions))
        .route(
            "/api/v1/ai/revisions/{id}/rollback",
            post(rollback_revision),
        )
        .with_state(state)
}

// ---------------------------------------------------------------------------
// Errors

#[derive(Debug)]
struct AiError {
    status: StatusCode,
    message: String,
}

impl AiError {
    fn new(status: StatusCode, message: impl Into<String>) -> Self {
        Self {
            status,
            message: message.into(),
        }
    }

    fn not_found(kind: &str, id: &str) -> Self {
        Self::new(
            StatusCode::NOT_FOUND,
            format!("{kind} '{id}' does not exist"),
        )
    }

    fn internal(error: impl std::fmt::Display) -> Self {
        tracing::error!(error = %error, "AI control-plane failure");
        Self::new(StatusCode::INTERNAL_SERVER_ERROR, "internal error")
    }
}

impl IntoResponse for AiError {
    fn into_response(self) -> Response {
        (self.status, Json(json!({ "error": self.message }))).into_response()
    }
}

type AiResult<T> = Result<T, AiError>;

// ---------------------------------------------------------------------------
// Input DTOs

#[derive(Deserialize)]
struct ProviderInput {
    id: Option<String>,
    name: String,
    protocol: String,
    base_url: Option<String>,
    auth_ref: Option<String>,
    capabilities: Option<Vec<String>>,
    config: Option<Value>,
    enabled: Option<bool>,
}

#[derive(Deserialize)]
struct AccountInput {
    id: Option<String>,
    name: String,
    credential_kind: Option<String>,
    secret_ref: Option<String>,
    oauth_ref: Option<String>,
    weight: Option<i64>,
    priority: Option<i64>,
    quota_limit: Option<i64>,
    quota_unit: Option<String>,
    quota_period: Option<String>,
    quota_used: Option<i64>,
    cooldown_until: Option<String>,
    config: Option<Value>,
    enabled: Option<bool>,
}

#[derive(Deserialize)]
struct ModelInput {
    id: Option<String>,
    name: String,
    kind: Option<AiModelKind>,
    capabilities: Option<Vec<String>>,
    formats: Option<Vec<String>>,
    route_paths: Option<Vec<String>>,
    balancer: Option<AiBalancer>,
    enabled: Option<bool>,
}

#[derive(Deserialize)]
struct TargetInput {
    id: Option<String>,
    target_model_id: Option<String>,
    provider_id: Option<String>,
    upstream_model: Option<String>,
    account_id: Option<String>,
    weight: Option<i64>,
    priority: Option<i64>,
    position: Option<i64>,
    config: Option<Value>,
    context_window: Option<i64>,
    max_output_tokens: Option<i32>,
    capabilities: Option<Value>,
    pricing: Option<Value>,
    semantic_description: Option<String>,
    enabled: Option<bool>,
}

#[derive(Deserialize)]
struct PolicyInput {
    id: Option<String>,
    name: String,
    policy_type: AiPolicyType,
    config: Option<Value>,
    enabled: Option<bool>,
}

#[derive(Deserialize)]
struct AttachmentInput {
    id: Option<String>,
    scope: AiPolicyScope,
    scope_id: Option<String>,
    order: Option<i32>,
    enabled: Option<bool>,
}

impl ProviderInput {
    fn into_record(self, id: String, created_at: i64, updated_at: i64) -> AiProvider {
        AiProvider {
            id,
            name: self.name,
            protocol: self.protocol,
            base_url: self.base_url.unwrap_or_default(),
            auth_ref: self.auth_ref,
            capabilities: self.capabilities.unwrap_or_default(),
            config: self.config.unwrap_or_else(empty_object),
            enabled: self.enabled.unwrap_or(true),
            created_at,
            updated_at,
        }
    }
}

impl AccountInput {
    fn into_record(
        self,
        id: String,
        provider_id: String,
        created_at: i64,
        updated_at: i64,
    ) -> AiProviderAccount {
        AiProviderAccount {
            id,
            provider_id,
            name: self.name,
            credential_kind: self.credential_kind.unwrap_or_else(|| "static".to_string()),
            secret_ref: self.secret_ref,
            oauth_ref: self.oauth_ref,
            weight: self.weight.unwrap_or(1),
            priority: self.priority.unwrap_or(0),
            quota_limit: self.quota_limit,
            quota_unit: self.quota_unit.unwrap_or_else(|| "requests".to_string()),
            quota_period: self.quota_period.unwrap_or_else(|| "none".to_string()),
            quota_used: self.quota_used.unwrap_or(0),
            cooldown_until: self.cooldown_until,
            enabled: self.enabled.unwrap_or(true),
            // OAuth state is server-managed; account CRUD must not set it.
            oauth_status: "none".to_string(),
            oauth_expires_at: None,
            config: self.config.unwrap_or_else(empty_object),
            created_at,
            updated_at,
        }
    }
}

impl ModelInput {
    fn into_record(self, id: String, created_at: i64, updated_at: i64) -> AiModel {
        AiModel {
            id,
            name: self.name,
            kind: self.kind.unwrap_or(AiModelKind::Model),
            capabilities: self
                .capabilities
                .unwrap_or_else(|| vec!["chat".to_string()]),
            formats: self.formats.unwrap_or_else(|| vec!["openai".to_string()]),
            route_paths: self.route_paths.unwrap_or_default(),
            balancer: self.balancer.unwrap_or_default(),
            enabled: self.enabled.unwrap_or(true),
            created_at,
            updated_at,
        }
    }
}

impl TargetInput {
    fn into_record(
        self,
        id: String,
        model_id: String,
        created_at: i64,
        updated_at: i64,
    ) -> AiModelTarget {
        AiModelTarget {
            id,
            model_id,
            target_model_id: self.target_model_id,
            provider_id: self.provider_id,
            upstream_model: self.upstream_model,
            account_id: self.account_id,
            weight: self.weight.unwrap_or(1),
            priority: self.priority.unwrap_or(0),
            position: self.position.unwrap_or(0),
            config: self.config.unwrap_or_else(empty_object),
            context_window: self.context_window,
            max_output_tokens: self.max_output_tokens,
            capabilities: self.capabilities.unwrap_or_else(empty_object),
            pricing: self.pricing.unwrap_or_else(empty_object),
            semantic_description: self.semantic_description,
            enabled: self.enabled.unwrap_or(true),
            created_at,
            updated_at,
        }
    }
}

impl PolicyInput {
    fn into_record(self, id: String, created_at: i64, updated_at: i64) -> AiPolicy {
        AiPolicy {
            id,
            name: self.name,
            policy_type: self.policy_type,
            config: self.config.unwrap_or_else(empty_object),
            enabled: self.enabled.unwrap_or(true),
            created_at,
            updated_at,
        }
    }
}

impl AttachmentInput {
    fn into_record(
        self,
        id: String,
        policy_id: String,
        created_at: i64,
        updated_at: i64,
    ) -> AiPolicyAttachment {
        AiPolicyAttachment {
            id,
            policy_id,
            scope: self.scope,
            scope_id: self.scope_id,
            order: self.order.unwrap_or(0),
            enabled: self.enabled.unwrap_or(true),
            created_at,
            updated_at,
        }
    }
}

fn empty_object() -> Value {
    Value::Object(serde_json::Map::new())
}

fn new_id() -> String {
    Uuid::new_v4().simple().to_string()
}

fn now() -> i64 {
    Utc::now().timestamp()
}

// ---------------------------------------------------------------------------
// Status and compiler endpoints

async fn get_status(State(state): State<AiControlState>) -> AiResult<Json<AiStatus>> {
    let data = state
        .storage
        .ai
        .load_control_plane()
        .await
        .map_err(AiError::internal)?;
    let revisions = state
        .storage
        .ai
        .list_revisions()
        .await
        .map_err(AiError::internal)?;
    Ok(Json(AiStatus {
        counts: entity_counts(&data),
        last_compile: state.last_compile().await,
        revisions: revisions.len(),
    }))
}

async fn validate(State(state): State<AiControlState>) -> AiResult<Json<AiCompileReport>> {
    let report = state.compile_current().await.map_err(AiError::internal)?;
    Ok(Json(report))
}

async fn preview(State(state): State<AiControlState>) -> AiResult<Json<AiPreview>> {
    let data = state
        .storage
        .ai
        .load_control_plane()
        .await
        .map_err(AiError::internal)?;
    let report = compile_report(&data);
    *state.last_compile.write().await = Some(report.clone());
    // Compilation may fail on a partially built control plane. The preview
    // still expands the raw records so operators can see what is stored.
    let models = match octra_ai_config::compile(&data) {
        Ok(compiled) => compiled
            .models
            .values()
            .map(|model| AiModelPreview {
                id: model.model.id.clone(),
                name: model.model.name.clone(),
                kind: model_kind_label(&model.model.kind).to_string(),
                targets: model
                    .targets
                    .iter()
                    .map(|target| AiTargetPreview {
                        id: target.target.id.clone(),
                        provider_id: target
                            .provider
                            .as_ref()
                            .map(|provider| provider.provider_id.clone()),
                        upstream_model: target
                            .provider
                            .as_ref()
                            .map(|provider| provider.upstream_model.clone()),
                        nested_model_id: target.nested_model_id.clone(),
                        account_id: target
                            .provider
                            .as_ref()
                            .and_then(|provider| provider.account_id.clone()),
                        policy_ids: target.policy_ids.clone(),
                    })
                    .collect(),
                policy_ids: model.policy_ids.clone(),
            })
            .collect(),
        Err(_) => raw_preview(&data),
    };
    Ok(Json(AiPreview { report, models }))
}

async fn apply(State(state): State<AiControlState>) -> AiResult<Response> {
    // Load once: the same control plane is compiled and captured in the
    // revision snapshot so the revision can be restored verbatim.
    let data = state
        .storage
        .ai
        .load_control_plane()
        .await
        .map_err(AiError::internal)?;
    let report = compile_report(&data);
    *state.last_compile.write().await = Some(report.clone());
    if !report.ok {
        return Ok((
            StatusCode::UNPROCESSABLE_ENTITY,
            Json(json!({
                "error": "control plane does not compile",
                "report": report,
            })),
        )
            .into_response());
    }

    let snapshot_json = serde_json::to_string(&data)
        .map_err(|err| AiError::internal(format!("cannot encode revision snapshot: {err}")))?;
    let timestamp = now();
    let revision = AiConfigRevisionRecord {
        id: format!("rev_{}", Uuid::new_v4().simple()),
        created_at: timestamp,
        actor: "api".to_string(),
        status: "applied".to_string(),
        diff_json: serde_json::to_string(&report.counts)
            .map_err(|err| AiError::internal(format!("cannot encode revision diff: {err}")))?,
        snapshot_json,
        applied_at: Some(timestamp),
    };
    state
        .storage
        .ai
        .insert_revision(&revision)
        .await
        .map_err(AiError::internal)?;

    state.reload_runtime().await?;

    Ok(Json(json!({
        "ok": true,
        "revision_id": revision.id,
        "report": report,
    }))
    .into_response())
}

/// Restores the control plane captured when a revision was applied.
///
/// The revision itself remains the audit record; a rollback does not create a
/// new revision.
async fn rollback_revision(
    State(state): State<AiControlState>,
    Path(id): Path<String>,
) -> AiResult<Response> {
    let revision = state
        .storage
        .ai
        .get_revision(&id)
        .await
        .map_err(AiError::internal)?
        .ok_or_else(|| AiError::not_found("revision", &id))?;

    if !snapshot_present(&revision.snapshot_json) {
        return Ok((
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": "revision has no snapshot" })),
        )
            .into_response());
    }

    let data: AiControlPlaneData =
        serde_json::from_str(&revision.snapshot_json).map_err(|err| {
            AiError::new(
                StatusCode::BAD_REQUEST,
                format!("revision snapshot is invalid: {err}"),
            )
        })?;

    let report = compile_report(&data);
    *state.last_compile.write().await = Some(report.clone());
    if !report.ok {
        return Ok((
            StatusCode::UNPROCESSABLE_ENTITY,
            Json(json!({
                "error": "restored control plane does not compile",
                "report": report,
            })),
        )
            .into_response());
    }

    state
        .storage
        .ai
        .replace_control_plane(&data)
        .await
        .map_err(AiError::internal)?;
    state.reload_runtime().await?;

    Ok(Json(json!({
        "ok": true,
        "revision_id": revision.id,
        "report": report,
    }))
    .into_response())
}

async fn list_revisions(
    State(state): State<AiControlState>,
) -> AiResult<Json<Vec<AiRevisionView>>> {
    let revisions = state
        .storage
        .ai
        .list_revisions()
        .await
        .map_err(AiError::internal)?;
    Ok(Json(
        revisions.into_iter().map(AiRevisionView::from).collect(),
    ))
}

// ---------------------------------------------------------------------------
// Providers

async fn list_providers(State(state): State<AiControlState>) -> AiResult<Json<Vec<AiProvider>>> {
    let providers = state
        .storage
        .ai
        .list_providers()
        .await
        .map_err(AiError::internal)?;
    Ok(Json(providers))
}

async fn create_provider(
    State(state): State<AiControlState>,
    Json(input): Json<ProviderInput>,
) -> AiResult<(StatusCode, Json<AiProvider>)> {
    let timestamp = now();
    let id = input.id.clone().unwrap_or_else(new_id);
    let provider = input.into_record(id, timestamp, timestamp);
    state
        .storage
        .ai
        .upsert_provider(&provider)
        .await
        .map_err(AiError::internal)?;
    Ok((StatusCode::CREATED, Json(provider)))
}

async fn get_provider(
    State(state): State<AiControlState>,
    Path(id): Path<String>,
) -> AiResult<Json<AiProvider>> {
    let provider = state
        .storage
        .ai
        .get_provider(&id)
        .await
        .map_err(AiError::internal)?
        .ok_or_else(|| AiError::not_found("provider", &id))?;
    Ok(Json(provider))
}

async fn update_provider(
    State(state): State<AiControlState>,
    Path(id): Path<String>,
    Json(input): Json<ProviderInput>,
) -> AiResult<Json<AiProvider>> {
    let created_at = state
        .storage
        .ai
        .get_provider(&id)
        .await
        .map_err(AiError::internal)?
        .map_or_else(now, |provider| provider.created_at);
    let provider = input.into_record(id, created_at, now());
    state
        .storage
        .ai
        .upsert_provider(&provider)
        .await
        .map_err(AiError::internal)?;
    Ok(Json(provider))
}

async fn delete_provider(
    State(state): State<AiControlState>,
    Path(id): Path<String>,
) -> AiResult<StatusCode> {
    let deleted = state
        .storage
        .ai
        .delete_provider(&id)
        .await
        .map_err(AiError::internal)?;
    if deleted {
        Ok(StatusCode::NO_CONTENT)
    } else {
        Err(AiError::not_found("provider", &id))
    }
}

// ---------------------------------------------------------------------------
// Accounts

async fn list_accounts(
    State(state): State<AiControlState>,
    Path(provider_id): Path<String>,
) -> AiResult<Json<Vec<AiProviderAccount>>> {
    let accounts = state
        .storage
        .ai
        .list_accounts(Some(&provider_id))
        .await
        .map_err(AiError::internal)?;
    Ok(Json(accounts))
}

async fn create_account(
    State(state): State<AiControlState>,
    Path(provider_id): Path<String>,
    Json(input): Json<AccountInput>,
) -> AiResult<(StatusCode, Json<AiProviderAccount>)> {
    if state
        .storage
        .ai
        .get_provider(&provider_id)
        .await
        .map_err(AiError::internal)?
        .is_none()
    {
        return Err(AiError::not_found("provider", &provider_id));
    }
    let timestamp = now();
    let id = input.id.clone().unwrap_or_else(new_id);
    let account = input.into_record(id, provider_id, timestamp, timestamp);
    state
        .storage
        .ai
        .upsert_account(&account)
        .await
        .map_err(AiError::internal)?;
    Ok((StatusCode::CREATED, Json(account)))
}

async fn get_account(
    State(state): State<AiControlState>,
    Path(id): Path<String>,
) -> AiResult<Json<AiProviderAccount>> {
    let account = state
        .storage
        .ai
        .get_account(&id)
        .await
        .map_err(AiError::internal)?
        .ok_or_else(|| AiError::not_found("account", &id))?;
    Ok(Json(account))
}

async fn update_account(
    State(state): State<AiControlState>,
    Path(id): Path<String>,
    Json(input): Json<AccountInput>,
) -> AiResult<Json<AiProviderAccount>> {
    let existing = state
        .storage
        .ai
        .get_account(&id)
        .await
        .map_err(AiError::internal)?
        .ok_or_else(|| AiError::not_found("account", &id))?;
    let account = input.into_record(id, existing.provider_id, existing.created_at, now());
    state
        .storage
        .ai
        .upsert_account(&account)
        .await
        .map_err(AiError::internal)?;
    Ok(Json(account))
}

async fn delete_account(
    State(state): State<AiControlState>,
    Path(id): Path<String>,
) -> AiResult<StatusCode> {
    let deleted = state
        .storage
        .ai
        .delete_account(&id)
        .await
        .map_err(AiError::internal)?;
    if deleted {
        Ok(StatusCode::NO_CONTENT)
    } else {
        Err(AiError::not_found("account", &id))
    }
}

// ---------------------------------------------------------------------------
// Models

async fn list_models(State(state): State<AiControlState>) -> AiResult<Json<Vec<AiModel>>> {
    let models = state
        .storage
        .ai
        .list_models()
        .await
        .map_err(AiError::internal)?;
    Ok(Json(models))
}

async fn create_model(
    State(state): State<AiControlState>,
    Json(input): Json<ModelInput>,
) -> AiResult<(StatusCode, Json<AiModel>)> {
    let timestamp = now();
    let id = input.id.clone().unwrap_or_else(new_id);
    let model = input.into_record(id, timestamp, timestamp);
    state
        .storage
        .ai
        .upsert_model(&model)
        .await
        .map_err(AiError::internal)?;
    Ok((StatusCode::CREATED, Json(model)))
}

async fn get_model(
    State(state): State<AiControlState>,
    Path(id): Path<String>,
) -> AiResult<Json<AiModel>> {
    let model = state
        .storage
        .ai
        .get_model(&id)
        .await
        .map_err(AiError::internal)?
        .ok_or_else(|| AiError::not_found("model", &id))?;
    Ok(Json(model))
}

async fn update_model(
    State(state): State<AiControlState>,
    Path(id): Path<String>,
    Json(input): Json<ModelInput>,
) -> AiResult<Json<AiModel>> {
    let created_at = state
        .storage
        .ai
        .get_model(&id)
        .await
        .map_err(AiError::internal)?
        .map_or_else(now, |model| model.created_at);
    let model = input.into_record(id, created_at, now());
    state
        .storage
        .ai
        .upsert_model(&model)
        .await
        .map_err(AiError::internal)?;
    Ok(Json(model))
}

async fn delete_model(
    State(state): State<AiControlState>,
    Path(id): Path<String>,
) -> AiResult<StatusCode> {
    let deleted = state
        .storage
        .ai
        .delete_model(&id)
        .await
        .map_err(AiError::internal)?;
    if deleted {
        Ok(StatusCode::NO_CONTENT)
    } else {
        Err(AiError::not_found("model", &id))
    }
}

// ---------------------------------------------------------------------------
// Targets

async fn list_targets(
    State(state): State<AiControlState>,
    Path(model_id): Path<String>,
) -> AiResult<Json<Vec<AiModelTarget>>> {
    let targets = state
        .storage
        .ai
        .list_targets(Some(&model_id))
        .await
        .map_err(AiError::internal)?;
    Ok(Json(targets))
}

async fn create_target(
    State(state): State<AiControlState>,
    Path(model_id): Path<String>,
    Json(input): Json<TargetInput>,
) -> AiResult<(StatusCode, Json<AiModelTarget>)> {
    if state
        .storage
        .ai
        .get_model(&model_id)
        .await
        .map_err(AiError::internal)?
        .is_none()
    {
        return Err(AiError::not_found("model", &model_id));
    }
    let timestamp = now();
    let id = input.id.clone().unwrap_or_else(new_id);
    let target = input.into_record(id, model_id, timestamp, timestamp);
    state
        .storage
        .ai
        .upsert_target(&target)
        .await
        .map_err(AiError::internal)?;
    Ok((StatusCode::CREATED, Json(target)))
}

async fn get_target(
    State(state): State<AiControlState>,
    Path(id): Path<String>,
) -> AiResult<Json<AiModelTarget>> {
    let target = state
        .storage
        .ai
        .get_target(&id)
        .await
        .map_err(AiError::internal)?
        .ok_or_else(|| AiError::not_found("target", &id))?;
    Ok(Json(target))
}

async fn update_target(
    State(state): State<AiControlState>,
    Path(id): Path<String>,
    Json(input): Json<TargetInput>,
) -> AiResult<Json<AiModelTarget>> {
    let existing = state
        .storage
        .ai
        .get_target(&id)
        .await
        .map_err(AiError::internal)?
        .ok_or_else(|| AiError::not_found("target", &id))?;
    let target = input.into_record(id, existing.model_id, existing.created_at, now());
    state
        .storage
        .ai
        .upsert_target(&target)
        .await
        .map_err(AiError::internal)?;
    Ok(Json(target))
}

async fn delete_target(
    State(state): State<AiControlState>,
    Path(id): Path<String>,
) -> AiResult<StatusCode> {
    let deleted = state
        .storage
        .ai
        .delete_target(&id)
        .await
        .map_err(AiError::internal)?;
    if deleted {
        Ok(StatusCode::NO_CONTENT)
    } else {
        Err(AiError::not_found("target", &id))
    }
}

// ---------------------------------------------------------------------------
// Policies

async fn list_policies(State(state): State<AiControlState>) -> AiResult<Json<Vec<AiPolicy>>> {
    let policies = state
        .storage
        .ai
        .list_policies()
        .await
        .map_err(AiError::internal)?;
    Ok(Json(policies))
}

async fn create_policy(
    State(state): State<AiControlState>,
    Json(input): Json<PolicyInput>,
) -> AiResult<(StatusCode, Json<AiPolicy>)> {
    let timestamp = now();
    let id = input.id.clone().unwrap_or_else(new_id);
    let policy = input.into_record(id, timestamp, timestamp);
    state
        .storage
        .ai
        .upsert_policy(&policy)
        .await
        .map_err(AiError::internal)?;
    Ok((StatusCode::CREATED, Json(policy)))
}

async fn get_policy(
    State(state): State<AiControlState>,
    Path(id): Path<String>,
) -> AiResult<Json<AiPolicy>> {
    let policy = state
        .storage
        .ai
        .get_policy(&id)
        .await
        .map_err(AiError::internal)?
        .ok_or_else(|| AiError::not_found("policy", &id))?;
    Ok(Json(policy))
}

async fn update_policy(
    State(state): State<AiControlState>,
    Path(id): Path<String>,
    Json(input): Json<PolicyInput>,
) -> AiResult<Json<AiPolicy>> {
    let created_at = state
        .storage
        .ai
        .get_policy(&id)
        .await
        .map_err(AiError::internal)?
        .map_or_else(now, |policy| policy.created_at);
    let policy = input.into_record(id, created_at, now());
    state
        .storage
        .ai
        .upsert_policy(&policy)
        .await
        .map_err(AiError::internal)?;
    Ok(Json(policy))
}

async fn delete_policy(
    State(state): State<AiControlState>,
    Path(id): Path<String>,
) -> AiResult<StatusCode> {
    let deleted = state
        .storage
        .ai
        .delete_policy(&id)
        .await
        .map_err(AiError::internal)?;
    if deleted {
        Ok(StatusCode::NO_CONTENT)
    } else {
        Err(AiError::not_found("policy", &id))
    }
}

// ---------------------------------------------------------------------------
// Attachments

async fn list_attachments(
    State(state): State<AiControlState>,
    Path(policy_id): Path<String>,
) -> AiResult<Json<Vec<AiPolicyAttachment>>> {
    let attachments = state
        .storage
        .ai
        .list_attachments(Some(&policy_id))
        .await
        .map_err(AiError::internal)?;
    Ok(Json(attachments))
}

async fn create_attachment(
    State(state): State<AiControlState>,
    Path(policy_id): Path<String>,
    Json(input): Json<AttachmentInput>,
) -> AiResult<(StatusCode, Json<AiPolicyAttachment>)> {
    if state
        .storage
        .ai
        .get_policy(&policy_id)
        .await
        .map_err(AiError::internal)?
        .is_none()
    {
        return Err(AiError::not_found("policy", &policy_id));
    }
    let timestamp = now();
    let id = input.id.clone().unwrap_or_else(new_id);
    let attachment = input.into_record(id, policy_id, timestamp, timestamp);
    state
        .storage
        .ai
        .upsert_attachment(&attachment)
        .await
        .map_err(AiError::internal)?;
    Ok((StatusCode::CREATED, Json(attachment)))
}

async fn get_attachment(
    State(state): State<AiControlState>,
    Path(id): Path<String>,
) -> AiResult<Json<AiPolicyAttachment>> {
    let attachment = state
        .storage
        .ai
        .get_attachment(&id)
        .await
        .map_err(AiError::internal)?
        .ok_or_else(|| AiError::not_found("attachment", &id))?;
    Ok(Json(attachment))
}

async fn update_attachment(
    State(state): State<AiControlState>,
    Path(id): Path<String>,
    Json(input): Json<AttachmentInput>,
) -> AiResult<Json<AiPolicyAttachment>> {
    let existing = state
        .storage
        .ai
        .get_attachment(&id)
        .await
        .map_err(AiError::internal)?
        .ok_or_else(|| AiError::not_found("attachment", &id))?;
    let attachment = input.into_record(id, existing.policy_id, existing.created_at, now());
    state
        .storage
        .ai
        .upsert_attachment(&attachment)
        .await
        .map_err(AiError::internal)?;
    Ok(Json(attachment))
}

async fn delete_attachment(
    State(state): State<AiControlState>,
    Path(id): Path<String>,
) -> AiResult<StatusCode> {
    let deleted = state
        .storage
        .ai
        .delete_attachment(&id)
        .await
        .map_err(AiError::internal)?;
    if deleted {
        Ok(StatusCode::NO_CONTENT)
    } else {
        Err(AiError::not_found("attachment", &id))
    }
}

// ---------------------------------------------------------------------------
// Compiler helpers

fn entity_counts(data: &AiControlPlaneData) -> AiEntityCounts {
    AiEntityCounts {
        providers: data.providers.len(),
        accounts: data.accounts.len(),
        models: data.models.len(),
        targets: data.targets.len(),
        policies: data.policies.len(),
        attachments: data.attachments.len(),
    }
}

fn compile_report(data: &AiControlPlaneData) -> AiCompileReport {
    match octra_ai_config::compile(data) {
        Ok(_) => AiCompileReport {
            ok: true,
            errors: Vec::new(),
            counts: entity_counts(data),
        },
        Err(errors) => AiCompileReport {
            ok: false,
            errors: errors
                .into_iter()
                .map(|error| AiCompileErrorView {
                    code: error.code,
                    message: error.message,
                })
                .collect(),
            counts: entity_counts(data),
        },
    }
}

fn model_kind_label(kind: &AiModelKind) -> &'static str {
    match kind {
        AiModelKind::Model => "model",
        AiModelKind::Group => "group",
    }
}

/// Builds the preview from raw records when compilation fails.
fn raw_preview(data: &AiControlPlaneData) -> Vec<AiModelPreview> {
    data.models
        .iter()
        .map(|model| AiModelPreview {
            id: model.id.clone(),
            name: model.name.clone(),
            kind: model_kind_label(&model.kind).to_string(),
            targets: data
                .targets
                .iter()
                .filter(|target| target.model_id == model.id)
                .map(|target| AiTargetPreview {
                    id: target.id.clone(),
                    provider_id: target.provider_id.clone(),
                    upstream_model: target.upstream_model.clone(),
                    nested_model_id: target.target_model_id.clone(),
                    account_id: target.account_id.clone(),
                    policy_ids: policy_ids_for(
                        data,
                        &[
                            (AiPolicyScope::Global, None),
                            (AiPolicyScope::Model, Some(model.id.as_str())),
                            (AiPolicyScope::Provider, target.provider_id.as_deref()),
                            (AiPolicyScope::Target, Some(target.id.as_str())),
                        ],
                    ),
                })
                .collect(),
            policy_ids: policy_ids_for(
                data,
                &[
                    (AiPolicyScope::Global, None),
                    (AiPolicyScope::Model, Some(model.id.as_str())),
                ],
            ),
        })
        .collect()
}

fn policy_ids_for(
    data: &AiControlPlaneData,
    scopes: &[(AiPolicyScope, Option<&str>)],
) -> Vec<String> {
    let mut matched: Vec<(u8, i32, &str, &str)> = Vec::new();

    for (scope, scope_id) in scopes {
        for attachment in data
            .attachments
            .iter()
            .filter(|attachment| attachment.enabled && &attachment.scope == scope)
        {
            if !scope_matches(attachment, *scope_id) {
                continue;
            }
            let Some(policy) = data
                .policies
                .iter()
                .find(|policy| policy.enabled && policy.id == attachment.policy_id)
            else {
                continue;
            };
            matched.push((
                scope_rank(scope),
                attachment.order,
                attachment.id.as_str(),
                policy.id.as_str(),
            ));
        }
    }

    matched.sort_by(|left, right| {
        left.0
            .cmp(&right.0)
            .then_with(|| left.1.cmp(&right.1))
            .then_with(|| left.2.cmp(right.2))
    });

    let mut seen = std::collections::HashSet::new();
    let mut policy_ids = Vec::new();
    for (_, _, _, policy_id) in matched {
        if seen.insert(policy_id) {
            policy_ids.push(policy_id.to_string());
        }
    }
    policy_ids
}

fn scope_matches(attachment: &AiPolicyAttachment, requested: Option<&str>) -> bool {
    match attachment.scope {
        AiPolicyScope::Global => attachment
            .scope_id
            .as_deref()
            .is_none_or(|scope_id| scope_id.trim().is_empty()),
        _ => match requested {
            Some(scope_id) if !scope_id.trim().is_empty() => {
                attachment.scope_id.as_deref() == Some(scope_id)
            }
            _ => false,
        },
    }
}

fn scope_rank(scope: &AiPolicyScope) -> u8 {
    match scope {
        AiPolicyScope::VirtualKey => 0,
        AiPolicyScope::Consumer => 1,
        AiPolicyScope::ConsumerGroup => 2,
        AiPolicyScope::Target => 3,
        AiPolicyScope::Model => 4,
        AiPolicyScope::Provider => 5,
        AiPolicyScope::Global => 6,
    }
}
