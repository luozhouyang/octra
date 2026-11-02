//! `/admin/plugin-artifacts` and installed WASM plugin lifecycle.

use std::path::PathBuf;
use std::sync::Arc;

use axum::Json;
use axum::extract::rejection::JsonRejection;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use octra_api_plugin_types::{
    Capability, EntityQuery, PluginError, PluginManifest, PluginRuntimeKind,
};
use octra_api_plugin_wasm::WasmPluginFactory;
use octra_plugin_storage::{copy_artifact, validate_artifact_dir};
use octra_storage::{PluginArtifactInput, PluginArtifactRecord};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::ApiControlState;

use super::error::{AdminError, AdminResult};
use super::list::{
    ListQuery, ListResponse, deserialize, list_response, read_json_body, to_value,
};
use super::reload_all;

#[derive(Debug, Deserialize)]
struct UploadArtifactRequest {
    /// Local directory containing plugin.json, plugin.wasm, and migrations/.
    path: String,
    #[serde(default)]
    signature: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct InstallRequest {
    artifact_id: Option<String>,
    approve_capabilities: Vec<Capability>,
    allow_destructive: bool,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct UninstallRequest {
    purge_data: bool,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub(super) struct EntityQueryParams {
    limit: Option<u32>,
    offset: Option<u32>,
}

pub async fn list_artifacts(
    State(state): State<ApiControlState>,
    Query(query): Query<ListQuery>,
) -> AdminResult<Json<ListResponse>> {
    let artifacts = state.storage.llm.list_plugin_artifacts(None).await?;
    let items = artifacts
        .iter()
        .map(to_value)
        .collect::<AdminResult<Vec<_>>>()?;
    Ok(list_response("/admin/plugin-artifacts", items, None, &query))
}

pub async fn get_artifact(
    State(state): State<ApiControlState>,
    Path(id): Path<String>,
) -> AdminResult<Json<Value>> {
    let artifact = state
        .storage
        .llm
        .get_plugin_artifact(&id)
        .await?
        .ok_or_else(|| AdminError::not_found(format!("plugin artifact '{id}' not found")))?;
    Ok(Json(to_value(&artifact)?))
}

pub async fn upload_artifact(
    State(state): State<ApiControlState>,
    payload: Result<Json<Value>, JsonRejection>,
) -> AdminResult<(StatusCode, Json<Value>)> {
    let input: UploadArtifactRequest = deserialize(read_json_body(payload)?)?;
    let wasm = wasm(&state)?;
    let source = PathBuf::from(&input.path);
    let layout = validate_artifact_dir(&source)
        .map_err(|error| AdminError::schema(format!("invalid artifact: {error}")))?;
    let size_bytes = layout
        .files
        .iter()
        .try_fold(0u64, |total, file| {
            std::fs::metadata(source.join(file))
                .map(|metadata| total + metadata.len())
                .map_err(|error| AdminError::schema(format!("cannot stat artifact file: {error}")))
        })?;
    if size_bytes > wasm.max_artifact_bytes {
        return Err(AdminError::schema(format!(
            "artifact is {size_bytes} bytes; the limit is {}",
            wasm.max_artifact_bytes
        )));
    }

    if let Some(existing) = state
        .storage
        .llm
        .get_plugin_artifact_by_name_version(&layout.manifest.name, &layout.manifest.version)
        .await?
    {
        if existing.hash == layout.hash {
            return Ok((StatusCode::OK, Json(to_value(&existing)?)));
        }
        return Err(AdminError::schema(
            "artifact version already exists with a different hash",
        ));
    }

    let destination = wasm
        .artifact_dir
        .join(&layout.manifest.name)
        .join(&layout.manifest.version);
    copy_artifact(&source, &destination)
        .map_err(|error| AdminError::schema(format!("cannot store artifact: {error}")))?;

    let manifest_json = serde_json::to_string(&layout.manifest)
        .map_err(|error| AdminError::internal(error.to_string()))?;
    let artifact = state
        .storage
        .llm
        .create_plugin_artifact(&PluginArtifactInput {
            name: layout.manifest.name.clone(),
            version: layout.manifest.version.clone(),
            api_version: layout.manifest.api_version.clone(),
            hash: layout.hash,
            signature: input.signature,
            manifest_json,
            path: destination.to_string_lossy().to_string(),
            size_bytes: i64::try_from(size_bytes).unwrap_or(i64::MAX),
        })
        .await?;
    Ok((StatusCode::CREATED, Json(to_value(&artifact)?)))
}

pub async fn install(
    State(state): State<ApiControlState>,
    Path(name): Path<String>,
    payload: Result<Json<Value>, JsonRejection>,
) -> AdminResult<(StatusCode, Json<Value>)> {
    let input: InstallRequest = deserialize(read_json_body(payload)?)?;
    let wasm = wasm(&state)?;

    if state.storage.llm.get_plugin_by_name(&name).await?.is_some() {
        return Err(AdminError::schema(
            "plugin is already installed; use upgrade",
        ));
    }

    let (manifest, artifact, artifact_dir) =
        resolve_artifact(&state, &name, input.artifact_id.as_deref()).await?;
    ensure_capabilities(&manifest, &input.approve_capabilities)?;

    let factory = WasmPluginFactory::from_artifact(&wasm.engine, &artifact_dir)
        .map_err(plugin_error)?;
    let store = wasm
        .storage_root
        .open(&name, manifest.entities.clone())
        .map_err(storage_error)?;
    let report = store
        .apply_migrations(&artifact_dir, &manifest.migrations, input.allow_destructive)
        .map_err(storage_error)?;

    let storage_path = wasm
        .storage_root
        .plugin_dir(&name)
        .map_err(storage_error)?
        .to_string_lossy()
        .to_string();
    let record = state
        .storage
        .llm
        .upsert_installed_plugin(
            &manifest,
            &artifact,
            &storage_path,
            &input.approve_capabilities,
        )
        .await?;

    wasm.catalog.insert(Arc::new(factory));
    wasm.host_provider
        .install(manifest, input.approve_capabilities);
    reload_all(&state).await?;

    Ok((
        StatusCode::CREATED,
        Json(json!({ "plugin": to_value(&record)?, "migrations": report })),
    ))
}

pub async fn upgrade(
    State(state): State<ApiControlState>,
    Path(name): Path<String>,
    payload: Result<Json<Value>, JsonRejection>,
) -> AdminResult<Json<Value>> {
    let input: InstallRequest = deserialize(read_json_body(payload)?)?;
    let wasm = wasm(&state)?;

    let existing = state
        .storage
        .llm
        .get_plugin_by_name(&name)
        .await?
        .ok_or_else(|| AdminError::not_found(format!("plugin '{name}' is not installed")))?;
    if existing.source != "wasm" {
        return Err(AdminError::schema("only wasm plugins can be upgraded"));
    }

    let (manifest, artifact, artifact_dir) =
        resolve_artifact(&state, &name, input.artifact_id.as_deref()).await?;
    ensure_capabilities(&manifest, &input.approve_capabilities)?;

    let factory = WasmPluginFactory::from_artifact(&wasm.engine, &artifact_dir)
        .map_err(plugin_error)?;
    let store = wasm
        .storage_root
        .open(&name, manifest.entities.clone())
        .map_err(storage_error)?;
    let report = store
        .apply_migrations(&artifact_dir, &manifest.migrations, input.allow_destructive)
        .map_err(storage_error)?;

    let storage_path = wasm
        .storage_root
        .plugin_dir(&name)
        .map_err(storage_error)?
        .to_string_lossy()
        .to_string();
    let record = state
        .storage
        .llm
        .upsert_installed_plugin(
            &manifest,
            &artifact,
            &storage_path,
            &input.approve_capabilities,
        )
        .await?;

    wasm.catalog.insert(Arc::new(factory));
    wasm.host_provider
        .install(manifest, input.approve_capabilities);
    reload_all(&state).await?;

    Ok(Json(
        json!({ "plugin": to_value(&record)?, "migrations": report }),
    ))
}

pub async fn uninstall(
    State(state): State<ApiControlState>,
    Path(name): Path<String>,
    payload: Result<Json<Value>, JsonRejection>,
) -> AdminResult<Json<Value>> {
    let input: UninstallRequest = deserialize(read_json_body(payload)?)?;
    let wasm = wasm(&state)?;
    let record = state
        .storage
        .llm
        .get_plugin_by_name(&name)
        .await?
        .ok_or_else(|| AdminError::not_found(format!("plugin '{name}' is not installed")))?;
    if record.source != "wasm" {
        return Err(AdminError::schema("only wasm plugins can be uninstalled"));
    }

    wasm.catalog.remove(&name);
    wasm.host_provider
        .uninstall(&name, input.purge_data)
        .map_err(storage_error)?;
    state.storage.llm.delete_installed_plugin(&name).await?;
    reload_all(&state).await?;

    Ok(Json(json!({
        "name": name,
        "uninstalled": true,
        "purged": input.purge_data,
    })))
}

pub async fn migrations(
    State(state): State<ApiControlState>,
    Path(name): Path<String>,
) -> AdminResult<Json<Value>> {
    let wasm = wasm(&state)?;
    let manifest = installed_manifest(&state, &name).await?;
    let store = wasm
        .storage_root
        .open(&name, manifest.entities)
        .map_err(storage_error)?;
    let ledger = store.ledger().map_err(storage_error)?;
    Ok(Json(json!({ "migrations": ledger })))
}

pub async fn entity_rows(
    State(state): State<ApiControlState>,
    Path((name, entity)): Path<(String, String)>,
    Query(query): Query<EntityQueryParams>,
) -> AdminResult<Json<Value>> {
    let wasm = wasm(&state)?;
    let manifest = installed_manifest(&state, &name).await?;
    let store = wasm
        .storage_root
        .open(&name, manifest.entities)
        .map_err(storage_error)?;
    let rows = octra_api_plugin_types::PluginStorage::query(
        &store,
        &entity,
        &EntityQuery {
            filters: Vec::new(),
            order_by: Vec::new(),
            limit: query.limit,
            offset: query.offset,
        },
    )
    .map_err(storage_error)?;
    Ok(Json(json!({ "data": rows })))
}

fn wasm(state: &ApiControlState) -> AdminResult<&Arc<crate::WasmControlState>> {
    state
        .wasm
        .as_ref()
        .ok_or_else(|| AdminError::schema("the wasm plugin runtime is disabled"))
}

async fn installed_manifest(
    state: &ApiControlState,
    name: &str,
) -> AdminResult<PluginManifest> {
    let record = state
        .storage
        .llm
        .get_plugin_by_name(name)
        .await?
        .ok_or_else(|| AdminError::not_found(format!("plugin '{name}' is not installed")))?;
    serde_json::from_str(&record.manifest_json)
        .map_err(|error| AdminError::internal(format!("invalid installed manifest: {error}")))
}

async fn resolve_artifact(
    state: &ApiControlState,
    name: &str,
    artifact_id: Option<&str>,
) -> AdminResult<(PluginManifest, PluginArtifactRecord, PathBuf)> {
    let artifact_id = artifact_id
        .ok_or_else(|| AdminError::schema("artifact_id is required"))?;
    let artifact = state
        .storage
        .llm
        .get_plugin_artifact(artifact_id)
        .await?
        .ok_or_else(|| {
            AdminError::not_found(format!("plugin artifact '{artifact_id}' not found"))
        })?;
    let directory = PathBuf::from(&artifact.path);
    let layout = validate_artifact_dir(&directory)
        .map_err(|error| AdminError::schema(format!("invalid artifact: {error}")))?;
    if layout.hash != artifact.hash {
        return Err(AdminError::schema(
            "artifact files do not match the recorded hash",
        ));
    }
    if layout.manifest.name != name
        || layout.manifest.name != artifact.name
        || layout.manifest.version != artifact.version
    {
        return Err(AdminError::schema(
            "artifact manifest does not match the requested plugin",
        ));
    }
    if layout.manifest.runtime != PluginRuntimeKind::Wasm {
        return Err(AdminError::schema("artifact is not a wasm plugin"));
    }
    Ok((layout.manifest, artifact, directory))
}

fn ensure_capabilities(
    manifest: &PluginManifest,
    approved: &[Capability],
) -> AdminResult<()> {
    for capability in &manifest.capabilities {
        if matches!(
            capability,
            Capability::Network
                | Capability::Secret
                | Capability::RequestBody
                | Capability::ResponseBody
        ) {
            return Err(AdminError::schema(format!(
                "capability {capability:?} is not supported by the wasm runtime"
            )));
        }
    }
    let requested = &manifest.capabilities;
    if requested.len() != approved.len()
        || !requested.iter().all(|capability| approved.contains(capability))
    {
        return Err(AdminError::schema(
            "approve_capabilities must match the manifest capabilities",
        ));
    }
    Ok(())
}

fn storage_error(error: impl std::fmt::Display) -> AdminError {
    AdminError::schema(format!("plugin storage: {error}"))
}

fn plugin_error(error: PluginError) -> AdminError {
    AdminError::schema(error.message)
}
