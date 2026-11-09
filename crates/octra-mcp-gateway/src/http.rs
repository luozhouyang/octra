//! Streamable HTTP transport: JSON-RPC POSTs to `mcp_servers.url`, responses
//! either `application/json` or `text/event-stream`, with the upstream's
//! `Mcp-Session-Id` echoed on every request after `initialize`.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use octra_data_plane::sse::{SseDecoder, SseEvent};
use octra_mcp_core::jsonrpc::{JsonRpcRequest, JsonRpcResponse};
use octra_mcp_core::protocol::methods;
use octra_mcp_core::{McpServer, jsonrpc};
use reqwest::StatusCode;
use serde_json::{Value, json};
use tokio::sync::{broadcast, watch};

use crate::client::{BoxFuture, McpError, UpstreamClient};

pub const SESSION_HEADER: &str = "mcp-session-id";
pub const PROTOCOL_VERSION_HEADER: &str = "mcp-protocol-version";
const MAX_BODY_BYTES: usize = 32 * 1024 * 1024;
const NOTIFICATION_TIMEOUT: Duration = Duration::from_secs(10);
const NOTIFICATION_CAPACITY: usize = 64;
const RECONNECT_BACKOFF_SECONDS: [u64; 6] = [1, 2, 5, 10, 20, 30];

/// Shared state for the background upstream GET stream.
struct NotificationState {
    sender: broadcast::Sender<Value>,
    closed: AtomicBool,
    closed_tx: watch::Sender<bool>,
}

impl NotificationState {
    fn new() -> Self {
        let (sender, _) = broadcast::channel(NOTIFICATION_CAPACITY);
        let (closed_tx, _) = watch::channel(false);
        Self {
            sender,
            closed: AtomicBool::new(false),
            closed_tx,
        }
    }

    fn is_closed(&self) -> bool {
        self.closed.load(Ordering::SeqCst)
    }

    fn close(&self) {
        self.closed.store(true, Ordering::SeqCst);
        let _ = self.closed_tx.send(true);
    }
}

pub struct HttpClient {
    server_id: String,
    url: String,
    client: reqwest::Client,
    headers: reqwest::header::HeaderMap,
    session_id: Mutex<Option<String>>,
    protocol_version: Mutex<Option<String>>,
    next_id: AtomicU64,
    notifications: Arc<NotificationState>,
    notification_stream_started: AtomicBool,
}

impl HttpClient {
    pub fn new(server: &McpServer, client: reqwest::Client) -> Self {
        Self {
            server_id: server.id.clone(),
            url: server.url.clone().unwrap_or_default(),
            client,
            headers: build_headers(&server.headers),
            session_id: Mutex::new(None),
            protocol_version: Mutex::new(None),
            next_id: AtomicU64::new(1),
            notifications: Arc::new(NotificationState::new()),
            notification_stream_started: AtomicBool::new(false),
        }
    }

    pub fn session_id(&self) -> Option<String> {
        self.session_id.lock().ok().and_then(|guard| guard.clone())
    }

    fn request_builder(&self, method: reqwest::Method) -> reqwest::RequestBuilder {
        let mut builder = self.client.request(method, &self.url).header(
            reqwest::header::ACCEPT,
            "application/json, text/event-stream",
        );

        for (name, value) in &self.headers {
            builder = builder.header(name, value);
        }
        if let Ok(session) = self.session_id.lock()
            && let Some(session) = session.as_deref()
        {
            builder = builder.header(SESSION_HEADER, session);
        }
        if let Ok(version) = self.protocol_version.lock()
            && let Some(version) = version.as_deref()
        {
            builder = builder.header(PROTOCOL_VERSION_HEADER, version);
        }
        builder
    }

    fn remember_session(&self, response: &reqwest::Response) {
        if let Some(value) = response
            .headers()
            .get(SESSION_HEADER)
            .and_then(|value| value.to_str().ok())
            .filter(|value| !value.is_empty())
            && let Ok(mut guard) = self.session_id.lock()
        {
            *guard = Some(value.to_string());
        }
    }

