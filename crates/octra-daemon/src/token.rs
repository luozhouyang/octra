use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::Path;

use anyhow::{Context, Result};

const TOKEN_ENV: &str = "OCTRA_CONTROL_TOKEN";

/// Returns the control plane token, creating a random one on first start.
/// `OCTRA_CONTROL_TOKEN` takes precedence for managed deployments.
pub fn load_or_create(path: &Path) -> Result<String> {
    if let Ok(token) = std::env::var(TOKEN_ENV) {
        let token = token.trim();
        if !token.is_empty() {
            return Ok(token.to_string());
        }
    }

    if path.exists() {
        ensure_not_symlink(path)?;
        ensure_private_permissions(path)?;
        let token = fs::read_to_string(path)
            .with_context(|| format!("cannot read control token {}", path.display()))?;
        let token = token.trim();
        if !token.is_empty() {
            return Ok(token.to_string());
        }
    }

    let token = format!(
        "{}{}",
        uuid::Uuid::new_v4().simple(),
        uuid::Uuid::new_v4().simple()
    );

    write_private(path, &token)
        .with_context(|| format!("cannot write control token {}", path.display()))?;

    Ok(token)
}

fn write_private(path: &Path, token: &str) -> Result<()> {
    ensure_not_symlink(path)?;
    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
    {
        fs::create_dir_all(parent)?;
    }

    let mut options = OpenOptions::new();
    options.write(true);
    if path.exists() {
        options.truncate(true);
    } else {
        options.create_new(true);
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options
        .open(path)
        .with_context(|| format!("cannot create control token {}", path.display()))?;
    file.write_all(format!("{token}\n").as_bytes())?;
    file.sync_all()?;
    ensure_private_permissions(path)?;
    Ok(())
}

#[cfg(unix)]
fn ensure_not_symlink(path: &Path) -> Result<()> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error.into()),
    };
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        anyhow::bail!(
            "control token path {} must be a regular file, not a symlink or special file",
            path.display()
        );
    }
    Ok(())
}

#[cfg(not(unix))]
fn ensure_not_symlink(path: &Path) -> Result<()> {
    if path.exists() && !path.is_file() {
        anyhow::bail!(
            "control token path {} must be a regular file",
            path.display()
        );
    }
    Ok(())
}

#[cfg(unix)]
fn ensure_private_permissions(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let mode = fs::metadata(path)?.permissions().mode() & 0o777;
    if mode != 0o600 {
        fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
    }
    Ok(())
}

#[cfg(not(unix))]
fn ensure_private_permissions(_path: &Path) -> Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reuses_existing_token() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("control.token");

        let first = load_or_create(&path).unwrap();
        let second = load_or_create(&path).unwrap();

        assert_eq!(first, second);
        assert!(!first.is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn rejects_symlinked_token_files() {
        use std::os::unix::fs::symlink;

        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("real.token");
        std::fs::write(&target, "existing\n").unwrap();
        let link = dir.path().join("control.token");
        symlink(&target, &link).unwrap();

        let error = load_or_create(&link).unwrap_err().to_string();
        assert!(error.contains("symlink"), "{error}");
    }

    #[cfg(unix)]
    #[test]
    fn token_file_is_owner_only() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("control.token");
        load_or_create(&path).unwrap();

        let mode = fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
    }
}
