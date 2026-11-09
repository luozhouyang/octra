//! Stdio transport: one child process per server, newline-delimited JSON-RPC
//! over stdin/stdout, request ids correlated through oneshot channels.
//!
//! A dedicated thread owns the child, its stdin, and the pending-request map.
//! It is spawned with the client and exits when the client closes or drops; on
//! exit it kills the child (also covered by `kill_on_drop`) and fails every
//! outstanding request, so the server manager can distinguish a dead
//! connection from a slow one and replace it.
//!
//! Blocking I/O on a plain thread keeps the async executors free: the worker
//! never holds a runtime resource while a slow child delays its answer. Calls
//! whose caller gave up (timeout, cancellation) are drained before the next
//! request is written, so a request queued behind a hung one is never executed
//! after its caller has moved on.

use std::collections::HashMap;
use std::fs::File;
use std::io::{BufRead, BufReader};
use std::path::Path;
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use octra_mcp_core::domain::secret_reference;
use octra_mcp_core::jsonrpc::{JsonRpcMessage, JsonRpcRequest, JsonRpcResponse, codes};
use octra_mcp_core::{McpServer, StdioConfig};
use serde_json::{Value, json};
use tokio::process::{Child, Command};
use tokio::sync::{broadcast, mpsc, oneshot};

use crate::client::{BoxFuture, McpError, UpstreamClient};

const NOTIFICATION_CAPACITY: usize = 64;

/// Environment variables a stdio MCP server needs to start normally. Secrets
/// and OCTRA_* variables are deliberately excluded.
const MINIMAL_ENV: [&str; 12] = [
    "PATH",
    "HOME",
    "USER",
    "LOGNAME",
    "TMPDIR",
    "TMP",
    "TEMP",
    "LANG",
    "LC_ALL",
    "TERM",
    "SHELL",
    "SYSTEMROOT",
];

enum Outgoing {
    Request {
        line: Vec<u8>,
        id: u64,
        reply: oneshot::Sender<JsonRpcResponse>,
    },
    Notify {
        line: Vec<u8>,
        reply: oneshot::Sender<Result<(), McpError>>,
    },
    /// Fails pending and queued requests immediately.
    Close,
}

/// Lifecycle state shared between the client handle and the worker thread.
struct Shared {
    closed: AtomicBool,
    /// Ids whose caller timed out before the request was written. A small
    /// Vec is enough: entries are cleared on every subsequent request.
    abandoned: Mutex<Vec<u64>>,
}

impl Shared {
    fn abandon(&self, id: u64) {
        self.abandoned
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .push(id);
    }

    /// Returns whether `id` was abandoned and clears the set.
    fn take_abandoned(&self, id: u64) -> bool {
        let mut abandoned = self
            .abandoned
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let found = abandoned.contains(&id);
        abandoned.clear();
        found
    }
}

pub struct StdioClient {
    default_timeout: Duration,
    next_id: AtomicU64,
    outgoing: mpsc::UnboundedSender<Outgoing>,
    shared: Arc<Shared>,
    notifications: broadcast::Sender<Value>,
}

impl StdioClient {
    /// Starts the child recorded in `command`, with arguments and environment
    /// from `config_json`. Header entries become child environment variables;
    /// `scheme:name` values are resolved as secret references.
    pub fn spawn(server: &McpServer, default_timeout: Duration) -> Result<Self, McpError> {
        let command = server
            .command
            .as_deref()
            .map(str::trim)
            .filter(|command| !command.is_empty())
            .ok_or_else(|| {
                McpError::Config(format!("stdio server '{}' has no command", server.name))
            })?;

        let options: StdioConfig = if server.config.is_null()
            || server.config.as_object().is_some_and(|map| map.is_empty())
        {
            StdioConfig::default()
        } else {
            serde_json::from_value(server.config.clone())
                .map_err(|err| McpError::Config(format!("invalid stdio config_json: {err}")))?
        };

        let mut cmd = Command::new(command);
        cmd.args(&options.args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true);
        // MCP servers are third-party code. Never inherit the daemon
        // environment: it may contain OCTRA_MASTER_KEY, the control token, or
        // cloud credentials. Only a small allow-list and explicitly configured
        // values reach the child.
        cmd.env_clear();
        for key in MINIMAL_ENV {
            if let Ok(value) = std::env::var(key) {
                cmd.env(key, value);
            }
        }
        cmd.env("OCTRA_MCP_SERVER", &server.name);
        if let Some(cwd) = options.cwd.as_deref() {
            cmd.current_dir(Path::new(cwd));
        }
        apply_configured_env(&mut cmd, &server.headers)?;
        for (key, value) in &options.env {
            cmd.env(key, value);
        }

        let mut child = cmd
            .spawn()
            .map_err(|err| McpError::Transport(format!("cannot start '{command}': {err}")))?;
        let stdin: std::process::ChildStdin = child
            .stdin
            .take()
            .ok_or_else(|| McpError::Transport("child stdin is unavailable".into()))?
            .into_owned_fd()
            .map(std::process::ChildStdin::from)
            .map_err(|err| McpError::Transport(format!("cannot write to child stdin: {err}")))?;
        // The blocking reader takes ownership of the raw pipe's file
        // descriptor; the async child handle keeps only the process.
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| McpError::Transport("child stdout is unavailable".into()))?;
        let stdout: File = stdout
            .into_owned_fd()
            .map(File::from)
            .map_err(|err| McpError::Transport(format!("cannot read child stdout: {err}")))?;

