//! Shared fixtures for the gateway integration tests.

#![allow(dead_code)]

use std::net::SocketAddr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use axum::Router;
use axum::body::Body;
use axum::extract::State;
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use http_body_util::BodyExt;
use octra_config::OctraConfig;
use octra_mcp_gateway::{McpControlState, McpDataState, McpRuntime};
use octra_storage::Storage;
use serde_json::{Value, json};
pub struct Upstream {
    pub addr: SocketAddr,
    pub state: MockState,
    pub handle: tokio::task::JoinHandle<()>,
}

impl Upstream {
    pub fn url(&self) -> String {
        format!("http://{}/mcp", self.addr)
    }
}

impl Drop for Upstream {
    fn drop(&mut self) {
        self.handle.abort();
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum UpstreamMode {
    Json,
    Sse,
}

#[derive(Clone)]
pub struct MockState {
    pub mode: UpstreamMode,
    pub calls: Arc<Mutex<Vec<String>>>,
    pub auth: Arc<Mutex<Vec<Option<String>>>>,
    pub session: String,
}

/// Starts a mock Streamable HTTP MCP server on `127.0.0.1:0`.
pub async fn start_upstream(mode: UpstreamMode) -> Upstream {
    let state = MockState {
        mode,
        calls: Arc::new(Mutex::new(Vec::new())),
        auth: Arc::new(Mutex::new(Vec::new())),
        session: format!(
            "session-{}",
            AtomicU64::new(1).fetch_add(1, Ordering::Relaxed)
        ),
    };

    let app = Router::new()
        .route("/mcp", post(handle))
        .with_state(state.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let handle = tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });

    Upstream {
        addr,
        state,
        handle,
    }
}

async fn handle(State(state): State<MockState>, headers: HeaderMap, body: Body) -> Response {
    let bytes = body.collect().await.unwrap().to_bytes();
    let request: Value = serde_json::from_slice(&bytes).unwrap();
    state.auth.lock().unwrap().push(
        headers
            .get(header::AUTHORIZATION)
            .and_then(|value| value.to_str().ok())
            .map(str::to_string),
    );

    let Some(id) = request.get("id").cloned() else {
        // The initialized notification is answered with 202 and no body.
        return StatusCode::ACCEPTED.into_response();
    };
    let method = request["method"].as_str().unwrap_or_default();
    let params = request.get("params").cloned().unwrap_or(Value::Null);

    let response = match method {
        "initialize" => json!({
            "jsonrpc": "2.0",
            "id": id,
            "result": {
                "protocolVersion": "2025-06-18",
                "capabilities": {"tools": {}},
                "serverInfo": {"name": "mock-upstream", "version": "0.1.0"}
            }
        }),
        "tools/list" => json!({
            "jsonrpc": "2.0",
            "id": id,
            "result": {
                "tools": [
                    {
                        "name": "echo",
                        "description": "Echoes the message argument",
                        "inputSchema": {"type": "object", "properties": {"message": {"type": "string"}}}
                    },
                    {
                        "name": "boom",
                        "description": "Fails with a tool error",
                        "inputSchema": {"type": "object"}
                    }
                ]
            }
        }),
        "tools/call" => {
            let tool = params["name"].as_str().unwrap_or_default().to_string();
            state.calls.lock().unwrap().push(tool.clone());
            let arguments = params.get("arguments").cloned().unwrap_or(Value::Null);
            if tool == "echo" {
                json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "result": {
                        "content": [{
                            "type": "text",
                            "text": arguments["message"].as_str().unwrap_or_default()
                        }],
                        "isError": false
                    }
                })
            } else {
                json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "error": {"code": -32000, "message": "tool refused"}
                })
            }
        }
        other => json!({
            "jsonrpc": "2.0",
            "id": id,
            "error": {"code": -32601, "message": format!("method not found: {other}")}
        }),
    };

    let mut response = match state.mode {
        UpstreamMode::Json => (
            StatusCode::OK,
            [(header::CONTENT_TYPE, "application/json")],
            serde_json::to_vec(&response).unwrap(),
        )
            .into_response(),
        UpstreamMode::Sse => {
            // A notification event first proves the client skips non-responses.
            let payload = format!(
                "event: message\ndata: {{\"jsonrpc\":\"2.0\",\"method\":\"notifications/message\",\"params\":{{}}}}\n\n\
                 event: message\ndata: {response}\n\n"
            );
            (
                StatusCode::OK,
                [(header::CONTENT_TYPE, "text/event-stream")],
                payload,
            )
                .into_response()
        }
    };
    response.headers_mut().insert(
        "mcp-session-id",
        HeaderValue::from_str(&state.session).unwrap(),
    );
    response
}

