//! Configuration loading and local workspace path resolution.

use std::fs;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};

use octra_core::{Error, Result};
use serde::{Deserialize, Serialize};

pub const DEFAULT_CONTROL_BIND: &str = "127.0.0.1:8081";
pub const DEFAULT_GATEWAY_BIND: &str = "127.0.0.1:8080";

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct OctraConfig {
    pub daemon: DaemonConfig,
    pub gateway: GatewayConfig,
    pub llm: LlmConfig,
    pub mcp: McpConfig,
    pub storage: StorageConfig,
    pub logging: LoggingConfig,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct DaemonConfig {
    pub control_bind: String,
}

/// Unified data plane listener shared by the API, LLM, and MCP gateways.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct GatewayConfig {
    pub listen_bind: String,
    pub request_timeout_secs: u64,
    pub connect_timeout_secs: u64,
    pub max_body_bytes: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct LlmConfig {
    pub request_timeout_secs: u64,
    pub stream_idle_timeout_secs: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct McpConfig {
    pub request_timeout_secs: u64,
    pub tool_call_timeout_secs: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct StorageConfig {
    pub path: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct LoggingConfig {
    pub level: String,
    pub format: LogFormat,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum LogFormat {
    #[default]
    Pretty,
    Json,
}

impl Default for DaemonConfig {
    fn default() -> Self {
        Self {
            control_bind: DEFAULT_CONTROL_BIND.to_string(),
        }
    }
}

impl Default for GatewayConfig {
    fn default() -> Self {
        Self {
            listen_bind: DEFAULT_GATEWAY_BIND.to_string(),
            request_timeout_secs: 60,
            connect_timeout_secs: 10,
            max_body_bytes: 32 * 1024 * 1024,
        }
    }
}

impl Default for LlmConfig {
    fn default() -> Self {
        Self {
            request_timeout_secs: 120,
            stream_idle_timeout_secs: 120,
        }
    }
}

impl Default for McpConfig {
    fn default() -> Self {
        Self {
            request_timeout_secs: 60,
            tool_call_timeout_secs: 120,
        }
    }
}

impl Default for StorageConfig {
    fn default() -> Self {
        Self {
            path: "octra.db".to_string(),
        }
    }
}

impl Default for LoggingConfig {
    fn default() -> Self {
        Self {
            level: "info".to_string(),
            format: LogFormat::Pretty,
        }
    }
}

/// Resolved local data directory for a single Octra installation.
#[derive(Debug, Clone)]
pub struct Workspace {
    pub home: PathBuf,
}

impl Workspace {
    pub fn resolve() -> Result<Self> {
        let home = match std::env::var_os("OCTRA_HOME") {
            Some(value) if !value.is_empty() => PathBuf::from(value),
            _ => dirs::home_dir()
                .ok_or_else(|| Error::Config("cannot determine the user home directory".into()))?
                .join(".octra"),
        };
        Ok(Self { home })
    }

    pub fn new(home: impl Into<PathBuf>) -> Self {
        Self { home: home.into() }
    }

    pub fn ensure(&self) -> Result<()> {
        fs::create_dir_all(&self.home).map_err(|err| {
            Error::Config(format!(
                "cannot create workspace {}: {err}",
                self.home.display()
            ))
        })
    }

    pub fn config_file(&self) -> PathBuf {
        self.home.join("config.toml")
    }

    pub fn token_file(&self) -> PathBuf {
        self.home.join("control.token")
    }

    pub fn database_path(&self, config: &OctraConfig) -> PathBuf {
        let configured = Path::new(&config.storage.path);
        if configured.is_absolute() {
            configured.to_path_buf()
        } else {
            self.home.join(configured)
        }
    }
}

pub fn load_or_create(workspace: &Workspace) -> Result<OctraConfig> {
    workspace.ensure()?;
    let path = workspace.config_file();

    let mut config = if path.exists() {
        let raw = fs::read_to_string(&path)
            .map_err(|err| Error::Config(format!("cannot read {}: {err}", path.display())))?;
        toml::from_str(&raw)
            .map_err(|err| Error::Config(format!("invalid config {}: {err}", path.display())))?
    } else {
        let config = OctraConfig::default();
        let raw = toml::to_string_pretty(&config)
            .map_err(|err| Error::Config(format!("cannot serialize default config: {err}")))?;
        fs::write(&path, raw)
            .map_err(|err| Error::Config(format!("cannot write {}: {err}", path.display())))?;
        config
    };

    apply_env_overrides(&mut config);
    validate(&config)?;
    Ok(config)
}

fn apply_env_overrides(config: &mut OctraConfig) {
    if let Ok(value) = std::env::var("OCTRA_CONTROL_BIND")
        && !value.trim().is_empty()
    {
        config.daemon.control_bind = value;
    }
    if let Ok(value) = std::env::var("OCTRA_GATEWAY_BIND")
        && !value.trim().is_empty()
    {
        config.gateway.listen_bind = value;
    }
    if let Ok(value) = std::env::var("OCTRA_LOG_LEVEL")
        && !value.trim().is_empty()
    {
        config.logging.level = value;
    }
    if let Ok(value) = std::env::var("OCTRA_LOG_FORMAT") {
        config.logging.format = match value.trim().to_ascii_lowercase().as_str() {
            "json" => LogFormat::Json,
            _ => LogFormat::Pretty,
        };
    }
}

fn validate(config: &OctraConfig) -> Result<()> {
    let control: SocketAddr = config.daemon.control_bind.parse().map_err(|err| {
        Error::Config(format!(
            "invalid daemon.control_bind '{}': {err}",
            config.daemon.control_bind
        ))
    })?;

    if !control.ip().is_loopback() {
        return Err(Error::Config(format!(
            "daemon.control_bind must be a loopback address for the local control plane, got {}",
            control
        )));
    }

    let gateway: SocketAddr = config.gateway.listen_bind.parse().map_err(|err| {
        Error::Config(format!(
            "invalid gateway.listen_bind '{}': {err}",
            config.gateway.listen_bind
        ))
    })?;

    if gateway == control {
        return Err(Error::Config(format!(
            "gateway.listen_bind and daemon.control_bind must differ, both are {gateway}"
        )));
    }

    if config.gateway.request_timeout_secs == 0
        || config.gateway.connect_timeout_secs == 0
        || config.llm.request_timeout_secs == 0
        || config.llm.stream_idle_timeout_secs == 0
        || config.mcp.request_timeout_secs == 0
        || config.mcp.tool_call_timeout_secs == 0
    {
        return Err(Error::Config(
            "timeout values must be greater than zero".into(),
        ));
    }

    if config.gateway.max_body_bytes == 0 {
        return Err(Error::Config(
            "gateway.max_body_bytes must be greater than zero".into(),
        ));
    }

    if config.storage.path.trim().is_empty() {
        return Err(Error::Config("storage.path must not be empty".into()));
    }

    if config.logging.level.trim().is_empty() {
        return Err(Error::Config("logging.level must not be empty".into()));
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_config_is_valid() {
        validate(&OctraConfig::default()).expect("defaults should be valid");
    }

    #[test]
    fn rejects_non_loopback_control_bind() {
        let mut config = OctraConfig::default();
        config.daemon.control_bind = "0.0.0.0:8081".into();
        assert!(validate(&config).is_err());
    }

    #[test]
    fn rejects_conflicting_listeners() {
        let mut config = OctraConfig::default();
        config.gateway.listen_bind = config.daemon.control_bind.clone();
        assert!(validate(&config).is_err());
    }

    #[test]
    fn rejects_invalid_gateway_bind() {
        let mut config = OctraConfig::default();
        config.gateway.listen_bind = "not-an-address".into();
        assert!(validate(&config).is_err());
    }

    #[test]
    fn relative_storage_path_resolves_under_workspace() {
        let workspace = Workspace::new("/tmp/octra-test");
        let path = workspace.database_path(&OctraConfig::default());
        assert_eq!(path, PathBuf::from("/tmp/octra-test/octra.db"));
    }

    #[test]
    fn writes_default_config_on_first_load() {
        let dir = tempfile::tempdir().unwrap();
        let workspace = Workspace::new(dir.path());
        let config = load_or_create(&workspace).unwrap();
        assert_eq!(config.daemon.control_bind, DEFAULT_CONTROL_BIND);
        assert!(workspace.config_file().exists());
    }
}
