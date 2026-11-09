//! Configuration loading and local workspace path resolution.

use std::collections::BTreeMap;
use std::fs;
use std::net::{IpAddr, SocketAddr};
use std::path::{Path, PathBuf};

use ipnet::IpNet;
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
    pub agents: AgentsConfig,
    pub storage: StorageConfig,
    pub logging: LoggingConfig,
    pub plugins: PluginsConfig,
    pub telemetry: TelemetryConfig,
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
    /// Proxy addresses trusted to supply the client address in `real_ip_header`.
    /// Empty ignores forwarded headers and uses the transport peer address.
    pub trusted_ips: Vec<String>,
    /// Header carrying the original client address, honored only from trusted
    /// proxies.
    pub real_ip_header: String,
    /// Walk `real_ip_header` right-to-left past trusted proxies.
    pub real_ip_recursive: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct LlmConfig {
    pub request_timeout_secs: u64,
    pub stream_idle_timeout_secs: u64,
    /// Header carrying a stable session id for account affinity.
    pub session_header: String,
    /// Header carrying a client intent hint for routing.
    pub intent_header: String,
    /// How long an account is skipped after a rate-limit or quota error.
    pub account_cooldown_secs: u64,
    /// Optional OpenAI-compatible embedding endpoint for semantic routing.
    pub semantic_embedding: Option<SemanticEmbeddingConfig>,
}

/// OpenAI-compatible embedding endpoint used to score semantic routing
/// targets. Local lexical vectors remain the fallback when this is unset or
/// the endpoint fails.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct SemanticEmbeddingConfig {
    pub base_url: String,
    pub model: String,
    pub auth_ref: Option<String>,
    pub timeout_ms: u64,
    pub cache_entries: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct McpConfig {
    pub request_timeout_secs: u64,
    pub tool_call_timeout_secs: u64,
}

/// Local agent discovery and configuration hosting.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct AgentsConfig {
    /// When set, discovery and config writes use this directory as the
    /// synthetic home instead of the real user home. Intended for testing.
    pub sandbox_home: Option<String>,
    /// Optional PATH used for binary detection in sandbox mode.
    pub sandbox_path: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct StorageConfig {
    pub path: String,
    pub retention: RetentionConfig,
}

/// Append-only data retention. A window of `0` keeps rows forever.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct RetentionConfig {
    pub enabled: bool,
    pub llm_usage_days: u32,
    pub request_logs_days: u32,
    pub mcp_invocations_days: u32,
    pub audit_logs_days: u32,
    pub usage_rollup_days: u32,
    /// Completed and failed plugin events. Pending events are never pruned.
    pub plugin_events_days: u32,
    pub interval_secs: u64,
    pub batch_size: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct LoggingConfig {
    pub level: String,
    pub format: LogFormat,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct PluginsConfig {
    /// Per-plugin SQLite databases.
    pub dir: String,
    /// Installed artifact directories.
    pub artifact_dir: String,
    pub wasm_enabled: bool,
    pub strict_boot: bool,
    /// Accept artifacts without a signature when `trusted_keys` is set.
    pub allow_unsigned: bool,
    /// Base64-encoded Ed25519 public keys. Empty disables signature checks.
    pub trusted_keys: Vec<String>,
    pub max_artifact_bytes: usize,
    /// Per-plugin database cap; zero leaves databases unbounded.
    pub max_plugin_db_bytes: u64,
    pub max_fuel: u64,
    pub max_memory_bytes: usize,
    /// Largest request or response body handed to a WASM plugin.
    pub max_body_bytes: usize,
    pub max_inflight: usize,
    pub default_timeout_ms: u64,
    /// Counter backend for LLM rate limits and budget guards. `database`
    /// shares counters across processes that use the same SQLite file.
    pub counter_backend: CounterBackend,
}

/// Optional periodic export of daemon metrics to an OTLP/HTTP collector.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct TelemetryConfig {
    pub enabled: bool,
    pub interval_secs: u64,
    pub service_name: String,
    pub otlp: Option<OtlpConfig>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct OtlpConfig {
    pub endpoint: String,
    pub headers: BTreeMap<String, String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum CounterBackend {
    Local,
    #[default]
    Database,
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
            trusted_ips: Vec::new(),
            real_ip_header: "x-forwarded-for".to_string(),
            real_ip_recursive: false,
        }
    }
}