        let (outgoing, receiver) = mpsc::unbounded_channel();
        let (notifications, _) = broadcast::channel(NOTIFICATION_CAPACITY);
        let shared = Arc::new(Shared {
            closed: AtomicBool::new(false),
            abandoned: Mutex::new(Vec::new()),
        });
        let worker = Worker {
            child,
            stdin,
            name: server.name.clone(),
            pending: HashMap::new(),
            outgoing: receiver,
            shared: shared.clone(),
            notifications: notifications.clone(),
        };
        if let Err(err) = std::thread::Builder::new()
            .name(format!("mcp-stdio-{}", server.name))
            .spawn(move || worker.run(stdout))
        {
            return Err(McpError::Transport(format!(
                "cannot start the MCP worker thread: {err}"
            )));
        }

        Ok(Self {
            default_timeout,
            next_id: AtomicU64::new(1),
            outgoing,
            shared,
            notifications,
        })
    }

    fn effective_timeout(&self, timeout: Duration) -> Duration {
        if timeout.is_zero() {
            self.default_timeout
        } else {
            timeout
        }
    }

    /// True once the worker has exited (child gone, `close`, or drop).
    pub fn is_closed(&self) -> bool {
        self.shared.closed.load(Ordering::SeqCst)
    }
}

fn apply_configured_env(
    cmd: &mut Command,
    headers: &serde_json::Map<String, Value>,
) -> Result<(), McpError> {
    for (name, value) in headers {
        let Some(raw) = value.as_str() else {
            return Err(McpError::Config(format!("header {name} must be a string")));
        };
        match secret_reference(value) {
            Some(reference) => cmd.env(name, resolve_reference(reference)?),
            None => cmd.env(name, raw),
        };
    }
    Ok(())
}

fn resolve_reference(reference: &str) -> Result<String, McpError> {
    let (scheme, name) = reference
        .split_once(':')
        .ok_or_else(|| McpError::Config(format!("invalid secret reference: {reference}")))?;
    match scheme {
        "env" => std::env::var(name).map_err(|_| {
            McpError::Config(format!(
                "environment variable {name} referenced by a secret is not set"
            ))
        }),
        other => Err(McpError::Unsupported(format!(
            "{other}: secret references cannot be resolved from the gateway host"
        ))),
    }
}

struct Worker {
    child: Child,
    stdin: std::process::ChildStdin,
    name: String,
    pending: HashMap<u64, oneshot::Sender<JsonRpcResponse>>,
    outgoing: mpsc::UnboundedReceiver<Outgoing>,
    shared: Arc<Shared>,
    notifications: broadcast::Sender<Value>,
}

impl Worker {
    /// Runs on its own thread with a current-thread runtime. A blocking reader
    /// thread serialises the child's stdout into a channel; the command loop
    /// selects between that channel and outgoing commands, so neither a slow
    /// child nor an idle pipe can block the other side.
    fn run(self, stdout: File) {
        let runtime = match tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
        {
            Ok(runtime) => runtime,
            Err(err) => {
                tracing::error!(server = %self.name, %err, "cannot build MCP worker runtime");
                return;
            }
        };
        runtime.block_on(self.run_async(stdout));
    }

