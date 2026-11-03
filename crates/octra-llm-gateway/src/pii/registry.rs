//! Registry and supervisor for configured PII engines.
//!
//! External engines are clients only. Managed engines are spawned by the
//! daemon over a Unix JSONL socket; a supervisor restarts them with backoff
//! and updates the shared readiness signal the plugin observes.

use std::collections::HashMap;
#[cfg(unix)]
use std::os::unix::fs::FileTypeExt;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::{Arc, RwLock};
use std::time::Duration;

use octra_pii_types::{EngineReadiness, PiiDetectorClient, PiiEngineResolver};
use octra_storage::LlmPiiEngineRecord;
use serde::Serialize;
use serde_json::Value;
use tokio::process::{Child, Command};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use super::catalog;
use super::client::{build_client, build_uds_client, readiness_code, validate_endpoint};
use super::local::LocalPiiClient;
use super::models::{LocalModelManager, ModelStatus};

type StatusMap = Arc<RwLock<HashMap<String, EngineStatus>>>;

#[derive(Debug, Clone, Serialize)]
pub struct EngineStatus {
    pub id: String,
    pub name: String,
    pub mode: String,
    pub transport: String,
    pub enabled: bool,
    pub readiness: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_error: Option<String>,
}

struct ManagedHandle {
    cancel: CancellationToken,
    task: JoinHandle<()>,
}

type ManagedBuild = (Arc<dyn PiiDetectorClient>, ManagedHandle, Arc<AtomicU8>);

impl Drop for ManagedHandle {
    fn drop(&mut self) {
        self.cancel.cancel();
        self.task.abort();
    }
}

pub struct PiiEngineRuntime {
    engines: RwLock<HashMap<String, Arc<dyn PiiDetectorClient>>>,
    status: StatusMap,
    managers: RwLock<HashMap<String, ManagedHandle>>,
    models: Option<Arc<LocalModelManager>>,
}

impl Default for PiiEngineRuntime {
    fn default() -> Self {
        Self::new()
    }
}

impl PiiEngineRuntime {
    pub fn new() -> Self {
        Self {
            engines: RwLock::new(HashMap::new()),
            status: Arc::new(RwLock::new(HashMap::new())),
            managers: RwLock::new(HashMap::new()),
            models: None,
        }
    }

    /// Enables local models stored under `root`. Without a root only external
    /// and managed engines are available.
    pub fn with_models_dir(root: PathBuf) -> Self {
        Self {
            models: Some(Arc::new(LocalModelManager::new(root))),
            ..Self::new()
        }
    }

    pub fn models(&self) -> Option<&Arc<LocalModelManager>> {
        self.models.as_ref()
    }

    pub fn model_statuses(&self) -> Vec<ModelStatus> {
        self.models
            .as_ref()
            .map(|models| {
                catalog::MODELS
                    .iter()
                    .map(|spec| models.status(spec))
                    .collect()
            })
            .unwrap_or_default()
    }

    pub fn model_status(&self, id: &str) -> Option<ModelStatus> {
        let models = self.models.as_ref()?;
        let spec = catalog::get(id)?;
        Some(models.status(spec))
    }

