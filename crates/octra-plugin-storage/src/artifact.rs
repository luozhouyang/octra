use std::path::{Component, Path, PathBuf};

use octra_api_plugin_types::{PluginKind, PluginManifest, PluginRuntimeKind};
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
    let manifest_path = dir.join("plugin.json");
    let raw = std::fs::read_to_string(&manifest_path).map_err(|error| {
        PluginStorageError::InvalidManifest(format!("cannot read plugin.json: {error}"))
    })?;
    let manifest: PluginManifest = serde_json::from_str(&raw)
        .map_err(|error| PluginStorageError::InvalidManifest(error.to_string()))?;
    validate_manifest(&manifest)?;

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

fn validate_manifest(manifest: &PluginManifest) -> Result<(), PluginStorageError> {
    validate_plugin_name(&manifest.name)?;
    validate_version(&manifest.version)?;
    if manifest.api_version != "1" {
        return Err(PluginStorageError::InvalidManifest(format!(
            "unsupported api_version '{}'",
            manifest.api_version
        )));
    }
    if manifest.kind != PluginKind::Api {
        return Err(PluginStorageError::InvalidManifest(
            "only API Gateway plugins can be installed here".to_string(),
        ));
    }
    if manifest.runtime != PluginRuntimeKind::Wasm {
        return Err(PluginStorageError::InvalidManifest(
            "artifact runtime must be wasm".to_string(),
        ));
    }
    for spec in &manifest.entities {
        entity::validate_spec(spec)?;
    }
    for migration in &manifest.migrations {
        validate_migration_path(&migration.file)?;
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
}
