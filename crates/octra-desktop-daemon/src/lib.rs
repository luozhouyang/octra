//! Lifecycle management for the Octra daemon owned by the desktop host.
//!
//! The desktop app may either attach to a daemon started by an operator or
//! spawn and supervise its own child process. This crate keeps that policy
//! independent from Tauri so the behavior can be exercised with ordinary
//! `cargo test`.

use std::fs::{self, File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::net::{SocketAddr, TcpStream, ToSocketAddrs};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use serde::Serialize;

const DAEMON_BINARY_ENV: &str = "OCTRA_DAEMON_BIN";
const START_TIMEOUT_ENV: &str = "OCTRA_DAEMON_START_TIMEOUT_MS";
const HEALTH_PATH: &str = "/api/v1/system/health";
const HEALTH_PROBE_TIMEOUT: Duration = Duration::from_millis(250);
const START_POLL_INTERVAL: Duration = Duration::from_millis(100);
const STOP_TIMEOUT: Duration = Duration::from_secs(5);
const LOG_TAIL_BYTES: u64 = 4 * 1024;
const MAX_HEALTH_RESPONSE_BYTES: usize = 64 * 1024;
const MISSING_VERSION_NOTE: &str =
    "the daemon did not report its version in the health response; treating it as compatible";

/// Resolved state of the daemon from the desktop host's point of view.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct DaemonStatus {
    pub running: bool,
    pub pid: Option<u32>,
    pub binary: Option<String>,
    pub control_url: String,
    pub healthy: bool,
    pub error: Option<String>,
    /// Daemon-reported version from /api/v1/system/health, when reachable.
    pub version: Option<String>,
    /// Desktop/host expected version.
    pub expected_version: String,
    /// Same major.minor, or no daemon version available.
    pub version_compatible: bool,
}

/// Health probe result including the optional version reported by the daemon.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct HealthProbe {
    healthy: bool,
    version: Option<String>,
    /// Non-fatal note about the probe, such as a missing version field.
    note: Option<String>,
}

/// Owns the daemon child process and knows where its local files live.
pub struct DaemonManager {
    home: PathBuf,
    control_bind: String,
    binary: Option<PathBuf>,
    expected_version: String,
    child: Option<Child>,
}

impl DaemonManager {
    /// Creates a manager for `home` and the loopback control bind address.
    ///
    /// The daemon binary is discovered relative to the current executable.
    /// Discovery is retried on start when no binary was found at construction.
    pub fn new(home: PathBuf, control_bind: String) -> Self {
        Self {
            home,
            control_bind,
            binary: discover_binary(),
            expected_version: env!("CARGO_PKG_VERSION").to_string(),
            child: None,
        }
    }

    /// Creates a manager pinned to a specific binary.
    ///
    /// This is primarily useful for tests and for embedding the lifecycle
    /// logic behind a host-provided executable path.
    pub fn with_binary(home: PathBuf, control_bind: String, binary: PathBuf) -> Self {
        Self {
            home,
            control_bind,
            binary: Some(binary),
            expected_version: env!("CARGO_PKG_VERSION").to_string(),
            child: None,
        }
    }

    /// Overrides the desktop version used for compatibility checks in tests.
    pub fn with_expected_version(mut self, expected_version: impl Into<String>) -> Self {
        self.expected_version = expected_version.into();
        self
    }

    /// Reports the current child state and performs one short health probe.
    pub fn status(&mut self) -> DaemonStatus {
        let error = self.reap_child();
        let probe = self.probe_health();
        self.snapshot(probe, error)
    }

    /// Starts a daemon child unless an instance is already healthy.
    pub async fn start(&mut self) -> Result<DaemonStatus, String> {
        let _ = self.reap_child();
        let probe = self.probe_health_async().await;
        if probe.healthy {
            if let Some(error) = self.version_mismatch(&probe) {
                if self.child.is_some() {
                    self.terminate_owned_child().await?;
                    return Err(format!(
                        "{error}; the incompatible daemon child was stopped"
                    ));
                }
                return Err(format!(
                    "{error}; the daemon at {} was started outside this app, stop it before starting a compatible one",
                    self.control_url()
                ));
            }
            return Ok(self.snapshot(probe, None));
        }
        if self.child.is_some() {
            return self.wait_for_health().await;
        }

        let binary = self.resolve_binary()?;
        let log_path = self.log_path();
        if let Some(parent) = log_path.parent() {
            fs::create_dir_all(parent)
                .map_err(|err| format!("cannot create {}: {err}", parent.display()))?;
        }
        let log = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&log_path)
            .map_err(|err| format!("cannot open {}: {err}", log_path.display()))?;
        set_private_file(&log_path)?;

