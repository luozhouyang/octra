use std::fs;
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
    fs::write(path, format!("{token}\n"))?;

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
    }

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
    fn token_file_is_owner_only() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("control.token");
        load_or_create(&path).unwrap();

        let mode = fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
    }
}
