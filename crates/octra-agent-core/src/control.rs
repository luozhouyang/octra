//! Control plane for local agent discovery and configuration.
//!
//! Endpoints live under `/api/v1/agents` and share the daemon's bearer-token
//! authentication. Applying a configuration always records a backup first and
//! then atomically replaces the file.

use std::path::PathBuf;
use std::sync::Arc;

use axum::Json;
use axum::Router;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post, put};
use octra_config::OctraConfig;
use octra_storage::Storage;
use octra_storage::agents::{AgentBackup, AgentProfile, NewAgentBackup, NewAgentProfile};
use serde::{Deserialize, Serialize};
use tracing::warn;
use url::Url;

use crate::adapter::{
    AgentAdapter, AgentContext, ApplyProfile, ConfigChange, ConfigView, Detection, mask_secret,
};
use crate::detect::detect;
use crate::error::AgentError;
use crate::fs::{read_optional, remove_file, sha256_hex, write_atomic};
use crate::{AgentRegistry, AgentResult};

#[derive(Clone)]
pub struct AgentControlState {
    storage: Arc<Storage>,
    config: Arc<OctraConfig>,
    registry: AgentRegistry,
    context: AgentContext,
    apply_lock: Arc<tokio::sync::Mutex<()>>,
}

impl AgentControlState {
    pub fn new(storage: Arc<Storage>, config: Arc<OctraConfig>) -> Self {
        Self {
            storage,
            config,
            registry: AgentRegistry::builtin(),
            context: AgentContext::from_env(),
            apply_lock: Arc::new(tokio::sync::Mutex::new(())),
        }
    }

    /// Builds a state rooted at a synthetic home directory. Used by tests and
    /// by an embedding host that wants to control the filesystem view.
    pub fn with_context(
        storage: Arc<Storage>,
        config: Arc<OctraConfig>,
        context: AgentContext,
    ) -> Self {
        Self {
            storage,
            config,
            registry: AgentRegistry::builtin(),
            context,
            apply_lock: Arc::new(tokio::sync::Mutex::new(())),
        }
    }

    fn adapter(&self, id: &str) -> AgentResult<Arc<dyn AgentAdapter>> {
        self.registry
            .get(id)
            .ok_or_else(|| AgentError::UnknownAgent(id.to_string()))
    }

    fn gateway_base_url(&self) -> String {
        local_base_url(&self.config.gateway.listen_bind)
    }
}

pub fn control_router(state: AgentControlState) -> Router {
    Router::new()
        .route("/api/v1/agents", get(list_agents))
        .route("/api/v1/agents/{id}", get(get_agent))
        .route("/api/v1/agents/{id}/config", get(get_config))
        .route(
            "/api/v1/agents/{id}/profiles",
            get(list_profiles).post(create_profile),
        )
        .route(
            "/api/v1/agents/{id}/profiles/{profile_id}",
            put(update_profile).delete(delete_profile),
        )
        .route("/api/v1/agents/{id}/preview", post(preview))
        .route("/api/v1/agents/{id}/apply", post(apply))
        .route("/api/v1/agents/{id}/restore", post(restore))
        .route("/api/v1/agents/{id}/backups", get(list_backups))
        .with_state(state)
}

// ----- views -------------------------------------------------------------

#[derive(Debug, Serialize)]
pub struct AgentView {
    pub id: String,
    pub name: String,
    pub vendor: String,
    pub detection: Detection,
    pub config: ConfigView,
    pub configured: bool,
    pub profile: Option<AgentProfile>,
    pub last_backup: Option<AgentBackup>,
    pub warnings: Vec<String>,
}

#[derive(Debug, Serialize)]
pub struct PreviewView {
    pub agent_id: String,
    pub path: String,
    pub exists: bool,
    pub current_sha256: Option<String>,
    pub changes: Vec<ConfigChange>,
    pub warnings: Vec<String>,
}

#[derive(Debug, Serialize)]
pub struct AppliedView {
    pub agent_id: String,
    pub path: String,
    pub backup_id: String,
    pub applied_at: String,
    pub changes: Vec<ConfigChange>,
    pub warnings: Vec<String>,
}

