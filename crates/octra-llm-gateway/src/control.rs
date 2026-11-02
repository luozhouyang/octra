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
    LlmModelInput, LlmModelRecord, LlmProviderAccountInput, LlmProviderAccountRecord,
    LlmProviderInput, LlmProviderRecord, LlmRouteRecord, LlmRoutingGroupRecord,
    LlmRoutingTargetInput, LlmRoutingTargetRecord, LlmStorageError, LlmUsageRecord,
    LlmVirtualKeyInput, LlmVirtualKeyRecord,
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::keys;
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
    Router::new()
        .merge(providers)
        .merge(models)
        .merge(groups)
        .merge(routes)
        .merge(keys)
        .merge(plugins)
        .route("/admin/llm/reload", post(reload))
        .route("/admin/llm/usage", get(usage))
        .with_state(state)
}

// ---------------------------------------------------------------------------
// Errors

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
        _ => body.permissions()?,
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
    Ok(extract::Json(PluginDetailView {
        plugin: record.into(),
        configs: configs.into_iter().map(PluginConfigView::from).collect(),
    }))
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
    Ok(extract::Json(PluginDetailView {
        plugin: record.into(),
        configs: configs.into_iter().map(PluginConfigView::from).collect(),
    }))
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
    pub recent: Vec<UsageRecordView>,
}

#[derive(Debug, Serialize)]
pub struct UsageTotals {
    pub requests: i64,
    pub errors: i64,
    pub input_tokens: i64,
    pub output_tokens: i64,
}

#[derive(Debug, Serialize)]
pub struct UsageModelRow {
    pub virtual_model_id: String,
    pub requests: i64,
    pub errors: i64,
    pub input_tokens: i64,
    pub output_tokens: i64,
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
        },
        by_model: by_model
            .into_iter()
            .map(|row| UsageModelRow {
                virtual_model_id: row.virtual_model_id,
                requests: row.requests,
                errors: row.errors,
                input_tokens: row.input_tokens,
                output_tokens: row.output_tokens,
            })
            .collect(),
        recent: recent.into_iter().map(UsageRecordView::from).collect(),
    }))
}
