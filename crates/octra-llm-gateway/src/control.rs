//! LLM control plane: CRUD for providers, models, routing groups, targets,
//! routes and virtual keys, plus reload and usage reporting.
//!
//! Mutations validate input (400), resolve references (404) and surface
//! uniqueness violations as 409, then reload the runtime synchronously so the
//! next data plane request sees the change.

use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::Response;
use axum::routing::{get, post, put};
use axum::{Router, extract};
use chrono::{DateTime, Utc};
use octra_llm_core::Protocol;
use octra_llm_plugin_types::LlmScope;
use octra_storage::plugins::{PluginConfigRecord, PluginRecord};
use octra_storage::{
    LlmModelInput, LlmModelRecord, LlmPiiEngineInput, LlmPiiEngineRecord, LlmProviderAccountInput,
    LlmProviderAccountRecord, LlmProviderInput, LlmProviderRecord, LlmRouteRecord,
    LlmRoutingGroupRecord, LlmRoutingTargetInput, LlmRoutingTargetRecord, LlmStorageError,
    LlmUsageRecord, LlmVirtualKeyInput, LlmVirtualKeyRecord, LlmVirtualKeyRotation,
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::HashMap;

use crate::config::ProviderConfig;
use crate::keys;
use crate::pii::EngineStatus;
use crate::runtime::{LlmControlState, LlmRuntime};

pub fn router(state: LlmControlState) -> Router {
    let providers = Router::new()
        .route(
            "/admin/llm/providers",
            get(list_providers).post(create_provider),
        )
        .route(
            "/admin/llm/providers/{id}",
            get(get_provider)
                .put(update_provider)
                .delete(delete_provider),
        )
        .route("/admin/llm/providers/{id}/cli/probe", post(cli_probe))
        .route(
            "/admin/llm/providers/{id}/accounts",
            get(list_provider_accounts).post(create_account),
        )
        .route(
            "/admin/llm/accounts/{id}",
            get(get_account).put(update_account).delete(delete_account),
        )
        .route(
            "/admin/llm/accounts/{id}/quota/reset",
            post(reset_account_quota),
        )
        .route(
            "/admin/llm/providers/{id}/accounts/{account}/oauth/start",
            post(oauth_start),
        )
        .route(
            "/admin/llm/providers/{id}/accounts/{account}/oauth/complete",
            post(oauth_complete),
        )
        .route(
            "/admin/llm/providers/{id}/accounts/{account}/oauth/refresh",
            post(oauth_refresh),
        )
        .route(
            "/admin/llm/providers/{id}/accounts/{account}/oauth/status",
            get(oauth_status),
        )
        .route(
            "/admin/llm/providers/{id}/accounts/{account}/oauth",
            axum::routing::delete(oauth_delete),
        );
    let models = Router::new()
        .route("/admin/llm/models", get(list_models).post(create_model))
        .route(
            "/admin/llm/models/{id}",
            get(get_model).put(update_model).delete(delete_model),
        );
    let groups = Router::new()
        .route(
            "/admin/llm/routing-groups",
            get(list_groups).post(create_group),
        )
        .route(
            "/admin/llm/routing-groups/{id}",
            get(get_group).put(update_group).delete(delete_group),
        )
        .route(
            "/admin/llm/routing-groups/{id}/targets",
            get(list_targets).post(create_target),
        )
        .route(
            "/admin/llm/routing-groups/{id}/targets/{target_id}",
            put(update_target).delete(delete_target),
        );
    let routes = Router::new()
        .route("/admin/llm/routes", get(list_routes).post(create_route))
        .route(
            "/admin/llm/routes/{id}",
            get(get_route).put(update_route).delete(delete_route),
        );
    let keys = Router::new()
        .route(
            "/admin/llm/virtual-keys",
            get(list_virtual_keys).post(create_virtual_key),
        )
        .route(
            "/admin/llm/virtual-keys/{id}",
            get(get_virtual_key)
                .put(update_virtual_key)
                .delete(delete_virtual_key),
        )
        .route(
            "/admin/llm/virtual-keys/{id}/rotate",
            post(rotate_virtual_key),
        );
    let plugins = Router::new()
        .route("/admin/llm/plugins", get(list_plugins))
        .route("/admin/llm/plugins/reload", post(reload_plugins))
        .route("/admin/llm/plugins/metrics", get(plugin_metrics))
        .route("/admin/llm/plugins/effective", get(plugin_effective_chain))
        .route(
            "/admin/llm/plugins/{id}",
            get(get_plugin).put(update_plugin),
        )
        .route(
            "/admin/llm/plugins/{id}/configs",
            get(list_plugin_configs).post(create_plugin_config),
        )
        .route(
            "/admin/llm/plugin-configs/{id}",
            put(update_plugin_config).delete(delete_plugin_config),
        );
    let pii = Router::new()
        .route(
            "/admin/llm/pii-engines",
            get(list_pii_engines).post(create_pii_engine),
        )
        .route(
            "/admin/llm/pii-engines/{id}",
            get(get_pii_engine)
                .put(update_pii_engine)
                .delete(delete_pii_engine),
        )
        .route("/admin/llm/pii-engines/{id}/status", get(pii_engine_status))
        .route(
            "/admin/llm/pii-engines/{id}/reload",
            post(reload_pii_engine),
        )
        .route("/admin/llm/pii-engines/{id}/start", post(start_pii_engine))
        .route("/admin/llm/pii-engines/{id}/stop", post(stop_pii_engine))
        .route(
            "/admin/llm/pii-engines/{id}/restart",
            post(restart_pii_engine),
        )
        .route("/admin/llm/pii-models", get(list_pii_models))
        .route(
            "/admin/llm/pii-models/{id}",
            get(get_pii_model).post(pii_model_action),
        );
    Router::new()
        .merge(providers)
        .merge(models)
        .merge(groups)
        .merge(routes)
        .merge(keys)
        .merge(plugins)
        .merge(pii)
        .route("/admin/llm/reload", post(reload))
        .route("/admin/llm/usage", get(usage))
        .with_state(state)
}

// ---------------------------------------------------------------------------
// Errors

#[derive(Debug)]
pub struct ApiError {
    status: StatusCode,
    message: String,
    field: Option<String>,
}

impl ApiError {
    pub fn new(status: StatusCode, message: impl Into<String>) -> Self {
        Self {
            status,
            message: message.into(),
            field: None,
        }
    }

    pub fn bad_request(message: impl Into<String>) -> Self {
        Self::new(StatusCode::BAD_REQUEST, message)
    }

    pub fn field(mut self, field: impl Into<String>) -> Self {
        self.field = Some(field.into());
        self
    }

    pub fn not_found(kind: &str, id: &str) -> Self {
        Self::new(StatusCode::NOT_FOUND, format!("{kind} {id} does not exist"))
    }

    pub fn conflict(message: impl Into<String>) -> Self {
        Self::new(StatusCode::CONFLICT, message)
    }

    pub fn internal(error: impl std::fmt::Display) -> Self {
        tracing::error!(error = %error, "llm control plane failure");
        Self::new(StatusCode::INTERNAL_SERVER_ERROR, "internal error")
    }

    pub fn from_storage(error: LlmStorageError) -> Self {
        match error {
            LlmStorageError::Conflict(message) => {
                Self::conflict(format!("conflicting record: {message}"))
            }
            LlmStorageError::Missing(message) => Self::new(StatusCode::NOT_FOUND, message),
            LlmStorageError::Backend(message) => {
                tracing::error!(%message, "llm storage failure");
                Self::new(StatusCode::INTERNAL_SERVER_ERROR, "storage error")
            }
        }
    }
}

impl ApiError {
    fn into_parts(self) -> (StatusCode, Value) {
        let mut body = json!({ "error": self.message });
        if let Some(field) = self.field {
            body["field"] = json!(field);
        }
        (self.status, body)
    }
}

impl axum::response::IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let (status, body) = self.into_parts();
        axum::response::IntoResponse::into_response((status, axum::Json(body)))
    }
}

type ApiResult<T> = Result<T, ApiError>;

fn non_empty(value: Option<String>, _field: &str) -> ApiResult<Option<String>> {
    match value {
        None => Ok(None),
        Some(value) if value.trim().is_empty() => Ok(None),
        Some(value) => Ok(Some(value.trim().to_string())),
    }
}

fn required(value: &Option<String>, field: &str) -> ApiResult<String> {
    match value.as_deref().map(str::trim) {
        Some(value) if !value.is_empty() => Ok(value.to_string()),
        _ => Err(ApiError::bad_request(format!("`{field}` is required")).field(field)),
    }
}

fn json_object(raw: Option<String>, field: &str) -> ApiResult<String> {
    let raw = match raw {
        Some(raw) if !raw.trim().is_empty() => raw,
        _ => return Ok("{}".to_string()),
    };
    let value: Value = serde_json::from_str(&raw).map_err(|error| {
        ApiError::bad_request(format!("`{field}` must be valid JSON: {error}")).field(field)
    })?;
    if !value.is_object() {
        return Err(ApiError::bad_request(format!("`{field}` must be a JSON object")).field(field));
    }
    Ok(value.to_string())
}

// ---------------------------------------------------------------------------
// Wire shapes

#[derive(Debug, Deserialize)]
pub struct ProviderBody {
    pub name: Option<String>,
    pub kind: Option<String>,
    pub protocol: Option<String>,
    pub auth_type: Option<String>,
    pub base_url: Option<String>,
    pub secret_ref: Option<String>,
    pub priority: Option<i64>,
    /// Accepts an object (preferred) or a JSON string.
    pub config: Option<Value>,
    pub config_json: Option<String>,
    pub enabled: Option<bool>,
}

impl ProviderBody {
    fn into_input(self, options: ProviderOptions) -> ApiResult<LlmProviderInput> {
        let name = required(&self.name, "name")?;
        let kind = self.kind.unwrap_or_else(|| "openai".to_string());
        let protocol = self.protocol.unwrap_or_else(|| "openai".to_string());
        if Protocol::parse(&protocol).is_none() {
            return Err(ApiError::bad_request(format!(
                "unknown protocol '{protocol}': use openai or anthropic"
            ))
            .field("protocol"));
        }
        let auth_type = self.auth_type.unwrap_or_else(|| match protocol.as_str() {
            "anthropic" => "anthropic".to_string(),
            _ => "bearer".to_string(),
        });
        if crate::config::AuthType::parse(&auth_type).is_none() {
            return Err(
                ApiError::bad_request(format!("unknown auth_type '{auth_type}'"))
                    .field("auth_type"),
            );
        }
        let secret_ref = non_empty(self.secret_ref, "secret_ref")?;
        if let Some(reference) = &secret_ref
            && !(reference.starts_with("env:") || reference.starts_with("file:"))
        {
            return Err(ApiError::bad_request(
                "secret_ref must use env:VAR or file:/path; plaintext secrets are not stored",
            )
            .field("secret_ref"));
        }
        let base_url = non_empty(self.base_url, "base_url")?;
        if let Some(url) = &base_url
            && !(url.starts_with("http://") || url.starts_with("https://"))
        {
            return Err(
                ApiError::bad_request("base_url must start with http:// or https://")
                    .field("base_url"),
            );
        }
        let config_json = match (self.config, self.config_json) {
            (Some(value), None) => {
                if !value.is_object() {
                    return Err(
                        ApiError::bad_request("`config` must be a JSON object").field("config")
                    );
                }
                value.to_string()
            }
            (None, value) => json_object(value, "config_json")?,
            (Some(_), Some(_)) => {
                return Err(ApiError::bad_request(
                    "send either `config` or `config_json`, not both",
                ));
            }
        };
        if let Ok(value) = serde_json::from_str::<Value>(&config_json)
            && let Some(cli) = value.get("cli")
        {
            crate::cli::CliConfig::from_value(cli)
                .map_err(|message| ApiError::bad_request(message).field("config.cli"))?;
        }
        Ok(LlmProviderInput {
            name,
            kind,
            protocol,
            auth_type,
            base_url,
            secret_ref,
            priority: self.priority.unwrap_or(0),
            config_json,
            enabled: self.enabled.unwrap_or(options.default_enabled),
        })
    }
}

