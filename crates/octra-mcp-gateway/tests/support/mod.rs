//! Shared fixtures for the gateway integration tests.

#![allow(dead_code)]

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use axum::Json;
use axum::Router;
use axum::body::Body;
use axum::extract::{Form, State};
use axum::http::{HeaderMap, HeaderValue, Method, StatusCode, Uri, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{any, get, post};
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

/// Test-only RSA private key. Never used outside the integration tests.
pub const TEST_RSA_PRIVATE_KEY: &str = "-----BEGIN PRIVATE KEY-----
MIIEvgIBADANBgkqhkiG9w0BAQEFAASCBKgwggSkAgEAAoIBAQDAtPuHYRsTQLaD
SeytfF+eplTZTyQ9GPf8RPu/TWpOXyDJnM66G2aXGL8o8ok3Mr/i+MUEG7fYc9+P
vFv3SBNG7BBkxvi8cB7trJie66vOVwNBB41FvkJtIQyxhkjy93DH5UvpxmmmXwAG
wz4EIxTEjLWMfG4yqeB2RjURgO46skTrLTzh/pwKztERrNEz3QOyfzCFIOoC3w5E
xnVKTXX48v0rkFb+lCKSMRf2tAKH+rXg35EQeLa35F+UFQWL2NCtqWPOqTsMXypm
3p6fVEeliTKGgOCJlQFLIal7SQbcNRl2IfqY/X+2SxrBOw88xGnYmYa6Jab82/Cp
EcvUsUl3AgMBAAECggEASNWeo8Jd+17gHKsaG1AAy/PN6fxxk+4V/HEOPXXDEi/+
MnVj5Q9qLP2bcpyIpCY4RE8GF9FqIRNakfb9Q28oagva/lv9syslZWtwm9AzNV/J
yRBTouKQNLLjG03H0J0ZMVRG5uqpD8qkNeohTGr99HPCQoDdHaT7Q3ixohX69I+3
aGKebqZH60L6n8gwgomC43KVaF6SMWwtj4O9fJKnZoodkX65txAe1pkR/ViGtrvj
W1ubZ1Ho7hzVjViEcRx+W1mp4BLaeUFO43s3b5bQvTpVQN0f9diekor/TM8aUElG
RyhcnlbdS3dO4tftVp2Az30x/QuNO3j7h1pHg07BcQKBgQD4q9ss2tEpY5GeHfKT
Ar3vWaJTRpxwhP3Q9fdyKHrAqaXUD9SKFp4OQQHjkYWBuX6NLY+kUfuGS+wYUxGK
FUxDl/FnzFG2NmPxtD0eANG8z1ndLalmxvgFK0kkUpc/xg83/fl1BW0ur39kvPqB
wXGo9I5WGvo+IAc7YT3iHPdVHQKBgQDGYuTFX/1njuFc17YF8NuOVSB8+TChbTH3
jJlXllGU/W6P/N64cd/RkNQqzMu9kFghRFhEhU1AoH7DC7Rb7Jt7AAPKN6nSY6Lg
LhJvSgyNuuOiX1QGiOAeAu7DS6S+bpmQ2evrTsddbNvZy4ulPy9fQV6UKrKsHN1m
YQwgeir4owKBgQCvnk9v4imNnXAdxvClum4BN8BYJ6ocHRBUHyHeUZA/uJAQsA1e
aJK/ZlG7Me6jqykg82hamDuWoM/gbeoZJulxY0FuGPIsSUVUUgJHyjRVr+q7NaBJ
oYNPwz4olNuUgoAE/yzJ5EczjWXe80fHk6J9vBk53luUvHHfEdHuep6CUQKBgC9P
dDkxrA/fPhVTCACgsOtfl7DWCGfmz4y2hfciTPBYA97osgCQf4zNmhte3ZOmca31
S4uDsqmf3LsM9oUZhvatSToXTjNzOWt28USDBz1hfVnCnBo22COwFVqTloUngQ3P
OnjSP68i/8cPyIyu5dA0maaCDDwJYi9BI5A3Mjo7AoGBAM9um0t5aDwYvJ8JBflW
WjA/1Kq+IUil1bML2RwPVVySuop+ZD3aEncK39C3LvJypHO6iUaQXjXU+CKQPZMn
s6HbVNMw5q1ZVAYk8M3KOFdNhV71zRHn2EH99QGRPADEraQL5F5jD0kyaKvpJ5h2
vrwktYc+KINIvEK4b4lN9EKh
-----END PRIVATE KEY-----";

/// The public half of [`TEST_RSA_PRIVATE_KEY`] as a JWKS document.
pub fn test_jwks(kid: &str) -> Value {
    json!({
        "keys": [{
            "kty": "RSA",
            "kid": kid,
            "use": "sig",
            "alg": "RS256",
            "n": "wLT7h2EbE0C2g0nsrXxfnqZU2U8kPRj3_ET7v01qTl8gyZzOuhtmlxi_KPKJNzK_4vjFBBu32HPfj7xb90gTRuwQZMb4vHAe7ayYnuurzlcDQQeNRb5CbSEMsYZI8vdwx-VL6cZppl8ABsM-BCMUxIy1jHxuMqngdkY1EYDuOrJE6y084f6cCs7REazRM90Dsn8whSDqAt8ORMZ1Sk11-PL9K5BW_pQikjEX9rQCh_q14N-REHi2t-RflBUFi9jQraljzqk7DF8qZt6en1RHpYkyhoDgiZUBSyGpe0kG3DUZdiH6mP1_tksawTsPPMRp2JmGuiWm_NvwqRHL1LFJdw",
            "e": "AQAB"
        }]
    })
}

/// Signs a test JWT with [`TEST_RSA_PRIVATE_KEY`] and `RS256`.
pub fn sign_test_token(kid: &str, claims: Value) -> String {
    use jsonwebtoken::{Algorithm, EncodingKey, Header, encode};

    let header = Header {
        alg: Algorithm::RS256,
        kid: Some(kid.to_string()),
        ..Default::default()
    };
    let key = EncodingKey::from_rsa_pem(TEST_RSA_PRIVATE_KEY.as_bytes())
        .expect("test RSA private key is valid");
    encode(&header, &claims, &key).expect("test token is encodable")
}

#[derive(Clone)]
pub struct JwksMockState {
    jwks: Arc<Mutex<Value>>,
    requests: Arc<AtomicU64>,
}

impl JwksMockState {
    pub fn set(&self, jwks: Value) {
        *self.jwks.lock().unwrap() = jwks;
    }

    pub fn requests(&self) -> u64 {
        self.requests.load(Ordering::Relaxed)
    }
}

/// A local JWKS endpoint used instead of an external identity provider.
pub struct JwksMock {
    pub addr: SocketAddr,
    pub state: JwksMockState,
    pub handle: tokio::task::JoinHandle<()>,
}

impl JwksMock {
    pub fn url(&self) -> String {
        format!("http://{}/.well-known/jwks.json", self.addr)
    }
}

impl Drop for JwksMock {
    fn drop(&mut self) {
        self.handle.abort();
    }
}

/// Starts a JWKS mock on `127.0.0.1:0`.
pub async fn start_jwks(jwks: Value) -> JwksMock {
    let state = JwksMockState {
        jwks: Arc::new(Mutex::new(jwks)),
        requests: Arc::new(AtomicU64::new(0)),
    };
    let app = Router::new()
        .route("/.well-known/jwks.json", get(serve_jwks))
        .with_state(state.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let handle = tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    JwksMock {
        addr,
        state,
        handle,
    }
}

async fn serve_jwks(State(state): State<JwksMockState>) -> Json<Value> {
    state.requests.fetch_add(1, Ordering::Relaxed);
    Json(state.jwks.lock().unwrap().clone())
}

#[derive(Clone)]
pub struct DiscoveryMockState {
    document: Arc<Mutex<Value>>,
    requests: Arc<AtomicU64>,
}

impl DiscoveryMockState {
    pub fn set(&self, document: Value) {
        *self.document.lock().unwrap() = document;
    }

    pub fn requests(&self) -> u64 {
        self.requests.load(Ordering::Relaxed)
    }
}

/// A local OpenID Connect discovery endpoint.
pub struct DiscoveryMock {
    pub addr: SocketAddr,
    pub state: DiscoveryMockState,
    pub handle: tokio::task::JoinHandle<()>,
}

impl DiscoveryMock {
    pub fn url(&self) -> String {
        format!("http://{}/.well-known/openid-configuration", self.addr)
    }
}

impl Drop for DiscoveryMock {
    fn drop(&mut self) {
        self.handle.abort();
    }
}

/// Starts a discovery mock on `127.0.0.1:0`.
pub async fn start_discovery(document: Value) -> DiscoveryMock {
    let state = DiscoveryMockState {
        document: Arc::new(Mutex::new(document)),
        requests: Arc::new(AtomicU64::new(0)),
    };
    let app = Router::new()
        .route("/.well-known/openid-configuration", get(serve_discovery))
        .with_state(state.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let handle = tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    DiscoveryMock {
        addr,
        state,
        handle,
    }
}

async fn serve_discovery(State(state): State<DiscoveryMockState>) -> Json<Value> {
    state.requests.fetch_add(1, Ordering::Relaxed);
    Json(state.document.lock().unwrap().clone())
}

#[derive(Clone)]
pub struct IntrospectionMockState {
    responses: Arc<Mutex<HashMap<String, Value>>>,
    requests: Arc<AtomicU64>,
    require_form_auth: Arc<AtomicBool>,
    last_authorization: Arc<Mutex<Option<String>>>,
    last_form: Arc<Mutex<Option<HashMap<String, String>>>>,
}

impl IntrospectionMockState {
    pub fn set_response(&self, token: &str, response: Value) {
        self.responses
            .lock()
            .unwrap()
            .insert(token.to_string(), response);
    }

    pub fn requests(&self) -> u64 {
        self.requests.load(Ordering::Relaxed)
    }

    /// When enabled, Basic authentication is rejected once so the gateway
    /// must retry with form credentials.
    pub fn require_form_auth(&self, required: bool) {
        self.require_form_auth.store(required, Ordering::Relaxed);
    }

    pub fn last_authorization(&self) -> Option<String> {
        self.last_authorization.lock().unwrap().clone()
    }

    pub fn last_form(&self) -> Option<HashMap<String, String>> {
        self.last_form.lock().unwrap().clone()
    }
}

/// A local RFC 7662 introspection endpoint.
pub struct IntrospectionMock {
    pub addr: SocketAddr,
    pub state: IntrospectionMockState,
    pub handle: tokio::task::JoinHandle<()>,
}

impl IntrospectionMock {
    pub fn url(&self) -> String {
        format!("http://{}/introspect", self.addr)
    }
}

impl Drop for IntrospectionMock {
    fn drop(&mut self) {
        self.handle.abort();
    }
}

/// Starts an introspection mock on `127.0.0.1:0`.
pub async fn start_introspection() -> IntrospectionMock {
    let state = IntrospectionMockState {
        responses: Arc::new(Mutex::new(HashMap::new())),
        requests: Arc::new(AtomicU64::new(0)),
        require_form_auth: Arc::new(AtomicBool::new(false)),
        last_authorization: Arc::new(Mutex::new(None)),
        last_form: Arc::new(Mutex::new(None)),
    };
    let app = Router::new()
        .route("/introspect", post(serve_introspection))
        .with_state(state.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let handle = tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    IntrospectionMock {
        addr,
        state,
        handle,
    }
}

async fn serve_introspection(
    State(state): State<IntrospectionMockState>,
    headers: HeaderMap,
    Form(form): Form<HashMap<String, String>>,
) -> Response {
    state.requests.fetch_add(1, Ordering::Relaxed);
    let authorization = headers
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .map(str::to_string);
    *state.last_authorization.lock().unwrap() = authorization.clone();
    *state.last_form.lock().unwrap() = Some(form.clone());

    let basic = authorization
        .as_deref()
        .is_some_and(|value| value.starts_with("Basic "));
    let form_auth = form.contains_key("client_id") && form.contains_key("client_secret");
    let accepted = if basic {
        !state.require_form_auth.load(Ordering::Relaxed)
    } else {
        form_auth
    };
    if !accepted {
        let mut response = StatusCode::UNAUTHORIZED.into_response();
        response.headers_mut().insert(
            header::WWW_AUTHENTICATE,
            HeaderValue::from_static("Basic realm=\"introspection\""),
        );
        return response;
    }

    let token = form.get("token").cloned().unwrap_or_default();
    let body = state
        .responses
        .lock()
        .unwrap()
        .get(&token)
        .cloned()
        .unwrap_or_else(|| json!({"active": false}));
    Json(body).into_response()
}

/// One request observed by the OpenAPI mock API.
#[derive(Clone, Debug)]
pub struct ApiRequest {
    pub method: String,
    pub path: String,
    pub query: String,
    pub headers: HeaderMap,
    pub body: Value,
}

#[derive(Clone)]
pub struct ApiMockState {
    pub requests: Arc<Mutex<Vec<ApiRequest>>>,
}

/// A minimal HTTP API used by OpenAPI conversion tests.
pub struct ApiMock {
    pub addr: SocketAddr,
    pub state: ApiMockState,
    pub handle: tokio::task::JoinHandle<()>,
}

impl ApiMock {
    pub fn url(&self) -> String {
        format!("http://{}", self.addr)
    }
}

impl Drop for ApiMock {
    fn drop(&mut self) {
        self.handle.abort();
    }
}

/// Starts a mock HTTP API on `127.0.0.1:0`.
///
/// Every request is recorded. `/text` returns plain text, `/fail` returns a
/// 500 with a body, and every other path echoes the request as JSON.
pub async fn start_api_mock() -> ApiMock {
    let state = ApiMockState {
        requests: Arc::new(Mutex::new(Vec::new())),
    };
    let app = Router::new()
        .route("/{*rest}", any(handle_api))
        .with_state(state.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let handle = tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    ApiMock {
        addr,
        state,
        handle,
    }
}

async fn handle_api(
    State(state): State<ApiMockState>,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
    body: Body,
) -> Response {
    let bytes = body.collect().await.unwrap().to_bytes();
    let body_value = if bytes.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&bytes)
            .unwrap_or_else(|_| Value::String(String::from_utf8_lossy(&bytes).into_owned()))
    };
    state.requests.lock().unwrap().push(ApiRequest {
        method: method.as_str().to_string(),
        path: uri.path().to_string(),
        query: uri.query().unwrap_or_default().to_string(),
        headers: headers.clone(),
        body: body_value.clone(),
    });

    match uri.path() {
        "/openapi.json" => Json(json!({
            "openapi": "3.0.3",
            "info": {"title": "Fetched", "version": "1.0.0"},
            "servers": [{"url": "https://api.example.test"}],
            "paths": {
                "/fetched": {
                    "get": {
                        "operationId": "fetchedTool",
                        "responses": {"200": {"description": "ok"}}
                    }
                }
            }
        }))
        .into_response(),
        "/text" => (
            StatusCode::OK,
            [(header::CONTENT_TYPE, "text/plain")],
            "plain body",
        )
            .into_response(),
        "/fail" => (
            StatusCode::INTERNAL_SERVER_ERROR,
            [(header::CONTENT_TYPE, "text/plain")],
            "upstream failed",
        )
            .into_response(),
        _ => Json(json!({
            "method": method.as_str(),
            "path": uri.path(),
            "query": uri.query().unwrap_or_default(),
            "authorization": header_string(&headers, header::AUTHORIZATION.as_str()),
            "x_api_key": header_string(&headers, "x-api-key"),
            "x_tenant": header_string(&headers, "x-tenant"),
            "body": body_value,
        }))
        .into_response(),
    }
}

fn header_string(headers: &HeaderMap, name: &str) -> Option<String> {
    headers
        .get(name)
        .and_then(|value| value.to_str().ok())
        .map(str::to_string)
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
        self.initialize_at("/mcp", api_key).await
    }

    pub async fn initialize_at(
        &self,
        path: &str,
        api_key: Option<&str>,
    ) -> (StatusCode, HeaderMap, Value) {
        self.post_mcp_at(
            path,
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
        self.rpc_at("/mcp", session, id, method, params, api_key)
            .await
    }

    pub async fn rpc_at(
        &self,
        path: &str,
        session: &str,
        id: Value,
        method: &str,
        params: Value,
        api_key: Option<&str>,
    ) -> (StatusCode, Value) {
        let (status, _, body) = self
            .post_mcp_at(
                path,
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
        self.post_mcp_at("/mcp", body, api_key, session).await
    }

    pub async fn post_mcp_with_content_type(
        &self,
        body: Value,
        api_key: Option<&str>,
        session: Option<&str>,
        content_type: &str,
    ) -> (StatusCode, HeaderMap, Value) {
        self.post_mcp_at_with_content_type("/mcp", body, api_key, session, content_type)
            .await
    }

    pub async fn post_mcp_at(
        &self,
        path: &str,
        body: Value,
        api_key: Option<&str>,
        session: Option<&str>,
    ) -> (StatusCode, HeaderMap, Value) {
        self.post_mcp_at_with_content_type(path, body, api_key, session, "application/json")
            .await
    }

    pub async fn post_mcp_at_with_content_type(
        &self,
        path: &str,
        body: Value,
        api_key: Option<&str>,
        session: Option<&str>,
        content_type: &str,
    ) -> (StatusCode, HeaderMap, Value) {
        let mut request = axum::http::Request::builder()
            .method("POST")
            .uri(path)
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
        self.open_session_at("/mcp", api_key).await
    }

    /// Initializes a session on a specific endpoint and returns its id.
    pub async fn open_session_at(&self, path: &str, api_key: Option<&str>) -> String {
        let (status, headers, body) = self.initialize_at(path, api_key).await;
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