#[derive(Debug, Serialize)]
pub struct RestoredView {
    pub agent_id: String,
    pub path: String,
    pub backup_id: String,
    pub restored_at: String,
    pub removed: bool,
}

#[derive(Debug, Serialize)]
pub struct ConfigResponse {
    #[serde(flatten)]
    pub config: ConfigView,
    pub agent_id: String,
}

// ----- request bodies ----------------------------------------------------

#[derive(Debug, Deserialize)]
pub struct ProfileBody {
    pub name: String,
    pub base_url: Option<String>,
    pub virtual_model_id: String,
    pub virtual_key_id: Option<String>,
    pub config_path: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
pub struct RenderBody {
    pub profile_id: Option<String>,
    pub base_url: Option<String>,
    pub virtual_model_id: Option<String>,
    pub virtual_key: Option<String>,
    /// Overrides the adapter's default config path. Absolute paths only.
    pub config_path: Option<String>,
    /// Hash returned by a previous preview; applying a changed file requires
    /// `force` when it no longer matches.
    pub expected_sha256: Option<String>,
    #[serde(default)]
    pub force: bool,
}

#[derive(Debug, Default, Deserialize)]
pub struct RestoreBody {
    pub backup_id: Option<String>,
    #[serde(default)]
    pub force: bool,
}

// ----- handlers ----------------------------------------------------------

async fn list_agents(State(state): State<AgentControlState>) -> ApiResult<Json<Vec<AgentView>>> {
    let mut agents = Vec::new();
    for adapter in state.registry.all() {
        agents.push(build_view(&state, adapter.as_ref()).await?);
    }
    Ok(Json(agents))
}

async fn get_agent(
    State(state): State<AgentControlState>,
    Path(id): Path<String>,
) -> ApiResult<Json<AgentView>> {
    let adapter = state.adapter(&id)?;
    Ok(Json(build_view(&state, adapter.as_ref()).await?))
}

async fn get_config(
    State(state): State<AgentControlState>,
    Path(id): Path<String>,
) -> ApiResult<Json<ConfigResponse>> {
    let adapter = state.adapter(&id)?;
    let profile = state.storage.agents.active_profile(&id).await?;
    let path = target_path(adapter.as_ref(), &state.context, profile.as_ref());
    let config = read_config(adapter.as_ref(), &path)?;
    Ok(Json(ConfigResponse {
        config,
        agent_id: id,
    }))
}

async fn list_profiles(
    State(state): State<AgentControlState>,
    Path(id): Path<String>,
) -> ApiResult<Json<Vec<AgentProfile>>> {
    let _ = state.adapter(&id)?;
    Ok(Json(state.storage.agents.list_profiles(Some(&id)).await?))
}

async fn create_profile(
    State(state): State<AgentControlState>,
    Path(id): Path<String>,
    Json(body): Json<ProfileBody>,
) -> ApiResult<(StatusCode, Json<AgentProfile>)> {
    let _ = state.adapter(&id)?;
    let input = profile_input(&state, &id, body)?;
    let profile = state.storage.agents.create_profile(&input).await?;
    Ok((StatusCode::CREATED, Json(profile)))
}

async fn update_profile(
    State(state): State<AgentControlState>,
    Path((id, profile_id)): Path<(String, String)>,
    Json(body): Json<ProfileBody>,
) -> ApiResult<Json<AgentProfile>> {
    let _ = state.adapter(&id)?;
    let existing = state
        .storage
        .agents
        .get_profile(&profile_id)
        .await?
        .ok_or_else(|| ApiError::not_found("profile", &profile_id))?;
    if existing.agent_id != id {
        return Err(ApiError::not_found("profile", &profile_id));
    }
    let input = profile_input(&state, &id, body)?;
    state
        .storage
        .agents
        .update_profile(&profile_id, &input)
        .await?
        .map(Json)
        .ok_or_else(|| ApiError::not_found("profile", &profile_id))
}

async fn delete_profile(
    State(state): State<AgentControlState>,
    Path((id, profile_id)): Path<(String, String)>,
) -> ApiResult<StatusCode> {
    let _ = state.adapter(&id)?;
    let existing = state
        .storage
        .agents
        .get_profile(&profile_id)
        .await?
        .ok_or_else(|| ApiError::not_found("profile", &profile_id))?;
    if existing.agent_id != id {
        return Err(ApiError::not_found("profile", &profile_id));
    }
    state.storage.agents.delete_profile(&profile_id).await?;
    Ok(StatusCode::NO_CONTENT)
}

async fn list_backups(
    State(state): State<AgentControlState>,
    Path(id): Path<String>,
) -> ApiResult<Json<Vec<AgentBackup>>> {
    let _ = state.adapter(&id)?;
    Ok(Json(state.storage.agents.list_backups(&id, 50).await?))
}

async fn preview(
    State(state): State<AgentControlState>,
    Path(id): Path<String>,
    Json(body): Json<RenderBody>,
) -> ApiResult<Json<PreviewView>> {
    let adapter = state.adapter(&id)?;
    let rendered = render(&state, adapter.as_ref(), &id, &body, false).await?;
    Ok(Json(PreviewView {
        agent_id: id,
        path: rendered.path,
        exists: rendered.exists,
        current_sha256: rendered.current_sha256,
        changes: rendered.rendered.changes,
        warnings: rendered.rendered.warnings,
    }))
}

async fn apply(
    State(state): State<AgentControlState>,
    Path(id): Path<String>,
    Json(body): Json<RenderBody>,
) -> ApiResult<Json<AppliedView>> {
    let adapter = state.adapter(&id)?;
    let _guard = state.apply_lock.lock().await;

    let rendered = render(&state, adapter.as_ref(), &id, &body, true).await?;
    if let Some(expected) = body.expected_sha256.as_deref() {
        let current = rendered.current_sha256.as_deref().unwrap_or_default();
        if current != expected && !body.force {
            return Err(ApiError::conflict(
                "the configuration changed since the preview; re-preview or force",
            ));
        }
    }

    let backup = state
        .storage
        .agents
        .insert_backup(&NewAgentBackup {
            agent_id: id.clone(),
            profile_id: rendered.profile_id.clone(),
            path: rendered.path.clone(),
            original_content: rendered.current.clone(),
            original_sha256: rendered
                .current_sha256
                .clone()
                .unwrap_or_else(|| sha256_hex("")),
            applied_sha256: sha256_hex(&rendered.rendered.content),
            file_existed: rendered.exists,
        })
        .await?;

    write_atomic(
        PathBuf::from(&rendered.path).as_path(),
        &rendered.rendered.content,
    )?;

    Ok(Json(AppliedView {
        agent_id: id,
        path: rendered.path,
        backup_id: backup.id,
        applied_at: backup.created_at,
        changes: rendered.rendered.changes,
        warnings: rendered.rendered.warnings,
    }))
}

async fn restore(
    State(state): State<AgentControlState>,
    Path(id): Path<String>,
    Json(body): Json<RestoreBody>,
) -> ApiResult<Json<RestoredView>> {
    let _adapter = state.adapter(&id)?;
    let _guard = state.apply_lock.lock().await;

    let backup = match body.backup_id.as_deref() {
        Some(backup_id) => state
            .storage
            .agents
            .get_backup(backup_id)
            .await?
            .ok_or_else(|| ApiError::not_found("backup", backup_id))?,
        None => state
            .storage
            .agents
            .latest_backup(&id)
            .await?
            .ok_or_else(|| ApiError::not_found("backup", &id))?,
    };
    if backup.agent_id != id {
        return Err(ApiError::not_found("backup", &backup.id));
    }

    let path = PathBuf::from(&backup.path);
    let current = read_optional(&path)?;
    let current_sha = current.as_deref().map(sha256_hex);
    if current_sha.as_deref() != Some(backup.applied_sha256.as_str()) && !body.force {
        return Err(ApiError::conflict(
            "the file changed after this apply; restore with force to overwrite it",
        ));
    }

    let removed = if backup.file_existed {
        let original = backup
            .original_content
            .as_deref()
            .ok_or_else(|| ApiError::conflict("this backup has no original content to restore"))?;
        write_atomic(&path, original)?;
        false
    } else {
        remove_file(&path)?;
        true
    };

    state.storage.agents.mark_restored(&backup.id).await?;
    let restored_at = state
        .storage
        .agents
        .get_backup(&backup.id)
        .await?
        .and_then(|backup| backup.restored_at)
        .unwrap_or_default();

    Ok(Json(RestoredView {
        agent_id: id,
        path: backup.path,
        backup_id: backup.id,
        restored_at,
        removed,
    }))
}

// ----- internals ---------------------------------------------------------

struct RenderedFile {
    path: String,
    exists: bool,
    current: Option<String>,
    current_sha256: Option<String>,
    profile_id: Option<String>,
    rendered: crate::adapter::RenderedConfig,
}

async fn render(
    state: &AgentControlState,
    adapter: &dyn AgentAdapter,
    agent_id: &str,
    body: &RenderBody,
    require_key: bool,
) -> AgentResult<RenderedFile> {
    let (base_url, virtual_model_id, profile_id, config_path) = match body.profile_id.as_deref() {
        Some(profile_id) => {
            let profile = state
                .storage
                .agents
                .get_profile(profile_id)
                .await?
                .ok_or_else(|| AgentError::MissingProfile(profile_id.to_string()))?;
            if profile.agent_id != agent_id {
                return Err(AgentError::MissingProfile(profile_id.to_string()));
            }
            (
                profile.base_url,
                profile.virtual_model_id,
                Some(profile.id),
                profile.config_path,
            )
        }
        None => (
            body.base_url
                .clone()
                .unwrap_or_else(|| state.gateway_base_url()),
            body.virtual_model_id
                .clone()
                .ok_or_else(|| AgentError::InvalidConfig("virtual_model_id is required".into()))?,
            None,
            body.config_path.clone(),
        ),
    };

    validate_base_url(&base_url)?;
    if let Some(path) = config_path.as_deref()
        && !PathBuf::from(path).is_absolute()
    {
        return Err(AgentError::InvalidConfig(
            "config_path must be absolute".into(),
        ));
    }
    let virtual_key = match body.virtual_key.as_deref() {
        Some(key) if !key.trim().is_empty() => key.trim().to_string(),
        _ if require_key => {
            return Err(AgentError::InvalidConfig(
                "virtual_key is required to apply a configuration".into(),
            ));
        }
        _ => "octra-preview-placeholder".to_string(),
    };

    let path = config_path
        .map(PathBuf::from)
        .unwrap_or_else(|| adapter.config_path(&state.context));
    let current = read_optional(&path)?;
    let expected_base_url = base_url.clone();
    let rendered = adapter.render(
        current.as_deref(),
        &path,
        &ApplyProfile {
            base_url,
            virtual_model_id,
            virtual_key,
        },
    )?;

    // Re-parse the proposed document to catch rendering bugs before writing.
    let proposed_view = adapter.read(&rendered.content, &path)?;
    let mut warnings = rendered.warnings;
    warnings.extend(adapter.health(&proposed_view, &expected_base_url));
    warnings.extend(proposed_view.warnings);

    Ok(RenderedFile {
        path: path.display().to_string(),
        exists: current.is_some(),
        current_sha256: current.as_deref().map(sha256_hex),
        current,
        profile_id,
        rendered: crate::adapter::RenderedConfig {
            content: rendered.content,
            changes: rendered.changes,
            warnings,
        },
    })
}

async fn build_view(
    state: &AgentControlState,
    adapter: &dyn AgentAdapter,
) -> AgentResult<AgentView> {
    let detection = detect(adapter, &state.context);
    let profile = state.storage.agents.active_profile(adapter.id()).await?;
    let path = target_path(adapter, &state.context, profile.as_ref());
    let config = match read_optional(&path) {
        Ok(Some(content)) => match adapter.read(&content, &path) {
            Ok(view) => view,
            Err(error) => ConfigView {
                warnings: vec![error.to_string()],
                ..ConfigView::missing(&path)
            },
        },
        Ok(None) => ConfigView::missing(&path),
        Err(error) => ConfigView {
            warnings: vec![error.to_string()],
            ..ConfigView::missing(&path)
        },
    };

    let expected = profile
        .as_ref()
        .map(|profile| profile.base_url.clone())
        .unwrap_or_else(|| state.gateway_base_url());
    let mut warnings = config.warnings.clone();
    warnings.extend(adapter.health(&config, &expected));
    let configured = config
        .base_url
        .as_deref()
        .is_some_and(|base| adapter.matches_base_url(base, &expected));

    let last_backup = state
        .storage
        .agents
        .list_backups(adapter.id(), 1)
        .await?
        .into_iter()
        .next();

    Ok(AgentView {
        id: adapter.id().to_string(),
        name: adapter.name().to_string(),
        vendor: adapter.vendor().to_string(),
        detection,
        config,
        configured,
        profile,
        last_backup,
        warnings,
    })
}

fn read_config(adapter: &dyn AgentAdapter, path: &std::path::Path) -> AgentResult<ConfigView> {
    match read_optional(path)? {
        Some(content) => adapter.read(&content, path),
        None => Ok(ConfigView::missing(path)),
    }
}

fn target_path(
    adapter: &dyn AgentAdapter,
    ctx: &AgentContext,
    profile: Option<&AgentProfile>,
) -> PathBuf {
    profile
        .and_then(|profile| profile.config_path.as_deref())
        .map(PathBuf::from)
        .unwrap_or_else(|| adapter.config_path(ctx))
}

fn profile_input(
    state: &AgentControlState,
    agent_id: &str,
    body: ProfileBody,
) -> AgentResult<NewAgentProfile> {
    let name = body.name.trim();
    if name.is_empty() {
        return Err(AgentError::InvalidConfig("profile name is required".into()));
    }
    if body.virtual_model_id.trim().is_empty() {
        return Err(AgentError::InvalidConfig(
            "virtual_model_id is required".into(),
        ));
    }
    let base_url = body
        .base_url
        .filter(|value| !value.trim().is_empty())
        .unwrap_or_else(|| state.gateway_base_url());
    validate_base_url(&base_url)?;
    if let Some(path) = body.config_path.as_deref() {
        let path = PathBuf::from(path);
        if !path.is_absolute() {
            return Err(AgentError::InvalidConfig(
                "config_path must be absolute".into(),
            ));
        }
    }

    Ok(NewAgentProfile {
        agent_id: agent_id.to_string(),
        name: name.to_string(),
        base_url: base_url.trim_end_matches('/').to_string(),
        virtual_model_id: body.virtual_model_id.trim().to_string(),
        virtual_key_id: body.virtual_key_id,
        config_path: body.config_path,
    })
}

fn validate_base_url(value: &str) -> AgentResult<()> {
    let url = Url::parse(value)
        .map_err(|error| AgentError::InvalidConfig(format!("invalid base URL: {error}")))?;
    if !matches!(url.scheme(), "http" | "https") {
        return Err(AgentError::InvalidConfig(
            "base URL must use http or https".into(),
        ));
    }
    if url.host_str().is_none() {
        return Err(AgentError::InvalidConfig(
            "base URL must include a host".into(),
        ));
    }
    Ok(())
}

/// Turns a listener bind address into a URL a local agent can reach.
fn local_base_url(bind: &str) -> String {
    let bind = bind.trim();
    if let Some(port) = bind.strip_prefix("0.0.0.0:") {
        return format!("http://127.0.0.1:{port}");
    }
    if let Some(port) = bind.strip_prefix("[::]:") {
        return format!("http://127.0.0.1:{port}");
    }
    format!("http://{}", bind.trim_end_matches('/'))
}

// ----- errors ------------------------------------------------------------

pub struct ApiError {
    status: StatusCode,
    message: String,
}

impl ApiError {
    fn not_found(what: &str, id: &str) -> Self {
        Self {
            status: StatusCode::NOT_FOUND,
            message: format!("{what} '{id}' not found"),
        }
    }