/// Full gateway fixture: storage, runtime, and both routers.
pub struct Gateway {
    pub storage: Arc<Storage>,
    pub runtime: Arc<McpRuntime>,
    pub config: Arc<OctraConfig>,
}

impl Gateway {
    pub async fn new() -> Self {
        Self::new_with_tool_timeout(OctraConfig::default().mcp.tool_call_timeout_secs).await
    }

    pub async fn new_with_tool_timeout(tool_call_timeout_secs: u64) -> Self {
        let storage = Arc::new(Storage::connect_in_memory().await.unwrap());
        let mut config = OctraConfig::default();
        config.mcp.tool_call_timeout_secs = tool_call_timeout_secs;
        let config = Arc::new(config);
        let runtime = Arc::new(
            McpRuntime::bootstrap(storage.clone(), config.clone())
                .await
                .unwrap(),
        );
        Self {
            storage,
            runtime,
            config,
        }
    }

    pub fn data_router(&self) -> Router {
        octra_mcp_gateway::data_plane_router(McpDataState::new(
            self.runtime.clone(),
            self.storage.clone(),
            self.config.clone(),
        ))
    }

    pub fn control_router(&self) -> Router {
        octra_mcp_gateway::control_router(McpControlState::new(
            self.runtime.clone(),
            self.storage.clone(),
        ))
    }

    /// Registers a Streamable HTTP server and discovers its tools.
    pub async fn add_http_server(&self, name: &str, upstream: &Upstream) -> String {
        let (status, server) = self
            .control(
                "POST",
                "/api/v1/mcp/servers",
                json!({
                    "name": name,
                    "transport": "http",
                    "url": upstream.url(),
                }),
            )
            .await;
        assert_eq!(
            status,
            StatusCode::CREATED,
            "create server failed: {server}"
        );
        assert_eq!(
            server["tool_count"], 2,
            "discovery should find the mock tools: {server}"
        );
        assert_eq!(server["protocol_version"], "2025-06-18");
        server["id"].as_str().unwrap().to_string()
    }

    /// Registers a Kong-style `listener` over the given source server names.
    ///
    /// Server records stay transport-backed, so the listener still needs a
    /// transport. The unroutable URL keeps discovery empty and makes the test
    /// catalog show exactly the aggregated source tools.
    pub async fn add_listener(&self, name: &str, sources: &[&str]) -> String {
        let (status, server) = self
            .control(
                "POST",
                "/api/v1/mcp/servers",
                json!({
                    "name": name,
                    "transport": "http",
                    "url": "http://127.0.0.1:9/mcp",
                    "type": "listener",
                    "sources": sources,
                }),
            )
            .await;
        assert_eq!(
            status,
            StatusCode::CREATED,
            "create listener failed: {server}"
        );
        assert_eq!(server["type"], "listener", "{server}");
        assert_eq!(server["tool_count"], 0, "{server}");
        assert_eq!(server["sources"], json!(sources));
        server["id"].as_str().unwrap().to_string()
    }

    pub async fn control(&self, method: &str, path: &str, body: Value) -> (StatusCode, Value) {
        let mut headers = HeaderMap::new();
        headers.insert(
            header::CONTENT_TYPE,
            HeaderValue::from_static("application/json"),
        );
        self.control_with(method, path, body, headers).await
    }

    pub async fn control_with(
        &self,
        method: &str,
        path: &str,
        body: Value,
        headers: HeaderMap,
    ) -> (StatusCode, Value) {
        let mut builder = axum::http::Request::builder().method(method).uri(path);
        for (name, value) in &headers {
            builder = builder.header(name, value);
        }
        let request = builder.body(Body::from(body.to_string())).unwrap();

        let response = tower::ServiceExt::oneshot(self.control_router(), request)
            .await
            .unwrap();
        decode(response).await
    }

    pub async fn control_delete(&self, path: &str) -> StatusCode {
        let request = axum::http::Request::builder()
            .method("DELETE")
            .uri(path)
            .body(Body::empty())
            .unwrap();
        let response = tower::ServiceExt::oneshot(self.control_router(), request)
            .await
            .unwrap();
        response.status()
    }

