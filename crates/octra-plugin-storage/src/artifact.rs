use std::path::{Component, Path, PathBuf};

use octra_api_plugin_types::{
    Capability, MAX_ADMIN_ROUTES, PluginKind, PluginManifest, PluginPhase, PluginRuntimeKind,
    canonical_admin_path, parse_admin_route,
};
use sha2::{Digest, Sha256};

use crate::entity;
use crate::error::PluginStorageError;
use crate::store::{validate_plugin_name, validate_version};

#[derive(Debug, Clone)]
pub struct ArtifactLayout {
    pub manifest: PluginManifest,
    pub hash: String,
    /// Paths relative to the artifact directory.
    pub files: Vec<PathBuf>,
}

pub fn validate_artifact_dir(dir: &Path) -> Result<ArtifactLayout, PluginStorageError> {
    validate_artifact_dir_for_kind(dir, PluginKind::Api)
}

/// Validates an artifact for one plugin kind. API and LLM artifacts share the
/// on-disk layout but have different capability rules.
pub fn validate_artifact_dir_for_kind(
    dir: &Path,
    kind: PluginKind,
) -> Result<ArtifactLayout, PluginStorageError> {
    let manifest_path = dir.join("plugin.json");
    let raw = std::fs::read_to_string(&manifest_path).map_err(|error| {
        PluginStorageError::InvalidManifest(format!("cannot read plugin.json: {error}"))
    })?;
    let manifest: PluginManifest = serde_json::from_str(&raw)
        .map_err(|error| PluginStorageError::InvalidManifest(error.to_string()))?;
    validate_manifest(&manifest, kind)?;

    let mut files = vec![PathBuf::from("plugin.json")];
    let wasm_path = dir.join("plugin.wasm");
    if manifest.runtime == PluginRuntimeKind::Wasm {
        if !wasm_path.is_file() {
            return Err(PluginStorageError::InvalidManifest(
                "plugin.wasm is required for wasm plugins".to_string(),
            ));
        }
        files.push(PathBuf::from("plugin.wasm"));
    }

    let mut versions = std::collections::HashSet::new();
    for migration in &manifest.migrations {
        if !versions.insert(migration.version.clone()) {
            return Err(PluginStorageError::InvalidManifest(format!(
                "migration version '{}' is declared twice",
                migration.version
            )));
        }
        let relative = validate_migration_path(&migration.file)?;
        if !dir.join(&relative).is_file() {
            return Err(PluginStorageError::InvalidManifest(format!(
                "migration file '{}' does not exist",
                migration.file
            )));
        }
        files.push(relative);
    }

    files.sort();
    let hash = hash_files(dir, &files)?;

    Ok(ArtifactLayout {
        manifest,
        hash,
        files,
    })
}

pub fn copy_artifact(
    source: &Path,
    destination: &Path,
) -> Result<ArtifactLayout, PluginStorageError> {
    let layout = validate_artifact_dir(source)?;
    if destination.exists() {
        let mut entries = std::fs::read_dir(destination)?;
        if entries.next().is_some() {
            return Err(PluginStorageError::InvalidManifest(format!(
                "artifact destination {} is not empty",
                destination.display()
            )));
        }
    } else {
        std::fs::create_dir_all(destination)?;
    }

    for relative in &layout.files {
        let target = destination.join(relative);
        if let Some(parent) = target.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::copy(source.join(relative), target)?;
    }
    Ok(layout)
}

fn validate_manifest(
    manifest: &PluginManifest,
    kind: PluginKind,
) -> Result<(), PluginStorageError> {
    validate_plugin_name(&manifest.name)?;
    validate_version(&manifest.version)?;
    if manifest.api_version != "1" {
        return Err(PluginStorageError::InvalidManifest(format!(
            "unsupported api_version '{}'",
            manifest.api_version
        )));
    }
    if manifest.kind != kind {
        return Err(PluginStorageError::InvalidManifest(format!(
            "artifact kind '{}' does not match the requested '{}'",
            manifest.kind.as_str(),
            kind.as_str()
        )));
    }
    if manifest.runtime != PluginRuntimeKind::Wasm {
        return Err(PluginStorageError::InvalidManifest(
            "artifact runtime must be wasm".to_string(),
        ));
    }
    if manifest.phases.contains(&PluginPhase::Certificate) {
        return Err(PluginStorageError::InvalidManifest(
            "the certificate phase is not supported by wasm plugins".to_string(),
        ));
    }
    if kind == PluginKind::Llm
        && (!manifest.entities.is_empty()
            || !manifest.migrations.is_empty()
            || !manifest.admin_routes.is_empty())
    {
        return Err(PluginStorageError::InvalidManifest(
            "LLM wasm plugins do not support entities, migrations or admin routes yet".to_string(),
        ));
    }
    for spec in &manifest.entities {
        entity::validate_spec(spec)?;
    }
    for migration in &manifest.migrations {
        validate_migration_path(&migration.file)?;
    }
    validate_admin_routes(manifest)?;
    Ok(())
}

