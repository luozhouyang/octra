//! Streamable HTTP transport: JSON-RPC POSTs to `mcp_servers.url`, responses
//! either `application/json` or `text/event-stream`, with the upstream's
//! `Mcp-Session-Id` echoed on every request after `initialize`.

use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use octra_data_plane::sse::SseDecoder;
use octra_mcp_core::jsonrpc::{JsonRpcRequest, JsonRpcResponse};
use octra_mcp_core::{McpServer, jsonrpc};
use reqwest::StatusCode;
use serde_json::{Value, json};

use crate::client::{BoxFuture, McpError, UpstreamClient};

pub const SESSION_HEADER: &str = "mcp-session-id";
pub const PROTOCOL_VERSION_HEADER: &str = "mcp-protocol-version";
const MAX_BODY_BYTES: usize = 32 * 1024 * 1024;
const NOTIFICATION_TIMEOUT: Duration = Duration::from_secs(10);

pub struct HttpClient {
    url: String,
    client: reqwest::Client,
    headers: reqwest::header::HeaderMap,
    session_id: Mutex<Option<String>>,
    protocol_version: Mutex<Option<String>>,
    next_id: AtomicU64,
}

impl HttpClient {
    pub fn new(server: &McpServer, client: reqwest::Client) -> Self {
        Self {
            url: server.url.clone().unwrap_or_default(),
            client,
            headers: build_headers(&server.headers),
            session_id: Mutex::new(None),
            protocol_version: Mutex::new(None),
            next_id: AtomicU64::new(1),
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
}