    /// Starts the upstream notification GET stream after the session id is
    /// known. The stream reconnects until the client is closed.
    fn start_notification_stream(&self) {
        if self
            .notification_stream_started
            .swap(true, Ordering::SeqCst)
        {
            return;
        }
        let Some(session_id) = self.session_id() else {
            self.notification_stream_started
                .store(false, Ordering::SeqCst);
            return;
        };
        if self.notifications.is_closed() {
            return;
        }
        let Ok(runtime) = tokio::runtime::Handle::try_current() else {
            self.notification_stream_started
                .store(false, Ordering::SeqCst);
            return;
        };
        let client = self.client.clone();
        let url = self.url.clone();
        let headers = self.headers.clone();
        let protocol_version = self.protocol_version();
        let server_id = self.server_id.clone();
        let state = self.notifications.clone();
        let closed_rx = self.notifications.closed_tx.subscribe();
        runtime.spawn(async move {
            run_notification_stream(
                client,
                url,
                headers,
                session_id,
                protocol_version,
                server_id,
                state,
                closed_rx,
            )
            .await;
        });
    }

    async fn post(&self, payload: Value, timeout: Duration) -> Result<Value, McpError> {
        let request_id = payload.get("id").cloned();
        let mut response = self
            .request_builder(reqwest::Method::POST)
            .header(reqwest::header::CONTENT_TYPE, "application/json")
            .json(&payload)
            .send()
            .await
            .map_err(|err| transport_error(&err))?;

        let status = response.status();
        self.remember_session(&response);

        if status == StatusCode::METHOD_NOT_ALLOWED {
            return Err(McpError::Unsupported(
                "upstream rejected the JSON-RPC POST with 405".into(),
            ));
        }
        if !status.is_success() {
            let body = read_text(&mut response).await;
            return Err(McpError::Transport(format!(
                "upstream returned HTTP {status}: {body}"
            )));
        }

        let content_type = response
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .unwrap_or_default()
            .to_ascii_lowercase();

        if content_type.starts_with("text/event-stream") {
            return read_sse_response(&mut response, request_id, timeout).await;
        }

        let body = read_text(&mut response).await;
        if body.trim().is_empty() {
            // Accepted with no response document: valid for notifications.
            return Ok(Value::Null);
        }
        let document = pick_response(&body, request_id)?;
        // A JSON payload is a response document; validate it here so callers
        // only see well-formed JSON-RPC.
        let response: JsonRpcResponse = serde_json::from_value(document).map_err(|err| {
            McpError::Transport(format!("upstream sent a malformed response: {err}"))
        })?;
        response.into_result().map_err(McpError::upstream)
    }
}

fn build_headers(headers: &serde_json::Map<String, Value>) -> reqwest::header::HeaderMap {
    let mut map = reqwest::header::HeaderMap::new();
    for (name, value) in headers {
        let Some(raw) = value.as_str() else { continue };
        let Ok(name) = axum::http::HeaderName::from_bytes(name.as_bytes()) else {
            tracing::warn!(header = name, "skipping invalid MCP header name");
            continue;
        };
        let resolved = match octra_mcp_core::domain::secret_reference(value) {
            Some(reference) => resolve_reference(reference),
            None => Some(raw.to_string()),
        };
        match resolved.and_then(|resolved| axum::http::HeaderValue::from_str(&resolved).ok()) {
            Some(value) => {
                map.append(name, value);
            }
            None => tracing::warn!(header = %name, "skipping MCP header without a usable value"),
        }
    }
    map
}

fn resolve_reference(reference: &str) -> Option<String> {
    let (scheme, name) = reference.split_once(':')?;
    match scheme {
        "env" => std::env::var(name).ok(),
        _ => {
            tracing::warn!(
                reference = scheme,
                "header secret reference cannot be resolved"
            );
            None
        }
    }
}