#[derive(Debug, Clone, Copy)]
pub struct ProviderOptions {
    pub default_enabled: bool,
}

#[derive(Debug, Serialize)]
pub struct ProviderView {
    pub id: String,
    pub name: String,
    pub kind: String,
    pub protocol: String,
    pub auth_type: String,
    pub base_url: Option<String>,
    pub secret_ref: Option<String>,
    pub priority: i64,
    pub config: Value,
    pub enabled: bool,
    pub created_at: String,
    pub updated_at: String,
}

impl From<LlmProviderRecord> for ProviderView {
    fn from(record: LlmProviderRecord) -> Self {
        Self {
            id: record.id,
            name: record.name,
            kind: record.kind,
            protocol: record.protocol,
            auth_type: record.auth_type,
            base_url: record.base_url,
            secret_ref: record.secret_ref,
            priority: record.priority,
            config: serde_json::from_str(&record.config_json).unwrap_or_else(|_| json!({})),
            enabled: record.enabled,
            created_at: record.created_at,
            updated_at: record.updated_at,
        }
    }
}

#[derive(Debug, Deserialize)]
pub struct ModelBody {
    pub provider_id: Option<String>,
    pub model_id: Option<String>,
    pub display_name: Option<String>,
    pub protocol: Option<String>,
    pub context_window: Option<i64>,
    pub max_output_tokens: Option<i64>,
    pub capabilities: Option<Value>,
    #[serde(default)]
    pub capabilities_json: Option<String>,
    pub pricing: Option<Value>,
    pub pricing_json: Option<String>,
    pub enabled: Option<bool>,
}

fn merge_json_field(object: Option<Value>, raw: Option<String>, field: &str) -> ApiResult<String> {
    match (object, raw) {
        (Some(value), None) => {
            if !value.is_object() {
                return Err(
                    ApiError::bad_request(format!("`{field}` must be a JSON object")).field(field),
                );
            }
            Ok(value.to_string())
        }
        (None, value) => json_object(value, field),
        (Some(_), Some(_)) => Err(ApiError::bad_request(format!(
            "send either `{field}` or `{field}_json`, not both"
        ))),
    }
}

impl ModelBody {
    fn into_input(self) -> ApiResult<LlmModelInput> {
        let protocol = non_empty(self.protocol, "protocol")?;
        if let Some(protocol) = &protocol
            && Protocol::parse(protocol).is_none()
        {
            return Err(
                ApiError::bad_request(format!("unknown protocol '{protocol}'")).field("protocol"),
            );
        }
        for (value, field) in [
            (self.context_window, "context_window"),
            (self.max_output_tokens, "max_output_tokens"),
        ] {
            if value.is_some_and(|value| value <= 0) {
                return Err(
                    ApiError::bad_request(format!("`{field}` must be positive")).field(field)
                );
            }
        }
        Ok(LlmModelInput {
            provider_id: required(&self.provider_id, "provider_id")?,
            model_id: required(&self.model_id, "model_id")?,
            display_name: non_empty(self.display_name, "display_name")?,
            protocol,
            context_window: self.context_window,
            max_output_tokens: self.max_output_tokens,
            capabilities: merge_json_field(
                self.capabilities,
                self.capabilities_json,
                "capabilities",
            )?,
            pricing_json: merge_json_field(self.pricing, self.pricing_json, "pricing")?,
            enabled: self.enabled.unwrap_or(true),
        })
    }
}

#[derive(Debug, Serialize)]
pub struct ModelView {
    pub id: String,
    pub provider_id: String,
    pub model_id: String,
    pub display_name: Option<String>,
    pub protocol: Option<String>,
    pub context_window: Option<i64>,
    pub max_output_tokens: Option<i64>,
    pub capabilities: Value,
    pub pricing: Value,
    pub enabled: bool,
    pub created_at: String,
}

impl From<LlmModelRecord> for ModelView {
    fn from(record: LlmModelRecord) -> Self {
        Self {
            id: record.id,
            provider_id: record.provider_id,
            model_id: record.model_id,
            display_name: record.display_name,
            protocol: record.protocol,
            context_window: record.context_window,
            max_output_tokens: record.max_output_tokens,
            capabilities: serde_json::from_str(&record.capabilities).unwrap_or_else(|_| json!({})),
            pricing: serde_json::from_str(&record.pricing_json).unwrap_or_else(|_| json!({})),
            enabled: record.enabled,
            created_at: record.created_at,
        }
    }
}

#[derive(Debug, Deserialize)]
pub struct GroupBody {
    pub name: Option<String>,
    pub strategy: Option<String>,
}

impl GroupBody {
    fn parts(&self) -> ApiResult<(String, String)> {
        let strategy = self
            .strategy
            .clone()
            .unwrap_or_else(|| "priority".to_string());
        if !matches!(
            strategy.as_str(),
            "priority"
                | "weighted"
                | "order"
                | "rotate"
                | "round_robin"
                | "usage"
                | "quota"
                | "smart"
                | "pace"
                | "intent"
        ) {
            return Err(ApiError::bad_request(
                "strategy must be priority, weighted, order, rotate, usage, quota, smart, pace or intent",
            )
            .field("strategy"));
        }
        Ok((required(&self.name, "name")?, strategy))
    }
}

#[derive(Debug, Serialize)]
pub struct GroupView {
    pub id: String,
    pub name: String,
    pub strategy: String,
    pub created_at: String,
    pub targets: Vec<TargetView>,
}

#[derive(Debug, Deserialize)]
pub struct TargetBody {
    pub provider_id: Option<String>,
    pub model_id: Option<String>,
    pub account_id: Option<String>,
    pub weight: Option<i64>,
    pub priority: Option<i64>,
    pub position: Option<i64>,
}

impl TargetBody {
    fn into_input(
        self,
        existing: Option<&LlmRoutingTargetRecord>,
    ) -> ApiResult<LlmRoutingTargetInput> {
        let weight = self
            .weight
            .or(existing.map(|target| target.weight))
            .unwrap_or(1);
        if !(1..=1000).contains(&weight) {
            return Err(
                ApiError::bad_request("`weight` must be between 1 and 1000").field("weight")
            );
        }
        Ok(LlmRoutingTargetInput {
            provider_id: self
                .provider_id
                .or_else(|| existing.map(|target| target.provider_id.clone()))
                .ok_or_else(|| {
                    ApiError::bad_request("`provider_id` is required").field("provider_id")
                })?
                .trim()
                .to_string(),
            model_id: self
                .model_id
                .or_else(|| existing.map(|target| target.model_id.clone()))
                .ok_or_else(|| ApiError::bad_request("`model_id` is required").field("model_id"))?
                .trim()
                .to_string(),
            account_id: self
                .account_id
                .or_else(|| existing.and_then(|target| target.account_id.clone()))
                .map(|value| value.trim().to_string())
                .filter(|value| !value.is_empty()),
            weight,
            priority: self
                .priority
                .or(existing.map(|target| target.priority))
                .unwrap_or(0),
            position: self
                .position
                .or(existing.map(|target| target.position))
                .unwrap_or(0),
        })
    }
}

#[derive(Debug, Serialize)]
pub struct TargetView {
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

impl From<LlmRoutingTargetRecord> for TargetView {
    fn from(record: LlmRoutingTargetRecord) -> Self {
        Self {
            id: record.id,
            group_id: record.group_id,
            provider_id: record.provider_id,
            model_id: record.model_id,
            account_id: record.account_id,
            weight: record.weight,
            priority: record.priority,
            position: record.position,
            created_at: record.created_at,
            updated_at: record.updated_at,
        }
    }
}

#[derive(Debug, Deserialize)]
pub struct RouteBody {
    pub virtual_model_id: Option<String>,
    pub routing_group_id: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct RouteView {
    pub id: String,
    pub virtual_model_id: String,
    pub routing_group_id: Option<String>,
    pub created_at: String,
    pub updated_at: String,
}

impl From<LlmRouteRecord> for RouteView {
    fn from(record: LlmRouteRecord) -> Self {
        Self {
            id: record.id,
            virtual_model_id: record.virtual_model_id,
            routing_group_id: record.routing_group_id,
            created_at: record.created_at,
            updated_at: record.updated_at,
        }
    }
}

#[derive(Debug, Deserialize)]
pub struct VirtualKeyBody {
    pub name: Option<String>,
    pub consumer_id: Option<String>,
    pub enabled: Option<bool>,
    /// RFC 3339 timestamp; an empty string clears the expiry.
    pub expires_at: Option<String>,
    pub permissions: Option<Value>,
    #[serde(default)]
    pub permissions_json: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct RotateVirtualKeyBody {
    pub name: Option<String>,
    /// RFC 3339 timestamp; an empty string clears the expiry.
    pub expires_at: Option<String>,
    pub grace_seconds: Option<u64>,
    pub permissions: Option<Value>,
    #[serde(default)]
    pub permissions_json: Option<String>,
}

impl RotateVirtualKeyBody {
    fn permissions(&self) -> ApiResult<Option<String>> {
        match (&self.permissions, &self.permissions_json) {
            (None, None) => Ok(None),
            (Some(_), Some(_)) => Err(ApiError::bad_request(
                "send either `permissions` or `permissions_json`, not both",
            )),
            (Some(value), None) => {
                if !value.is_object() {
                    return Err(ApiError::bad_request("`permissions` must be a JSON object")
                        .field("permissions"));
                }
                Ok(Some(value.to_string()))
            }
            (None, Some(raw)) => Ok(Some(raw.clone())),
        }
    }
}

fn parse_expiry(value: Option<String>) -> ApiResult<Option<DateTime<Utc>>> {
    match value {
        None => Ok(None),
        Some(value) if value.trim().is_empty() => Ok(None),
        Some(value) => DateTime::parse_from_rfc3339(value.trim())
            .map(|parsed| Some(parsed.with_timezone(&Utc)))
            .map_err(|error| {
                ApiError::bad_request(format!("`expires_at` must be RFC 3339: {error}"))
                    .field("expires_at")
            }),
    }
}

fn validate_permissions(permissions: &str) -> ApiResult<()> {
    crate::vk_policy::VirtualKeyPolicy::parse(permissions)
        .map(|_| ())
        .map_err(|error| ApiError::bad_request(error).field("permissions"))
}

impl VirtualKeyBody {
    fn name(&self) -> ApiResult<String> {
        required(&self.name, "name")
    }