        let stdout = log
            .try_clone()
            .map_err(|err| format!("cannot duplicate {}: {err}", log_path.display()))?;
        let stderr = log
            .try_clone()
            .map_err(|err| format!("cannot duplicate {}: {err}", log_path.display()))?;

        let child = Command::new(&binary)
            .env("OCTRA_HOME", &self.home)
            .stdin(Stdio::null())
            .stdout(Stdio::from(stdout))
            .stderr(Stdio::from(stderr))
            .spawn()
            .map_err(|err| {
                format!(
                    "cannot start the Octra daemon at {}: {err}; rebuild it with `cargo build -p octra-daemon` or set {DAEMON_BINARY_ENV}",
                    binary.display()
                )
            })?;

        self.binary = Some(binary);
        self.child = Some(child);
        self.wait_for_health().await
    }

    /// Stops the child owned by this manager.
    ///
    /// A daemon that is healthy but was started externally is never killed;
    /// the caller gets an error explaining how to stop it instead.
    pub async fn stop(&mut self) -> Result<DaemonStatus, String> {
        if self.child.is_some() {
            self.terminate_owned_child().await?;
            let probe = self.probe_health_async().await;
            return Ok(self.snapshot(probe, None));
        }

        let probe = self.probe_health_async().await;
        if probe.healthy {
            return Err(format!(
                "the Octra daemon at {} was started outside this app; stop it from the process that launched it",
                self.control_url()
            ));
        }

        Ok(self.snapshot(probe, None))
    }

    /// Stops the owned child and starts it again.
    pub async fn restart(&mut self) -> Result<DaemonStatus, String> {
        self.stop().await?;
        self.start().await
    }

    async fn wait_for_health(&mut self) -> Result<DaemonStatus, String> {
        let timeout = start_timeout();
        let deadline = Instant::now() + timeout;
        loop {
            if let Some(error) = self.reap_child() {
                let tail = read_log_tail(&self.log_path());
                return Err(with_log_tail(error, &tail));
            }
            let probe = self.probe_health_async().await;
            if probe.healthy {
                if let Some(error) = self.version_mismatch(&probe) {
                    self.terminate_owned_child().await?;
                    return Err(format!(
                        "{error}; the incompatible daemon child was stopped"
                    ));
                }
                return Ok(self.snapshot(probe, None));
            }
            if Instant::now() >= deadline {
                self.terminate_owned_child().await?;
                return Err(format!(
                    "the Octra daemon did not become healthy at {} within {:?}; see {}",
                    self.control_url(),
                    timeout,
                    self.log_path().display()
                ));
            }
            tokio::time::sleep(START_POLL_INTERVAL).await;
        }
    }

    fn version_mismatch(&self, probe: &HealthProbe) -> Option<String> {
        let (compatible, error) =
            version_compatibility(probe.version.as_deref(), &self.expected_version);
        if compatible { None } else { error }
    }

    async fn terminate_owned_child(&mut self) -> Result<(), String> {
        let Some(mut child) = self.child.take() else {
            return Ok(());
        };
        let pid = child.id();

        #[cfg(unix)]
        {
            use nix::sys::signal::{Signal, kill};
            use nix::unistd::Pid;
            let _ = kill(Pid::from_raw(pid as i32), Signal::SIGTERM);
        }

        #[cfg(not(unix))]
        child
            .kill()
            .map_err(|err| format!("cannot stop the daemon process {pid}: {err}"))?;

        let deadline = Instant::now() + STOP_TIMEOUT;
        loop {
            match child.try_wait() {
                Ok(Some(_)) => return Ok(()),
                Ok(None) => {}
                Err(err) => return Err(format!("cannot inspect the daemon process {pid}: {err}")),
            }
            if Instant::now() >= deadline {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }

        child
            .kill()
            .map_err(|err| format!("cannot force-stop the daemon process {pid}: {err}"))?;
        let _ = child.wait();
        Ok(())
    }

    fn reap_child(&mut self) -> Option<String> {
        let log_path = self.log_path();
        let child = self.child.as_mut()?;
        match child.try_wait() {
            Ok(Some(status)) => {
                self.child = None;
                Some(format!(
                    "the Octra daemon exited with {status}; see {}",
                    log_path.display()
                ))
            }
            Ok(None) => None,
            Err(err) => Some(format!("cannot inspect the daemon child: {err}")),
        }
    }

    fn snapshot(&self, probe: HealthProbe, error: Option<String>) -> DaemonStatus {
        let pid = self.child.as_ref().map(Child::id);
        let (version_compatible, version_error) =
            version_compatibility(probe.version.as_deref(), &self.expected_version);
        DaemonStatus {
            running: self.child.is_some() || probe.healthy,
            pid,
            binary: self.binary.as_ref().map(|path| path.display().to_string()),
            control_url: self.control_url(),
            healthy: probe.healthy,
            error: merge_notes([error, probe.note, version_error]),
            version: probe.version,
            expected_version: self.expected_version.clone(),
            version_compatible,
        }
    }

    fn resolve_binary(&mut self) -> Result<PathBuf, String> {
        if let Some(binary) = self.binary.clone() {
            return Ok(binary);
        }
        let binary = discover_binary().ok_or_else(|| {
            format!(
                "cannot find `octra-daemon` next to the desktop app or in target/debug or target/release; set {DAEMON_BINARY_ENV} to an explicit path"
            )
        })?;
        self.binary = Some(binary.clone());
        Ok(binary)
    }

    fn control_url(&self) -> String {
        format!("http://{}", self.control_bind)
    }

    fn log_path(&self) -> PathBuf {
        self.home.join("daemon.log")
    }

    fn probe_health(&self) -> HealthProbe {
        probe_health(&self.control_bind, &self.home, HEALTH_PROBE_TIMEOUT)
    }

    async fn probe_health_async(&self) -> HealthProbe {
        probe_health_async(&self.control_bind, &self.home, HEALTH_PROBE_TIMEOUT).await
    }
}