    fn conflict(message: impl Into<String>) -> Self {
        Self {
            status: StatusCode::CONFLICT,
            message: message.into(),
        }
    }
}

impl From<octra_core::Error> for ApiError {
    fn from(error: octra_core::Error) -> Self {
        warn!(%error, "agent storage operation failed");
        Self {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            message: "agent storage operation failed".to_string(),
        }
    }
}

impl From<AgentError> for ApiError {
    fn from(error: AgentError) -> Self {
        match error {
            AgentError::UnknownAgent(id) => Self::not_found("agent", &id),
            AgentError::MissingProfile(id) => Self::not_found("profile", &id),
            AgentError::InvalidConfig(message) => Self {
                status: StatusCode::BAD_REQUEST,
                message,
            },
            AgentError::Conflict(message) => Self::conflict(message),
            AgentError::Io(error) => {
                warn!(%error, "agent file operation failed");
                Self {
                    status: StatusCode::INTERNAL_SERVER_ERROR,
                    message: "agent file operation failed".to_string(),
                }
            }
            AgentError::Storage(error) => error.into(),
        }
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (
            self.status,
            Json(serde_json::json!({ "error": self.message })),
        )
            .into_response()
    }
}

type ApiResult<T> = Result<T, ApiError>;

/// Mask helper is re-exported for the desktop preview badge.
pub fn masked_key(value: &str) -> String {
    mask_secret(value)
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use octra_config::OctraConfig;
    use octra_storage::Storage;
    use tempfile::tempdir;

    use super::{AgentControlState, RenderBody, build_view, render};
    use crate::AgentRegistry;
    use crate::adapter::AgentContext;

    #[tokio::test]
    async fn preview_and_apply_round_trip_with_a_backup() {
        let home = tempdir().unwrap();
        let storage = Arc::new(Storage::connect_in_memory().await.unwrap());
        let state = AgentControlState::with_context(
            storage.clone(),
            Arc::new(OctraConfig::default()),
            AgentContext::for_home(home.path()),
        );
        let adapter = AgentRegistry::builtin().get("claude-code").unwrap();
        let body = RenderBody {
            base_url: Some("http://127.0.0.1:8080".to_string()),
            virtual_model_id: Some("octra-default".to_string()),
            virtual_key: Some("octra-abcdefghijklmnop".to_string()),
            ..RenderBody::default()
        };

        let rendered = render(&state, adapter.as_ref(), "claude-code", &body, true)
            .await
            .unwrap();
        assert!(rendered.rendered.content.contains("ANTHROPIC_BASE_URL"));

        let view = build_view(&state, adapter.as_ref()).await.unwrap();
        assert!(!view.config.exists);
        assert!(!view.configured);

        let profile = storage
            .agents
            .create_profile(&octra_storage::NewAgentProfile {
                agent_id: "claude-code".to_string(),
                name: "default".to_string(),
                base_url: "http://127.0.0.1:8080".to_string(),
                virtual_model_id: "octra-default".to_string(),
                virtual_key_id: None,
                config_path: None,
            })
            .await
            .unwrap();

        let view = build_view(&state, adapter.as_ref()).await.unwrap();
        assert_eq!(view.profile.unwrap().id, profile.id);
    }

    #[test]
    fn local_base_url_maps_wildcard_binds_to_loopback() {
        assert_eq!(
            super::local_base_url("0.0.0.0:8080"),
            "http://127.0.0.1:8080"
        );
        assert_eq!(super::local_base_url("[::]:8080"), "http://127.0.0.1:8080");
        assert_eq!(
            super::local_base_url("127.0.0.1:8080"),
            "http://127.0.0.1:8080"
        );
    }
}
