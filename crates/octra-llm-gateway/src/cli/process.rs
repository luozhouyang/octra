//! Safe process execution for CLI providers.
//!
//! Commands are argv arrays: no shell is involved. The environment is cleared
//! and rebuilt from an explicit allowlist so unrelated secrets never leak to a
//! third-party binary. A timeout is enforced on every read, and on Unix the
//! child starts its own process group so a timeout kills the whole tree.

use std::path::PathBuf;
use std::process::Stdio;
use std::time::{Duration, Instant};

use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader, Lines};
use tokio::process::{Child, ChildStderr, ChildStdout, Command};
use tokio::task::JoinHandle;

use octra_llm_core::GatewayError;

/// Upper bound on captured stderr used to explain a failed run.
pub const MAX_STDERR_BYTES: usize = 4096;

/// Everything needed to start one CLI invocation.
#[derive(Debug, Clone)]
pub struct SpawnSpec {
    pub command: String,
    pub args: Vec<String>,
    pub env: Vec<(String, String)>,
    pub working_dir: Option<PathBuf>,
    /// Prompt written to stdin; `None` keeps stdin closed immediately.
    pub stdin: Option<String>,
    pub timeout: Duration,
    pub max_output_bytes: usize,
}

/// A running CLI invocation with bounded, timeout-guarded stdout.
pub struct ChildSession {
    child: Child,
    stdout: Lines<BufReader<ChildStdout>>,
    stderr: Option<JoinHandle<String>>,
    deadline: Instant,
    max_output_bytes: usize,
    read_bytes: usize,
    exit: Option<CliExit>,
}

/// Exit information gathered after stdout closed.
#[derive(Debug, Clone)]
pub struct CliExit {
    pub success: bool,
    pub code: Option<i32>,
    pub stderr: String,
}

fn timeout_error() -> GatewayError {
    GatewayError::timeout("CLI provider timed out").with_code("cli_timeout")
}

fn spawn_error(message: impl Into<String>) -> GatewayError {
    GatewayError::upstream(502, message).with_code("cli_spawn_failed")
}

fn io_error(message: impl Into<String>) -> GatewayError {
    GatewayError::upstream(502, message).with_code("cli_io_error")
}

impl ChildSession {
    /// Starts the command and writes the prompt to stdin in the background.
    pub fn spawn(spec: &SpawnSpec) -> Result<Self, GatewayError> {
        if spec.command.trim().is_empty() {
            return Err(spawn_error("CLI command is empty"));
        }
        if let Some(dir) = &spec.working_dir {
            std::fs::create_dir_all(dir).map_err(|error| {
                spawn_error(format!(
                    "cannot create CLI working directory {}: {error}",
                    dir.display()
                ))
            })?;
        }

        let mut command = Command::new(&spec.command);
        command
            .args(&spec.args)
            .env_clear()
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        for (key, value) in &spec.env {
            command.env(key, value);
        }
        if let Some(dir) = &spec.working_dir {
            command.current_dir(dir);
        }
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            // Own process group so a timeout can kill subprocesses too.
            command.as_std_mut().process_group(0);
        }

        let mut child = command.spawn().map_err(|error| {
            spawn_error(format!("cannot start CLI '{}': {error}", spec.command))
        })?;