impl Drop for DaemonManager {
    fn drop(&mut self) {
        if let Some(mut child) = self.child.take() {
            #[cfg(unix)]
            {
                use nix::sys::signal::{Signal, kill};
                use nix::unistd::Pid;
                let _ = kill(Pid::from_raw(child.id() as i32), Signal::SIGTERM);
            }
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

/// Finds the daemon binary without spawning it.
///
/// Search order: `OCTRA_DAEMON_BIN`, a sibling `octra-daemon`, Tauri sidecar
/// naming, then the workspace `target/debug` and `target/release` fallbacks.
pub fn discover_binary() -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?;
    let env_override = std::env::var_os(DAEMON_BINARY_ENV)
        .filter(|value| !value.is_empty())
        .map(PathBuf::from);
    discover_binary_for(&exe, env_override.as_deref())
}

fn discover_binary_for(exe: &Path, env_override: Option<&Path>) -> Option<PathBuf> {
    if let Some(path) = env_override {
        return Some(path.to_path_buf());
    }

    let directory = exe.parent()?;
    let plain = directory.join(executable_name("octra-daemon"));
    if plain.is_file() {
        return Some(plain);
    }

    let triple = target_triple();
    if !triple.is_empty() {
        let sidecar = directory.join(executable_name(&format!("octra-daemon-{triple}")));
        if sidecar.is_file() {
            return Some(sidecar);
        }
    }

    for ancestor in exe.ancestors() {
        for profile in ["debug", "release"] {
            let candidate = ancestor
                .join("target")
                .join(profile)
                .join(executable_name("octra-daemon"));
            if candidate.is_file() {
                return Some(candidate);
            }
        }
    }

    None
}

fn executable_name(name: &str) -> String {
    if cfg!(windows) {
        format!("{name}.exe")
    } else {
        name.to_string()
    }
}

fn target_triple() -> &'static str {
    option_env!("OCTRA_TARGET_TRIPLE").unwrap_or("")
}

fn start_timeout() -> Duration {
    std::env::var(START_TIMEOUT_ENV)
        .ok()
        .and_then(|raw| raw.trim().parse::<u64>().ok())
        .filter(|millis| *millis > 0)
        .map(Duration::from_millis)
        .unwrap_or(Duration::from_secs(10))
}

fn probe_health(control_bind: &str, home: &Path, timeout: Duration) -> HealthProbe {
    let Some(address) = resolve_address(control_bind) else {
        return HealthProbe::default();
    };
    let Some(token) = control_token(home) else {
        return HealthProbe::default();
    };
    let Ok(mut stream) = TcpStream::connect_timeout(&address, timeout) else {
        return HealthProbe::default();
    };
    let _ = stream.set_read_timeout(Some(timeout));
    let _ = stream.set_write_timeout(Some(timeout));

    let request = format!(
        "GET {HEALTH_PATH} HTTP/1.1\r\nHost: {control_bind}\r\nAuthorization: Bearer {token}\r\nConnection: close\r\n\r\n"
    );
    if stream.write_all(request.as_bytes()).is_err() {
        return HealthProbe::default();
    }

    let Some(response) = read_response(&mut stream) else {
        return HealthProbe::default();
    };
    parse_health_response(&response)
}

fn resolve_address(control_bind: &str) -> Option<SocketAddr> {
    control_bind.to_socket_addrs().ok()?.next()
}

async fn probe_health_async(control_bind: &str, home: &Path, timeout: Duration) -> HealthProbe {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let Some(address) = resolve_address(control_bind) else {
        return HealthProbe::default();
    };
    let Some(token) = control_token(home) else {
        return HealthProbe::default();
    };
    let request = format!(
        "GET {HEALTH_PATH} HTTP/1.1\r\nHost: {control_bind}\r\nAuthorization: Bearer {token}\r\nConnection: close\r\n\r\n"
    );

    let probe = async {
        let mut stream = tokio::net::TcpStream::connect(address).await.ok()?;
        stream.write_all(request.as_bytes()).await.ok()?;
        let mut response = Vec::new();
        let mut buffer = [0_u8; 1024];
        loop {
            let count = stream.read(&mut buffer).await.ok()?;
            if count == 0 {
                return Some(parse_health_response(&response));
            }
            response.extend_from_slice(&buffer[..count]);
            if response_complete(&response) || response.len() >= MAX_HEALTH_RESPONSE_BYTES {
                return Some(parse_health_response(&response));
            }
        }
    };

    tokio::time::timeout(timeout, probe)
        .await
        .ok()
        .flatten()
        .unwrap_or_default()
}

fn read_response(stream: &mut TcpStream) -> Option<Vec<u8>> {
    let mut response = Vec::new();
    let mut buffer = [0_u8; 1024];
    loop {
        match stream.read(&mut buffer) {
            Ok(0) => return Some(response),
            Ok(count) => {
                response.extend_from_slice(&buffer[..count]);
                if response_complete(&response) || response.len() >= MAX_HEALTH_RESPONSE_BYTES {
                    return Some(response);
                }
            }
            Err(err) if err.kind() == std::io::ErrorKind::Interrupted => {}
            Err(err)
                if matches!(
                    err.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) =>
            {
                return Some(response);
            }
            Err(_) => return None,
        }
    }
}

fn response_complete(response: &[u8]) -> bool {
    let Some(header_end) = find_header_end(response) else {
        return false;
    };
    let headers = String::from_utf8_lossy(&response[..header_end]);
    for line in headers.lines() {
        let Some((name, value)) = line.split_once(':') else {
            continue;
        };
        if name.eq_ignore_ascii_case("content-length") {
            return value
                .trim()
                .parse::<usize>()
                .map(|length| response.len().saturating_sub(header_end) >= length)
                .unwrap_or(true);
        }
    }
    true
}

fn find_header_end(response: &[u8]) -> Option<usize> {
    response
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .map(|index| index + 4)
}

fn parse_health_response(response: &[u8]) -> HealthProbe {
    let Some(header_end) = find_header_end(response) else {
        return HealthProbe::default();
    };
    let head = String::from_utf8_lossy(&response[..header_end]);
    let healthy = head.starts_with("HTTP/1.1 2") || head.starts_with("HTTP/1.0 2");
    let version = serde_json::from_slice::<serde_json::Value>(&response[header_end..])
        .ok()
        .and_then(|body| {
            body.get("version")
                .and_then(|value| value.as_str())
                .map(str::to_string)
        })
        .filter(|version| !version.trim().is_empty());
    let note = if healthy && version.is_none() {
        Some(MISSING_VERSION_NOTE.to_string())
    } else {
        None
    };
    HealthProbe {
        healthy,
        version,
        note,
    }
}

/// Compares major.minor version pairs, ignoring patch differences.
fn version_compatibility(daemon: Option<&str>, expected: &str) -> (bool, Option<String>) {
    let Some(raw) = daemon else {
        return (true, None);
    };
    let Some(reported) = parse_major_minor(raw) else {
        return (
            false,
            Some(format!(
                "the daemon reported the invalid version `{raw}`; this app expects {expected}"
            )),
        );
    };
    let Some(expected_pair) = parse_major_minor(expected) else {
        return (
            false,
            Some(format!(
                "this app's expected version `{expected}` is invalid and cannot be compared with the daemon version {raw}"
            )),
        );
    };
    if reported == expected_pair {
        (true, None)
    } else {
        (
            false,
            Some(format!(
                "the daemon version {raw} is incompatible with the desktop version {expected}; both must share the same major.minor version (daemon {}.{}, desktop {}.{})",
                reported.0, reported.1, expected_pair.0, expected_pair.1
            )),
        )
    }
}

fn parse_major_minor(version: &str) -> Option<(u64, u64)> {
    let core = version.trim();
    let core = core.strip_prefix('v').unwrap_or(core);
    let mut parts = core.split('.');
    let major = parts.next()?.parse::<u64>().ok()?;
    let minor = parts.next()?.parse::<u64>().ok()?;
    Some((major, minor))
}

fn merge_notes(notes: impl IntoIterator<Item = Option<String>>) -> Option<String> {
    let notes: Vec<String> = notes.into_iter().flatten().collect();
    if notes.is_empty() {
        None
    } else {
        Some(notes.join("; "))
    }
}

fn control_token(home: &Path) -> Option<String> {
    let token = fs::read_to_string(home.join("control.token")).ok()?;
    let token = token.trim();
    if token.is_empty() {
        None
    } else {
        Some(token.to_string())
    }
}

fn set_private_file(path: &Path) -> Result<(), String> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o600))
            .map_err(|err| format!("cannot restrict {}: {err}", path.display()))?;
    }
    #[cfg(not(unix))]
    let _ = path;
    Ok(())
}