const ADMIN_METHODS: [&str; 7] = ["GET", "POST", "PUT", "PATCH", "DELETE", "HEAD", "OPTIONS"];

fn validate_admin_routes(manifest: &PluginManifest) -> Result<(), PluginStorageError> {
    let declares = manifest.capabilities.contains(&Capability::AdminApi);
    if manifest.admin_routes.is_empty() {
        if declares {
            return Err(PluginStorageError::InvalidManifest(
                "the admin_api capability requires at least one admin_routes entry".to_string(),
            ));
        }
        return Ok(());
    }
    if !declares {
        return Err(PluginStorageError::InvalidManifest(
            "admin_routes require the admin_api capability".to_string(),
        ));
    }
    if manifest.admin_routes.len() > MAX_ADMIN_ROUTES {
        return Err(PluginStorageError::InvalidManifest(format!(
            "a plugin may declare at most {MAX_ADMIN_ROUTES} admin routes"
        )));
    }

    let mut seen = std::collections::HashSet::new();
    for route in &manifest.admin_routes {
        let method = route.method.trim().to_ascii_uppercase();
        if !ADMIN_METHODS.contains(&method.as_str()) {
            return Err(PluginStorageError::InvalidManifest(format!(
                "admin route '{}' has an unsupported method '{}'",
                route.path, route.method
            )));
        }
        let segments =
            parse_admin_route(&route.path).map_err(PluginStorageError::InvalidManifest)?;
        let canonical = canonical_admin_path(&segments);
        if !seen.insert((method, canonical.clone())) {
            return Err(PluginStorageError::InvalidManifest(format!(
                "admin route '{canonical}' is declared twice for the same method"
            )));
        }
    }
    Ok(())
}

fn validate_migration_path(file: &str) -> Result<PathBuf, PluginStorageError> {
    let path = Path::new(file);
    if path.is_absolute()
        || path.components().any(|component| {
            matches!(
                component,
                Component::ParentDir | Component::RootDir | Component::Prefix(_)
            )
        })
    {
        return Err(PluginStorageError::InvalidManifest(format!(
            "migration path '{file}' must be relative to the artifact"
        )));
    }
    if !path.starts_with("migrations") {
        return Err(PluginStorageError::InvalidManifest(format!(
            "migration path '{file}' must live under migrations/"
        )));
    }
    Ok(path.to_path_buf())
}