    fn permissions(&self) -> ApiResult<String> {
        merge_json_field(
            self.permissions.clone(),
            self.permissions_json.clone(),
            "permissions",
        )
    }
}

#[derive(Debug, Serialize)]
pub struct VirtualKeyView {
    pub id: String,
    pub name: String,
    pub key_prefix: String,
    pub consumer_id: Option<String>,
    pub enabled: bool,
    pub expires_at: Option<DateTime<Utc>>,
    pub last_used_at: Option<String>,
    pub permissions: Value,
    /// Stable identity shared by every key in one rotation family.
    pub key_family: String,
    /// Key replaced by this one, if it was created by rotation.
    pub rotated_from: Option<String>,
    pub created_at: String,
}

impl From<LlmVirtualKeyRecord> for VirtualKeyView {
    fn from(record: LlmVirtualKeyRecord) -> Self {
        Self {
            id: record.id,
            name: record.name,
            key_prefix: record.key_prefix,
            consumer_id: record.consumer_id,
            enabled: record.enabled,
            expires_at: record.expires_at,
            last_used_at: record.last_used_at,
            permissions: serde_json::from_str(&record.permissions_json)
                .unwrap_or_else(|_| json!({})),
            key_family: record.key_family,
            rotated_from: record.rotated_from,
            created_at: record.created_at,
        }
    }
}

#[derive(Debug, Serialize)]
pub struct CreatedVirtualKey {
    #[serde(flatten)]
    pub key: VirtualKeyView,
    /// Plaintext key, returned exactly once.
    pub key_value: String,
    pub warning: &'static str,
}

#[derive(Debug, Serialize)]
pub struct RotatedVirtualKey {
    #[serde(flatten)]
    pub key: VirtualKeyView,
    /// Plaintext key, returned exactly once.
    pub key_value: String,
    pub warning: &'static str,
    /// Id of the key this one replaces.
    pub replaces: String,
    pub previous_expires_at: Option<String>,
}

// ---------------------------------------------------------------------------
// Providers

async fn list_providers(
    State(state): State<LlmControlState>,
) -> ApiResult<extract::Json<Vec<ProviderView>>> {
    let records = state
        .storage
        .llm
        .list_providers()
        .await
        .map_err(ApiError::from_storage)?;
    Ok(extract::Json(
        records.into_iter().map(ProviderView::from).collect(),
    ))
}

async fn get_provider(
    State(state): State<LlmControlState>,
    Path(id): Path<String>,
) -> ApiResult<extract::Json<ProviderView>> {
    let record = state
        .storage
        .llm
        .get_provider(&id)
        .await
        .map_err(ApiError::from_storage)?
        .ok_or_else(|| ApiError::not_found("provider", &id))?;
    Ok(extract::Json(record.into()))
}

async fn create_provider(
    State(state): State<LlmControlState>,
    extract::Json(body): extract::Json<ProviderBody>,
) -> ApiResult<(StatusCode, extract::Json<ProviderView>)> {
    let input = body.into_input(ProviderOptions {
        default_enabled: true,
    })?;
    let record = state
        .storage
        .llm
        .create_provider(&input)
        .await
        .map_err(ApiError::from_storage)?;
    reload_now(&state).await?;
    Ok((StatusCode::CREATED, extract::Json(record.into())))
}

async fn update_provider(
    State(state): State<LlmControlState>,
    Path(id): Path<String>,
    extract::Json(body): extract::Json<ProviderBody>,
) -> ApiResult<extract::Json<ProviderView>> {
    let existing = state
        .storage
        .llm
        .get_provider(&id)
        .await
        .map_err(ApiError::from_storage)?
        .ok_or_else(|| ApiError::not_found("provider", &id))?;
    let mut input = body.into_input(ProviderOptions {
        default_enabled: existing.enabled,
    })?;
    // PUT is a full replace, but omitted optional fields keep their values so
    // a client that only flips `enabled` does not lose the secret reference.
    if input.secret_ref.is_none() {
        input.secret_ref = existing.secret_ref;
    }
    if input.base_url.is_none() {
        input.base_url = existing.base_url;
    }
    if input.config_json == "{}" && existing.config_json != "{}" {
        input.config_json = existing.config_json;
    }
    let record = state
        .storage
        .llm
        .update_provider(&id, &input)
        .await
        .map_err(ApiError::from_storage)?;
    reload_now(&state).await?;
    Ok(extract::Json(record.into()))
}

async fn delete_provider(
    State(state): State<LlmControlState>,
    Path(id): Path<String>,
) -> ApiResult<StatusCode> {
    if !state
        .storage
        .llm
        .delete_provider(&id)
        .await
        .map_err(ApiError::from_storage)?
    {
        return Err(ApiError::not_found("provider", &id));
    }
    reload_now(&state).await?;
    Ok(StatusCode::NO_CONTENT)
}

/// Runs the configured CLI binary's version command so an operator can verify
/// a machine before sending traffic to it. Always 200: `ok` carries the result.
async fn cli_probe(
    State(state): State<LlmControlState>,
    Path(id): Path<String>,
) -> ApiResult<extract::Json<Value>> {
    let record = state
        .storage
        .llm
        .get_provider(&id)
        .await
        .map_err(ApiError::from_storage)?
        .ok_or_else(|| ApiError::not_found("provider", &id))?;
    let provider = ProviderConfig::from_record(&record)
        .map_err(|error| ApiError::bad_request(format!("{error:?}")))?;
    let Some(cli) = provider.cli else {
        return Err(ApiError::bad_request("provider has no `cli` configuration"));
    };
    Ok(extract::Json(match crate::cli::probe(&cli).await {
        Ok(version) => json!({
            "ok": true,
            "command": cli.command,
            "version": version,
        }),
        Err(error) => json!({
            "ok": false,
            "command": cli.command,
            "error": error,
        }),
    }))
}

// ---------------------------------------------------------------------------
// Provider accounts

#[derive(Debug, Deserialize)]
pub struct AccountBody {
    pub name: Option<String>,
    pub secret_ref: Option<String>,
    pub weight: Option<i64>,
    pub priority: Option<i64>,
    pub enabled: Option<bool>,
    pub quota_limit: Option<i64>,
    pub quota_unit: Option<String>,
    pub quota_period: Option<String>,
    pub config: Option<Value>,
}

impl AccountBody {
    fn into_input(
        self,
        existing: Option<&LlmProviderAccountRecord>,
    ) -> ApiResult<LlmProviderAccountInput> {
        let weight = self
            .weight
            .or(existing.map(|account| account.weight))
            .unwrap_or(1);
        if !(1..=1000).contains(&weight) {
            return Err(
                ApiError::bad_request("`weight` must be between 1 and 1000").field("weight")
            );
        }
        let quota_unit = self
            .quota_unit
            .or_else(|| existing.map(|account| account.quota_unit.clone()))
            .unwrap_or_else(|| "requests".to_string());
        if !matches!(quota_unit.as_str(), "requests" | "tokens" | "cost_micros") {
            return Err(ApiError::bad_request(
                "`quota_unit` must be requests, tokens or cost_micros",
            )
            .field("quota_unit"));
        }
        let quota_period = self
            .quota_period
            .or_else(|| existing.map(|account| account.quota_period.clone()))
            .unwrap_or_else(|| "none".to_string());
        if !matches!(quota_period.as_str(), "none" | "day" | "month") {
            return Err(
                ApiError::bad_request("`quota_period` must be none, day or month")
                    .field("quota_period"),
            );
        }
        if self.quota_limit.is_some_and(|limit| limit < 0) {
            return Err(
                ApiError::bad_request("`quota_limit` must not be negative").field("quota_limit")
            );
        }
        let name = self
            .name
            .or_else(|| existing.map(|account| account.name.clone()))
            .ok_or_else(|| ApiError::bad_request("`name` is required").field("name"))?
            .trim()
            .to_string();
        if name.is_empty() {
            return Err(ApiError::bad_request("`name` is required").field("name"));
        }
        Ok(LlmProviderAccountInput {
            name,
            secret_ref: match self.secret_ref {
                Some(value) => non_empty(Some(value), "secret_ref")?,
                None => existing.and_then(|account| account.secret_ref.clone()),
            },
            weight,
            priority: self
                .priority
                .or(existing.map(|account| account.priority))
                .unwrap_or(0),
            enabled: self
                .enabled
                .or(existing.map(|account| account.enabled))
                .unwrap_or(true),
            quota_limit: self
                .quota_limit
                .or(existing.and_then(|account| account.quota_limit)),
            quota_unit,
            quota_period,
            config_json: match self.config {
                Some(value) => value.to_string(),
                None => existing
                    .map(|account| account.config_json.clone())
                    .unwrap_or_else(|| "{}".to_string()),
            },
        })
    }
}

#[derive(Debug, Serialize)]
pub struct AccountView {
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
    pub config: Value,
    pub created_at: String,
    pub updated_at: String,
}

impl From<LlmProviderAccountRecord> for AccountView {
    fn from(record: LlmProviderAccountRecord) -> Self {
        Self {
            id: record.id,
            provider_id: record.provider_id,
            name: record.name,
            secret_ref: record.secret_ref,
            weight: record.weight,
            priority: record.priority,
            enabled: record.enabled,
            quota_limit: record.quota_limit,
            quota_unit: record.quota_unit,
            quota_period: record.quota_period,
            quota_used: record.quota_used,
            quota_reset_at: record.quota_reset_at,
            cooldown_until: record.cooldown_until,
            last_error: record.last_error,
            credential_kind: record.credential_kind,
            oauth_status: record.oauth_status,
            oauth_expires_at: record.oauth_expires_at,
            config: serde_json::from_str(&record.config_json).unwrap_or_else(|_| json!({})),
            created_at: record.created_at,
            updated_at: record.updated_at,
        }
    }
}

async fn list_provider_accounts(
    State(state): State<LlmControlState>,
    Path(provider_id): Path<String>,
) -> ApiResult<extract::Json<Vec<AccountView>>> {
    if state
        .storage
        .llm
        .get_provider(&provider_id)
        .await
        .map_err(ApiError::from_storage)?
        .is_none()
    {
        return Err(ApiError::not_found("provider", &provider_id));
    }
    let records = state
        .storage
        .llm
        .list_provider_accounts(&provider_id)
        .await
        .map_err(ApiError::from_storage)?;
    Ok(extract::Json(
        records.into_iter().map(AccountView::from).collect(),
    ))
}

async fn get_account(
    State(state): State<LlmControlState>,
    Path(id): Path<String>,
) -> ApiResult<extract::Json<AccountView>> {
    let record = state
        .storage
        .llm
        .get_account(&id)
        .await
        .map_err(ApiError::from_storage)?
        .ok_or_else(|| ApiError::not_found("account", &id))?;
    Ok(extract::Json(record.into()))
}

async fn create_account(
    State(state): State<LlmControlState>,
    Path(provider_id): Path<String>,
    extract::Json(body): extract::Json<AccountBody>,
) -> ApiResult<(StatusCode, extract::Json<AccountView>)> {
    let input = body.into_input(None)?;
    let record = state
        .storage
        .llm
        .create_account(&provider_id, &input)
        .await
        .map_err(ApiError::from_storage)?;
    reload_now(&state).await?;
    Ok((StatusCode::CREATED, extract::Json(record.into())))
}

async fn update_account(
    State(state): State<LlmControlState>,
    Path(id): Path<String>,
    extract::Json(body): extract::Json<AccountBody>,
) -> ApiResult<extract::Json<AccountView>> {
    let existing = state
        .storage
        .llm
        .get_account(&id)
        .await
        .map_err(ApiError::from_storage)?
        .ok_or_else(|| ApiError::not_found("account", &id))?;
    let input = body.into_input(Some(&existing))?;
    let record = state
        .storage
        .llm
        .update_account(&id, &input)
        .await
        .map_err(ApiError::from_storage)?;
    reload_now(&state).await?;
    Ok(extract::Json(record.into()))
}

async fn delete_account(
    State(state): State<LlmControlState>,
    Path(id): Path<String>,
) -> ApiResult<StatusCode> {
    if !state
        .storage
        .llm
        .delete_account(&id)
        .await
        .map_err(ApiError::from_storage)?
    {
        return Err(ApiError::not_found("account", &id));
    }
    reload_now(&state).await?;
    Ok(StatusCode::NO_CONTENT)
}

async fn reset_account_quota(
    State(state): State<LlmControlState>,
    Path(id): Path<String>,
) -> ApiResult<extract::Json<AccountView>> {
    if !state
        .storage
        .llm
        .reset_account_quota(&id)
        .await
        .map_err(ApiError::from_storage)?
    {
        return Err(ApiError::not_found("account", &id));
    }
    state.runtime.accounts().reset(&id);
    reload_now(&state).await?;
    let record = state
        .storage
        .llm
        .get_account(&id)
        .await
        .map_err(ApiError::from_storage)?
        .ok_or_else(|| ApiError::not_found("account", &id))?;
    Ok(extract::Json(record.into()))
}

// ---------------------------------------------------------------------------
// Models

async fn list_models(
    State(state): State<LlmControlState>,
) -> ApiResult<extract::Json<Vec<ModelView>>> {
    let records = state
        .storage
        .llm
        .list_models()
        .await
        .map_err(ApiError::from_storage)?;
    Ok(extract::Json(
        records.into_iter().map(ModelView::from).collect(),
    ))
}

async fn get_model(
    State(state): State<LlmControlState>,
    Path(id): Path<String>,
) -> ApiResult<extract::Json<ModelView>> {
    let record = state
        .storage
        .llm
        .get_model(&id)
        .await
        .map_err(ApiError::from_storage)?
        .ok_or_else(|| ApiError::not_found("model", &id))?;
    Ok(extract::Json(record.into()))
}

async fn create_model(
    State(state): State<LlmControlState>,
    extract::Json(body): extract::Json<ModelBody>,
) -> ApiResult<(StatusCode, extract::Json<ModelView>)> {
    let input = body.into_input()?;
    let record = state
        .storage
        .llm
        .create_model(&input)
        .await
        .map_err(ApiError::from_storage)?;
    reload_now(&state).await?;
    Ok((StatusCode::CREATED, extract::Json(record.into())))
}

async fn update_model(
    State(state): State<LlmControlState>,
    Path(id): Path<String>,
    extract::Json(body): extract::Json<ModelBody>,
) -> ApiResult<extract::Json<ModelView>> {
    let existing = state
        .storage
        .llm
        .get_model(&id)
        .await
        .map_err(ApiError::from_storage)?
        .ok_or_else(|| ApiError::not_found("model", &id))?;
    let mut input = body.into_input()?;
    if input.capabilities == "{}" && existing.capabilities != "{}" {
        input.capabilities = existing.capabilities;
    }
    if input.pricing_json == "{}" && existing.pricing_json != "{}" {
        input.pricing_json = existing.pricing_json;
    }
    let record = state
        .storage
        .llm
        .update_model(&id, &input)
        .await
        .map_err(ApiError::from_storage)?;
    reload_now(&state).await?;
    Ok(extract::Json(record.into()))
}

async fn delete_model(
    State(state): State<LlmControlState>,
    Path(id): Path<String>,
) -> ApiResult<StatusCode> {
    if !state
        .storage
        .llm
        .delete_model(&id)
        .await
        .map_err(ApiError::from_storage)?
    {
        return Err(ApiError::not_found("model", &id));
    }
    reload_now(&state).await?;
    Ok(StatusCode::NO_CONTENT)
}

// ---------------------------------------------------------------------------
// Routing groups and targets

async fn group_view(
    state: &LlmControlState,
    record: LlmRoutingGroupRecord,
) -> ApiResult<GroupView> {
    let targets = state
        .storage
        .llm
        .list_routing_targets()
        .await
        .map_err(ApiError::from_storage)?
        .into_iter()
        .filter(|target| target.group_id == record.id)
        .map(TargetView::from)
        .collect();
    Ok(GroupView {
        id: record.id,
        name: record.name,
        strategy: record.strategy,
        created_at: record.created_at,
        targets,
    })
}

async fn list_groups(
    State(state): State<LlmControlState>,
) -> ApiResult<extract::Json<Vec<GroupView>>> {
    let records = state
        .storage
        .llm
        .list_routing_groups()
        .await
        .map_err(ApiError::from_storage)?;
    let mut views = Vec::with_capacity(records.len());
    for record in records {
        views.push(group_view(&state, record).await?);
    }
    Ok(extract::Json(views))
}

async fn get_group(
    State(state): State<LlmControlState>,
    Path(id): Path<String>,
) -> ApiResult<extract::Json<GroupView>> {
    let record = state
        .storage
        .llm
        .get_routing_group(&id)
        .await
        .map_err(ApiError::from_storage)?
        .ok_or_else(|| ApiError::not_found("routing group", &id))?;
    Ok(extract::Json(group_view(&state, record).await?))
}

async fn create_group(
    State(state): State<LlmControlState>,
    extract::Json(body): extract::Json<GroupBody>,
) -> ApiResult<(StatusCode, extract::Json<GroupView>)> {
    let (name, strategy) = body.parts()?;
    let record = state
        .storage
        .llm
        .create_routing_group(&name, &strategy)
        .await
        .map_err(ApiError::from_storage)?;
    reload_now(&state).await?;
    Ok((
        StatusCode::CREATED,
        extract::Json(group_view(&state, record).await?),
    ))
}

async fn update_group(
    State(state): State<LlmControlState>,
    Path(id): Path<String>,
    extract::Json(body): extract::Json<GroupBody>,
) -> ApiResult<extract::Json<GroupView>> {
    let existing = state
        .storage
        .llm
        .get_routing_group(&id)
        .await
        .map_err(ApiError::from_storage)?
        .ok_or_else(|| ApiError::not_found("routing group", &id))?;
    let (name, strategy) = GroupBody {
        name: body.name.or(Some(existing.name)),
        strategy: body.strategy.or(Some(existing.strategy)),
    }
    .parts()?;
    let record = state
        .storage
        .llm
        .update_routing_group(&id, &name, &strategy)
        .await
        .map_err(ApiError::from_storage)?;
    reload_now(&state).await?;
    Ok(extract::Json(group_view(&state, record).await?))
}

async fn delete_group(
    State(state): State<LlmControlState>,
    Path(id): Path<String>,
) -> ApiResult<StatusCode> {
    if !state
        .storage
        .llm
        .delete_routing_group(&id)
        .await
        .map_err(ApiError::from_storage)?
    {
        return Err(ApiError::not_found("routing group", &id));
    }
    reload_now(&state).await?;
    Ok(StatusCode::NO_CONTENT)
}

async fn list_targets(
    State(state): State<LlmControlState>,
    Path(id): Path<String>,
) -> ApiResult<extract::Json<Vec<TargetView>>> {
    if !state
        .storage
        .llm
        .group_exists(&id)
        .await
        .map_err(ApiError::from_storage)?
    {
        return Err(ApiError::not_found("routing group", &id));
    }
    let targets = state
        .storage
        .llm
        .list_routing_targets()
        .await
        .map_err(ApiError::from_storage)?
        .into_iter()
        .filter(|target| target.group_id == id)
        .map(TargetView::from)
        .collect();
    Ok(extract::Json(targets))
}

async fn create_target(
    State(state): State<LlmControlState>,
    Path(id): Path<String>,
    extract::Json(body): extract::Json<TargetBody>,
) -> ApiResult<(StatusCode, extract::Json<TargetView>)> {
    let input = body.into_input(None)?;
    let record = state
        .storage
        .llm
        .create_routing_target(&id, &input)
        .await
        .map_err(ApiError::from_storage)?;
    reload_now(&state).await?;
    Ok((StatusCode::CREATED, extract::Json(record.into())))
}

async fn update_target(
    State(state): State<LlmControlState>,
    Path((id, target_id)): Path<(String, String)>,
    extract::Json(body): extract::Json<TargetBody>,
) -> ApiResult<extract::Json<TargetView>> {
    let existing = state
        .storage
        .llm
        .get_routing_target(&target_id)
        .await
        .map_err(ApiError::from_storage)?
        .filter(|target| target.group_id == id)
        .ok_or_else(|| ApiError::not_found("routing target", &target_id))?;
    let input = body.into_input(Some(&existing))?;
    let record = state
        .storage
        .llm
        .update_routing_target(&target_id, &input)
        .await
        .map_err(ApiError::from_storage)?;
    reload_now(&state).await?;
    Ok(extract::Json(record.into()))
}

async fn delete_target(
    State(state): State<LlmControlState>,
    Path((_id, target_id)): Path<(String, String)>,
) -> ApiResult<StatusCode> {
    if !state
        .storage
        .llm
        .delete_routing_target(&target_id)
        .await
        .map_err(ApiError::from_storage)?
    {
        return Err(ApiError::not_found("routing target", &target_id));
    }
    reload_now(&state).await?;
    Ok(StatusCode::NO_CONTENT)
}

// ---------------------------------------------------------------------------
// Routes

async fn list_routes(
    State(state): State<LlmControlState>,
) -> ApiResult<extract::Json<Vec<RouteView>>> {
    let records = state
        .storage
        .llm
        .list_routes()
        .await
        .map_err(ApiError::from_storage)?;
    Ok(extract::Json(
        records.into_iter().map(RouteView::from).collect(),
    ))
}

async fn get_route(
    State(state): State<LlmControlState>,
    Path(id): Path<String>,
) -> ApiResult<extract::Json<RouteView>> {
    let record = state
        .storage
        .llm
        .get_route(&id)
        .await
        .map_err(ApiError::from_storage)?
        .ok_or_else(|| ApiError::not_found("route", &id))?;
    Ok(extract::Json(record.into()))
}

async fn create_route(
    State(state): State<LlmControlState>,
    extract::Json(body): extract::Json<RouteBody>,
) -> ApiResult<(StatusCode, extract::Json<RouteView>)> {
    let virtual_model_id = required(&body.virtual_model_id, "virtual_model_id")?;
    let group = non_empty(body.routing_group_id, "routing_group_id")?;
    let record = state
        .storage
        .llm
        .create_route(&virtual_model_id, group.as_deref())
        .await
        .map_err(ApiError::from_storage)?;
    reload_now(&state).await?;
    Ok((StatusCode::CREATED, extract::Json(record.into())))
}

async fn update_route(
    State(state): State<LlmControlState>,
    Path(id): Path<String>,
    extract::Json(body): extract::Json<RouteBody>,
) -> ApiResult<extract::Json<RouteView>> {
    let existing = state
        .storage
        .llm
        .get_route(&id)
        .await
        .map_err(ApiError::from_storage)?
        .ok_or_else(|| ApiError::not_found("route", &id))?;
    let virtual_model_id = body
        .virtual_model_id
        .or(Some(existing.virtual_model_id))
        .ok_or_else(|| ApiError::bad_request("`virtual_model_id` is required"))?;
    let group = non_empty(body.routing_group_id, "routing_group_id")?;
    let record = state
        .storage
        .llm
        .update_route(&id, &virtual_model_id, group.as_deref())
        .await
        .map_err(ApiError::from_storage)?;
    reload_now(&state).await?;
    Ok(extract::Json(record.into()))
}

async fn delete_route(
    State(state): State<LlmControlState>,
    Path(id): Path<String>,
) -> ApiResult<StatusCode> {
    if !state
        .storage
        .llm
        .delete_route(&id)
        .await
        .map_err(ApiError::from_storage)?
    {
        return Err(ApiError::not_found("route", &id));
    }
    reload_now(&state).await?;
    Ok(StatusCode::NO_CONTENT)
}

// ---------------------------------------------------------------------------
// Virtual keys

async fn list_virtual_keys(
    State(state): State<LlmControlState>,
) -> ApiResult<extract::Json<Vec<VirtualKeyView>>> {
    let records = state
        .storage
        .llm
        .list_virtual_keys()
        .await
        .map_err(ApiError::from_storage)?;
    Ok(extract::Json(
        records.into_iter().map(VirtualKeyView::from).collect(),
    ))
}

async fn get_virtual_key(
    State(state): State<LlmControlState>,
    Path(id): Path<String>,
) -> ApiResult<extract::Json<VirtualKeyView>> {
    let record = state
        .storage
        .llm
        .get_virtual_key(&id)
        .await
        .map_err(ApiError::from_storage)?
        .ok_or_else(|| ApiError::not_found("virtual key", &id))?;
    Ok(extract::Json(record.into()))
}

async fn create_virtual_key(
    State(state): State<LlmControlState>,
    extract::Json(body): extract::Json<VirtualKeyBody>,
) -> ApiResult<(StatusCode, extract::Json<CreatedVirtualKey>)> {
    let name = body.name()?;
    let permissions = body.permissions()?;
    validate_permissions(&permissions)?;
    let expires_at = parse_expiry(body.expires_at)?;
    let plaintext = keys::generate_key();
    let input = LlmVirtualKeyInput {
        name,
        key_prefix: keys::display_prefix(&plaintext),
        key_hash: keys::hash_key(&plaintext),
        consumer_id: non_empty(body.consumer_id, "consumer_id")?,
        enabled: body.enabled.unwrap_or(true),
        expires_at,
        permissions_json: permissions,
        key_family: None,
    };
    let record = state
        .storage
        .llm
        .create_virtual_key(&input)
        .await
        .map_err(ApiError::from_storage)?;
    reload_now(&state).await?;
    Ok((
        StatusCode::CREATED,
        extract::Json(CreatedVirtualKey {
            key: record.into(),
            key_value: plaintext,
            warning: "store this key now; it cannot be retrieved again",
        }),
    ))
}

async fn update_virtual_key(
    State(state): State<LlmControlState>,
    Path(id): Path<String>,
    extract::Json(body): extract::Json<VirtualKeyBody>,
) -> ApiResult<extract::Json<VirtualKeyView>> {
    let existing = state
        .storage
        .llm
        .get_virtual_key(&id)
        .await
        .map_err(ApiError::from_storage)?
        .ok_or_else(|| ApiError::not_found("virtual key", &id))?;
    let permissions = match (body.permissions.clone(), body.permissions_json.clone()) {
        (None, None) => existing.permissions_json,
        _ => {
            let permissions = body.permissions()?;
            validate_permissions(&permissions)?;
            permissions
        }
    };
    let expires_at = match body.expires_at {
        Some(_) => parse_expiry(body.expires_at)?,
        None => existing.expires_at,
    };
    let input = LlmVirtualKeyInput {
        name: body.name.unwrap_or(existing.name),
        key_prefix: existing.key_prefix,
        key_hash: existing.key_hash,
        consumer_id: match body.consumer_id {
            Some(raw) => non_empty(Some(raw), "consumer_id")?,
            None => existing.consumer_id,
        },
        enabled: body.enabled.unwrap_or(existing.enabled),
        expires_at,
        permissions_json: permissions,
        key_family: None,
    };
    let record = state
        .storage
        .llm
        .update_virtual_key(&id, &input)
        .await
        .map_err(ApiError::from_storage)?;
    reload_now(&state).await?;
    Ok(extract::Json(record.into()))
}

async fn rotate_virtual_key(
    State(state): State<LlmControlState>,
    Path(id): Path<String>,
    extract::Json(body): extract::Json<RotateVirtualKeyBody>,
) -> ApiResult<(StatusCode, extract::Json<RotatedVirtualKey>)> {
    let existing = state
        .storage
        .llm
        .get_virtual_key(&id)
        .await
        .map_err(ApiError::from_storage)?
        .ok_or_else(|| ApiError::not_found("virtual key", &id))?;
    let now = Utc::now();
    if !existing.enabled {
        return Err(ApiError::conflict("cannot rotate a disabled virtual key"));
    }
    if existing.expires_at.is_some_and(|expires| expires <= now) {
        return Err(ApiError::conflict("cannot rotate an expired virtual key"));
    }

    let permissions = match body.permissions()? {
        Some(permissions) => {
            validate_permissions(&permissions)?;
            Some(permissions)
        }
        None => None,
    };
    let expires_at = match body.expires_at {
        Some(_) => parse_expiry(body.expires_at)?,
        None => existing.expires_at.filter(|expires| *expires > now),
    };
    let name = match body.name {
        Some(name) => {
            let name = name.trim();
            if name.is_empty() {
                return Err(ApiError::bad_request("name must not be empty"));
            }
            name.to_string()
        }
        None => existing.name.clone(),
    };
    let grace_seconds = body.grace_seconds.unwrap_or(86_400).min(2_592_000);
    let plaintext = keys::generate_key();
    let rotation = LlmVirtualKeyRotation {
        name,
        key_prefix: keys::display_prefix(&plaintext),
        key_hash: keys::hash_key(&plaintext),
        expires_at,
        permissions_json: permissions,
        grace_seconds,
    };
    let result = state
        .storage
        .llm
        .rotate_virtual_key(&id, &rotation)
        .await
        .map_err(ApiError::from_storage)?;
    reload_now(&state).await?;
    Ok((
        StatusCode::CREATED,
        extract::Json(RotatedVirtualKey {
            key: result.key.into(),
            key_value: plaintext,
            warning: "store this key now; it cannot be retrieved again",
            replaces: result.replaces,
            previous_expires_at: result.previous_expires_at.map(|value| value.to_rfc3339()),
        }),
    ))
}

async fn delete_virtual_key(
    State(state): State<LlmControlState>,
    Path(id): Path<String>,
) -> ApiResult<StatusCode> {
    if !state
        .storage
        .llm
        .delete_virtual_key(&id)
        .await
        .map_err(ApiError::from_storage)?
    {
        return Err(ApiError::not_found("virtual key", &id));
    }
    reload_now(&state).await?;
    Ok(StatusCode::NO_CONTENT)
}

// ---------------------------------------------------------------------------
// Runtime

async fn reload(State(state): State<LlmControlState>) -> ApiResult<extract::Json<Value>> {
    let response = reload_runtime(&state.runtime).await?;
    if let Some(plugins) = &state.plugins {
        plugins.reload().await.map_err(ApiError::internal)?;
    }
    Ok(response)
}

async fn reload_plugins(State(state): State<LlmControlState>) -> ApiResult<extract::Json<Value>> {
    let plugins = state.plugins.as_ref().ok_or_else(|| {
        ApiError::new(
            StatusCode::SERVICE_UNAVAILABLE,
            "LLM plugin runtime is not enabled",
        )
    })?;
    plugins.reload().await.map_err(ApiError::internal)?;
    Ok(extract::Json(json!({
        "status": "reloaded",
        "plugins": plugins.registry().manifests().len(),
    })))
}

async fn plugin_metrics(State(state): State<LlmControlState>) -> ApiResult<extract::Json<Value>> {
    let plugins = state.plugins.as_ref().ok_or_else(|| {
        ApiError::new(
            StatusCode::SERVICE_UNAVAILABLE,
            "LLM plugin runtime is not enabled",
        )
    })?;
    Ok(extract::Json(json!({ "plugins": plugins.metrics() })))
}

#[derive(Debug, Deserialize)]
pub struct EffectiveChainQuery {
    pub consumer_id: Option<String>,
    pub virtual_key_id: Option<String>,
    pub model: Option<String>,
    pub provider_id: Option<String>,
}

async fn plugin_effective_chain(
    State(state): State<LlmControlState>,
    Query(query): Query<EffectiveChainQuery>,
) -> ApiResult<extract::Json<Value>> {
    let plugins = state.plugins.as_ref().ok_or_else(|| {
        ApiError::new(
            StatusCode::SERVICE_UNAVAILABLE,
            "LLM plugin runtime is not enabled",
        )
    })?;
    let model = query
        .model
        .as_deref()
        .map(str::trim)
        .filter(|model| !model.is_empty())
        .ok_or_else(|| ApiError::bad_request("model is required"))?;
    let chain = plugins.effective_chain(
        query.consumer_id.as_deref(),
        query.virtual_key_id.as_deref(),
        model,
        query.provider_id.as_deref(),
    );
    Ok(extract::Json(json!({
        "model": model,
        "provider_id": query.provider_id,
        "chain": chain,
    })))
}

/// Reloads after a mutation so the next data plane request sees the change.
async fn reload_pii_engines(state: &LlmControlState) -> ApiResult<()> {
    let Some(pii) = &state.pii else {
        return Ok(());
    };
    let records = state
        .storage
        .llm
        .list_pii_engines()
        .await
        .map_err(ApiError::from_storage)?;
    for warning in pii.reload(&records) {
        tracing::warn!("{warning}");
    }
    if let Some(plugins) = &state.plugins {
        plugins.reload().await.map_err(ApiError::internal)?;
    }
    Ok(())
}

async fn reload_now(state: &LlmControlState) -> ApiResult<()> {
    state.runtime.reload().await.map_err(ApiError::internal)?;
    if let Some(plugins) = &state.plugins {
        plugins.reload().await.map_err(ApiError::internal)?;
    }
    Ok(())
}

pub async fn reload_runtime(runtime: &LlmRuntime) -> ApiResult<extract::Json<Value>> {
    runtime.reload().await.map_err(ApiError::internal)?;
    let snapshot = runtime.snapshot().await;
    Ok(extract::Json(json!({
        "status": "reloaded",
        "generated_at": snapshot.generated_at,
        "providers": snapshot.enabled_providers().count(),
        "accounts": snapshot.data.accounts.len(),
        "routes": snapshot.model_listing().len(),
        "issues": snapshot.issues,
    })))
}

// ---------------------------------------------------------------------------
// LLM plugins

#[derive(Debug, Serialize)]
pub struct PluginView {
    pub id: String,
    pub name: String,
    pub version: String,
    pub enabled: bool,
    pub priority: i64,
    pub description: String,
    pub updated_at: String,
}

impl From<PluginRecord> for PluginView {
    fn from(record: PluginRecord) -> Self {
        Self {
            id: record.id,
            name: record.name,
            version: record.version,
            enabled: record.enabled,
            priority: record.priority,
            description: record.description,
            updated_at: record.updated_at,
        }
    }
}

#[derive(Debug, Serialize)]
pub struct PluginConfigView {
    pub id: String,
    pub plugin_id: String,
    pub scope: String,
    pub scope_id: String,
    pub enabled: bool,
    pub config: Value,
    pub created_at: String,
    pub updated_at: String,
}

impl From<PluginConfigRecord> for PluginConfigView {
    fn from(record: PluginConfigRecord) -> Self {
        Self {
            id: record.id,
            plugin_id: record.plugin_id,
            scope: record.scope,
            scope_id: record.scope_id,
            enabled: record.enabled,
            config: serde_json::from_str(&record.config_json).unwrap_or_else(|_| json!({})),
            created_at: record.created_at,
            updated_at: record.updated_at,
        }
    }
}

#[derive(Debug, Serialize)]
pub struct PluginDetailView {
    pub plugin: PluginView,
    pub configs: Vec<PluginConfigView>,
    /// Parsed JSON Schema for the plugin configuration. Invalid or absent
    /// schemas are exposed as `null` instead of failing the request.
    pub config_schema: Value,
}

impl PluginDetailView {
    fn new(record: PluginRecord, configs: Vec<PluginConfigRecord>) -> Self {
        let config_schema = serde_json::from_str(&record.config_schema_json).unwrap_or(Value::Null);
        Self {
            plugin: record.into(),
            configs: configs.into_iter().map(PluginConfigView::from).collect(),
            config_schema,
        }
    }
}

#[derive(Debug, Deserialize)]
pub struct PluginFlagsBody {
    pub enabled: Option<bool>,
    pub priority: Option<i64>,
}

#[derive(Debug, Deserialize)]
pub struct PluginConfigBody {
    pub scope: Option<String>,
    pub scope_id: Option<String>,
    pub enabled: Option<bool>,
    pub config: Option<Value>,
}

async fn list_plugins(
    State(state): State<LlmControlState>,
) -> ApiResult<extract::Json<Vec<PluginView>>> {
    let records = state
        .storage
        .llm
        .list_plugins()
        .await
        .map_err(ApiError::internal)?;
    Ok(extract::Json(
        records
            .into_iter()
            .filter(|record| record.kind == "llm")
            .map(PluginView::from)
            .collect(),
    ))
}

async fn get_plugin(
    State(state): State<LlmControlState>,
    Path(id): Path<String>,
) -> ApiResult<extract::Json<PluginDetailView>> {
    let record = llm_plugin(&state, &id).await?;
    let configs = state
        .storage
        .llm
        .list_plugin_configs(&id)
        .await
        .map_err(ApiError::internal)?;
    Ok(extract::Json(PluginDetailView::new(record, configs)))
}

async fn update_plugin(
    State(state): State<LlmControlState>,
    Path(id): Path<String>,
    extract::Json(body): extract::Json<PluginFlagsBody>,
) -> ApiResult<extract::Json<PluginDetailView>> {
    let _ = llm_plugin(&state, &id).await?;
    let record = state
        .storage
        .llm
        .update_plugin_flags(&id, body.enabled, body.priority)
        .await
        .map_err(ApiError::internal)?
        .ok_or_else(|| ApiError::not_found("plugin", &id))?;
    reload_now(&state).await?;
    let configs = state
        .storage
        .llm
        .list_plugin_configs(&id)
        .await
        .map_err(ApiError::internal)?;
    Ok(extract::Json(PluginDetailView::new(record, configs)))
}

async fn list_plugin_configs(
    State(state): State<LlmControlState>,
    Path(id): Path<String>,
) -> ApiResult<extract::Json<Vec<PluginConfigView>>> {
    let _ = llm_plugin(&state, &id).await?;
    let configs = state
        .storage
        .llm
        .list_plugin_configs(&id)
        .await
        .map_err(ApiError::internal)?;
    Ok(extract::Json(
        configs.into_iter().map(PluginConfigView::from).collect(),
    ))
}

async fn create_plugin_config(
    State(state): State<LlmControlState>,
    Path(id): Path<String>,
    extract::Json(body): extract::Json<PluginConfigBody>,
) -> ApiResult<(StatusCode, extract::Json<PluginConfigView>)> {
    let plugin = llm_plugin(&state, &id).await?;
    let scope = llm_scope(&body)?;
    ensure_plugin_scope(&state, &plugin.name, scope)?;
    let config = body.config.unwrap_or_else(|| json!({}));
    validate_plugin_config(&state, &plugin.name, &config)?;
    ensure_scope_target(&state, scope, body.scope_id.as_deref()).await?;
    let record = state
        .storage
        .llm
        .create_plugin_config_raw(
            &id,
            scope.as_str(),
            body.scope_id.as_deref(),
            body.enabled.unwrap_or(true),
            &config.to_string(),
        )
        .await
        .map_err(ApiError::internal)?;
    reload_now(&state).await?;
    Ok((
        StatusCode::CREATED,
        extract::Json(PluginConfigView::from(record)),
    ))
}

async fn update_plugin_config(
    State(state): State<LlmControlState>,
    Path(id): Path<String>,
    extract::Json(body): extract::Json<PluginConfigBody>,
) -> ApiResult<extract::Json<PluginConfigView>> {
    let existing = state
        .storage
        .llm
        .get_plugin_config(&id)
        .await
        .map_err(ApiError::internal)?
        .ok_or_else(|| ApiError::not_found("plugin config", &id))?;
    let plugin = llm_plugin(&state, &existing.plugin_id).await?;
    if let Some(scope) = LlmScope::parse(existing.scope.trim()) {
        ensure_plugin_scope(&state, &plugin.name, scope)?;
    }
    let config_json = match &body.config {
        Some(config) => {
            validate_plugin_config(&state, &plugin.name, config)?;
            Some(config.to_string())
        }
        None => None,
    };
    let record = state
        .storage
        .llm
        .update_plugin_config(&id, body.enabled, config_json.as_deref())
        .await
        .map_err(ApiError::internal)?
        .ok_or_else(|| ApiError::not_found("plugin config", &id))?;
    reload_now(&state).await?;
    Ok(extract::Json(PluginConfigView::from(record)))
}

async fn delete_plugin_config(
    State(state): State<LlmControlState>,
    Path(id): Path<String>,
) -> ApiResult<StatusCode> {
    if !state
        .storage
        .llm
        .delete_plugin_config(&id)
        .await
        .map_err(ApiError::internal)?
    {
        return Err(ApiError::not_found("plugin config", &id));
    }
    reload_now(&state).await?;
    Ok(StatusCode::NO_CONTENT)
}

async fn llm_plugin(state: &LlmControlState, id: &str) -> ApiResult<PluginRecord> {
    let record = state
        .storage
        .llm
        .get_plugin(id)
        .await
        .map_err(ApiError::internal)?
        .ok_or_else(|| ApiError::not_found("plugin", id))?;
    if record.kind != "llm" {
        return Err(ApiError::not_found("LLM plugin", id));
    }
    Ok(record)
}

fn llm_scope(body: &PluginConfigBody) -> ApiResult<LlmScope> {
    let raw = body
        .scope
        .as_deref()
        .map(str::trim)
        .filter(|scope| !scope.is_empty())
        .ok_or_else(|| ApiError::bad_request("`scope` is required").field("scope"))?;
    LlmScope::parse(raw).ok_or_else(|| {
        ApiError::bad_request("`scope` must be global, consumer, virtual_key, model or provider")
            .field("scope")
    })
}

fn ensure_plugin_scope(state: &LlmControlState, plugin: &str, scope: LlmScope) -> ApiResult<()> {
    let Some(plugins) = &state.plugins else {
        return Err(ApiError::new(
            StatusCode::SERVICE_UNAVAILABLE,
            "LLM plugin runtime is not enabled",
        ));
    };
    let registry = plugins.registry();
    if registry.supports_scope(plugin, scope) {
        return Ok(());
    }
    let supported = registry
        .supported_scopes(plugin)
        .iter()
        .map(|scope| scope.as_str())
        .collect::<Vec<_>>()
        .join(", ");
    Err(ApiError::bad_request(format!(
        "{plugin} does not support {} scope; supported scopes: {supported}",
        scope.as_str()
    ))
    .field("scope"))
}

fn validate_plugin_config(state: &LlmControlState, plugin: &str, config: &Value) -> ApiResult<()> {
    if !config.is_object() {
        return Err(ApiError::bad_request("`config` must be a JSON object").field("config"));
    }
    match &state.plugins {
        Some(plugins) => plugins
            .registry()
            .validate(plugin, config)
            .map_err(|error| ApiError::bad_request(error.message).field("config")),
        None => Err(ApiError::new(
            StatusCode::SERVICE_UNAVAILABLE,
            "LLM plugin runtime is not enabled",
        )),
    }
}

async fn ensure_scope_target(
    state: &LlmControlState,
    scope: LlmScope,
    scope_id: Option<&str>,
) -> ApiResult<()> {
    if scope == LlmScope::Global {
        return Ok(());
    }
    let id = scope_id
        .map(str::trim)
        .filter(|id| !id.is_empty())
        .ok_or_else(|| ApiError::bad_request("`scope_id` is required").field("scope_id"))?;
    let exists = match scope {
        LlmScope::Global => true,
        LlmScope::Consumer => state
            .storage
            .api
            .get_consumer(id)
            .await
            .map_err(ApiError::internal)?
            .is_some(),
        LlmScope::VirtualKey => state
            .storage
            .llm
            .get_virtual_key(id)
            .await
            .map_err(ApiError::from_storage)?
            .is_some(),
        LlmScope::Model => state
            .storage
            .llm
            .get_model(id)
            .await
            .map_err(ApiError::from_storage)?
            .is_some(),
        LlmScope::Provider => state
            .storage
            .llm
            .get_provider(id)
            .await
            .map_err(ApiError::from_storage)?
            .is_some(),
    };
    if exists {
        Ok(())
    } else {
        Err(ApiError::not_found(scope.as_str(), id))
    }
}

// ---------------------------------------------------------------------------
// Usage

#[derive(Debug, Deserialize)]
pub struct UsageQuery {
    pub limit: Option<i64>,
}

#[derive(Debug, Serialize)]
pub struct UsageView {
    pub totals: UsageTotals,
    pub by_model: Vec<UsageModelRow>,
    pub by_provider: Vec<UsageProviderRow>,
    pub recent: Vec<UsageRecordView>,
}

#[derive(Debug, Serialize)]
pub struct UsageTotals {
    pub requests: i64,
    pub errors: i64,
    pub input_tokens: i64,
    pub output_tokens: i64,
    pub cost_micros: i64,
}

#[derive(Debug, Serialize)]
pub struct UsageModelRow {
    pub virtual_model_id: String,
    pub requests: i64,
    pub errors: i64,
    pub input_tokens: i64,
    pub output_tokens: i64,
    pub cost_micros: i64,
}

#[derive(Debug, Serialize)]
pub struct UsageProviderRow {
    pub provider_id: String,
    pub requests: i64,
    pub errors: i64,
    pub input_tokens: i64,
    pub output_tokens: i64,
    pub cost_micros: i64,
}

#[derive(Debug, Serialize)]
pub struct UsageRecordView {
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

impl From<LlmUsageRecord> for UsageRecordView {
    fn from(record: LlmUsageRecord) -> Self {
        Self {
            id: record.id,
            request_id: record.request_id,
            virtual_model_id: record.virtual_model_id,
            provider_id: record.provider_id,
            model_id: record.model_id,
            account_id: record.account_id,
            account_name: record.account_name,
            api_key_id: record.api_key_id,
            protocol: record.protocol,
            status_code: record.status_code,
            latency_ms: record.latency_ms,
            input_tokens: record.input_tokens,
            output_tokens: record.output_tokens,
            cache_read_tokens: record.cache_read_tokens,
            cache_write_tokens: record.cache_write_tokens,
            reasoning_tokens: record.reasoning_tokens,
            cost_micros: record.cost_micros,
            cost_currency: record.cost_currency,
            finish_reason: record.finish_reason,
            error: record.error,
            created_at: record.created_at,
        }
    }
}

async fn usage(
    State(state): State<LlmControlState>,
    Query(query): Query<UsageQuery>,
) -> ApiResult<extract::Json<UsageView>> {
    let limit = query.limit.unwrap_or(50).clamp(1, 500);
    let totals = state
        .storage
        .llm
        .usage_totals()
        .await
        .map_err(ApiError::from_storage)?;
    let by_model = state
        .storage
        .llm
        .usage_summary()
        .await
        .map_err(ApiError::from_storage)?;
    let by_provider = state
        .storage
        .llm
        .usage_by_provider()
        .await
        .map_err(ApiError::from_storage)?;
    let recent = state
        .storage
        .llm
        .list_usage(limit)
        .await
        .map_err(ApiError::from_storage)?;
    Ok(extract::Json(UsageView {
        totals: UsageTotals {
            requests: totals.requests,
            errors: totals.errors,
            input_tokens: totals.input_tokens,
            output_tokens: totals.output_tokens,
            cost_micros: totals.cost_micros,
        },
        by_model: by_model
            .into_iter()
            .map(|row| UsageModelRow {
                virtual_model_id: row.virtual_model_id,
                requests: row.requests,
                errors: row.errors,
                input_tokens: row.input_tokens,
                output_tokens: row.output_tokens,
                cost_micros: row.cost_micros,
            })
            .collect(),
        by_provider: by_provider
            .into_iter()
            .map(|row| UsageProviderRow {
                provider_id: row.provider_id,
                requests: row.requests,
                errors: row.errors,
                input_tokens: row.input_tokens,
                output_tokens: row.output_tokens,
                cost_micros: row.cost_micros,
            })
            .collect(),
        recent: recent.into_iter().map(UsageRecordView::from).collect(),
    }))
}

// ---------------------------------------------------------------------------
// PII engines

#[derive(Debug, Deserialize)]
pub struct PiiEngineBody {
    pub name: Option<String>,
    pub mode: Option<String>,
    pub transport: Option<String>,
    pub endpoint: Option<String>,
    pub command: Option<String>,
    pub args: Option<Vec<String>>,
    pub env_allow: Option<Vec<String>>,
    pub model_dir: Option<String>,
    pub config: Option<Value>,
    pub enabled: Option<bool>,
}

impl PiiEngineBody {
    fn into_input(self, existing: Option<&LlmPiiEngineRecord>) -> ApiResult<LlmPiiEngineInput> {
        let name = required(
            &self
                .name
                .clone()
                .or_else(|| existing.map(|record| record.name.clone())),
            "name",
        )?;
        let mode = self
            .mode
            .clone()
            .or_else(|| existing.map(|record| record.mode.clone()))
            .unwrap_or_else(|| "external".to_string());
        if !matches!(mode.as_str(), "external" | "managed" | "local") {
            return Err(
                ApiError::bad_request("`mode` must be external, managed or local").field("mode"),
            );
        }
        let transport = self
            .transport
            .clone()
            .or_else(|| existing.map(|record| record.transport.clone()))
            .unwrap_or_else(|| {
                if mode == "local" {
                    "in_process".to_string()
                } else {
                    "http_loopback".to_string()
                }
            });
        if !matches!(
            transport.as_str(),
            "http_loopback" | "uds_jsonl" | "in_process"
        ) {
            return Err(ApiError::bad_request(
                "`transport` must be http_loopback, uds_jsonl or in_process",
            )
            .field("transport"));
        }
        if mode == "local" && transport != "in_process" {
            return Err(ApiError::bad_request(
                "local PII engines require the in_process transport",
            )
            .field("transport"));
        }
        if mode == "managed" && transport != "uds_jsonl" {
            return Err(ApiError::bad_request(
                "managed PII engines require the uds_jsonl transport",
            )
            .field("transport"));
        }
        if mode == "external" && transport == "in_process" {
            return Err(ApiError::bad_request(
                "external PII engines require http_loopback or uds_jsonl",
            )
            .field("transport"));
        }
        let endpoint = non_empty(
            self.endpoint
                .or_else(|| existing.and_then(|record| record.endpoint.clone())),
            "endpoint",
        )?;
        if mode == "managed" {
            let endpoint_value = endpoint.as_deref().ok_or_else(|| {
                ApiError::bad_request("managed PII engines require `endpoint`").field("endpoint")
            })?;
            crate::pii::validate_endpoint(&transport, endpoint_value)
                .map_err(|message| ApiError::bad_request(message).field("endpoint"))?;
        } else if mode == "external" {
            let endpoint_value = endpoint.as_deref().ok_or_else(|| {
                ApiError::bad_request("external PII engines require `endpoint`").field("endpoint")
            })?;
            crate::pii::validate_endpoint(&transport, endpoint_value)
                .map_err(|message| ApiError::bad_request(message).field("endpoint"))?;
        }
        let command = non_empty(
            self.command
                .or_else(|| existing.and_then(|record| record.command.clone())),
            "command",
        )?;
        if mode == "managed" && command.is_none() {
            return Err(
                ApiError::bad_request("managed PII engines require `command`").field("command"),
            );
        }
        let args = self
            .args
            .or_else(|| existing.map(|record| record.args.clone()))
            .unwrap_or_default();
        if args.iter().any(|arg| arg.trim().is_empty()) {
            return Err(ApiError::bad_request("`args` entries must not be empty").field("args"));
        }
        let env_allow = self
            .env_allow
            .or_else(|| existing.map(|record| record.env_allow.clone()))
            .unwrap_or_default();
        if env_allow
            .iter()
            .any(|name| name.trim().is_empty() || name.contains('=') || name.contains('\0'))
        {
            return Err(ApiError::bad_request(
                "`env_allow` entries must be plain environment variable names",
            )
            .field("env_allow"));
        }
        let model_dir = non_empty(
            self.model_dir
                .or_else(|| existing.and_then(|record| record.model_dir.clone())),
            "model_dir",
        )?;
        let config_json = match self.config {
            Some(Value::Object(_)) => self
                .config
                .clone()
                .map(|value| value.to_string())
                .unwrap_or_else(|| "{}".to_string()),
            Some(_) => {
                return Err(ApiError::bad_request("`config` must be a JSON object").field("config"));
            }
            None => existing
                .map(|record| record.config_json.clone())
                .unwrap_or_else(|| "{}".to_string()),
        };
        let enabled = self
            .enabled
            .or_else(|| existing.map(|record| record.enabled))
            .unwrap_or(true);
        Ok(LlmPiiEngineInput {
            name,
            mode,
            transport,
            endpoint,
            command,
            args,
            env_allow,
            model_dir,
            config_json,
            enabled,
        })
    }
}

#[derive(Debug, Serialize)]
pub struct PiiEngineView {
    pub id: String,
    pub name: String,
    pub mode: String,
    pub transport: String,
    pub endpoint: Option<String>,
    pub command: Option<String>,
    pub args: Vec<String>,
    pub env_allow: Vec<String>,
    pub model_dir: Option<String>,
    pub config: Value,
    pub enabled: bool,
    pub created_at: String,
    pub updated_at: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub status: Option<EngineStatus>,
}

impl PiiEngineView {
    fn new(record: &LlmPiiEngineRecord, status: Option<EngineStatus>) -> Self {
        Self {
            id: record.id.clone(),
            name: record.name.clone(),
            mode: record.mode.clone(),
            transport: record.transport.clone(),
            endpoint: record.endpoint.clone(),
            command: record.command.clone(),
            args: record.args.clone(),
            env_allow: record.env_allow.clone(),
            model_dir: record.model_dir.clone(),
            config: serde_json::from_str(&record.config_json).unwrap_or_else(|_| json!({})),
            enabled: record.enabled,
            created_at: record.created_at.clone(),
            updated_at: record.updated_at.clone(),
            status,
        }
    }
}

fn pii_status_map(state: &LlmControlState) -> HashMap<String, EngineStatus> {
    state
        .pii
        .as_ref()
        .map(|pii| {
            pii.statuses()
                .into_iter()
                .map(|status| (status.name.clone(), status))
                .collect()
        })
        .unwrap_or_default()
}

async fn list_pii_engines(
    State(state): State<LlmControlState>,
) -> ApiResult<extract::Json<Vec<PiiEngineView>>> {
    let records = state
        .storage
        .llm
        .list_pii_engines()
        .await
        .map_err(ApiError::from_storage)?;
    let statuses = pii_status_map(&state);
    Ok(extract::Json(
        records
            .iter()
            .map(|record| PiiEngineView::new(record, statuses.get(&record.name).cloned()))
            .collect(),
    ))
}

async fn get_pii_engine(
    State(state): State<LlmControlState>,
    Path(id): Path<String>,
) -> ApiResult<extract::Json<PiiEngineView>> {
    let record = state
        .storage
        .llm
        .get_pii_engine(&id)
        .await
        .map_err(ApiError::from_storage)?
        .ok_or_else(|| ApiError::not_found("pii engine", &id))?;
    let status = state
        .pii
        .as_ref()
        .and_then(|pii| pii.get_status(&record.name));
    Ok(extract::Json(PiiEngineView::new(&record, status)))
}

async fn create_pii_engine(
    State(state): State<LlmControlState>,
    extract::Json(body): extract::Json<PiiEngineBody>,
) -> ApiResult<(StatusCode, extract::Json<PiiEngineView>)> {
    let input = body.into_input(None)?;
    let record = state
        .storage
        .llm
        .create_pii_engine(&input)
        .await
        .map_err(ApiError::from_storage)?;
    reload_pii_engines(&state).await?;
    let status = state
        .pii
        .as_ref()
        .and_then(|pii| pii.get_status(&record.name));
    Ok((
        StatusCode::CREATED,
        extract::Json(PiiEngineView::new(&record, status)),
    ))
}

async fn update_pii_engine(
    State(state): State<LlmControlState>,
    Path(id): Path<String>,
    extract::Json(body): extract::Json<PiiEngineBody>,
) -> ApiResult<extract::Json<PiiEngineView>> {
    let existing = state
        .storage
        .llm
        .get_pii_engine(&id)
        .await
        .map_err(ApiError::from_storage)?
        .ok_or_else(|| ApiError::not_found("pii engine", &id))?;
    let input = body.into_input(Some(&existing))?;
    let record = state
        .storage
        .llm
        .update_pii_engine(&id, &input)
        .await
        .map_err(ApiError::from_storage)?;
    reload_pii_engines(&state).await?;
    let status = state
        .pii
        .as_ref()
        .and_then(|pii| pii.get_status(&record.name));
    Ok(extract::Json(PiiEngineView::new(&record, status)))
}

async fn delete_pii_engine(
    State(state): State<LlmControlState>,
    Path(id): Path<String>,
) -> ApiResult<StatusCode> {
    state
        .storage
        .llm
        .delete_pii_engine(&id)
        .await
        .map_err(ApiError::from_storage)?;
    reload_pii_engines(&state).await?;
    Ok(StatusCode::NO_CONTENT)
}

async fn pii_engine_status(
    State(state): State<LlmControlState>,
    Path(id): Path<String>,
) -> ApiResult<extract::Json<Value>> {
    let record = state
        .storage
        .llm
        .get_pii_engine(&id)
        .await
        .map_err(ApiError::from_storage)?
        .ok_or_else(|| ApiError::not_found("pii engine", &id))?;
    let status = state
        .pii
        .as_ref()
        .and_then(|pii| pii.get_status(&record.name));
    Ok(extract::Json(json!({
        "id": record.id,
        "name": record.name,
        "status": status,
    })))
}

async fn reload_pii_engine(
    State(state): State<LlmControlState>,
    Path(_id): Path<String>,
) -> ApiResult<extract::Json<Value>> {
    reload_pii_engines(&state).await?;
    Ok(extract::Json(json!({ "status": "reloaded" })))
}

async fn start_pii_engine(
    State(state): State<LlmControlState>,
    Path(id): Path<String>,
) -> ApiResult<extract::Json<Value>> {
    let record = state
        .storage
        .llm
        .get_pii_engine(&id)
        .await
        .map_err(ApiError::from_storage)?
        .ok_or_else(|| ApiError::not_found("pii engine", &id))?;
    let pii = state.pii.as_ref().ok_or_else(|| {
        ApiError::new(
            StatusCode::SERVICE_UNAVAILABLE,
            "PII engine runtime is not enabled",
        )
    })?;
    pii.start(&record).map_err(ApiError::bad_request)?;
    if let Some(plugins) = &state.plugins {
        plugins.reload().await.map_err(ApiError::internal)?;
    }
    Ok(extract::Json(json!({
        "status": "started",
        "engine": pii.get_status(&record.name),
    })))
}

async fn stop_pii_engine(
    State(state): State<LlmControlState>,
    Path(id): Path<String>,
) -> ApiResult<extract::Json<Value>> {
    let record = state
        .storage
        .llm
        .get_pii_engine(&id)
        .await
        .map_err(ApiError::from_storage)?
        .ok_or_else(|| ApiError::not_found("pii engine", &id))?;
    let pii = state.pii.as_ref().ok_or_else(|| {
        ApiError::new(
            StatusCode::SERVICE_UNAVAILABLE,
            "PII engine runtime is not enabled",
        )
    })?;
    pii.stop(&record.name);
    if let Some(plugins) = &state.plugins {
        plugins.reload().await.map_err(ApiError::internal)?;
    }
    Ok(extract::Json(json!({
        "status": "stopped",
        "engine": pii.get_status(&record.name),
    })))
}

async fn restart_pii_engine(
    State(state): State<LlmControlState>,
    Path(id): Path<String>,
) -> ApiResult<extract::Json<Value>> {
    let record = state
        .storage
        .llm
        .get_pii_engine(&id)
        .await
        .map_err(ApiError::from_storage)?
        .ok_or_else(|| ApiError::not_found("pii engine", &id))?;
    let pii = state.pii.as_ref().ok_or_else(|| {
        ApiError::new(
            StatusCode::SERVICE_UNAVAILABLE,
            "PII engine runtime is not enabled",
        )
    })?;
    pii.stop(&record.name);
    if let Err(message) = pii.start(&record) {
        if let Some(plugins) = &state.plugins {
            plugins.reload().await.map_err(ApiError::internal)?;
        }
        return Err(ApiError::bad_request(message));
    }
    if let Some(plugins) = &state.plugins {
        plugins.reload().await.map_err(ApiError::internal)?;
    }
    Ok(extract::Json(json!({
        "status": "restarted",
        "engine": pii.get_status(&record.name),
    })))
}

// ---------------------------------------------------------------------------
// Local PII models

#[derive(Debug, Serialize)]
pub struct PiiModelView {
    #[serde(flatten)]
    model: crate::pii::ModelStatus,
    enabled: bool,
    threshold: f32,
    #[serde(skip_serializing_if = "Option::is_none")]
    record_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    engine_readiness: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    engine_error: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct PiiModelAction {
    #[serde(rename = "do")]
    action: String,
    #[serde(default)]
    score: Option<f32>,
}

fn pii_runtime(state: &LlmControlState) -> ApiResult<&crate::pii::PiiEngineRuntime> {
    state.pii.as_deref().ok_or_else(|| {
        ApiError::new(
            StatusCode::SERVICE_UNAVAILABLE,
            "PII model runtime is not enabled",
        )
    })
}

fn local_threshold(record: Option<&LlmPiiEngineRecord>, spec: &crate::pii::ModelSpec) -> f32 {
    record
        .and_then(|record| serde_json::from_str::<Value>(&record.config_json).ok())
        .and_then(|config| config.get("threshold").and_then(Value::as_f64))
        .map(|value| (value as f32).clamp(spec.floor, 0.999))
        .unwrap_or(spec.default_threshold)
}

async fn pii_model_view(
    state: &LlmControlState,
    spec: &'static crate::pii::ModelSpec,
) -> ApiResult<PiiModelView> {
    let pii = pii_runtime(state)?;
    let model = pii
        .model_status(spec.id)
        .ok_or_else(|| ApiError::not_found("PII model", spec.id))?;
    let record = state
        .storage
        .llm
        .get_pii_engine_by_name(spec.id)
        .await
        .map_err(ApiError::from_storage)?;
    let engine = pii.get_status(spec.id);
    Ok(PiiModelView {
        threshold: local_threshold(record.as_ref(), spec),
        enabled: record.as_ref().is_some_and(|record| record.enabled),
        record_id: record.map(|record| record.id),
        engine_readiness: engine.as_ref().map(|engine| engine.readiness.to_string()),
        engine_error: engine.and_then(|engine| engine.last_error),
        model,
    })
}

async fn list_pii_models(
    State(state): State<LlmControlState>,
) -> ApiResult<extract::Json<Vec<PiiModelView>>> {
    let pii = pii_runtime(&state)?;
    let records = state
        .storage
        .llm
        .list_pii_engines()
        .await
        .map_err(ApiError::from_storage)?;
    let mut views = Vec::new();
    for spec in crate::pii::catalog::MODELS {
        let model = pii
            .model_status(spec.id)
            .ok_or_else(|| ApiError::not_found("PII model", spec.id))?;
        let record = records.iter().find(|record| record.name == spec.id);
        let engine = pii.get_status(spec.id);
        views.push(PiiModelView {
            threshold: local_threshold(record, spec),
            enabled: record.is_some_and(|record| record.enabled),
            record_id: record.map(|record| record.id.clone()),
            engine_readiness: engine.as_ref().map(|engine| engine.readiness.to_string()),
            engine_error: engine.and_then(|engine| engine.last_error),
            model,
        });
    }
    Ok(extract::Json(views))
}

async fn get_pii_model(
    State(state): State<LlmControlState>,
    Path(id): Path<String>,
) -> ApiResult<extract::Json<PiiModelView>> {
    let spec =
        crate::pii::catalog::get(&id).ok_or_else(|| ApiError::not_found("PII model", &id))?;
    Ok(extract::Json(pii_model_view(&state, spec).await?))
}

async fn ensure_local_record(
    state: &LlmControlState,
    spec: &'static crate::pii::ModelSpec,
    enabled: bool,
    threshold: f32,
) -> ApiResult<LlmPiiEngineRecord> {
    let models = pii_runtime(state)?.models().ok_or_else(|| {
        ApiError::new(
            StatusCode::SERVICE_UNAVAILABLE,
            "PII model directory is not configured",
        )
    })?;
    let input = LlmPiiEngineInput {
        name: spec.id.to_string(),
        mode: "local".to_string(),
        transport: "in_process".to_string(),
        endpoint: None,
        command: None,
        args: Vec::new(),
        env_allow: Vec::new(),
        model_dir: Some(models.model_dir(spec).to_string_lossy().into_owned()),
        config_json: json!({ "model": spec.id, "threshold": threshold }).to_string(),
        enabled,
    };
    match state
        .storage
        .llm
        .get_pii_engine_by_name(spec.id)
        .await
        .map_err(ApiError::from_storage)?
    {
        Some(existing) => state
            .storage
            .llm
            .update_pii_engine(&existing.id, &input)
            .await
            .map_err(ApiError::from_storage),
        None => state
            .storage
            .llm
            .create_pii_engine(&input)
            .await
            .map_err(ApiError::from_storage),
    }
}

async fn pii_model_action(
    State(state): State<LlmControlState>,
    Path(id): Path<String>,
    extract::Json(body): extract::Json<PiiModelAction>,
) -> ApiResult<extract::Json<PiiModelView>> {
    let spec =
        crate::pii::catalog::get(&id).ok_or_else(|| ApiError::not_found("PII model", &id))?;
    let pii = pii_runtime(&state)?;
    let models = pii.models().ok_or_else(|| {
        ApiError::new(
            StatusCode::SERVICE_UNAVAILABLE,
            "PII model directory is not configured",
        )
    })?;
    let existing = state
        .storage
        .llm
        .get_pii_engine_by_name(spec.id)
        .await
        .map_err(ApiError::from_storage)?;
    let enabled = existing.as_ref().is_some_and(|record| record.enabled);
    let threshold = local_threshold(existing.as_ref(), spec);

    match body.action.as_str() {
        "on" | "pick" => {
            ensure_local_record(&state, spec, true, threshold).await?;
            let status = models.ensure(spec).await.map_err(ApiError::bad_request)?;
            if status.installed {
                reload_pii_engines(&state).await?;
            } else {
                let control = state.clone();
                tokio::spawn(async move {
                    let Some(models) = control.pii.as_ref().and_then(|pii| pii.models()) else {
                        return;
                    };
                    let status = models.wait(spec).await;
                    tracing::info!(
                        model = spec.id,
                        state = ?status.state,
                        "local PII model download finished"
                    );
                    if let Err(error) = reload_pii_engines(&control).await {
                        tracing::warn!(?error, "cannot reload PII engines after download");
                    }
                });
            }
        }
        "off" => {
            models.cancel(spec);
            ensure_local_record(&state, spec, false, threshold).await?;
            pii.stop(spec.id);
            reload_pii_engines(&state).await?;
        }
        "download" => {
            models.ensure(spec).await.map_err(ApiError::bad_request)?;
        }
        "cancel" => {
            models.cancel(spec);
        }
        "remove" => {
            models.remove(spec).map_err(ApiError::bad_request)?;
            ensure_local_record(&state, spec, false, threshold).await?;
            pii.stop(spec.id);
            reload_pii_engines(&state).await?;
        }
        "score" => {
            let score = body.score.unwrap_or(0.0);
            let threshold = if score == 0.0 {
                spec.default_threshold
            } else if (spec.floor..1.0).contains(&score) {
                score
            } else {
                return Err(ApiError::bad_request(format!(
                    "score must be 0 or between {} and 1",
                    spec.floor
                ))
                .field("score"));
            };
            ensure_local_record(&state, spec, enabled, threshold).await?;
            reload_pii_engines(&state).await?;
        }
        other => {
            return Err(
                ApiError::bad_request(format!("unknown PII model action '{other}'")).field("do"),
            );
        }
    }

    Ok(extract::Json(pii_model_view(&state, spec).await?))
}

// ---------------------------------------------------------------------------
// OAuth credentials

#[derive(Debug, Deserialize)]
pub struct OAuthCompleteBody {
    pub code: String,
    pub state: String,
}

async fn oauth_provider(state: &LlmControlState, provider_id: &str) -> ApiResult<ProviderConfig> {
    let record = state
        .storage
        .llm
        .get_provider(provider_id)
        .await
        .map_err(ApiError::from_storage)?
        .ok_or_else(|| ApiError::not_found("provider", provider_id))?;
    let provider = ProviderConfig::from_record(&record)
        .map_err(|error| ApiError::bad_request(format!("{error:?}")))?;
    if provider.oauth.is_none() {
        return Err(ApiError::bad_request(
            "provider has no `oauth` configuration",
        ));
    }
    Ok(provider)
}

async fn oauth_account(
    state: &LlmControlState,
    provider_id: &str,
    account_id: &str,
) -> ApiResult<LlmProviderAccountRecord> {
    let account = state
        .storage
        .llm
        .get_account(account_id)
        .await
        .map_err(ApiError::from_storage)?
        .ok_or_else(|| ApiError::not_found("account", account_id))?;
    if account.provider_id != provider_id {
        return Err(ApiError::bad_request(
            "account does not belong to this provider",
        ));
    }
    Ok(account)
}

fn oauth_error(error: octra_llm_core::GatewayError) -> ApiError {
    ApiError::new(StatusCode::BAD_GATEWAY, error.message)
}

async fn oauth_start(
    State(state): State<LlmControlState>,
    Path((provider_id, account_id)): Path<(String, String)>,
) -> ApiResult<extract::Json<crate::credentials::FlowStart>> {
    let provider = oauth_provider(&state, &provider_id).await?;
    let account = oauth_account(&state, &provider_id, &account_id).await?;
    let flow = state
        .oauth_flows
        .begin(&provider, &account.id)
        .map_err(ApiError::bad_request)?;
    Ok(extract::Json(flow))
}

async fn oauth_complete(
    State(state): State<LlmControlState>,
    Path((provider_id, account_id)): Path<(String, String)>,
    extract::Json(body): extract::Json<OAuthCompleteBody>,
) -> ApiResult<extract::Json<Value>> {
    let provider = oauth_provider(&state, &provider_id).await?;
    let account = oauth_account(&state, &provider_id, &account_id).await?;
    let flow = state
        .oauth_flows
        .take(&body.state)
        .ok_or_else(|| ApiError::bad_request("unknown or expired OAuth state"))?;
    if flow.provider_id != provider_id || flow.account_id != account.id {
        return Err(ApiError::bad_request(
            "OAuth state does not match this account",
        ));
    }
    state
        .oauth
        .exchange_code(
            &provider,
            &account.id,
            &body.code,
            &flow.verifier,
            &flow.redirect_uri,
        )
        .await
        .map_err(oauth_error)?;
    oauth_status(State(state), Path((provider_id, account_id))).await
}

async fn oauth_refresh(
    State(state): State<LlmControlState>,
    Path((provider_id, account_id)): Path<(String, String)>,
) -> ApiResult<extract::Json<Value>> {
    let provider = oauth_provider(&state, &provider_id).await?;
    let account = oauth_account(&state, &provider_id, &account_id).await?;
    state
        .oauth
        .refresh_account(&provider, &account.id)
        .await
        .map_err(oauth_error)?;
    oauth_status(State(state), Path((provider_id, account_id))).await
}

async fn oauth_status(
    State(state): State<LlmControlState>,
    Path((provider_id, account_id)): Path<(String, String)>,
) -> ApiResult<extract::Json<Value>> {
    let account = oauth_account(&state, &provider_id, &account_id).await?;
    Ok(extract::Json(json!({
        "account_id": account.id,
        "provider_id": account.provider_id,
        "credential_kind": account.credential_kind,
        "oauth_status": account.oauth_status,
        "oauth_expires_at": account.oauth_expires_at,
    })))
}

async fn oauth_delete(
    State(state): State<LlmControlState>,
    Path((provider_id, account_id)): Path<(String, String)>,
) -> ApiResult<StatusCode> {
    let account = oauth_account(&state, &provider_id, &account_id).await?;
    state
        .storage
        .llm
        .delete_oauth_token(&account.id)
        .await
        .map_err(ApiError::from_storage)?;
    state.oauth.invalidate(&account.id);
    Ok(StatusCode::NO_CONTENT)
}