    // ----- data plane ----------------------------------------------------

    pub async fn initialize(&self, api_key: Option<&str>) -> (StatusCode, HeaderMap, Value) {
        self.post_mcp(
            json!({
                "jsonrpc": "2.0",
                "id": "init",
                "method": "initialize",
                "params": {
                    "protocolVersion": "2025-06-18",
                    "capabilities": {},
                    "clientInfo": {"name": "integration-test", "version": "0.0.1"}
                }
            }),
            api_key,
            None,
        )
        .await
    }

    pub async fn rpc(
        &self,
        session: &str,
        id: Value,
        method: &str,
        params: Value,
        api_key: Option<&str>,
    ) -> (StatusCode, Value) {
        let (status, _, body) = self
            .post_mcp(
                json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}),
                api_key,
                Some(session),
            )
            .await;
        (status, body)
    }

    pub async fn post_mcp(
        &self,
        body: Value,
        api_key: Option<&str>,
        session: Option<&str>,
    ) -> (StatusCode, HeaderMap, Value) {
        self.post_mcp_with_content_type(body, api_key, session, "application/json")
            .await
    }

    pub async fn post_mcp_with_content_type(
        &self,
        body: Value,
        api_key: Option<&str>,
        session: Option<&str>,
        content_type: &str,
    ) -> (StatusCode, HeaderMap, Value) {
        let mut request = axum::http::Request::builder()
            .method("POST")
            .uri("/mcp")
            .header(header::CONTENT_TYPE, content_type);
        if let Some(key) = api_key {
            request = request.header(header::AUTHORIZATION, format!("Bearer {key}"));
        }
        if let Some(session) = session {
            request = request.header("mcp-session-id", session);
        }
        let request = request.body(Body::from(body.to_string())).unwrap();

        let response = tower::ServiceExt::oneshot(self.data_router(), request)
            .await
            .unwrap();
        let headers = response.headers().clone();
        let (status, value) = decode(response).await;
        (status, headers, value)
    }

    /// Initializes a session and returns its id.
    pub async fn open_session(&self, api_key: Option<&str>) -> String {
        let (status, headers, body) = self.initialize(api_key).await;
        assert_eq!(status, StatusCode::OK, "initialize failed: {body}");
        assert_eq!(body["result"]["protocolVersion"], "2025-06-18");
        let session = headers
            .get("mcp-session-id")
            .expect("initialize must set Mcp-Session-Id")
            .to_str()
            .unwrap()
            .to_string();
        assert!(!session.is_empty());
        session
    }
}

pub async fn decode(response: Response) -> (StatusCode, Value) {
    let status = response.status();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    let value = if bytes.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&bytes).unwrap_or(Value::Null)
    };
    (status, value)
}

/// Creates an API key through the control plane and returns it once.
pub async fn create_key(gateway: &Gateway, name: &str) -> String {
    let (status, body) = gateway
        .control("POST", "/api/v1/mcp/keys", json!({"name": name}))
        .await;
    assert_eq!(status, StatusCode::CREATED, "create key failed: {body}");
    body["key_value"].as_str().unwrap().to_string()
}

/// Creates a keyed consumer through the control plane and returns the key
/// plaintext together with the new consumer id.
pub async fn create_keyed_consumer(gateway: &Gateway, name: &str) -> (String, String) {
    let (status, body) = gateway
        .control(
            "POST",
            "/api/v1/mcp/keys",
            json!({"name": name, "consumer_name": name}),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "create key failed: {body}");
    let key = body["key_value"].as_str().unwrap().to_string();
    let consumer_id = body["consumer_id"].as_str().unwrap().to_string();
    (key, consumer_id)
}

/// Creates a consumer group with the given consumer ids as members.
pub async fn create_consumer_group(gateway: &Gateway, name: &str, members: &[&str]) -> String {
    let group = gateway
        .storage
        .api
        .create_consumer_group(&octra_api_core::CreateConsumerGroup {
            name: name.to_string(),
            ..Default::default()
        })
        .await
        .unwrap();
    for consumer_id in members {
        gateway
            .storage
            .api
            .add_consumer_group_member(&group.id, consumer_id)
            .await
            .unwrap();
    }
    group.id
}