fn transport_error(err: &reqwest::Error) -> McpError {
    if err.is_timeout() {
        McpError::Timeout
    } else {
        McpError::Transport(format!("upstream request failed: {err}"))
    }
}

fn reconnect_delay(attempt: usize) -> Duration {
    let seconds = RECONNECT_BACKOFF_SECONDS[attempt.min(RECONNECT_BACKOFF_SECONDS.len() - 1)];
    Duration::from_secs(seconds)
}

/// Runs the upstream GET SSE channel, reconnecting until the client closes.
#[allow(clippy::too_many_arguments)]
async fn run_notification_stream(
    client: reqwest::Client,
    url: String,
    headers: reqwest::header::HeaderMap,
    session_id: String,
    protocol_version: Option<String>,
    server_id: String,
    state: Arc<NotificationState>,
    mut closed_rx: watch::Receiver<bool>,
) {
    let mut attempt = 0usize;
    loop {
        if state.is_closed() {
            return;
        }

        let mut request_headers = headers.clone();
        request_headers.insert(
            reqwest::header::ACCEPT,
            reqwest::header::HeaderValue::from_static("text/event-stream"),
        );
        if let Ok(value) = reqwest::header::HeaderValue::from_str(&session_id) {
            request_headers.insert(SESSION_HEADER, value);
        }
        if let Some(version) = protocol_version.as_deref()
            && let Ok(value) = reqwest::header::HeaderValue::from_str(version)
        {
            request_headers.insert(PROTOCOL_VERSION_HEADER, value);
        }

        match client.get(&url).headers(request_headers).send().await {
            Ok(mut response) => {
                let status = response.status();
                if status == StatusCode::METHOD_NOT_ALLOWED || status == StatusCode::NOT_FOUND {
                    tracing::info!(
                        server = %server_id,
                        status = %status,
                        "upstream does not expose an MCP notification stream; notifications disabled"
                    );
                    return;
                }
                if !status.is_success() {
                    tracing::warn!(
                        server = %server_id,
                        status = %status,
                        "upstream MCP notification stream returned an error"
                    );
                } else {
                    attempt = 0;
                    tracing::debug!(
                        server = %server_id,
                        "connected to upstream MCP notification stream"
                    );
                    let mut decoder = SseDecoder::new();
                    loop {
                        tokio::select! {
                            chunk = response.chunk() => match chunk {
                                Ok(Some(chunk)) => {
                                    for event in decoder.push(&chunk) {
                                        forward_event(&event, &state, &server_id);
                                    }
                                }
                                Ok(None) => {
                                    if let Some(event) = decoder.finish() {
                                        forward_event(&event, &state, &server_id);
                                    }
                                    tracing::debug!(
                                        server = %server_id,
                                        "upstream MCP notification stream closed"
                                    );
                                    break;
                                }
                                Err(err) => {
                                    tracing::warn!(
                                        server = %server_id,
                                        error = %err,
                                        "upstream MCP notification stream failed"
                                    );
                                    break;
                                }
                            },
                            changed = closed_rx.changed() => {
                                if changed.is_err() || *closed_rx.borrow() {
                                    return;
                                }
                            }
                        }
                    }
                }
            }
            Err(err) => {
                tracing::warn!(
                    server = %server_id,
                    error = %err,
                    "cannot connect to the upstream MCP notification stream"
                );
            }
        }

        let delay = reconnect_delay(attempt);
        attempt = attempt.saturating_add(1);
        tokio::select! {
            _ = tokio::time::sleep(delay) => {}
            changed = closed_rx.changed() => {
                if changed.is_err() || *closed_rx.borrow() {
                    return;
                }
            }
        }
    }
}

fn forward_event(event: &SseEvent, state: &NotificationState, server_id: &str) {
    let Ok(value) = serde_json::from_str::<Value>(&event.data) else {
        return;
    };
    for notification in notification_objects(value) {
        let method = notification
            .get("method")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        if state.sender.send(notification).is_ok() {
            tracing::trace!(server = %server_id, method, "forwarded upstream MCP notification");
        }
    }
}