fn hash_files(dir: &Path, files: &[PathBuf]) -> Result<String, PluginStorageError> {
    let mut hasher = Sha256::new();
    for relative in files {
        let data = std::fs::read(dir.join(relative))?;
        hasher.update(relative.to_string_lossy().as_bytes());
        hasher.update([0]);
        hasher.update((data.len() as u64).to_le_bytes());
        hasher.update(&data);
    }
    Ok(hex::encode(hasher.finalize()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::io::Write;

    fn artifact(dir: &Path, manifest: &str, migration: Option<&str>) {
        fs::create_dir_all(dir.join("migrations")).unwrap();
        let mut file = fs::File::create(dir.join("plugin.json")).unwrap();
        file.write_all(manifest.as_bytes()).unwrap();
        fs::write(dir.join("plugin.wasm"), b"wasm").unwrap();
        if let Some(sql) = migration {
            fs::write(dir.join("migrations/000_base.sql"), sql).unwrap();
        }
    }

    fn manifest(migrations: &str) -> String {
        format!(
            r#"{{
                "api_version": "1",
                "name": "demo-plugin",
                "version": "1.0.0",
                "kind": "api",
                "runtime": "wasm",
                "priority": 100,
                "description": "demo",
                "capabilities": [],
                "migrations": {migrations}
            }}"#
        )
    }

    #[test]
    fn validates_and_hashes_an_artifact() {
        let dir = tempfile::tempdir().unwrap();
        artifact(
            dir.path(),
            &manifest(r#"[{"version":"000_base","file":"migrations/000_base.sql"}]"#),
            Some("CREATE TABLE demo (id TEXT PRIMARY KEY);"),
        );
        let layout = validate_artifact_dir(dir.path()).unwrap();
        assert_eq!(layout.manifest.name, "demo-plugin");
        assert_eq!(layout.files.len(), 3);
        assert_eq!(layout.hash.len(), 64);
    }

    #[test]
    fn accepts_llm_artifacts_only_for_the_llm_kind() {
        let dir = tempfile::tempdir().unwrap();
        let llm = r#"{
            "api_version": "1",
            "name": "llm-demo",
            "version": "1.0.0",
            "kind": "llm",
            "runtime": "wasm",
            "priority": 0,
            "description": "demo",
            "capabilities": [],
            "phases": ["request"]
        }"#;
        artifact(dir.path(), llm, None);
        assert!(validate_artifact_dir_for_kind(dir.path(), PluginKind::Llm).is_ok());
        assert!(validate_artifact_dir(dir.path()).is_err());
    }

    #[test]
    fn llm_artifacts_reject_api_store_features() {
        let dir = tempfile::tempdir().unwrap();
        let llm = r#"{
            "api_version": "1",
            "name": "llm-demo",
            "version": "1.0.0",
            "kind": "llm",
            "runtime": "wasm",
            "priority": 0,
            "description": "demo",
            "capabilities": [],
            "entities": [{"name": "demo"}]
        }"#;
        artifact(dir.path(), llm, None);
        assert!(validate_artifact_dir_for_kind(dir.path(), PluginKind::Llm).is_err());
    }

    #[test]
    fn rejects_missing_wasm() {
        let dir = tempfile::tempdir().unwrap();
        artifact(dir.path(), &manifest("[]"), None);
        fs::remove_file(dir.path().join("plugin.wasm")).unwrap();
        assert!(validate_artifact_dir(dir.path()).is_err());
    }

    #[test]
    fn rejects_migration_outside_migrations() {
        let dir = tempfile::tempdir().unwrap();
        artifact(
            dir.path(),
            &manifest(r#"[{"version":"000_base","file":"base.sql"}]"#),
            Some("SELECT 1;"),
        );
        fs::rename(
            dir.path().join("migrations/000_base.sql"),
            dir.path().join("base.sql"),
        )
        .unwrap();
        assert!(validate_artifact_dir(dir.path()).is_err());
    }

    fn capability_manifest(capabilities: &str) -> String {
        format!(
            r#"{{
                "api_version": "1",
                "name": "demo-plugin",
                "version": "1.0.0",
                "kind": "api",
                "runtime": "wasm",
                "priority": 100,
                "description": "demo",
                "capabilities": {capabilities},
                "migrations": []
            }}"#
        )
    }

    #[test]
    fn accepts_manifests_declaring_the_auth_capability() {
        let dir = tempfile::tempdir().unwrap();
        artifact(dir.path(), &capability_manifest(r#"["auth"]"#), None);
        let layout = validate_artifact_dir(dir.path()).unwrap();
        assert!(
            layout
                .manifest
                .capabilities
                .contains(&octra_api_plugin_types::Capability::Auth)
        );
    }

    #[test]
    fn ignores_the_legacy_auth_flag() {
        let dir = tempfile::tempdir().unwrap();
        let legacy = capability_manifest(r#"["auth"]"#).replacen(
            r#""capabilities""#,
            r#""auth": true, "capabilities""#,
            1,
        );
        artifact(dir.path(), &legacy, None);
        assert!(validate_artifact_dir(dir.path()).is_ok());
    }

    fn admin_manifest(capabilities: &str, routes: &str) -> String {
        format!(
            r#"{{
                "api_version": "1",
                "name": "demo-plugin",
                "version": "1.0.0",
                "kind": "api",
                "runtime": "wasm",
                "priority": 100,
                "description": "demo",
                "capabilities": {capabilities},
                "admin_routes": {routes},
                "migrations": []
            }}"#
        )
    }

    fn admin_artifact(manifest: &str) -> bool {
        let dir = tempfile::tempdir().unwrap();
        artifact(dir.path(), manifest, None);
        validate_artifact_dir(dir.path()).is_ok()
    }

    #[test]
    fn accepts_valid_admin_routes() {
        assert!(admin_artifact(&admin_manifest(
            r#"["admin_api"]"#,
            r#"[
                {"method":"GET","path":"/status","summary":"Status"},
                {"method":"DELETE","path":"/consumers/{consumer}/usage"}
            ]"#,
        )));
    }

    #[test]
    fn rejects_admin_routes_without_the_capability() {
        assert!(!admin_artifact(&admin_manifest(
            r#"[]"#,
            r#"[{"method":"GET","path":"/status"}]"#,
        )));
    }

    #[test]
    fn rejects_the_capability_without_admin_routes() {
        assert!(!admin_artifact(&admin_manifest(r#"["admin_api"]"#, "[]")));
    }

    #[test]
    fn rejects_invalid_admin_route_methods_and_paths() {
        for route in [
            r#"{"method":"TRACE","path":"/status"}"#,
            r#"{"method":"GET","path":"status"}"#,
            r#"{"method":"GET","path":"/a/../b"}"#,
            r#"{"method":"GET","path":"/a/*"}"#,
            r#"{"method":"GET","path":"/a/{1bad}"}"#,
        ] {
            assert!(
                !admin_artifact(&admin_manifest(r#"["admin_api"]"#, &format!("[{route}]"))),
                "expected {route} to fail"
            );
        }
    }

    #[test]
    fn rejects_duplicate_admin_routes() {
        assert!(!admin_artifact(&admin_manifest(
            r#"["admin_api"]"#,
            r#"[
                {"method":"GET","path":"/a/{x}"},
                {"method":"GET","path":"/a/{y}"}
            ]"#,
        )));
    }

    #[test]
    fn rejects_too_many_admin_routes() {
        let routes: Vec<_> = (0..=MAX_ADMIN_ROUTES)
            .map(|index| {
                serde_json::json!({
                    "method": "GET",
                    "path": format!("/route-{index}"),
                })
            })
            .collect();
        let routes = serde_json::to_string(&routes).unwrap();
        assert!(!admin_artifact(&admin_manifest(
            r#"["admin_api"]"#,
            &routes
        )));
    }
}