    async fn run_async(mut self, stdout: File) {
        // A dedicated blocking reader serialises stdout lines into a channel;
        // the command loop below selects between that channel and commands.
        // A plain thread is used (not `read_line` inside `select!`) because a
        // cancelled `read_line` would lose already-read bytes.
        let (lines, mut stdout_lines) = mpsc::channel::<std::io::Result<String>>(64);
        let reader = std::thread::Builder::new()
            .name(format!("mcp-stdio-read-{}", self.name))
            .spawn(move || {
                let mut reader = BufReader::new(stdout);
                let mut buffer = String::new();
                loop {
                    buffer.clear();
                    match reader.read_line(&mut buffer) {
                        Ok(0) => break,
                        Ok(_) => {
                            if lines
                                .blocking_send(Ok(std::mem::take(&mut buffer)))
                                .is_err()
                            {
                                break;
                            }
                        }
                        Err(err) => {
                            let _ = lines.blocking_send(Err(err));
                            break;
                        }
                    }
                }
            });
        if let Err(err) = reader {
            tracing::error!(server = %self.name, %err, "cannot start the MCP stdout reader");
            return self.shutdown();
        }

        loop {
            tokio::select! {
                biased;
                message = self.outgoing.recv() => match message {
                    Some(command) => {
                        if !self.handle(command) {
                            return self.shutdown();
                        }
                    }
                    None => return self.shutdown(),
                },
                line = stdout_lines.recv() => match line {
                    Some(Ok(line)) => self.dispatch(line.trim_end()),
                    Some(Err(err)) => {
                        tracing::warn!(server = %self.name, %err, "MCP stdout read failed");
                        return self.shutdown();
                    }
                    // The reader ends at EOF, which means the child is gone.
                    None => return self.shutdown(),
                },
            }
        }
    }

    /// Handles one command; returns false when the connection should close.
    fn handle(&mut self, command: Outgoing) -> bool {
        match command {
            Outgoing::Request { line, id, reply } => {
                if self.shared.take_abandoned(id) {
                    return true;
                }
                self.pending.insert(id, reply);
                if let Err(err) = self.write(&line)
                    && let Some(reply) = self.pending.remove(&id)
                {
                    let _ = reply.send(error_response(&err.to_string()));
                }
            }
            Outgoing::Notify { line, reply } => {
                let _ = reply.send(self.write(&line));
            }
            Outgoing::Close => return false,
        }
        true
    }

    fn write(&mut self, line: &[u8]) -> Result<(), McpError> {
        use std::io::Write as _;
        self.stdin
            .write_all(line)
            .and_then(|()| self.stdin.flush())
            .map_err(|err| McpError::Transport(format!("cannot write to the MCP process: {err}")))
    }

    fn dispatch(&mut self, line: &str) {
        for message in JsonRpcMessage::parse_stream_frame(line) {
            match message {
                JsonRpcMessage::Response(response) => {
                    if let Some(id) = response.id.as_u64()
                        && let Some(reply) = self.pending.remove(&id)
                    {
                        let _ = reply.send(response);
                    }
                }
                JsonRpcMessage::Request(request) if request.is_notification() => {
                    let method = request.method.clone();
                    let Ok(value) = serde_json::to_value(&request) else {
                        continue;
                    };
                    if self.notifications.send(value).is_err() {
                        tracing::debug!(
                            server = %self.name,
                            method,
                            "dropped MCP notification without subscribers"
                        );
                    }
                }
                JsonRpcMessage::Request(request) => {
                    // Octra advertises no client capabilities, so a server
                    // request (roots, sampling) has nothing to answer with.
                    tracing::debug!(method = %request.method, "ignoring server request");
                }
            }
        }
    }

    fn shutdown(mut self) {
        self.shared.closed.store(true, Ordering::SeqCst);
        if let Err(err) = self.child.start_kill() {
            tracing::warn!(server = %self.name, %err, "cannot kill the MCP process");
        }
        for (_, reply) in self.pending.drain() {
            let _ = reply.send(error_response("the MCP process exited"));
        }
        while let Ok(message) = self.outgoing.try_recv() {
            match message {
                Outgoing::Request { reply, .. } => {
                    let _ = reply.send(error_response("the MCP process exited"));
                }
                Outgoing::Notify { reply, .. } => {
                    let _ = reply.send(Err(McpError::Transport("the MCP process exited".into())));
                }
                Outgoing::Close => {}
            }
        }
    }
}

fn error_response(reason: &str) -> JsonRpcResponse {
    JsonRpcResponse::failure(
        Value::Null,
        octra_mcp_core::jsonrpc::JsonRpcError::new(codes::INTERNAL_ERROR, reason),
    )
}

fn encode_line(value: &impl serde::Serialize) -> Result<Vec<u8>, McpError> {
    let mut line = serde_json::to_vec(value)
        .map_err(|err| McpError::Internal(format!("cannot encode JSON-RPC message: {err}")))?;
    line.push(b'\n');
    Ok(line)
}