    /// Rebuilds the registry from stored definitions. Returns warnings for
    /// definitions that could not be built; they stay visible in status.
    pub fn reload(&self, records: &[LlmPiiEngineRecord]) -> Vec<String> {
        let old_managers = std::mem::take(&mut *self.write_managers());
        drop(old_managers);

        let mut engines = HashMap::new();
        let mut managers = HashMap::new();
        let mut warnings = Vec::new();

        {
            let mut status = self.write_status();
            status.clear();
            for record in records {
                status.insert(record.name.clone(), base_status(record));
            }
        }

        for record in records {
            if !record.enabled {
                continue;
            }
            match record.mode.as_str() {
                "external" => match build_client(record) {
                    Ok(client) => {
                        set_status(
                            &self.status,
                            &record.name,
                            readiness_label(client.readiness()),
                            None,
                        );
                        engines.insert(record.name.clone(), client);
                    }
                    Err(error) => {
                        set_status(&self.status, &record.name, "failed", Some(error.clone()));
                        warnings.push(format!(
                            "PII engine '{}' could not be built: {error}",
                            record.name
                        ));
                    }
                },
                "managed" => match self.build_managed(record) {
                    Ok((client, handle, _readiness)) => {
                        engines.insert(record.name.clone(), client);
                        managers.insert(record.name.clone(), handle);
                    }
                    Err(error) => {
                        set_status(&self.status, &record.name, "failed", Some(error.clone()));
                        warnings.push(format!(
                            "PII engine '{}' could not be started: {error}",
                            record.name
                        ));
                    }
                },
                "local" => match self.build_local(record) {
                    Ok(client) => {
                        engines.insert(record.name.clone(), client);
                    }
                    Err(error) => {
                        set_status(&self.status, &record.name, "failed", Some(error.clone()));
                        warnings.push(format!(
                            "local PII model '{}' is unavailable: {error}",
                            record.name
                        ));
                    }
                },
                other => {
                    set_status(
                        &self.status,
                        &record.name,
                        "failed",
                        Some(format!("unknown engine mode '{other}'")),
                    );
                    warnings.push(format!(
                        "PII engine '{}' has unknown mode '{other}'",
                        record.name
                    ));
                }
            }
        }

        *self.write_engines() = engines;
        *self.write_managers() = managers;
        warnings
    }

    /// Starts one engine definition without rebuilding the whole registry.
    pub fn start(&self, record: &LlmPiiEngineRecord) -> Result<(), String> {
        if self
            .engines
            .read()
            .unwrap_or_else(|error| error.into_inner())
            .contains_key(&record.name)
        {
            return Ok(());
        }
        self.write_status()
            .insert(record.name.clone(), base_status(record));
        match record.mode.as_str() {
            "external" => {
                let client = build_client(record)?;
                set_status(
                    &self.status,
                    &record.name,
                    readiness_label(client.readiness()),
                    None,
                );
                self.write_engines().insert(record.name.clone(), client);
            }
            "managed" => {
                let (client, handle, _readiness) = self.build_managed(record)?;
                self.write_engines().insert(record.name.clone(), client);
                self.write_managers().insert(record.name.clone(), handle);
            }
            "local" => {
                let client = self.build_local(record)?;
                self.write_engines().insert(record.name.clone(), client);
            }
            other => {
                let message = format!("unknown engine mode '{other}'");
                set_status(&self.status, &record.name, "failed", Some(message.clone()));
                return Err(message);
            }
        }
        Ok(())
    }

    /// Stops one engine and drops its client and supervisor.
    pub fn stop(&self, name: &str) {
        if let Some(handle) = self.write_managers().remove(name) {
            drop(handle);
        }
        self.write_engines().remove(name);
        if let Some(status) = self.write_status().get_mut(name) {
            status.readiness = "stopped";
            status.last_error = None;
        } else {
            self.write_status().insert(
                name.to_string(),
                EngineStatus {
                    id: String::new(),
                    name: name.to_string(),
                    mode: String::new(),
                    transport: String::new(),
                    enabled: false,
                    readiness: "stopped",
                    last_error: None,
                },
            );
        }
    }

    pub fn statuses(&self) -> Vec<EngineStatus> {
        let mut statuses: Vec<EngineStatus> = self
            .status
            .read()
            .unwrap_or_else(|error| error.into_inner())
            .values()
            .cloned()
            .collect();
        statuses.sort_by(|left, right| left.name.cmp(&right.name));
        statuses
    }

    pub fn get_status(&self, name: &str) -> Option<EngineStatus> {
        self.status
            .read()
            .unwrap_or_else(|error| error.into_inner())
            .get(name)
            .cloned()
    }

    fn build_managed(&self, record: &LlmPiiEngineRecord) -> Result<ManagedBuild, String> {
        let spec = ManagedSpec::from_record(record)?;
        let (client, readiness) = build_uds_client(spec.socket_string(), spec.env_timeout());
        let cancel = CancellationToken::new();
        let status = self.status.clone();
        let task_readiness = readiness.clone();
        let handle = ManagedHandle {
            cancel: cancel.clone(),
            task: tokio::spawn(async move {
                supervise(spec, status, task_readiness, cancel).await;
            }),
        };
        Ok((client, handle, readiness))
    }