impl Default for LlmConfig {
    fn default() -> Self {
        Self {
            request_timeout_secs: 120,
            stream_idle_timeout_secs: 120,
            session_header: "x-octra-session".to_string(),
            intent_header: "x-octra-intent".to_string(),
            account_cooldown_secs: 60,
            semantic_embedding: None,
        }
    }
}

impl Default for SemanticEmbeddingConfig {
    fn default() -> Self {
        Self {
            base_url: String::new(),
            model: String::new(),
            auth_ref: None,
            timeout_ms: 1_500,
            cache_entries: 1_024,
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
            retention: RetentionConfig::default(),
        }
    }
}

impl Default for RetentionConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            llm_usage_days: 90,
            request_logs_days: 30,
            mcp_invocations_days: 90,
            audit_logs_days: 365,
            usage_rollup_days: 400,
            plugin_events_days: 7,
            interval_secs: 3_600,
            batch_size: 5_000,
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

impl Default for TelemetryConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            interval_secs: 30,
            service_name: "octra".to_string(),
            otlp: None,
        }
    }
}

impl Default for PluginsConfig {
    fn default() -> Self {
        Self {
            dir: "plugins".to_string(),
            artifact_dir: "plugin-artifacts".to_string(),
            wasm_enabled: true,
            strict_boot: false,
            allow_unsigned: false,
            trusted_keys: Vec::new(),
            max_artifact_bytes: 16 * 1024 * 1024,
            max_plugin_db_bytes: 64 * 1024 * 1024,
            max_fuel: 10_000_000,
            max_memory_bytes: 64 * 1024 * 1024,
            max_body_bytes: 1024 * 1024,
            max_inflight: 64,
            default_timeout_ms: 100,
            counter_backend: CounterBackend::default(),
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

    pub fn plugin_dir(&self, config: &OctraConfig) -> PathBuf {
        self.resolve_path(&config.plugins.dir)
    }

    pub fn plugin_artifact_dir(&self, config: &OctraConfig) -> PathBuf {
        self.resolve_path(&config.plugins.artifact_dir)
    }

    /// Local model cache. Downloaded model files are deliberately kept out of
    /// the settings directory so configuration sync never carries gigabytes.
    pub fn model_dir(&self) -> PathBuf {
        self.home.join("cache").join("models")
    }

    fn resolve_path(&self, configured: &str) -> PathBuf {
        let path = Path::new(configured);
        if path.is_absolute() {
            path.to_path_buf()
        } else {
            self.home.join(path)
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

/// HTTP token characters permitted in a header name (RFC 9110 `tchar`).
fn is_header_name(name: &str) -> bool {
    !name.is_empty()
        && name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~".contains(&byte))
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

    for rule in &config.gateway.trusted_ips {
        if rule.parse::<IpAddr>().is_err() && rule.parse::<IpNet>().is_err() {
            return Err(Error::Config(format!(
                "invalid gateway.trusted_ips entry '{rule}': expected an IP address or CIDR"
            )));
        }
    }

    if !is_header_name(&config.gateway.real_ip_header) {
        return Err(Error::Config(format!(
            "invalid gateway.real_ip_header '{}': expected an HTTP header name",
            config.gateway.real_ip_header
        )));
    }

    if let Some(embedding) = &config.llm.semantic_embedding {
        if !(embedding.base_url.starts_with("http://")
            || embedding.base_url.starts_with("https://"))
        {
            return Err(Error::Config(format!(
                "llm.semantic_embedding.base_url '{}' must start with http:// or https://",
                embedding.base_url
            )));
        }
        if embedding.model.trim().is_empty() {
            return Err(Error::Config(
                "llm.semantic_embedding.model must not be empty".into(),
            ));
        }
        if !(100..=10_000).contains(&embedding.timeout_ms) {
            return Err(Error::Config(
                "llm.semantic_embedding.timeout_ms must be between 100 and 10000".into(),
            ));
        }
        if !(1..=65_536).contains(&embedding.cache_entries) {
            return Err(Error::Config(
                "llm.semantic_embedding.cache_entries must be between 1 and 65536".into(),
            ));
        }
    }

    if config.storage.path.trim().is_empty() {
        return Err(Error::Config("storage.path must not be empty".into()));
    }

    let retention = &config.storage.retention;
    if !(60..=86_400).contains(&retention.interval_secs) {
        return Err(Error::Config(
            "storage.retention.interval_secs must be between 60 and 86400".into(),
        ));
    }
    if !(1..=100_000).contains(&retention.batch_size) {
        return Err(Error::Config(
            "storage.retention.batch_size must be between 1 and 100000".into(),
        ));
    }
    if retention.llm_usage_days != 0 && retention.llm_usage_days < 32 {
        return Err(Error::Config(
            "storage.retention.llm_usage_days must be 0 or at least 32 to cover a monthly virtual key limit"
                .into(),
        ));
    }
    if retention.usage_rollup_days != 0
        && retention.llm_usage_days != 0
        && retention.usage_rollup_days < retention.llm_usage_days
    {
        return Err(Error::Config(
            "storage.retention.usage_rollup_days must be 0 or no shorter than llm_usage_days"
                .into(),
        ));
    }

    if config.logging.level.trim().is_empty() {
        return Err(Error::Config("logging.level must not be empty".into()));
    }

    if !(5..=3_600).contains(&config.telemetry.interval_secs) {
        return Err(Error::Config(
            "telemetry.interval_secs must be between 5 and 3600".into(),
        ));
    }
    if config.telemetry.service_name.trim().is_empty() {
        return Err(Error::Config(
            "telemetry.service_name must not be empty".into(),
        ));
    }
    if let Some(otlp) = &config.telemetry.otlp {
        let parsed = url::Url::parse(&otlp.endpoint).map_err(|error| {
            Error::Config(format!(
                "invalid telemetry.otlp.endpoint '{}': {error}",
                otlp.endpoint
            ))
        })?;
        if !(parsed.scheme() == "http" || parsed.scheme() == "https") || parsed.host_str().is_none()
        {
            return Err(Error::Config(format!(
                "telemetry.otlp.endpoint '{}' must be an absolute http:// or https:// URL",
                otlp.endpoint
            )));
        }
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
    fn rejects_invalid_retention_windows() {
        let mut config = OctraConfig::default();
        config.storage.retention.interval_secs = 0;
        assert!(validate(&config).is_err());

        let mut config = OctraConfig::default();
        config.storage.retention.batch_size = 0;
        assert!(validate(&config).is_err());

        let mut config = OctraConfig::default();
        config.storage.retention.llm_usage_days = 1;
        assert!(validate(&config).is_err());

        let mut config = OctraConfig::default();
        config.storage.retention.llm_usage_days = 90;
        config.storage.retention.usage_rollup_days = 30;
        assert!(validate(&config).is_err());
    }

    #[test]
    fn validates_trusted_proxy_settings() {
        let mut config = OctraConfig::default();
        assert!(config.gateway.trusted_ips.is_empty());
        assert_eq!(config.gateway.real_ip_header, "x-forwarded-for");
        assert!(!config.gateway.real_ip_recursive);
        assert!(validate(&config).is_ok());

        config.gateway.trusted_ips = vec!["10.0.0.0/8".into(), "2001:db8::1".into()];
        assert!(validate(&config).is_ok());

        config.gateway.trusted_ips = vec!["not-an-address".into()];
        assert!(validate(&config).is_err());

        let mut config = OctraConfig::default();
        config.gateway.real_ip_header = String::new();
        assert!(validate(&config).is_err());

        config.gateway.real_ip_header = "not a header".into();
        assert!(validate(&config).is_err());

        config.gateway.real_ip_header = "x-real-ip".into();
        assert!(validate(&config).is_ok());
    }

    #[test]
    fn semantic_embedding_defaults_to_none() {
        let config = OctraConfig::default();
        assert!(config.llm.semantic_embedding.is_none());
        assert!(validate(&config).is_ok());
    }

    #[test]
    fn parses_semantic_embedding_config() {
        let config: OctraConfig = toml::from_str(
            r#"
[llm.semantic_embedding]
base_url = "https://embeddings.example.com/v1"
model = "text-embedding-3-small"
auth_ref = "env:OCTRA_EMBEDDING_KEY"
timeout_ms = 2500
cache_entries = 4096
"#,
        )
        .unwrap();
        let embedding = config.llm.semantic_embedding.as_ref().unwrap();
        assert_eq!(embedding.base_url, "https://embeddings.example.com/v1");
        assert_eq!(embedding.model, "text-embedding-3-small");
        assert_eq!(
            embedding.auth_ref.as_deref(),
            Some("env:OCTRA_EMBEDDING_KEY")
        );
        assert_eq!(embedding.timeout_ms, 2500);
        assert_eq!(embedding.cache_entries, 4096);
        assert!(validate(&config).is_ok());

        let defaults: OctraConfig = toml::from_str(
            r#"
[llm.semantic_embedding]
base_url = "http://127.0.0.1:11434/v1"
model = "nomic-embed-text"
"#,
        )
        .unwrap();
        let embedding = defaults.llm.semantic_embedding.as_ref().unwrap();
        assert_eq!(embedding.timeout_ms, 1_500);
        assert_eq!(embedding.cache_entries, 1_024);
        assert!(embedding.auth_ref.is_none());
    }

    #[test]
    fn rejects_invalid_semantic_embedding_config() {
        let mut config = OctraConfig::default();
        config.llm.semantic_embedding = Some(SemanticEmbeddingConfig {
            base_url: "http://127.0.0.1:11434/v1".into(),
            model: "nomic-embed-text".into(),
            ..SemanticEmbeddingConfig::default()
        });
        assert!(validate(&config).is_ok());

        let mut invalid_base = config.clone();
        invalid_base
            .llm
            .semantic_embedding
            .as_mut()
            .unwrap()
            .base_url = "ftp://127.0.0.1/v1".into();
        assert!(validate(&invalid_base).is_err());

        let mut invalid_model = config.clone();
        invalid_model.llm.semantic_embedding.as_mut().unwrap().model = "  ".into();
        assert!(validate(&invalid_model).is_err());

        let mut invalid_timeout = config.clone();
        invalid_timeout
            .llm
            .semantic_embedding
            .as_mut()
            .unwrap()
            .timeout_ms = 99;
        assert!(validate(&invalid_timeout).is_err());

        let mut invalid_cache = config.clone();
        invalid_cache
            .llm
            .semantic_embedding
            .as_mut()
            .unwrap()
            .cache_entries = 0;
        assert!(validate(&invalid_cache).is_err());
    }

    #[test]
    fn telemetry_defaults_to_disabled() {
        let config = OctraConfig::default();
        assert!(!config.telemetry.enabled);
        assert_eq!(config.telemetry.interval_secs, 30);
        assert_eq!(config.telemetry.service_name, "octra");
        assert!(config.telemetry.otlp.is_none());
        assert!(validate(&config).is_ok());
    }

    #[test]
    fn parses_otlp_telemetry_config() {
        let config: OctraConfig = toml::from_str(
            r#"
[telemetry]
enabled = true
interval_secs = 15
service_name = "octra-test"

[telemetry.otlp]
endpoint = "http://127.0.0.1:4318/"
headers = { authorization = "Bearer test" }
"#,
        )
        .unwrap();

        let telemetry = &config.telemetry;
        assert!(telemetry.enabled);
        assert_eq!(telemetry.interval_secs, 15);
        assert_eq!(telemetry.service_name, "octra-test");
        let otlp = telemetry.otlp.as_ref().unwrap();
        assert_eq!(otlp.endpoint, "http://127.0.0.1:4318/");
        assert_eq!(
            otlp.headers.get("authorization").map(String::as_str),
            Some("Bearer test")
        );
        assert!(validate(&config).is_ok());
    }

    #[test]
    fn rejects_invalid_telemetry_config() {
        let mut config = OctraConfig::default();
        config.telemetry.interval_secs = 4;
        assert!(validate(&config).is_err());

        let mut config = OctraConfig::default();
        config.telemetry.interval_secs = 3_601;
        assert!(validate(&config).is_err());

        let mut config = OctraConfig::default();
        config.telemetry.service_name = "  ".into();
        assert!(validate(&config).is_err());

        let mut config = OctraConfig::default();
        config.telemetry.otlp = Some(OtlpConfig {
            endpoint: "127.0.0.1:4318".into(),
            headers: BTreeMap::new(),
        });
        assert!(validate(&config).is_err());

        let mut config = OctraConfig::default();
        config.telemetry.otlp = Some(OtlpConfig {
            endpoint: "ftp://127.0.0.1:4318".into(),
            headers: BTreeMap::new(),
        });
        assert!(validate(&config).is_err());

        let mut config = OctraConfig::default();
        config.telemetry.otlp = Some(OtlpConfig {
            endpoint: "http://127.0.0.1:4318".into(),
            headers: BTreeMap::new(),
        });
        assert!(validate(&config).is_ok());
    }

    #[test]
    fn telemetry_enabled_without_otlp_is_allowed() {
        let mut config = OctraConfig::default();
        config.telemetry.enabled = true;
        assert!(validate(&config).is_ok());
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