impl UpstreamClient for StdioClient {
    fn request<'a>(
        &'a self,
        method: &'a str,
        params: Option<Value>,
        timeout: Duration,
    ) -> BoxFuture<'a, Result<Value, McpError>> {
        Box::pin(async move {
            if self.is_closed() {
                return Err(McpError::Transport("the MCP process is not running".into()));
            }

            let id = self.next_id.fetch_add(1, Ordering::Relaxed);
            let request = JsonRpcRequest::new(json!(id), method, params);
            let line = encode_line(&request)?;

            let (reply, response) = oneshot::channel();
            self.outgoing
                .send(Outgoing::Request { line, id, reply })
                .map_err(|_| McpError::Transport("the MCP process is not running".into()))?;

            match tokio::time::timeout(self.effective_timeout(timeout), response).await {
                Ok(Ok(response)) => response.into_result().map_err(McpError::upstream),
                Ok(Err(_)) => Err(McpError::Transport("the MCP process exited".into())),
                Err(_) => {
                    self.shared.abandon(id);
                    Err(McpError::Timeout)
                }
            }
        })
    }

    fn notify<'a>(&'a self, method: &'a str) -> BoxFuture<'a, Result<(), McpError>> {
        Box::pin(async move {
            let note = JsonRpcRequest::notification(method, None);
            let line = encode_line(&note)?;
            let (reply, result) = oneshot::channel();
            self.outgoing
                .send(Outgoing::Notify { line, reply })
                .map_err(|_| McpError::Transport("the MCP process is not running".into()))?;
            match tokio::time::timeout(self.default_timeout, result).await {
                Ok(Ok(result)) => result,
                Ok(Err(_)) => Err(McpError::Transport("the MCP process exited".into())),
                Err(_) => Err(McpError::Timeout),
            }
        })
    }

    fn protocol_version(&self) -> Option<String> {
        None
    }

    fn subscribe_notifications(&self) -> Option<broadcast::Receiver<Value>> {
        Some(self.notifications.subscribe())
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }

    fn close(&self) {
        let _ = self.outgoing.send(Outgoing::Close);
    }
}

impl Drop for StdioClient {
    fn drop(&mut self) {
        // Dropping the sender disconnects the worker, which kills the child.
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const NOTIFICATION_SCRIPT: &str = r#"
while IFS= read -r line; do
  case "$line" in
    *'"method":"initialize"'*)
      printf '%s\n' '{"jsonrpc":"2.0","id":1,"result":{"protocolVersion":"2025-06-18","capabilities":{"tools":{}},"serverInfo":{"name":"mock","version":"0.1.0"}}}'
      printf '%s\n' '{"jsonrpc":"2.0","method":"notifications/tools/list_changed","params":{"source":"stdio"}}'
      ;;
    *'"method":"tools/list"'*)
      printf '%s\n' '{"jsonrpc":"2.0","id":2,"result":{"tools":[]}}'
      ;;
  esac
done
"#;

    #[tokio::test]
    async fn forwards_notifications_from_the_worker() {
        let server = McpServer {
            id: "stdio-notifications".to_string(),
            name: "stdio-notifications".to_string(),
            transport: octra_mcp_core::Transport::Stdio,
            mode: octra_mcp_core::domain::MCP_SERVER_MODE_UPSTREAM_SERVER.to_string(),
            sources: Vec::new(),
            route_path: None,
            acl_allow_when: Vec::new(),
            acl_deny_when: Vec::new(),
            auth_strategy_id: None,
            command: Some("sh".to_string()),
            url: None,
            enabled: true,
            config: json!({"args": ["-c", NOTIFICATION_SCRIPT]}),
            headers: serde_json::Map::new(),
            protocol_version: None,
            last_seen_at: None,
            created_at: String::new(),
            updated_at: String::new(),
        };
        let client = StdioClient::spawn(&server, Duration::from_secs(5)).unwrap();
        let mut notifications = client
            .subscribe_notifications()
            .expect("stdio exposes notifications");

        let init = crate::client::initialize(&client, Duration::from_secs(5))
            .await
            .unwrap();
        assert_eq!(init.protocol_version, "2025-06-18");

        let notification = tokio::time::timeout(Duration::from_secs(5), notifications.recv())
            .await
            .expect("notification arrives")
            .expect("notification channel stays open");
        assert_eq!(notification["method"], "notifications/tools/list_changed");
        assert_eq!(notification["params"]["source"], "stdio");
        assert!(notification.get("id").is_none());

        client.close();
    }
}
