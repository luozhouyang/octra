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
    Capability, EntityQuery, PluginError, PluginManifest, PluginRuntimeKind,
};
use octra_api_plugin_wasm::{PluginStatsSnapshot, WasmPluginFactory};
use octra_plugin_storage::{copy_artifact, validate_artifact_dir};
use octra_storage::{PluginArtifactInput, PluginArtifactRecord};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::ApiControlState;

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
    let layout = validate_artifact_dir(&source)
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

    let factory =
        WasmPluginFactory::from_artifact(&wasm.engine, &artifact_dir).map_err(plugin_error)?;
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

    let factory =
        WasmPluginFactory::from_artifact(&wasm.engine, &artifact_dir).map_err(plugin_error)?;
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

fn wasm(state: &ApiControlState) -> AdminResult<&Arc<crate::WasmControlState>> {
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
    let layout = validate_artifact_dir(&directory)
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
}
