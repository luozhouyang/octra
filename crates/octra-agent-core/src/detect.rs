//! Executable and configuration-file detection.

use std::ffi::OsStr;
use std::path::{Path, PathBuf};

use crate::adapter::{AgentAdapter, AgentContext, Detection};

pub fn detect(adapter: &dyn AgentAdapter, ctx: &AgentContext) -> Detection {
    let config_path = adapter.config_path(ctx);
    Detection {
        installed: find_binary(adapter.binary(), &ctx.path_var).is_some(),
        binary: find_binary(adapter.binary(), &ctx.path_var).map(|path| path.display().to_string()),
        config_exists: config_path.is_file(),
        config_path: config_path.display().to_string(),
    }
}

/// Searches `PATH` for an executable without invoking a shell.
pub fn find_binary(name: &str, path_var: &OsStr) -> Option<PathBuf> {
    for directory in std::env::split_paths(path_var).filter(|dir| !dir.as_os_str().is_empty()) {
        for candidate in candidate_names(name) {
            let path = directory.join(candidate);
            if is_executable(&path) {
                return Some(path);
            }
        }
    }
    None
}

/// Windows can resolve executables through `PATHEXT`; probe the common
/// suffixes so detection works without a running shell.
pub fn candidate_names(name: &str) -> Vec<String> {
    #[allow(unused_mut)]
    let mut names = vec![name.to_string()];
    #[cfg(windows)]
    {
        for extension in ["exe", "cmd", "bat", "ps1"] {
            names.push(format!("{name}.{extension}"));
        }
    }
    names
}

fn is_executable(path: &Path) -> bool {
    let Ok(metadata) = path.metadata() else {
        return false;
    };
    if !metadata.is_file() {
        return false;
    }

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        metadata.permissions().mode() & 0o111 != 0
    }
    #[cfg(not(unix))]
    {
        true
    }
}

#[cfg(test)]
mod tests {
    use std::ffi::OsString;
    use std::fs;

    use tempfile::tempdir;

    use super::{candidate_names, find_binary};

    #[test]
    fn finds_an_executable_on_path() {
        let directory = tempdir().unwrap();
        let binary = directory.path().join("claude");
        fs::write(&binary, "#!/bin/sh\n").unwrap();

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&binary, fs::Permissions::from_mode(0o755)).unwrap();
        }

        let path_var = OsString::from(directory.path());
        assert_eq!(find_binary("claude", &path_var), Some(binary));
        assert_eq!(find_binary("missing", &path_var), None);
    }

    #[test]
    fn always_probes_the_bare_name() {
        assert_eq!(
            candidate_names("codex").first().map(String::as_str),
            Some("codex")
        );
    }
}