fn read_log_tail(path: &Path) -> String {
    let Ok(mut file) = File::open(path) else {
        return String::new();
    };
    let Ok(metadata) = file.metadata() else {
        return String::new();
    };
    let start = metadata.len().saturating_sub(LOG_TAIL_BYTES);
    if file.seek(SeekFrom::Start(start)).is_err() {
        return String::new();
    }
    let mut raw = String::new();
    if file.read_to_string(&mut raw).is_err() {
        return String::new();
    }
    raw.trim_end().to_string()
}

fn with_log_tail(error: String, tail: &str) -> String {
    if tail.is_empty() {
        error
    } else {
        format!("{error}\n--- daemon.log ---\n{tail}")
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::Path;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};

    use axum::Router;
    use axum::extract::State;
    use axum::http::{HeaderMap, StatusCode, header};
    use axum::response::Json;
    use axum::routing::get;
    use serde_json::json;
    use tempfile::TempDir;

    use super::*;

    #[derive(Clone)]
    struct MockState {
        expected: String,
        healthy_after: Duration,
        started: Instant,
        available: Arc<AtomicBool>,
        version: Option<String>,
    }

    async fn mock_health_server_after(
        token: &str,
        healthy_after: Duration,
    ) -> (String, Arc<AtomicBool>, tokio::task::JoinHandle<()>) {
        mock_health_server_with_version(token, healthy_after, None).await
    }

    async fn mock_health_server_with_version(
        token: &str,
        healthy_after: Duration,
        version: Option<&str>,
    ) -> (String, Arc<AtomicBool>, tokio::task::JoinHandle<()>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let available = Arc::new(AtomicBool::new(true));
        let state = MockState {
            expected: format!("Bearer {token}"),
            healthy_after,
            started: Instant::now(),
            available: available.clone(),
            version: version.map(str::to_string),
        };
        let app = Router::new()
            .route(
                HEALTH_PATH,
                get(
                    |State(state): State<MockState>, headers: HeaderMap| async move {
                        let authorized = headers
                            .get(header::AUTHORIZATION)
                            .and_then(|value| value.to_str().ok())
                            == Some(state.expected.as_str());
                        if !authorized {
                            return (
                                StatusCode::UNAUTHORIZED,
                                Json(json!({ "error": "unauthorized" })),
                            );
                        }
                        if !state.available.load(Ordering::SeqCst)
                            || state.started.elapsed() < state.healthy_after
                        {
                            return (
                                StatusCode::SERVICE_UNAVAILABLE,
                                Json(json!({ "status": "starting" })),
                            );
                        }
                        let mut body = json!({ "status": "ok" });
                        if let Some(version) = &state.version {
                            body["version"] = json!(version);
                        }
                        (StatusCode::OK, Json(body))
                    },
                ),
            )
            .with_state(state);
        let handle = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        (address.to_string(), available, handle)
    }

    fn write_token(home: &Path, token: &str) {
        fs::write(home.join("control.token"), format!("{token}\n")).unwrap();
    }

    #[cfg(unix)]
    fn write_script(path: &Path, body: &str) {
        use std::os::unix::fs::PermissionsExt;
        fs::write(path, format!("#!/bin/sh\n{body}\n")).unwrap();
        fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
    }

    fn fake_binary(directory: &Path, name: &str) -> PathBuf {
        let path = directory.join(name);
        fs::write(&path, b"fake").unwrap();
        path
    }

    #[test]
    fn discovery_prefers_the_env_override() {
        let dir = TempDir::new().unwrap();
        let exe = fake_binary(dir.path(), "octra-desktop");
        let sibling = fake_binary(dir.path(), "octra-daemon");
        let override_path = fake_binary(dir.path(), "custom-daemon");

        let discovered = discover_binary_for(&exe, Some(&override_path)).unwrap();
        assert_eq!(discovered, override_path);
        assert_ne!(discovered, sibling);
    }

    #[test]
    fn discovery_finds_a_sibling_binary() {
        let dir = TempDir::new().unwrap();
        let exe = fake_binary(dir.path(), "octra-desktop");
        let sibling = fake_binary(dir.path(), &executable_name("octra-daemon"));

        let discovered = discover_binary_for(&exe, None).unwrap();
        assert_eq!(discovered, sibling);
    }

    #[test]
    fn discovery_finds_a_tauri_sidecar() {
        let dir = TempDir::new().unwrap();
        let exe = fake_binary(dir.path(), "octra-desktop");
        let triple = target_triple();
        assert!(
            !triple.is_empty(),
            "build.rs must provide the target triple"
        );
        let sidecar = fake_binary(
            dir.path(),
            &executable_name(&format!("octra-daemon-{triple}")),
        );

        let discovered = discover_binary_for(&exe, None).unwrap();
        assert_eq!(discovered, sidecar);
    }

    #[test]
    fn discovery_falls_back_to_the_workspace_target_directory() {
        let dir = TempDir::new().unwrap();
        let exe_dir = dir.path().join("apps/desktop/src-tauri/target/debug");
        fs::create_dir_all(&exe_dir).unwrap();
        let exe = exe_dir.join("octra-desktop");
        fs::write(&exe, b"fake").unwrap();
        let fallback_dir = dir.path().join("target/debug");
        fs::create_dir_all(&fallback_dir).unwrap();
        let fallback = fallback_dir.join(executable_name("octra-daemon"));
        fs::write(&fallback, b"fake").unwrap();

        let discovered = discover_binary_for(&exe, None).unwrap();
        assert_eq!(discovered, fallback);
    }

    #[test]
    fn version_compatibility_accepts_patch_differences() {
        assert_eq!(version_compatibility(Some("1.2.9"), "1.2.3"), (true, None));
        assert_eq!(version_compatibility(Some("1.2"), "1.2.0"), (true, None));
        assert_eq!(version_compatibility(Some("v1.2.7"), "1.2.3"), (true, None));
    }

    #[test]
    fn version_compatibility_rejects_major_minor_mismatch() {
        let (compatible, error) = version_compatibility(Some("1.3.0"), "1.2.3");
        assert!(!compatible);
        let error = error.unwrap();
        assert!(error.contains("1.3.0"), "{error}");
        assert!(error.contains("1.2.3"), "{error}");

        assert!(!version_compatibility(Some("2.2.3"), "1.2.3").0);
        assert!(!version_compatibility(Some("1.2.3"), "2.2.3").0);
    }

    #[test]
    fn missing_daemon_version_is_compatible() {
        assert_eq!(version_compatibility(None, "1.2.3"), (true, None));
    }

    #[test]
    fn invalid_daemon_version_is_incompatible() {
        let (compatible, error) = version_compatibility(Some("not-a-version"), "1.2.3");
        assert!(!compatible);
        let error = error.unwrap();
        assert!(error.contains("not-a-version"), "{error}");
        assert!(error.contains("invalid"), "{error}");
    }

    #[test]
    fn status_probes_an_external_daemon() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut request = [0_u8; 1024];
            let _ = stream.read(&mut request);
            stream
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
                .unwrap();
        });

        let home = TempDir::new().unwrap();
        write_token(home.path(), "status-token");
        let mut manager = DaemonManager::with_binary(
            home.path().to_path_buf(),
            address.to_string(),
            home.path().join("missing"),
        );

        let status = manager.status();
        assert!(status.healthy);
        assert!(status.running);
        assert_eq!(status.pid, None);
        server.join().unwrap();
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn start_and_stop_supervise_an_owned_child() {
        let home = TempDir::new().unwrap();
        let script_dir = TempDir::new().unwrap();
        let script = script_dir.path().join("fake-daemon");
        write_script(&script, "sleep 30");
        write_token(home.path(), "test-token");

        let (bind, available, server) =
            mock_health_server_after("test-token", Duration::from_secs(1)).await;
        let mut manager =
            DaemonManager::with_binary(home.path().to_path_buf(), bind.clone(), script.clone());

        let started = manager.start().await.unwrap();
        assert!(started.running);
        assert!(started.healthy);
        assert!(started.pid.is_some());
        assert_eq!(started.control_url, format!("http://{bind}"));
        assert_eq!(started.binary, Some(script.display().to_string()));

        let log = home.path().join("daemon.log");
        assert!(log.exists());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = fs::metadata(&log).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600);
        }

        available.store(false, Ordering::SeqCst);
        let stopped = manager.stop().await.unwrap();
        assert!(!stopped.running);
        assert!(!stopped.healthy);
        assert_eq!(stopped.pid, None);

        server.abort();
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn start_reuses_an_external_healthy_daemon() {
        let home = TempDir::new().unwrap();
        write_token(home.path(), "test-token");
        let (bind, _available, server) =
            mock_health_server_after("test-token", Duration::ZERO).await;
        let missing = home.path().join("missing-daemon");
        let mut manager = DaemonManager::with_binary(home.path().to_path_buf(), bind, missing);

        let started = manager.start().await.unwrap();
        assert!(started.running);
        assert!(started.healthy);
        assert_eq!(started.pid, None);
        assert_eq!(started.version, None);
        assert!(started.version_compatible);
        assert_eq!(started.expected_version, env!("CARGO_PKG_VERSION"));
        assert!(
            started
                .error
                .as_deref()
                .unwrap_or("")
                .contains("did not report")
        );

        let error = manager.stop().await.unwrap_err();
        assert!(error.contains("started outside this app"), "{error}");

        server.abort();
    }

    #[tokio::test]
    async fn start_accepts_a_compatible_external_daemon_and_reports_versions() {
        let home = TempDir::new().unwrap();
        write_token(home.path(), "test-token");
        let (bind, _available, server) =
            mock_health_server_with_version("test-token", Duration::ZERO, Some("1.2.99")).await;
        let missing = home.path().join("missing-daemon");
        let mut manager = DaemonManager::with_binary(home.path().to_path_buf(), bind, missing)
            .with_expected_version("1.2.3");

        let started = manager.start().await.unwrap();
        assert!(started.healthy);
        assert_eq!(started.version.as_deref(), Some("1.2.99"));
        assert_eq!(started.expected_version, "1.2.3");
        assert!(started.version_compatible);
        assert!(started.error.is_none());

        server.abort();
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn start_refuses_an_incompatible_external_daemon() {
        let home = TempDir::new().unwrap();
        write_token(home.path(), "test-token");
        let (bind, _available, server) =
            mock_health_server_with_version("test-token", Duration::ZERO, Some("9.9.9")).await;
        let missing = home.path().join("missing-daemon");
        let mut manager = DaemonManager::with_binary(home.path().to_path_buf(), bind, missing)
            .with_expected_version("1.2.3");

        let error = manager.start().await.unwrap_err();
        assert!(error.contains("incompatible"), "{error}");
        assert!(error.contains("9.9.9"), "{error}");
        assert!(error.contains("1.2.3"), "{error}");

        let status = manager.status();
        assert!(status.healthy);
        assert_eq!(status.version.as_deref(), Some("9.9.9"));
        assert_eq!(status.expected_version, "1.2.3");
        assert!(!status.version_compatible);

        server.abort();
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn start_stops_an_owned_child_with_an_incompatible_version() {
        let home = TempDir::new().unwrap();
        let script_dir = TempDir::new().unwrap();
        let script = script_dir.path().join("fake-daemon");
        write_script(&script, "sleep 30");
        write_token(home.path(), "test-token");

        let (bind, _available, server) = mock_health_server_with_version(
            "test-token",
            Duration::from_millis(200),
            Some("9.9.9"),
        )
        .await;
        let mut manager = DaemonManager::with_binary(home.path().to_path_buf(), bind, script)
            .with_expected_version("1.2.3");

        let error = manager.start().await.unwrap_err();
        assert!(error.contains("incompatible"), "{error}");
        assert!(error.contains("stopped"), "{error}");
        assert_eq!(manager.status().pid, None);

        server.abort();
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn status_reports_a_missing_version_as_compatible() {
        let home = TempDir::new().unwrap();
        write_token(home.path(), "test-token");
        let (bind, _available, server) =
            mock_health_server_after("test-token", Duration::ZERO).await;
        let mut manager = DaemonManager::with_binary(
            home.path().to_path_buf(),
            bind,
            home.path().join("missing-daemon"),
        )
        .with_expected_version("1.2.3");

        let status = manager.status();
        assert!(status.healthy);
        assert_eq!(status.version, None);
        assert!(status.version_compatible);
        assert!(
            status
                .error
                .as_deref()
                .unwrap_or("")
                .contains("did not report")
        );

        server.abort();
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn status_reports_an_invalid_version_as_incompatible() {
        let home = TempDir::new().unwrap();
        write_token(home.path(), "test-token");
        let (bind, _available, server) =
            mock_health_server_with_version("test-token", Duration::ZERO, Some("not-a-version"))
                .await;
        let mut manager = DaemonManager::with_binary(
            home.path().to_path_buf(),
            bind,
            home.path().join("missing-daemon"),
        )
        .with_expected_version("1.2.3");

        let status = manager.status();
        assert!(status.healthy);
        assert_eq!(status.version.as_deref(), Some("not-a-version"));
        assert!(!status.version_compatible);
        assert!(status.error.as_deref().unwrap_or("").contains("invalid"));

        server.abort();
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn start_reports_an_early_exit_and_the_log_path() {
        let home = TempDir::new().unwrap();
        let script_dir = TempDir::new().unwrap();
        let script = script_dir.path().join("failing-daemon");
        write_script(&script, "echo 'boom' >&2\nexit 3");
        write_token(home.path(), "test-token");

        let mut manager = DaemonManager::with_binary(
            home.path().to_path_buf(),
            "127.0.0.1:9".to_string(),
            script,
        );

        let error = manager.start().await.unwrap_err();
        assert!(error.contains("exited with"), "{error}");
        assert!(error.contains("daemon.log"), "{error}");
        assert!(error.contains("boom"), "{error}");
        assert_eq!(manager.status().pid, None);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn stop_without_an_owned_child_reports_the_current_state() {
        let home = TempDir::new().unwrap();
        let mut manager = DaemonManager::new(home.path().to_path_buf(), "127.0.0.1:9".to_string());

        let status = manager.stop().await.unwrap();
        assert!(!status.running);
        assert!(!status.healthy);
        assert_eq!(status.pid, None);
        assert!(status.error.is_none());
    }
}