    fn build_local(
        &self,
        record: &LlmPiiEngineRecord,
    ) -> Result<Arc<dyn PiiDetectorClient>, String> {
        let config: Value = serde_json::from_str(&record.config_json).unwrap_or(Value::Null);
        let model_id = config
            .get("model")
            .and_then(Value::as_str)
            .unwrap_or(&record.name);
        let spec = catalog::get(model_id)
            .ok_or_else(|| format!("unknown local PII model '{model_id}'"))?;
        let threshold = config
            .get("threshold")
            .and_then(Value::as_f64)
            .map(|value| value as f32)
            .unwrap_or(spec.default_threshold)
            .clamp(spec.floor, 0.999);
        let dir = match (&self.models, &record.model_dir) {
            (Some(models), _) => models.model_dir(spec),
            (None, Some(dir)) => PathBuf::from(dir),
            (None, None) => {
                return Err("local PII models require a model directory".to_string());
            }
        };
        let local = LocalPiiClient::load(spec, &dir, threshold);
        set_status(
            &self.status,
            &record.name,
            readiness_label(local.readiness()),
            local.error().map(str::to_string),
        );
        let client: Arc<dyn PiiDetectorClient> = local;
        Ok(client)
    }

    fn write_engines(
        &self,
    ) -> std::sync::RwLockWriteGuard<'_, HashMap<String, Arc<dyn PiiDetectorClient>>> {
        self.engines
            .write()
            .unwrap_or_else(|error| error.into_inner())
    }

    fn write_managers(&self) -> std::sync::RwLockWriteGuard<'_, HashMap<String, ManagedHandle>> {
        self.managers
            .write()
            .unwrap_or_else(|error| error.into_inner())
    }

    fn write_status(&self) -> std::sync::RwLockWriteGuard<'_, HashMap<String, EngineStatus>> {
        self.status
            .write()
            .unwrap_or_else(|error| error.into_inner())
    }
}

impl PiiEngineResolver for PiiEngineRuntime {
    fn resolve(&self, name: &str) -> Option<Arc<dyn PiiDetectorClient>> {
        self.engines
            .read()
            .unwrap_or_else(|error| error.into_inner())
            .get(name)
            .cloned()
    }
}

fn base_status(record: &LlmPiiEngineRecord) -> EngineStatus {
    let readiness = if !record.enabled {
        "disabled"
    } else if matches!(record.mode.as_str(), "managed" | "local") {
        "starting"
    } else {
        "ready"
    };
    EngineStatus {
        id: record.id.clone(),
        name: record.name.clone(),
        mode: record.mode.clone(),
        transport: record.transport.clone(),
        enabled: record.enabled,
        readiness,
        last_error: None,
    }
}

fn readiness_label(readiness: EngineReadiness) -> &'static str {
    match readiness {
        EngineReadiness::Ready => "ready",
        EngineReadiness::Starting => "starting",
        EngineReadiness::Degraded => "degraded",
        EngineReadiness::Failed => "failed",
    }
}

#[derive(Debug, Clone)]
struct ManagedSpec {
    name: String,
    program: String,
    args: Vec<String>,
    env_allow: Vec<String>,
    socket: PathBuf,
    startup_timeout: Duration,
    max_restarts: u32,
    backoff: Duration,
}

