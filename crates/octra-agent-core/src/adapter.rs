//! Shared contract for local coding-agent adapters.
//!
//! An adapter owns exactly one agent's configuration format. It never talks to
//! the network and never writes files itself; the control plane renders a
//! proposed document and performs the backup and atomic write. This keeps the
//! security-sensitive file operations in one place.

use std::path::{Path, PathBuf};

use serde::Serialize;

use crate::AgentError;

/// Filesystem view used by every adapter.
///
/// It is constructed from the process environment in production and from a
/// temporary directory in tests, so adapters never read global state directly.
#[derive(Debug, Clone)]
pub struct AgentContext {
    pub home: PathBuf,
    pub config_dir: PathBuf,
    pub path_var: std::ffi::OsString,
}

impl AgentContext {
    pub fn from_env() -> Self {
        let home = dirs::home_dir().unwrap_or_else(|| PathBuf::from("."));
        let config_dir = dirs::config_dir().unwrap_or_else(|| home.join(".config"));
        let path_var = std::env::var_os("PATH").unwrap_or_default();
        Self {
            home,
            config_dir,
            path_var,
        }
    }

    /// Context rooted at a synthetic home directory, used by tests.
    pub fn for_home(home: impl Into<PathBuf>) -> Self {
        let home = home.into();
        let config_dir = home.join(".config");
        Self {
            home,
            config_dir,
            path_var: std::env::var_os("PATH").unwrap_or_default(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Detection {
    pub installed: bool,
    pub binary: Option<String>,
    pub config_path: String,
    pub config_exists: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ConfigField {
    pub key: String,
    pub value: Option<String>,
    /// True when the value is a credential and has been reduced to a prefix.
    pub masked: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ConfigView {
    pub path: String,
    pub exists: bool,
    pub base_url: Option<String>,
    pub model: Option<String>,
    /// True when the configuration already targets an Octra-compatible
    /// provider entry, regardless of whether it matches this gateway.
    pub provider_configured: bool,
    pub fields: Vec<ConfigField>,
    pub warnings: Vec<String>,
}

impl ConfigView {
    pub fn missing(path: &Path) -> Self {
        Self {
            path: path.display().to_string(),
            exists: false,
            base_url: None,
            model: None,
            provider_configured: false,
            fields: Vec::new(),
            warnings: Vec::new(),
        }
    }
}

/// Values rendered into an agent's configuration.
#[derive(Debug, Clone)]
pub struct ApplyProfile {
    pub base_url: String,
    pub virtual_model_id: String,
    pub virtual_key: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ConfigChange {
    pub field: String,
    pub before: Option<String>,
    pub after: Option<String>,
}

#[derive(Debug, Clone)]
pub struct RenderedConfig {
    pub content: String,
    pub changes: Vec<ConfigChange>,
    pub warnings: Vec<String>,
}

/// One supported agent.
pub trait AgentAdapter: Send + Sync {
    fn id(&self) -> &'static str;

    fn name(&self) -> &'static str;

    fn vendor(&self) -> &'static str;

    /// Executable name searched on `PATH`.
    fn binary(&self) -> &'static str;

    fn config_path(&self, ctx: &AgentContext) -> PathBuf;

    /// Parses a document and extracts the fields Octra cares about. Secrets in
    /// the returned view must already be masked.
    fn read(&self, content: &str, path: &Path) -> Result<ConfigView, AgentError>;

    /// Merges the profile into an existing document, preserving unrelated
    /// settings. `current` is `None` when the file does not exist yet.
    fn render(
        &self,
        current: Option<&str>,
        path: &Path,
        profile: &ApplyProfile,
    ) -> Result<RenderedConfig, AgentError>;

    /// Human-readable problems found in an existing configuration.
    fn health(&self, view: &ConfigView, expected_base_url: &str) -> Vec<String> {
        if !view.exists {
            return Vec::new();
        }
        match view.base_url.as_deref() {
            Some(base) if same_base_url(base, expected_base_url) => Vec::new(),
            Some(base) => vec![format!(
                "points at {base} instead of the local gateway {expected_base_url}"
            )],
            None => vec!["has no base URL".to_string()],
        }
    }
}

pub fn same_base_url(left: &str, right: &str) -> bool {
    left.trim_end_matches('/') == right.trim_end_matches('/')
}

/// Keeps the first and last four characters of a credential.
pub fn mask_secret(value: &str) -> String {
    let chars: Vec<char> = value.chars().collect();
    if chars.len() <= 8 {
        return "********".to_string();
    }
    let prefix: String = chars.iter().take(4).collect();
    let suffix: String = chars.iter().skip(chars.len() - 4).collect();
    format!("{prefix}…{suffix}")
}

pub fn base_url_for(base_url: &str, suffix: &str) -> String {
    format!("{}{suffix}", base_url.trim_end_matches('/'))
}

#[cfg(test)]
mod tests {
    use super::{mask_secret, same_base_url};

    #[test]
    fn masks_short_and_long_secrets() {
        assert_eq!(mask_secret("short"), "********");
        assert_eq!(mask_secret("octra-1234567890"), "octr…7890");
    }

    #[test]
    fn compares_base_urls_without_a_trailing_slash() {
        assert!(same_base_url("http://127.0.0.1:8080/", "http://127.0.0.1:8080"));
        assert!(!same_base_url("http://127.0.0.1:8080", "http://127.0.0.1:9080"));
    }
}
