//! File helpers shared by the control plane.
//!
//! Writes are atomic: the new document is written to a sibling temporary file
//! with restrictive permissions, flushed, and renamed over the target. A
//! crash therefore leaves either the old file or the new one, never a partial
//! configuration.

use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};

use crate::AgentError;

/// Agent configuration files can contain credentials; keep them small and
/// private.
pub const MAX_CONFIG_BYTES: u64 = 1024 * 1024;

pub fn sha256_hex(content: &str) -> String {
    let digest = Sha256::digest(content.as_bytes());
    hex_encode(&digest)
}

fn hex_encode(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut output = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        output.push(char::from(HEX[usize::from(byte >> 4)]));
        output.push(char::from(HEX[usize::from(byte & 0x0f)]));
    }
    output
}

pub fn read_optional(path: &Path) -> Result<Option<String>, AgentError> {
    match fs::metadata(path) {
        Ok(metadata) => {
            if !metadata.is_file() {
                return Err(AgentError::InvalidConfig(format!(
                    "{} is not a regular file",
                    path.display()
                )));
            }
            if metadata.len() > MAX_CONFIG_BYTES {
                return Err(AgentError::InvalidConfig(format!(
                    "{} is larger than {} bytes",
                    path.display(),
                    MAX_CONFIG_BYTES
                )));
            }
            Ok(Some(fs::read_to_string(path)?))
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(AgentError::Io(error)),
    }
}

/// Atomically replaces `path` with `content`, creating parent directories.
///
/// Symlinked configurations are written through to their target so the link
/// (a common dotfile setup) survives.
pub fn write_atomic(path: &Path, content: &str) -> Result<(), AgentError> {
    let path = resolve_target(path)?;
    let parent = path.parent().ok_or_else(|| {
        AgentError::InvalidConfig(format!("{} has no parent directory", path.display()))
    })?;
    fs::create_dir_all(parent)?;

    let temporary = temporary_path(&path);
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&temporary)?;

    let result = (|| -> Result<(), std::io::Error> {
        file.write_all(content.as_bytes())?;
        file.sync_all()?;
        drop(file);
        set_private_permissions(&temporary)?;
        fs::rename(&temporary, &path)?;
        sync_parent(parent);
        Ok(())
    })();

    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result.map_err(AgentError::Io)
}

fn resolve_target(path: &Path) -> Result<PathBuf, AgentError> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() => {
            fs::canonicalize(path).map_err(AgentError::Io)
        }
        Ok(_) => Ok(path.to_path_buf()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(path.to_path_buf()),
        Err(error) => Err(AgentError::Io(error)),
    }
}

/// Removes a file created by an apply. Missing files are not an error.
pub fn remove_file(path: &Path) -> Result<(), AgentError> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(AgentError::Io(error)),
    }
}

fn temporary_path(path: &Path) -> PathBuf {
    let name = path
        .file_name()
        .map(|name| name.to_string_lossy().to_string())
        .unwrap_or_else(|| "config".to_string());
    path.with_file_name(format!(".{name}.octra-{}.tmp", uuid::Uuid::new_v4()))
}

#[cfg(unix)]
fn set_private_permissions(path: &Path) -> Result<(), std::io::Error> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(0o600))
}

#[cfg(not(unix))]
fn set_private_permissions(_path: &Path) -> Result<(), std::io::Error> {
    Ok(())
}

#[cfg(unix)]
fn sync_parent(parent: &Path) {
    if let Ok(directory) = fs::File::open(parent) {
        let _ = directory.sync_all();
    }
}

#[cfg(not(unix))]
fn sync_parent(_parent: &Path) {}

#[cfg(test)]
mod tests {
    use std::fs;

    use tempfile::tempdir;

    use super::{read_optional, sha256_hex, write_atomic};

    #[test]
    fn hashes_content_stably() {
        assert_eq!(
            sha256_hex("hello"),
            "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824"
        );
    }

    #[test]
    fn writes_atomically_and_sets_private_permissions() {
        let directory = tempdir().unwrap();
        let path = directory.path().join("nested/config.json");

        write_atomic(&path, "{\"a\":1}\n").unwrap();

        assert_eq!(
            read_optional(&path).unwrap().as_deref(),
            Some("{\"a\":1}\n")
        );
        assert_eq!(fs::read_dir(path.parent().unwrap()).unwrap().count(), 1);

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = fs::metadata(&path).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600);
        }
    }

    #[test]
    fn missing_files_are_not_an_error() {
        let directory = tempdir().unwrap();
        assert!(
            read_optional(&directory.path().join("absent.json"))
                .unwrap()
                .is_none()
        );
    }

    #[cfg(unix)]
    #[test]
    fn writes_through_symlinked_configs() {
        let directory = tempdir().unwrap();
        let target = directory.path().join("settings.json");
        let link = directory.path().join("link.json");
        fs::write(&target, "{}\n").unwrap();
        std::os::unix::fs::symlink(&target, &link).unwrap();

        write_atomic(&link, "{\"updated\":true}\n").unwrap();

        assert!(
            fs::symlink_metadata(&link)
                .unwrap()
                .file_type()
                .is_symlink()
        );
        assert_eq!(fs::read_to_string(&target).unwrap(), "{\"updated\":true}\n");
    }
}