impl ManagedSpec {
    fn from_record(record: &LlmPiiEngineRecord) -> Result<Self, String> {
        if record.transport != "uds_jsonl" {
            return Err("managed PII engines require the uds_jsonl transport".to_string());
        }
        let endpoint = record
            .endpoint
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .ok_or_else(|| "managed PII engines require `endpoint`".to_string())?;
        validate_endpoint("uds_jsonl", endpoint)?;
        let socket = PathBuf::from(endpoint.strip_prefix("unix:").unwrap_or(endpoint));
        let program = record
            .command
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .ok_or_else(|| "managed PII engines require `command`".to_string())?;
        let socket_text = socket
            .to_str()
            .ok_or_else(|| "PII engine socket path is not valid UTF-8".to_string())?
            .to_string();
        let model_dir = record.model_dir.clone().unwrap_or_default();
        let mut args = Vec::with_capacity(record.args.len());
        for arg in &record.args {
            if arg.contains("{model_dir}") && model_dir.is_empty() {
                return Err("engine args use {model_dir} but no model_dir is configured".into());
            }
            args.push(
                arg.replace("{socket}", &socket_text)
                    .replace("{model_dir}", &model_dir),
            );
        }
        let config: Value = serde_json::from_str(&record.config_json).unwrap_or(Value::Null);
        let startup_timeout = config
            .get("startup_timeout_ms")
            .and_then(Value::as_u64)
            .unwrap_or(30_000)
            .max(100);
        let max_restarts = config
            .get("max_restarts")
            .and_then(Value::as_u64)
            .map(|value| value.min(100) as u32)
            .unwrap_or(5);
        let backoff = config
            .get("backoff_ms")
            .and_then(Value::as_u64)
            .unwrap_or(500)
            .max(100);
        Ok(Self {
            name: record.name.clone(),
            program: program.to_string(),
            args,
            env_allow: record.env_allow.clone(),
            socket,
            startup_timeout: Duration::from_millis(startup_timeout),
            max_restarts,
            backoff: Duration::from_millis(backoff),
        })
    }

    fn socket_string(&self) -> String {
        self.socket.to_string_lossy().into_owned()
    }

    fn env_timeout(&self) -> Duration {
        Duration::from_millis(200)
    }

    fn command(&self) -> Command {
        let mut command = Command::new(&self.program);
        command.args(&self.args);
        command.env_clear();
        for name in &self.env_allow {
            if let Ok(value) = std::env::var(name) {
                command.env(name, value);
            }
        }
        command.env("OCTRA_PII_SOCKET", &self.socket);
        command.kill_on_drop(true);
        #[cfg(unix)]
        command.process_group(0);
        command
    }
}

async fn supervise(
    spec: ManagedSpec,
    status: StatusMap,
    readiness: Arc<AtomicU8>,
    cancel: CancellationToken,
) {
    let mut attempt = 0u32;
    #[allow(unused_assignments)]
    let mut last_error: Option<String> = None;
    loop {
        if cancel.is_cancelled() {
            return;
        }
        set_status(&status, &spec.name, "starting", None);
        readiness.store(readiness_code(EngineReadiness::Starting), Ordering::Relaxed);
        match spawn_and_wait(&spec, &cancel).await {
            Ok(Some(mut child)) => {
                attempt = 0;
                set_status(&status, &spec.name, "ready", None);
                readiness.store(readiness_code(EngineReadiness::Ready), Ordering::Relaxed);
                tokio::select! {
                    _ = cancel.cancelled() => {
                        terminate(&mut child).await;
                        return;
                    }
                    exit = child.wait() => {
                        readiness.store(readiness_code(EngineReadiness::Failed), Ordering::Relaxed);
                        let message = format!("engine exited: {exit:?}");
                        set_status(
                            &status,
                            &spec.name,
                            "failed",
                            Some(message.clone()),
                        );
                        last_error = Some(message);
                    }
                }
            }
            Ok(None) => return,
            Err(error) => {
                readiness.store(readiness_code(EngineReadiness::Failed), Ordering::Relaxed);
                set_status(&status, &spec.name, "failed", Some(error.clone()));
                tracing::warn!(engine = %spec.name, error = %error, "managed PII engine failed to start");
                last_error = Some(error);
            }
        }
        attempt += 1;
        if attempt > spec.max_restarts {
            readiness.store(readiness_code(EngineReadiness::Failed), Ordering::Relaxed);
            let message = match last_error {
                Some(error) => format!("restart limit reached: {error}"),
                None => "restart limit reached".to_string(),
            };
            set_status(&status, &spec.name, "failed", Some(message));
            return;
        }
        let shift = (attempt - 1).min(6);
        let delay = spec.backoff.saturating_mul(1u32 << shift);
        tokio::select! {
            _ = cancel.cancelled() => return,
            _ = tokio::time::sleep(delay) => {}
        }
    }
}