/// Extracts JSON-RPC notification objects (`method` present, `id` absent)
/// from one SSE payload, which may be a batch.
fn notification_objects(value: Value) -> Vec<Value> {
    match value {
        Value::Array(items) => items.into_iter().filter(is_notification).collect(),
        other if is_notification(&other) => vec![other],
        _ => Vec::new(),
    }
}

fn is_notification(value: &Value) -> bool {
    value.get("method").is_some() && value.get("id").is_none()
}

async fn read_text(response: &mut reqwest::Response) -> String {
    let mut buffer = Vec::new();
    loop {
        match response.chunk().await {
            Ok(Some(chunk)) => {
                buffer.extend_from_slice(&chunk);
                if buffer.len() > MAX_BODY_BYTES {
                    buffer.truncate(MAX_BODY_BYTES);
                    break;
                }
            }
            Ok(None) => break,
            Err(err) => return format!("<body read failed: {err}>"),
        }
    }
    String::from_utf8_lossy(&buffer).into_owned()
}

/// A JSON body may be one response, a batch, or newline-delimited responses.
/// Picks the one whose id matches the request, else the first response.
fn pick_response(body: &str, request_id: Option<Value>) -> Result<Value, McpError> {
    let trimmed = body.trim();
    if trimmed.is_empty() {
        return Err(McpError::Transport(
            "upstream returned an empty body".into(),
        ));
    }

    let mut documents = Vec::new();
    if let Ok(value) = serde_json::from_str::<Value>(trimmed) {
        documents.push(value);
    } else {
        for line in trimmed
            .lines()
            .map(str::trim)
            .filter(|line| !line.is_empty())
        {
            if let Ok(value) = serde_json::from_str::<Value>(line) {
                documents.push(value);
            }
        }
    }

    let values: Vec<Value> = documents
        .into_iter()
        .flat_map(|document| match document {
            Value::Array(items) => items,
            other => vec![other],
        })
        .collect();

    select_response(values, request_id).ok_or_else(|| {
        McpError::Transport(format!("upstream sent no JSON-RPC response: {trimmed}"))
    })
}

fn select_response(values: Vec<Value>, request_id: Option<Value>) -> Option<Value> {
    let mut fallback = None;
    for value in values {
        if value.get("id").is_none()
            || (value.get("result").is_none() && value.get("error").is_none())
        {
            continue;
        }
        if request_id
            .as_ref()
            .is_some_and(|id| value.get("id") == Some(id))
        {
            return Some(value);
        }
        fallback.get_or_insert(value);
    }
    fallback
}

async fn read_sse_response(
    response: &mut reqwest::Response,
    request_id: Option<Value>,
    _timeout: Duration,
) -> Result<Value, McpError> {
    let mut decoder = SseDecoder::new();
    let mut matched: Option<Value> = None;
    let mut fallback: Option<Value> = None;
    let mut total = 0usize;

    loop {
        let chunk = match response.chunk().await {
            Ok(Some(chunk)) => chunk,
            Ok(None) => break,
            Err(err) => return Err(transport_error(&err)),
        };
        total = total.saturating_add(chunk.len());
        if total > MAX_BODY_BYTES {
            return Err(McpError::Transport(
                "upstream SSE response is too large".into(),
            ));
        }

        for (value, is_match) in decode_events(decoder.push(&chunk), request_id.as_ref()) {
            if is_match {
                matched = Some(value);
            } else if fallback.is_none() {
                fallback = Some(value);
            }
        }
        if let Some(value) = matched.take() {
            return document_result(value);
        }
    }

    if let Some(event) = decoder.finish() {
        for (value, is_match) in decode_events(vec![event], request_id.as_ref()) {
            if is_match {
                matched = Some(value);
            } else if fallback.is_none() {
                fallback = Some(value);
            }
        }
    }

    matched
        .or(fallback)
        .map(document_result)
        .unwrap_or_else(|| {
            Err(McpError::Transport(
                "upstream SSE stream ended without a JSON-RPC response".into(),
            ))
        })
}