        if let Some(prompt) = spec.stdin.clone() {
            let mut stdin = child.stdin.take();
            tokio::spawn(async move {
                if let Some(mut stdin) = stdin.take() {
                    let _ = stdin.write_all(prompt.as_bytes()).await;
                    let _ = stdin.shutdown().await;
                }
            });
        } else {
            drop(child.stdin.take());
        }

        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| spawn_error("CLI stdout is unavailable"))?;
        let stderr = child
            .stderr
            .take()
            .map(|stderr| tokio::spawn(drain_stderr(stderr)));
        let stdout = BufReader::new(stdout).lines();

        Ok(Self {
            child,
            stdout,
            stderr,
            deadline: Instant::now() + spec.timeout,
            max_output_bytes: spec.max_output_bytes,
            read_bytes: 0,
            exit: None,
        })
    }

    /// Reads the next stdout line, enforcing the timeout and output cap.
    pub async fn next_line(&mut self) -> Result<Option<String>, GatewayError> {
        let Some(remaining) = self.remaining() else {
            self.kill();
            return Err(timeout_error());
        };
        match tokio::time::timeout(remaining, self.stdout.next_line()).await {
            Err(_) => {
                self.kill();
                Err(timeout_error())
            }
            Ok(Err(error)) => Err(io_error(format!("cannot read CLI output: {error}"))),
            Ok(Ok(line)) => {
                if let Some(line) = &line {
                    self.read_bytes = self.read_bytes.saturating_add(line.len() + 1);
                    if self.read_bytes > self.max_output_bytes {
                        self.kill();
                        return Err(GatewayError::upstream(
                            502,
                            format!(
                                "CLI output exceeded the {} byte limit",
                                self.max_output_bytes
                            ),
                        )
                        .with_code("cli_output_limit"));
                    }
                }
                Ok(line)
            }
        }
    }

    /// Waits for the process to exit and collects its stderr.
    pub async fn finish(&mut self) -> Result<CliExit, GatewayError> {
        if let Some(exit) = &self.exit {
            return Ok(exit.clone());
        }
        let remaining = self.remaining().ok_or_else(|| {
            self.kill();
            timeout_error()
        })?;
        let status = match tokio::time::timeout(remaining, self.child.wait()).await {
            Err(_) => {
                self.kill();
                return Err(timeout_error());
            }
            Ok(Err(error)) => {
                return Err(io_error(format!("cannot wait for CLI exit: {error}")));
            }
            Ok(Ok(status)) => status,
        };
        let stderr = match self.stderr.take() {
            Some(task) => task.await.unwrap_or_default(),
            None => String::new(),
        };
        let exit = CliExit {
            success: status.success(),
            code: status.code(),
            stderr,
        };
        self.exit = Some(exit.clone());
        Ok(exit)
    }

    fn remaining(&self) -> Option<Duration> {
        self.deadline
            .checked_duration_since(Instant::now())
            .filter(|remaining| !remaining.is_zero())
    }

    /// Kills the child and, on Unix, its whole process group.
    pub fn kill(&mut self) {
        #[cfg(unix)]
        if let Some(pid) = self.child.id() {
            let _ = nix::sys::signal::killpg(
                nix::unistd::Pid::from_raw(pid as i32),
                nix::sys::signal::Signal::SIGKILL,
            );
        }
        let _ = self.child.start_kill();
        self.exit.get_or_insert_with(|| CliExit {
            success: false,
            code: None,
            stderr: "CLI process was killed".to_string(),
        });
    }
}

impl Drop for ChildSession {
    fn drop(&mut self) {
        if self.exit.is_none() {
            self.kill();
        }
    }
}

async fn drain_stderr(mut stderr: ChildStderr) -> String {
    let mut out = Vec::new();
    let mut buffer = [0u8; 1024];
    loop {
        match stderr.read(&mut buffer).await {
            Ok(0) | Err(_) => break,
            Ok(read) => {
                if out.len() < MAX_STDERR_BYTES {
                    let room = MAX_STDERR_BYTES - out.len();
                    out.extend_from_slice(&buffer[..read.min(room)]);
                }
            }
        }
    }
    String::from_utf8_lossy(&out).trim().to_string()
}

/// Runs `command` with `args` and returns its first non-empty output line.
/// Used by the control plane to validate a CLI binary before enabling it.
pub async fn probe(command: &str, args: &[String], timeout: Duration) -> Result<String, String> {
    if command.trim().is_empty() {
        return Err("CLI command is empty".to_string());
    }
    let mut process = Command::new(command);
    process
        .args(args)
        .env_clear()
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    for key in ["PATH", "HOME", "TMPDIR", "LANG", "LC_ALL"] {
        if let Ok(value) = std::env::var(key) {
            process.env(key, value);
        }
    }

    let output = tokio::time::timeout(timeout, process.output())
        .await
        .map_err(|_| "CLI probe timed out".to_string())?
        .map_err(|error| format!("cannot run '{command}': {error}"))?;

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    let version = stdout
        .lines()
        .chain(stderr.lines())
        .map(str::trim)
        .find(|line| !line.is_empty())
        .unwrap_or("")
        .to_string();

    if output.status.success() {
        Ok(version)
    } else {
        let code = output
            .status
            .code()
            .map(|code| code.to_string())
            .unwrap_or_else(|| "signal".to_string());
        Err(if version.is_empty() {
            format!("'{command}' exited with status {code}")
        } else {
            format!("'{command}' exited with status {code}: {version}")
        })
    }
}