fn set_status(status: &StatusMap, name: &str, readiness: &'static str, error: Option<String>) {
    if let Some(entry) = status
        .write()
        .unwrap_or_else(|error| error.into_inner())
        .get_mut(name)
    {
        entry.readiness = readiness;
        entry.last_error = error;
    }
}

async fn spawn_and_wait(
    spec: &ManagedSpec,
    cancel: &CancellationToken,
) -> Result<Option<Child>, String> {
    remove_stale_socket(&spec.socket);
    let mut child = spec
        .command()
        .spawn()
        .map_err(|error| format!("cannot spawn '{}': {error}", spec.program))?;
    let deadline = tokio::time::Instant::now() + spec.startup_timeout;
    loop {
        if cancel.is_cancelled() {
            terminate(&mut child).await;
            return Ok(None);
        }
        match tokio::net::UnixStream::connect(&spec.socket).await {
            Ok(_) => return Ok(Some(child)),
            Err(_) => {
                if let Some(status) = child
                    .try_wait()
                    .map_err(|error| format!("cannot inspect PII engine process: {error}"))?
                {
                    return Err(format!(
                        "engine exited before its socket became ready: {status}"
                    ));
                }
            }
        }
        if tokio::time::Instant::now() >= deadline {
            terminate(&mut child).await;
            return Err(format!(
                "engine did not become ready within {}ms",
                spec.startup_timeout.as_millis()
            ));
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

fn remove_stale_socket(path: &PathBuf) {
    if let Ok(metadata) = std::fs::symlink_metadata(path)
        && metadata.file_type().is_socket()
    {
        let _ = std::fs::remove_file(path);
    }
}

async fn terminate(child: &mut Child) {
    #[cfg(unix)]
    if let Some(pid) = child.id() {
        use nix::sys::signal::{Signal, kill};
        use nix::unistd::Pid;
        let _ = kill(Pid::from_raw(-(pid as i32)), Signal::SIGTERM);
    }
    let _ = tokio::time::timeout(Duration::from_secs(2), child.wait()).await;
    if child.try_wait().ok().flatten().is_none() {
        let _ = child.start_kill();
        let _ = child.wait().await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(name: &str, mode: &str, transport: &str) -> LlmPiiEngineRecord {
        LlmPiiEngineRecord {
            id: name.into(),
            name: name.into(),
            mode: mode.into(),
            transport: transport.into(),
            endpoint: Some(format!("/tmp/octra-{name}.sock")),
            command: Some("/bin/true".into()),
            args: vec!["--socket".into(), "{socket}".into()],
            env_allow: vec!["PATH".into()],
            model_dir: None,
            config_json: "{\"max_restarts\":0,\"backoff_ms\":100,\"startup_timeout_ms\":200}"
                .into(),
            enabled: true,
            created_at: String::new(),
            updated_at: String::new(),
        }
    }

    #[test]
    fn managed_spec_requires_uds_transport() {
        let record = record("http-managed", "managed", "http_loopback");
        assert!(ManagedSpec::from_record(&record).is_err());
    }

    #[test]
    fn managed_spec_templates_socket_arguments() {
        let record = record("templated", "managed", "uds_jsonl");
        let spec = ManagedSpec::from_record(&record).unwrap();
        assert_eq!(spec.args[0], "--socket");
        assert_eq!(spec.args[1], "/tmp/octra-templated.sock");
        assert_eq!(spec.socket_string(), "/tmp/octra-templated.sock");
    }

    #[tokio::test]
    async fn managed_spawn_failure_is_reported() {
        let mut record = record("missing-binary", "managed", "uds_jsonl");
        record.command = Some("/nonexistent/octra-pii-engine".into());
        let runtime = PiiEngineRuntime::new();
        runtime.reload(&[record]);
        tokio::time::sleep(Duration::from_millis(300)).await;
        let status = runtime.get_status("missing-binary").unwrap();
        assert_eq!(status.readiness, "failed");
        assert!(status.last_error.unwrap().contains("cannot spawn"));
    }
}
