//! `/admin/plugin-artifacts` and installed WASM plugin lifecycle.

use std::path::PathBuf;
use std::sync::Arc;

use axum::Json;
use axum::extract::rejection::JsonRejection;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use base64::Engine;
use base64::engine::general_purpose::STANDARD as BASE64;
use ed25519_dalek::{Signature, VerifyingKey};
use octra_api_plugin_types::{
    Capability, EntityQuery, PluginError, PluginKind, PluginManifest, PluginRuntimeKind,
};
use octra_api_plugin_wasm::{InstalledWasmPlugin, PluginStatsSnapshot, WasmPluginFactory};
use octra_plugin_storage::{copy_artifact_for_kind, validate_artifact_dir_any};
use octra_storage::{PluginArtifactInput, PluginArtifactRecord};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::{ApiControlState, LlmWasmLifecycle, WasmControlState};

use super::error::{AdminError, AdminResult};
use super::list::{ListQuery, ListResponse, deserialize, list_response, read_json_body, to_value};
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
    Ok(list_response(
        "/admin/plugin-artifacts",
        items,
        None,
        &query,
    ))
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
    let layout = validate_artifact_dir_any(&source)
        .map_err(|error| AdminError::schema(format!("invalid artifact: {error}")))?;
    let size_bytes = layout.files.iter().try_fold(0u64, |total, file| {
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

    verify_signature(
        &wasm.trusted_keys,
        wasm.allow_unsigned,
        &layout.hash,
        input.signature.as_deref(),
    )?;

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
    copy_artifact_for_kind(&source, &destination, layout.manifest.kind)
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

    if manifest.kind == PluginKind::Llm {
        let lifecycle = llm_lifecycle(&state)?;
        let record = state
            .storage
            .llm
            .upsert_installed_plugin(&manifest, &artifact, "", &input.approve_capabilities)
            .await?;
        if let Err(error) = lifecycle.install(manifest, artifact_dir, None).await {
            // The lifecycle hook has already restored the runtime factory.
            let _ = state.storage.llm.delete_installed_plugin(&name).await;
            return Err(AdminError::schema(format!(
                "cannot activate LLM plugin: {error}"
            )));
        }
        return Ok((
            StatusCode::CREATED,
            Json(json!({ "plugin": to_value(&record)?, "migrations": [] })),
        ));
    }

    let factory =
        WasmPluginFactory::from_artifact(&wasm.engine, &artifact_dir).map_err(plugin_error)?;
    let component_hash = artifact.hash.clone();
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

    // Activate the runtime before writing the catalog row. The row is what
    // marks the plugin as installed, so a failed reload must not leave one
    // behind for a plugin the runtime never accepted.
    let previous = ApiPluginRuntime::capture(wasm, &name);
    wasm.catalog.insert(Arc::new(factory));
    wasm.host_provider
        .install(manifest.clone(), input.approve_capabilities.clone());
    if let Err(error) = reload_all(&state).await {
        previous.restore(wasm, &name);
        if let Err(rollback) = reload_all(&state).await {
            tracing::warn!(
                ?rollback,
                plugin = %name,
                "cannot restore API plugin chains after a failed install"
            );
        }
        wasm.engine.remove_component(&component_hash);
        return Err(error);
    }

    let record = match state
        .storage
        .llm
        .upsert_installed_plugin(
            &manifest,
            &artifact,
            &storage_path,
            &input.approve_capabilities,
        )
        .await
    {
        Ok(record) => record,
        Err(error) => {
            previous.restore(wasm, &name);
            if let Err(rollback) = reload_all(&state).await {
                tracing::warn!(
                    ?rollback,
                    plugin = %name,
                    "cannot restore API plugin chains after a failed install"
                );
            }
            wasm.engine.remove_component(&component_hash);
            return Err(error.into());
        }
    };

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

    if manifest.kind == PluginKind::Llm {
        let lifecycle = llm_lifecycle(&state)?;
        let old_manifest: PluginManifest = serde_json::from_str(&existing.manifest_json)
            .map_err(|error| AdminError::internal(format!("invalid old manifest: {error}")))?;
        let old_artifact = match existing.artifact_id.as_deref() {
            Some(id) => state.storage.llm.get_plugin_artifact(id).await?,
            None => None,
        };
        let old_storage_path = existing.storage_path.clone().unwrap_or_default();
        let old_capabilities = existing.approved_capabilities.clone();
        let record = state
            .storage
            .llm
            .upsert_installed_plugin(&manifest, &artifact, "", &input.approve_capabilities)
            .await?;
        if let Err(error) = lifecycle
            .install(
                manifest.clone(),
                artifact_dir,
                existing.artifact_hash.as_deref(),
            )
            .await
        {
            // Restore the old catalog row; the lifecycle hook has already
            // restored the old runtime factory. Without an artifact record
            // the old row cannot be rebuilt, so remove the new one instead.
            match old_artifact {
                Some(old_artifact) => {
                    let _ = state
                        .storage
                        .llm
                        .upsert_installed_plugin(
                            &old_manifest,
                            &old_artifact,
                            &old_storage_path,
                            &old_capabilities,
                        )
                        .await;
                }
                None => {
                    let _ = state.storage.llm.delete_installed_plugin(&name).await;
                }
            }
            return Err(AdminError::schema(format!(
                "cannot activate LLM plugin: {error}"
            )));
        }
        return Ok(Json(
            json!({ "plugin": to_value(&record)?, "migrations": [] }),
        ));
    }

    let previous = ApiPluginRuntime::capture(wasm, &name);
    let previous_hash = existing.artifact_hash.clone();
    let factory =
        WasmPluginFactory::from_artifact(&wasm.engine, &artifact_dir).map_err(plugin_error)?;
    let component_hash = artifact.hash.clone();
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

    wasm.catalog.insert(Arc::new(factory));
    wasm.host_provider
        .install(manifest.clone(), input.approve_capabilities.clone());
    if let Err(error) = reload_all(&state).await {
        previous.restore(wasm, &name);
        if let Err(rollback) = reload_all(&state).await {
            tracing::warn!(
                ?rollback,
                plugin = %name,
                "cannot restore API plugin chains after a failed upgrade"
            );
        }
        wasm.engine.remove_component(&component_hash);
        return Err(error);
    }

    let record = match state
        .storage
        .llm
        .upsert_installed_plugin(
            &manifest,
            &artifact,
            &storage_path,
            &input.approve_capabilities,
        )
        .await
    {
        Ok(record) => record,
        Err(error) => {
            previous.restore(wasm, &name);
            if let Err(rollback) = reload_all(&state).await {
                tracing::warn!(
                    ?rollback,
                    plugin = %name,
                    "cannot restore API plugin chains after a failed upgrade"
                );
            }
            wasm.engine.remove_component(&component_hash);
            return Err(error.into());
        }
    };

    // The old component is no longer referenced by any live factory.
    if previous_hash
        .as_deref()
        .is_some_and(|hash| hash != component_hash)
    {
        wasm.engine
            .remove_component(previous_hash.as_deref().unwrap_or_default());
    }

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

    if record.kind == PluginKind::Llm.as_str() {
        let lifecycle = llm_lifecycle(&state)?;
        lifecycle
            .uninstall(&name, record.artifact_hash.as_deref())
            .await
            .map_err(AdminError::schema)?;
        if let Err(error) = state.storage.llm.delete_installed_plugin(&name).await {
            // The runtime dropped the plugin; restore it from the artifact so
            // the database and the runtime stay consistent.
            if let Some(path) = record.artifact_path.as_deref()
                && let Ok(manifest) = serde_json::from_str::<PluginManifest>(&record.manifest_json)
            {
                let _ = lifecycle.install(manifest, PathBuf::from(path), None).await;
            }
            return Err(AdminError::internal(error.to_string()));
        }
        return Ok(Json(json!({
            "name": name,
            "uninstalled": true,
            "purged": false,
        })));
    }

    // Delete the catalog row first: plugin instances cascade with it, so the
    // next reload compiles without this plugin. Nothing has changed if this
    // step fails.
    state.storage.llm.delete_installed_plugin(&name).await?;
    wasm.catalog.remove(&name);
    wasm.host_provider
        .uninstall(&name, false)
        .map_err(storage_error)?;
    reload_all(&state).await?;

    let purged = if input.purge_data {
        match wasm.host_provider.uninstall(&name, true) {
            Ok(()) => true,
            Err(error) => {
                tracing::warn!(plugin = %name, %error, "cannot purge plugin data");
                false
            }
        }
    } else {
        false
    };
    if let Some(hash) = record.artifact_hash.as_deref() {
        wasm.engine.remove_component(hash);
    }

    Ok(Json(json!({
        "name": name,
        "uninstalled": true,
        "purged": purged,
    })))
}

pub async fn health(
    State(state): State<ApiControlState>,
    Path(name): Path<String>,
) -> AdminResult<Json<Value>> {
    let wasm = wasm(&state)?;
    let record = state
        .storage
        .llm
        .get_plugin_by_name(&name)
        .await?
        .ok_or_else(|| AdminError::not_found(format!("plugin '{name}' is not installed")))?;
    let installed = record.source == "wasm";
    let in_catalog = wasm.catalog.contains(&name);
    let in_host = wasm.host_provider.contains(&name);
    let healthy = installed && record.enabled && in_catalog && in_host;
    Ok(Json(json!({
        "name": name,
        "installed": installed,
        "enabled": record.enabled,
        "catalog": in_catalog,
        "host": in_host,
        "storage_bytes": wasm.host_provider.storage_bytes(&name),
        "healthy": healthy,
    })))
}

pub async fn metrics(
    State(state): State<ApiControlState>,
    Path(name): Path<String>,
) -> AdminResult<Json<Value>> {
    let wasm = wasm(&state)?;
    if let Some(metrics) = wasm.engine.metrics().snapshot(&name) {
        return Ok(Json(json!({ "name": name, "metrics": metrics })));
    }
    if state.storage.llm.get_plugin_by_name(&name).await?.is_none() {
        return Err(AdminError::not_found(format!(
            "plugin '{name}' is not installed"
        )));
    }
    Ok(Json(json!({
        "name": name,
        "metrics": PluginStatsSnapshot::default(),
    })))
}

pub async fn all_metrics(State(state): State<ApiControlState>) -> AdminResult<Json<Value>> {
    let wasm = wasm(&state)?;
    let data = wasm
        .engine
        .metrics()
        .snapshots()
        .into_iter()
        .map(|(name, metrics)| json!({ "name": name, "metrics": metrics }))
        .collect::<Vec<_>>();
    Ok(Json(json!({ "data": data })))
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

pub(super) fn wasm(state: &ApiControlState) -> AdminResult<&Arc<crate::WasmControlState>> {
    state
        .wasm
        .as_ref()
        .ok_or_else(|| AdminError::schema("the wasm plugin runtime is disabled"))
}

async fn installed_manifest(state: &ApiControlState, name: &str) -> AdminResult<PluginManifest> {
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
    let artifact_id = artifact_id.ok_or_else(|| AdminError::schema("artifact_id is required"))?;
    let artifact = state
        .storage
        .llm
        .get_plugin_artifact(artifact_id)
        .await?
        .ok_or_else(|| {
            AdminError::not_found(format!("plugin artifact '{artifact_id}' not found"))
        })?;
    let directory = PathBuf::from(&artifact.path);
    let layout = validate_artifact_dir_any(&directory)
        .map_err(|error| AdminError::schema(format!("invalid artifact: {error}")))?;
    if layout.hash != artifact.hash {
        return Err(AdminError::schema(
            "artifact files do not match the recorded hash",
        ));
    }
    let wasm_state = wasm(state)?;
    verify_signature(
        &wasm_state.trusted_keys,
        wasm_state.allow_unsigned,
        &layout.hash,
        artifact.signature.as_deref(),
    )?;
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

/// Verifies an optional Ed25519 signature over the artifact hash.
///
/// With no trusted keys configured, signatures are recorded but not checked,
/// which keeps the existing local-upload workflow. Once at least one key is
/// configured, every artifact must carry a signature that verifies under one
/// of them unless `allow_unsigned` is set.
fn verify_signature(
    trusted_keys: &[String],
    allow_unsigned: bool,
    hash: &str,
    signature: Option<&str>,
) -> AdminResult<()> {
    if trusted_keys.is_empty() {
        return Ok(());
    }

    let Some(signature) = signature else {
        return if allow_unsigned {
            Ok(())
        } else {
            Err(AdminError::schema("artifact signature is required"))
        };
    };

    let bytes = BASE64
        .decode(signature)
        .map_err(|_| AdminError::schema("signature is not valid base64"))?;
    let signature = Signature::from_slice(&bytes)
        .map_err(|_| AdminError::schema("signature must be 64 bytes"))?;

    for key in trusted_keys {
        let bytes = BASE64
            .decode(key)
            .map_err(|_| AdminError::schema("trusted key is not valid base64"))?;
        let bytes: [u8; 32] = bytes
            .try_into()
            .map_err(|_| AdminError::schema("trusted key must be 32 bytes"))?;
        let key = VerifyingKey::from_bytes(&bytes)
            .map_err(|_| AdminError::schema("trusted key is not a valid ed25519 key"))?;
        if key.verify_strict(hash.as_bytes(), &signature).is_ok() {
            return Ok(());
        }
    }

    Err(AdminError::schema(
        "artifact signature does not match a trusted key",
    ))
}

fn ensure_capabilities(manifest: &PluginManifest, approved: &[Capability]) -> AdminResult<()> {
    for capability in &manifest.capabilities {
        if matches!(capability, Capability::Network | Capability::Secret) {
            return Err(AdminError::schema(format!(
                "capability {capability:?} is not supported by the wasm runtime"
            )));
        }
    }
    let requested = &manifest.capabilities;
    if requested.len() != approved.len()
        || !requested
            .iter()
            .all(|capability| approved.contains(capability))
    {
        return Err(AdminError::schema(
            "approve_capabilities must match the manifest capabilities",
        ));
    }
    Ok(())
}

/// Runtime half of an installed API plugin. Captured before activation so a
/// failed reload, or a failed catalog write, can put the previous factory and
/// host registration back.
struct ApiPluginRuntime {
    factory: Option<Arc<WasmPluginFactory>>,
    host: Option<InstalledWasmPlugin>,
}

impl ApiPluginRuntime {
    fn capture(wasm: &WasmControlState, name: &str) -> Self {
        Self {
            factory: wasm.catalog.get(name),
            host: wasm.host_provider.plugin(name),
        }
    }

    fn restore(self, wasm: &WasmControlState, name: &str) {
        match self.factory {
            Some(factory) => wasm.catalog.insert(factory),
            None => wasm.catalog.remove(name),
        }
        match self.host {
            Some(host) => wasm
                .host_provider
                .install(host.manifest, host.approved_capabilities),
            None => {
                if let Err(error) = wasm.host_provider.uninstall(name, false) {
                    tracing::warn!(
                        plugin = name,
                        %error,
                        "cannot detach the API plugin from the host"
                    );
                }
            }
        }
    }
}

fn llm_lifecycle(state: &ApiControlState) -> AdminResult<&Arc<dyn LlmWasmLifecycle>> {
    state
        .llm_wasm
        .as_ref()
        .ok_or_else(|| AdminError::schema("LLM wasm plugins are not enabled in this daemon"))
}

fn storage_error(error: impl std::fmt::Display) -> AdminError {
    AdminError::schema(format!("plugin storage: {error}"))
}

fn plugin_error(error: PluginError) -> AdminError {
    AdminError::schema(error.message)
}

#[cfg(test)]
mod tests {
    use super::*;
    use ed25519_dalek::Signer;
    use octra_api_plugin_types::ApiGatewayPluginFactory;
    use octra_api_plugin_wasm::{WasmEngine, WasmHostProvider, WasmLimits, WasmPluginCatalog};
    use octra_plugin_storage::PluginStorageRoot;

    fn manifest(capabilities: &[&str]) -> PluginManifest {
        serde_json::from_value(json!({
            "name": "demo",
            "version": "1.0.0",
            "kind": "api",
            "runtime": "wasm",
            "priority": 100,
            "description": "",
            "capabilities": capabilities,
        }))
        .unwrap()
    }

    #[test]
    fn capability_approval_must_match_the_manifest() {
        let manifest = manifest(&["storage_entities"]);
        assert!(ensure_capabilities(&manifest, &[Capability::StorageEntities]).is_ok());
        assert!(ensure_capabilities(&manifest, &[]).is_err());
        assert!(
            ensure_capabilities(&manifest, &[Capability::StorageEntities, Capability::Log])
                .is_err()
        );
    }

    #[test]
    fn rejects_capabilities_the_runtime_cannot_enforce() {
        let manifest = manifest(&["network"]);
        assert!(ensure_capabilities(&manifest, &[Capability::Network]).is_err());
    }

    fn signed(hash: &str) -> (String, String) {
        let signing = ed25519_dalek::SigningKey::from_bytes(&[7u8; 32]);
        let key = BASE64.encode(signing.verifying_key().as_bytes());
        let signature = BASE64.encode(signing.sign(hash.as_bytes()).to_bytes());
        (key, signature)
    }

    #[test]
    fn signatures_are_optional_without_trusted_keys() {
        assert!(verify_signature(&[], false, "hash", None).is_ok());
    }

    #[test]
    fn accepts_a_signature_from_a_trusted_key() {
        let (key, signature) = signed("hash");
        assert!(verify_signature(&[key], false, "hash", Some(&signature)).is_ok());
    }

    #[test]
    fn rejects_a_signature_for_a_different_hash() {
        let (key, signature) = signed("hash");
        assert!(verify_signature(&[key], false, "other", Some(&signature)).is_err());
    }

    #[test]
    fn requires_a_signature_when_keys_are_configured() {
        let (key, signature) = signed("hash");
        let keys = std::slice::from_ref(&key);
        assert!(verify_signature(keys, false, "hash", None).is_err());
        assert!(verify_signature(keys, true, "hash", None).is_ok());
        assert!(verify_signature(keys, false, "hash", Some(&signature)).is_ok());
    }

    #[test]
    fn rejects_malformed_keys_and_signatures() {
        let (key, signature) = signed("hash");
        assert!(
            verify_signature(
                &["not base64!".to_string()],
                false,
                "hash",
                Some(&signature)
            )
            .is_err()
        );
        assert!(
            verify_signature(
                std::slice::from_ref(&key),
                false,
                "hash",
                Some("not base64!")
            )
            .is_err()
        );
        let short = BASE64.encode([0u8; 63]);
        assert!(verify_signature(&[key], false, "hash", Some(&short)).is_err());
    }

    fn artifact(dir: &std::path::Path, name: &str, version: &str) -> PathBuf {
        std::fs::create_dir_all(dir).unwrap();
        std::fs::write(
            dir.join("plugin.json"),
            json!({
                "api_version": "1",
                "name": name,
                "version": version,
                "kind": "api",
                "runtime": "wasm",
                "priority": 0,
                "description": "",
                "capabilities": [],
            })
            .to_string(),
        )
        .unwrap();
        std::fs::write(
            dir.join("plugin.wasm"),
            [0x00, 0x61, 0x73, 0x6d, 0x0d, 0x00, 0x01, 0x00],
        )
        .unwrap();
        dir.to_path_buf()
    }

    fn control_state(root: &std::path::Path) -> WasmControlState {
        let engine = Arc::new(WasmEngine::new(WasmLimits::default()).unwrap());
        let storage_root = Arc::new(PluginStorageRoot::new(root.join("storage")));
        WasmControlState {
            engine,
            catalog: Arc::new(WasmPluginCatalog::new()),
            host_provider: Arc::new(WasmHostProvider::new(storage_root.clone(), Vec::new())),
            storage_root,
            artifact_dir: root.join("artifacts"),
            max_artifact_bytes: 1 << 20,
            trusted_keys: Vec::new(),
            allow_unsigned: true,
        }
    }

    #[test]
    fn runtime_snapshot_tracks_and_restores_the_previous_registration() {
        let temp = tempfile::tempdir().unwrap();
        let wasm = control_state(temp.path());
        let v1 = artifact(&temp.path().join("v1"), "demo", "1.0.0");
        let v2 = artifact(&temp.path().join("v2"), "demo", "2.0.0");

        // Restoring an empty snapshot removes a registration added after it.
        let empty = ApiPluginRuntime::capture(&wasm, "demo");
        let factory = Arc::new(WasmPluginFactory::from_artifact(&wasm.engine, &v2).unwrap());
        let manifest = factory.manifest();
        wasm.catalog.insert(factory);
        wasm.host_provider.install(manifest, Vec::new());
        assert!(wasm.catalog.contains("demo"));
        assert!(wasm.host_provider.contains("demo"));
        empty.restore(&wasm, "demo");
        assert!(!wasm.catalog.contains("demo"));
        assert!(!wasm.host_provider.contains("demo"));

        // Restoring a captured registration puts the previous factory and
        // host manifest back.
        let factory = Arc::new(WasmPluginFactory::from_artifact(&wasm.engine, &v1).unwrap());
        wasm.catalog.insert(factory.clone());
        wasm.host_provider.install(factory.manifest(), Vec::new());
        let previous = ApiPluginRuntime::capture(&wasm, "demo");
        let factory = Arc::new(WasmPluginFactory::from_artifact(&wasm.engine, &v2).unwrap());
        wasm.catalog.insert(factory.clone());
        wasm.host_provider.install(factory.manifest(), Vec::new());
        previous.restore(&wasm, "demo");
        assert_eq!(
            wasm.catalog.get("demo").unwrap().manifest().version,
            "1.0.0"
        );
        assert_eq!(
            wasm.host_provider.plugin("demo").unwrap().manifest.version,
            "1.0.0"
        );
    }
}