fn decode_events(
    events: Vec<octra_data_plane::sse::SseEvent>,
    request_id: Option<&Value>,
) -> Vec<(Value, bool)> {
    let mut out = Vec::new();
    for event in events {
        let values = match serde_json::from_str::<Value>(&event.data) {
            Ok(Value::Array(items)) => items,
            Ok(value) => vec![value],
            Err(_) => continue,
        };
        for value in values {
            if value.get("id").is_none() {
                continue;
            }
            let is_match = request_id.is_some_and(|id| value.get("id") == Some(id));
            out.push((value, is_match));
        }
    }
    out
}

fn document_result(value: Value) -> Result<Value, McpError> {
    let response: JsonRpcResponse = serde_json::from_value(value)
        .map_err(|err| McpError::Transport(format!("upstream sent a malformed response: {err}")))?;
    response.into_result().map_err(McpError::upstream)
}

impl UpstreamClient for HttpClient {
    fn request<'a>(
        &'a self,
        method: &'a str,
        params: Option<Value>,
        timeout: Duration,
    ) -> BoxFuture<'a, Result<Value, McpError>> {
        Box::pin(async move {
            let id = json!(self.next_id.fetch_add(1, Ordering::Relaxed));
            let request = JsonRpcRequest::new(id, method, params);
            let payload = serde_json::to_value(&request)
                .map_err(|err| McpError::Internal(format!("cannot encode request: {err}")))?;

            let result = tokio::time::timeout(timeout, self.post(payload, timeout))
                .await
                .map_err(|_| McpError::Timeout)??;

            if method == octra_mcp_core::protocol::methods::INITIALIZE
                && let Some(version) = result.get("protocolVersion").and_then(Value::as_str)
                && let Ok(mut guard) = self.protocol_version.lock()
            {
                *guard = Some(version.to_string());
            }
            if method == methods::INITIALIZE {
                self.start_notification_stream();
            }
            Ok(result)
        })
    }

    fn notify<'a>(&'a self, method: &'a str) -> BoxFuture<'a, Result<(), McpError>> {
        Box::pin(async move {
            let note = jsonrpc::notification(method, None);
            let exchange = self.post(note, NOTIFICATION_TIMEOUT);
            tokio::time::timeout(NOTIFICATION_TIMEOUT, exchange)
                .await
                .map_err(|_| McpError::Timeout)??;
            Ok(())
        })
    }

    fn protocol_version(&self) -> Option<String> {
        self.protocol_version
            .lock()
            .ok()
            .and_then(|guard| guard.clone())
    }

    fn subscribe_notifications(&self) -> Option<broadcast::Receiver<Value>> {
        Some(self.notifications.sender.subscribe())
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }

    fn close(&self) {
        // Forget the upstream session id; no further requests are sent on it.
        // Termination itself is best effort and not sent here because `close`
        // runs without a runtime guarantee (drop paths).
        if let Ok(mut session) = self.session_id.lock() {
            *session = None;
        }
        self.notifications.close();
    }
}

impl Drop for HttpClient {
    fn drop(&mut self) {
        self.notifications.close();
    }
}

/// Builds a JSON-RPC error document for callers that need to forward upstream
/// failures verbatim.
pub fn error_document(id: Value, error: octra_mcp_core::jsonrpc::JsonRpcError) -> Value {
    serde_json::to_value(JsonRpcResponse::failure(id, error)).unwrap_or(Value::Null)
}

/// JSON-RPC error document for a transport-level failure.
pub fn transport_document(id: Value, err: &McpError) -> Value {
    error_document(
        id,
        octra_mcp_core::jsonrpc::JsonRpcError::with_data(
            err.code(),
            err.to_string(),
            json!({ "transport": true }),
        ),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn picks_response_by_id_from_batches_and_lines() {
        let body = r#"[
            {"jsonrpc":"2.0","id":1,"result":{"a":1}},
            {"jsonrpc":"2.0","id":2,"result":{"a":2}}
        ]"#;
        let picked = pick_response(body, Some(json!(2))).unwrap();
        assert_eq!(picked["id"], 2);

        let lines = "{\"jsonrpc\":\"2.0\",\"id\":5,\"result\":{\"x\":true}}\n\
                     {\"jsonrpc\":\"2.0\",\"id\":6,\"result\":{\"x\":false}}\n";
        let picked = pick_response(lines, Some(json!(6))).unwrap();
        assert_eq!(picked["result"]["x"], false);
    }

    #[test]
    fn rejects_payloads_without_responses() {
        assert!(pick_response("", Some(json!(1))).is_err());
        assert!(pick_response("{\"jsonrpc\":\"2.0\",\"method\":\"x\"}", None).is_err());
    }

    #[test]
    fn selects_sse_response_by_id_and_ignores_notifications() {
        let decoder_events = vec![
            octra_data_plane::sse::SseEvent {
                event: None,
                data: r#"{"jsonrpc":"2.0","method":"notifications/message","params":{}}"#.into(),
                id: None,
                retry: None,
            },
            octra_data_plane::sse::SseEvent {
                event: Some("message".into()),
                data: r#"{"jsonrpc":"2.0","id":9,"result":{"ok":true}}"#.into(),
                id: None,
                retry: None,
            },
        ];
        let decoded = decode_events(decoder_events, Some(&json!(9)));
        assert_eq!(decoded.len(), 1);
        assert!(decoded[0].1);
        assert_eq!(decoded[0].0["result"]["ok"], true);
    }

    #[test]
    fn builds_literal_headers_and_skips_unresolvable_references() {
        let mut map = serde_json::Map::new();
        map.insert("Authorization".into(), json!("Bearer literal"));
        // `keyring:` cannot be resolved from the host, so the header is skipped
        // rather than sent with the reference as its value.
        map.insert("X-Secret".into(), json!("keyring:service/token"));
        let headers = build_headers(&map);
        assert_eq!(headers.get("authorization").unwrap(), "Bearer literal");
        assert!(headers.get("x-secret").is_none());
    }

    #[test]
    fn transport_errors_carry_json_rpc_codes() {
        let document = transport_document(json!(1), &McpError::Timeout);
        assert_eq!(document["error"]["code"], -32006);
        assert_eq!(document["id"], 1);
    }

    #[test]
    fn extracts_notifications_from_sse_payloads() {
        let payload = json!([
            {"jsonrpc": "2.0", "method": "notifications/tools/list_changed", "params": {"a": 1}},
            {"jsonrpc": "2.0", "id": 3, "result": {}},
            {"jsonrpc": "2.0", "id": 4, "method": "server/request"}
        ]);
        let notifications = notification_objects(payload);
        assert_eq!(notifications.len(), 1);
        assert_eq!(
            notifications[0]["method"],
            "notifications/tools/list_changed"
        );
        assert_eq!(notifications[0]["params"]["a"], 1);

        assert!(notification_objects(json!({"jsonrpc": "2.0", "id": 1, "result": {}})).is_empty());
        assert_eq!(
            notification_objects(json!({"jsonrpc": "2.0", "method": "notifications/message"}))
                .len(),
            1
        );
    }

    #[test]
    fn reconnect_backoff_grows_and_caps() {
        assert_eq!(reconnect_delay(0), Duration::from_secs(1));
        assert_eq!(reconnect_delay(1), Duration::from_secs(2));
        assert_eq!(reconnect_delay(2), Duration::from_secs(5));
        assert_eq!(reconnect_delay(100), Duration::from_secs(30));
    }
}
