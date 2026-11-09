//! Test harness: mock upstreams, in-memory storage and gateway wiring.

#![allow(dead_code)]

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use axum::Router;
use axum::body::Body;
use axum::extract::State;
use axum::http::{HeaderMap, Method, Request, StatusCode, header};
use axum::response::Response;
use axum::routing::{any, post};
use bytes::Bytes;
use http_body_util::BodyExt;
use octra_ai_config::{
    AiBalancer, AiModel, AiModelKind, AiModelTarget, AiPolicy, AiPolicyAttachment, AiPolicyScope,
    AiPolicyType, AiProvider, AiProviderAccount,
};
use octra_config::OctraConfig;
use octra_llm_gateway::{
    LlmControlState, LlmDataState, LlmPluginRuntime, LlmRuntime, LlmUpstream, control_router,
};
use octra_storage::Storage;
use serde_json::{Value, json};
use tokio::task::JoinHandle;
use tower::ServiceExt;

pub const COMPLETION_PATH: &str = "/v1/chat/completions";
pub const MESSAGES_PATH: &str = "/v1/messages";

/// A request captured by a mock upstream.
#[derive(Debug, Clone)]
pub struct Captured {
    pub method: Method,
    pub path: String,
    pub headers: HeaderMap,
    pub body: Value,
}

impl Captured {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers.get(name).and_then(|value| value.to_str().ok())
    }
}

pub struct MockUpstream {
    pub base_url: String,
    pub captured: Arc<Mutex<Vec<Captured>>>,
    handle: JoinHandle<()>,
}

impl Drop for MockUpstream {
    fn drop(&mut self) {
        self.handle.abort();
    }
}

pub fn test_upstream(config: &OctraConfig) -> LlmUpstream {
    LlmUpstream::with_test_key(config, "test-key")
}

impl MockUpstream {
    pub async fn start(config: &OctraConfig, handler: fn(Captured) -> Response) -> Self {
        let captured: Arc<Mutex<Vec<Captured>>> = Arc::new(Mutex::new(Vec::new()));
        let app = Router::new()
            .route("/{*path}", any(capture))
            .with_state(HandlerState {
                captured: captured.clone(),
                handler,
                upstream: test_upstream(config),
            });
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let handle = tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        Self {
            base_url: format!("http://{address}/v1"),
            captured,
            handle,
        }
    }

    pub fn requests(&self) -> Vec<Captured> {
        self.captured.lock().unwrap().clone()
    }

    pub fn request_count(&self) -> usize {
        self.captured.lock().unwrap().len()
    }
}

/// A local OpenAI-compatible embedding endpoint. It records parsed request
/// bodies and delegates the response to the supplied handler.
pub struct MockEmbedding {
    pub base_url: String,
    captured: Arc<Mutex<Vec<Captured>>>,
    handle: JoinHandle<()>,
}

impl Drop for MockEmbedding {
    fn drop(&mut self) {
        self.handle.abort();
    }
}

impl MockEmbedding {
    pub async fn start(handler: fn(Captured) -> Response) -> Self {
        let captured: Arc<Mutex<Vec<Captured>>> = Arc::new(Mutex::new(Vec::new()));
        let app = Router::new()
            .route("/{*path}", any(embedding_capture))
            .with_state(EmbeddingHandlerState {
                captured: captured.clone(),
                handler,
            });
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let handle = tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        Self {
            base_url: format!("http://{address}/v1"),
            captured,
            handle,
        }
    }

    pub fn requests(&self) -> Vec<Captured> {
        self.captured.lock().unwrap().clone()
    }

    pub fn request_count(&self) -> usize {
        self.captured.lock().unwrap().len()
    }
}

#[derive(Clone)]
struct EmbeddingHandlerState {
    captured: Arc<Mutex<Vec<Captured>>>,
    handler: fn(Captured) -> Response,
}

async fn embedding_capture(
    State(state): State<EmbeddingHandlerState>,
    method: Method,
    uri: axum::http::Uri,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let request = Captured {
        method,
        path: uri.path().to_string(),
        headers,
        body: serde_json::from_slice(&body).unwrap_or(Value::Null),
    };
    state.captured.lock().unwrap().push(request.clone());
    (state.handler)(request)
}

#[derive(Clone)]
struct HandlerState {
    captured: Arc<Mutex<Vec<Captured>>>,
    handler: fn(Captured) -> Response,
    upstream: LlmUpstream,
}

async fn capture(
    State(state): State<HandlerState>,
    method: Method,
    uri: axum::http::Uri,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let request = Captured {
        method,
        path: uri.path().to_string(),
        headers: headers.clone(),
        body: serde_json::from_slice(&body).unwrap_or(Value::Null),
    };
    if let Some(expected) = state.upstream.test_key() {
        let bearer = format!("Bearer {expected}");
        let authorized = headers.get("authorization").and_then(|v| v.to_str().ok())
            == Some(bearer.as_str())
            || headers.get("x-api-key").and_then(|v| v.to_str().ok()) == Some(expected)
            || headers.get("x-goog-api-key").and_then(|v| v.to_str().ok()) == Some(expected);
        if !authorized {
            return json_status(
                StatusCode::UNAUTHORIZED,
                json!({"error": {"message": "upstream rejected the credentials"}}),
            );
        }
    }
    state.captured.lock().unwrap().push(request.clone());
    (state.handler)(request)
}

pub fn json_response(value: Value) -> Response {
    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(value.to_string()))
        .unwrap()
}

pub fn json_status(status: StatusCode, value: Value) -> Response {
    Response::builder()
        .status(status)
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(value.to_string()))
        .unwrap()
}

// ---------------------------------------------------------------------------
// Fake CLI binaries

/// A temporary executable script used to stand in for a local agent CLI.
pub struct FakeCli {
    pub path: String,
    dir: std::path::PathBuf,
}

impl Drop for FakeCli {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

#[cfg(unix)]
pub fn fake_cli(script: &str) -> FakeCli {
    use std::os::unix::fs::PermissionsExt;

    let dir = std::env::temp_dir().join(format!("octra-cli-{}", uuid::Uuid::new_v4().simple()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("cli.sh");
    std::fs::write(&path, format!("#!/bin/sh\n{script}\n")).unwrap();
    let mut permissions = std::fs::metadata(&path).unwrap().permissions();
    permissions.set_mode(0o755);
    std::fs::set_permissions(&path, permissions).unwrap();
    FakeCli {
        path: path.to_string_lossy().into_owned(),
        dir,
    }
}

// ---------------------------------------------------------------------------
// OAuth token endpoint mock

#[derive(Clone)]
struct TokenEndpointState {
    forms: Arc<Mutex<Vec<String>>>,
    issued: Arc<Mutex<u64>>,
}

/// A local OAuth token endpoint that records form bodies and issues sequential
/// access/refresh token pairs.
pub struct MockTokenEndpoint {
    pub url: String,
    pub forms: Arc<Mutex<Vec<String>>>,
    handle: JoinHandle<()>,
}

impl Drop for MockTokenEndpoint {
    fn drop(&mut self) {
        self.handle.abort();
    }
}

async fn issue_token(State(state): State<TokenEndpointState>, body: String) -> axum::Json<Value> {
    state.forms.lock().unwrap().push(body);
    let mut issued = state.issued.lock().unwrap();
    *issued += 1;
    axum::Json(json!({
        "access_token": format!("access-{issued}"),
        "refresh_token": format!("refresh-{issued}"),
        "token_type": "Bearer",
        "expires_in": 3600,
        "scope": "read offline_access"
    }))
}

pub async fn mock_token_endpoint() -> MockTokenEndpoint {
    let forms = Arc::new(Mutex::new(Vec::new()));
    let state = TokenEndpointState {
        forms: forms.clone(),
        issued: Arc::new(Mutex::new(0)),
    };
    let app = Router::new()
        .route("/token", post(issue_token))
        .with_state(state);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let handle = tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    MockTokenEndpoint {
        url: format!("http://{address}/token"),
        forms,
        handle,
    }
}

pub fn sse_response(body: String) -> Response {
    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "text/event-stream")
        .body(Body::from(body))
        .unwrap()
}

/// A fixed OpenAI chat completion for model `m`.
pub fn openai_completion(text: &str) -> Value {
    json!({
        "id": "chatcmpl-test",
        "object": "chat.completion",
        "created": 1,
        "model": "m",
        "choices": [{
            "index": 0,
            "message": {"role": "assistant", "content": text},
            "finish_reason": "stop"
        }],
        "usage": {"prompt_tokens": 11, "completion_tokens": 7, "total_tokens": 18}
    })
}

pub fn anthropic_message(text: &str) -> Value {
    json!({
        "id": "msg_test",
        "type": "message",
        "role": "assistant",
        "model": "m",
        "content": [{"type": "text", "text": text}],
        "stop_reason": "end_turn",
        "stop_sequence": null,
        "usage": {"input_tokens": 13, "output_tokens": 9}
    })
}

pub fn openai_tool_completion() -> Value {
    json!({
        "id": "chatcmpl-tool",
        "object": "chat.completion",
        "created": 1,
        "model": "m",
        "choices": [{
            "index": 0,
            "message": {
                "role": "assistant",
                "content": null,
                "tool_calls": [{
                    "id": "call_t1",
                    "type": "function",
                    "function": {"name": "get_weather", "arguments": "{\"city\":\"Paris\"}"}
                }]
            },
            "finish_reason": "tool_calls"
        }],
        "usage": {"prompt_tokens": 20, "completion_tokens": 5, "total_tokens": 25}
    })
}

pub fn anthropic_tool_message() -> Value {
    json!({
        "id": "msg_tool",
        "type": "message",
        "role": "assistant",
        "model": "m",
        "content": [{
            "type": "tool_use",
            "id": "toolu_t1",
            "name": "get_weather",
            "input": {"city": "Paris"}
        }],
        "stop_reason": "tool_use",
        "stop_sequence": null,
        "usage": {"input_tokens": 21, "output_tokens": 6}
    })
}

pub fn openai_chunk(delta: Value, finish: Option<&str>) -> String {
    format!(
        "data: {}\n\n",
        json!({
            "id": "chatcmpl-s",
            "object": "chat.completion.chunk",
            "created": 1,
            "model": "m",
            "choices": [{"index": 0, "delta": delta, "finish_reason": finish}]
        })
    )
}

pub fn openai_text_stream(text: &str) -> String {
    let mut out = String::new();
    out.push_str(&openai_chunk(json!({"role": "assistant"}), None));
    out.push_str(&openai_chunk(json!({"content": text}), None));
    out.push_str(&openai_chunk(json!({}), Some("stop")));
    out.push_str(&format!(
        "data: {}\n\n",
        json!({
            "id": "chatcmpl-s",
            "object": "chat.completion.chunk",
            "created": 1,
            "model": "m",
            "choices": [],
            "usage": {"prompt_tokens": 3, "completion_tokens": 4, "total_tokens": 7}
        })
    ));
    out.push_str("data: [DONE]\n\n");
    out
}

pub fn openai_tool_stream() -> String {
    let mut out = String::new();
    out.push_str(&openai_chunk(
        json!({"role": "assistant", "content": ""}),
        None,
    ));
    out.push_str(&openai_chunk(
        json!({"tool_calls": [{"index": 0, "id": "call_s1", "type": "function", "function": {"name": "get_weather", "arguments": ""}}]}),
        None,
    ));
    out.push_str(&openai_chunk(
        json!({"tool_calls": [{"index": 0, "function": {"arguments": "{\"city\":"}}]}),
        None,
    ));
    out.push_str(&openai_chunk(
        json!({"tool_calls": [{"index": 0, "function": {"arguments": "\"Paris\"}"}}]}),
        None,
    ));
    out.push_str(&openai_chunk(json!({}), Some("tool_calls")));
    out.push_str(&format!(
        "data: {}\n\n",
        json!({
            "id": "chatcmpl-s",
            "object": "chat.completion.chunk",
            "created": 1,
            "model": "m",
            "choices": [],
            "usage": {"prompt_tokens": 8, "completion_tokens": 2, "total_tokens": 10}
        })
    ));
    out.push_str("data: [DONE]\n\n");
    out
}

pub fn anthropic_text_stream(text: &str) -> String {
    format!(
        concat!(
            "event: message_start\ndata: {{\"type\":\"message_start\",\"message\":{{\"id\":\"msg_s\",\"type\":\"message\",\"role\":\"assistant\",\"model\":\"m\",\"content\":[],\"stop_reason\":null,\"stop_sequence\":null,\"usage\":{{\"input_tokens\":5,\"output_tokens\":1}}}}}}\n\n",
            "event: content_block_start\ndata: {{\"type\":\"content_block_start\",\"index\":0,\"content_block\":{{\"type\":\"text\",\"text\":\"\"}}}}\n\n",
            "event: content_block_delta\ndata: {{\"type\":\"content_block_delta\",\"index\":0,\"delta\":{{\"type\":\"text_delta\",\"text\":\"{text}\"}}}}\n\n",
            "event: content_block_stop\ndata: {{\"type\":\"content_block_stop\",\"index\":0}}\n\n",
            "event: message_delta\ndata: {{\"type\":\"message_delta\",\"delta\":{{\"stop_reason\":\"end_turn\",\"stop_sequence\":null}},\"usage\":{{\"output_tokens\":6}}}}\n\n",
            "event: message_stop\ndata: {{\"type\":\"message_stop\"}}\n\n"
        ),
        text = text
    )
}

pub fn anthropic_tool_stream() -> String {
    concat!(
        "event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_s\",\"type\":\"message\",\"role\":\"assistant\",\"model\":\"m\",\"content\":[],\"stop_reason\":null,\"stop_sequence\":null,\"usage\":{\"input_tokens\":5,\"output_tokens\":1}}}\n\n",
        "event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"tool_use\",\"id\":\"toolu_s1\",\"name\":\"get_weather\",\"input\":{}}}\n\n",
        "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"{\\\"city\\\":\"}}\n\n",
        "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"\\\"Paris\\\"}\"}}\n\n",
        "event: content_block_stop\ndata: {\"type\":\"content_block_stop\",\"index\":0}\n\n",
        "event: message_delta\ndata: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"tool_use\",\"stop_sequence\":null},\"usage\":{\"output_tokens\":2}}\n\n",
        "event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n"
    )
    .to_string()
}

/// Gateway wiring on top of an in-memory storage.
pub struct Harness {
    pub storage: Arc<Storage>,
    pub runtime: Arc<LlmRuntime>,
    pub config: Arc<OctraConfig>,
    pub plugins: Arc<LlmPluginRuntime>,
    pub pii: Arc<octra_llm_gateway::pii::PiiEngineRuntime>,
    pub models: Option<tempfile::TempDir>,
    /// Model metadata recorded by `model*` helpers before the route exists.
    model_metadata: Mutex<HashMap<(String, String), TargetMetadata>>,
}

/// Target metadata carried by an AI model target.
#[derive(Debug, Clone, Default)]
struct TargetMetadata {
    context_window: Option<i64>,
    max_output_tokens: Option<i64>,
    capabilities: Option<Value>,
    pricing: Option<Value>,
}

fn parse_json_object(raw: &str) -> Value {
    serde_json::from_str(raw).unwrap_or_else(|_| json!({}))
}

fn new_ai_id(prefix: &str) -> String {
    format!("{prefix}-{}", uuid::Uuid::new_v4().simple())
}

fn apply_metadata(target: &mut AiModelTarget, metadata: &TargetMetadata) {
    if let Some(context_window) = metadata.context_window {
        target.context_window = Some(context_window);
    }
    if let Some(max_output_tokens) = metadata.max_output_tokens {
        target.max_output_tokens = Some(max_output_tokens as i32);
    }
    if let Some(capabilities) = &metadata.capabilities {
        target.capabilities = capabilities.clone();
    }
    if let Some(pricing) = &metadata.pricing {
        target.pricing = pricing.clone();
    }
}

const TEST_SECRET_REF: &str = "env:OCTRA_TEST_UNUSED_SECRET";

impl Harness {
    pub async fn new() -> Self {
        let storage = Arc::new(Storage::connect_in_memory().await.unwrap());
        let config = Arc::new(OctraConfig::default());
        let runtime = Arc::new(
            LlmRuntime::bootstrap(storage.clone(), config.clone())
                .await
                .unwrap(),
        );
        let models = tempfile::tempdir().unwrap();
        let pii = Arc::new(octra_llm_gateway::pii::PiiEngineRuntime::with_models_dir(
            models.path().to_path_buf(),
        ));
        let plugins = Arc::new(
            LlmPluginRuntime::bootstrap(
                storage.clone(),
                Arc::new(octra_llm_plugin_builtin::registry_with(Some(pii.clone()))),
            )
            .await
            .unwrap(),
        );
        Self {
            storage,
            runtime,
            config,
            plugins,
            pii,
            models: Some(models),
            model_metadata: Mutex::new(HashMap::new()),
        }
    }

    /// Data plane router whose outbound calls carry a fixed test key.
    pub fn data_router(&self) -> Router {
        LlmDataState::new(
            self.runtime.clone(),
            self.storage.clone(),
            self.config.clone(),
        )
        .with_upstream(test_upstream(&self.config))
        .with_plugins(self.plugins.clone())
        .into_router()
    }

    /// Data plane router with the real credential resolution and OAuth cache,
    /// without the fixed test key.
    pub fn data_router_plain(&self) -> Router {
        LlmDataState::new(
            self.runtime.clone(),
            self.storage.clone(),
            self.config.clone(),
        )
        .with_plugins(self.plugins.clone())
        .into_router()
    }

    pub fn control_router(&self) -> Router {
        control_router(
            LlmControlState::new(self.runtime.clone(), self.storage.clone())
                .with_plugins(self.plugins.clone())
                .with_pii_engines(self.pii.clone()),
        )
    }

    /// Creates a plugin config through storage and reloads the chains.
    pub async fn plugin_config(
        &self,
        plugin_name: &str,
        scope: &str,
        scope_id: Option<&str>,
        config: Value,
    ) -> String {
        let plugin = self
            .storage
            .llm
            .get_plugin_by_name(plugin_name)
            .await
            .unwrap()
            .unwrap_or_else(|| panic!("plugin {plugin_name} is not registered"));
        let record = self
            .storage
            .llm
            .create_plugin_config_raw(&plugin.id, scope, scope_id, true, &config.to_string())
            .await
            .unwrap();
        self.plugins.reload().await.unwrap();
        record.id
    }

    /// Creates an AI-native provider pointing at a mock upstream.
    pub async fn provider(
        &self,
        name: &str,
        protocol: &str,
        base_url: &str,
        auth_type: &str,
        priority: i64,
    ) -> String {
        self.provider_with_config(name, protocol, base_url, auth_type, priority, "{}")
            .await
    }

    /// Creates an AI-native provider with an explicit `config_json`.
    pub async fn provider_with_config(
        &self,
        name: &str,
        protocol: &str,
        base_url: &str,
        _auth_type: &str,
        _priority: i64,
        config_json: &str,
    ) -> String {
        let id = new_ai_id("provider");
        let now = chrono::Utc::now().timestamp();
        self.storage
            .ai
            .upsert_provider(&AiProvider {
                id: id.clone(),
                name: name.to_string(),
                protocol: protocol.to_string(),
                base_url: base_url.to_string(),
                auth_ref: Some(TEST_SECRET_REF.to_string()),
                capabilities: vec!["chat".to_string()],
                config: parse_json_object(config_json),
                enabled: true,
                created_at: now,
                updated_at: now,
            })
            .await
            .unwrap();
        self.runtime.reload().await.unwrap();
        id
    }

    pub async fn route_to(&self, virtual_model: &str, targets: &[(String, i64, i64)]) {
        let expanded: Vec<(String, String, i64, i64)> = targets
            .iter()
            .map(|(provider_id, priority, weight)| {
                (provider_id.clone(), "m".to_string(), *priority, *weight)
            })
            .collect();
        self.route_to_strategy(virtual_model, "priority", &expanded)
            .await;
    }

    pub async fn model(&self, provider_id: &str, model_id: &str) {
        self.model_priced(provider_id, model_id, "{}").await;
    }

    pub async fn model_priced(&self, provider_id: &str, model_id: &str, pricing_json: &str) {
        let key = (provider_id.to_string(), model_id.to_string());
        let mut metadata = self
            .model_metadata
            .lock()
            .unwrap()
            .get(&key)
            .cloned()
            .unwrap_or_default();
        metadata.pricing = Some(parse_json_object(pricing_json));
        self.record_metadata(key, metadata).await;
        self.runtime.reload().await.unwrap();
    }

    pub async fn model_with(
        &self,
        provider_id: &str,
        model_id: &str,
        context_window: Option<i64>,
        capabilities: &str,
        pricing_json: &str,
    ) {
        let key = (provider_id.to_string(), model_id.to_string());
        let metadata = TargetMetadata {
            context_window,
            max_output_tokens: None,
            capabilities: Some(parse_json_object(capabilities)),
            pricing: Some(parse_json_object(pricing_json)),
        };
        self.record_metadata(key, metadata).await;
        self.runtime.reload().await.unwrap();
    }

    async fn record_metadata(&self, key: (String, String), metadata: TargetMetadata) {
        self.model_metadata
            .lock()
            .unwrap()
            .insert(key.clone(), metadata.clone());
        let targets = self.storage.ai.list_targets(None).await.unwrap();
        for mut target in targets {
            let matches_provider = target.provider_id.as_deref() == Some(key.0.as_str());
            let matches_model = target.upstream_model.as_deref() == Some(key.1.as_str());
            if matches_provider && matches_model {
                apply_metadata(&mut target, &metadata);
                self.storage.ai.upsert_target(&target).await.unwrap();
            }
        }
    }

    pub async fn route_to_strategy(
        &self,
        virtual_model: &str,
        strategy: &str,
        targets: &[(String, String, i64, i64)],
    ) {
        let now = chrono::Utc::now().timestamp();
        let model = AiModel {
            id: new_ai_id("model"),
            name: virtual_model.to_string(),
            kind: AiModelKind::Model,
            capabilities: vec!["chat".to_string()],
            formats: vec!["openai".to_string()],
            route_paths: vec!["/v1/chat/completions".to_string()],
            balancer: AiBalancer {
                strategy: strategy.to_string(),
                retries: 0,
                failover_criteria: Vec::new(),
            },
            enabled: true,
            created_at: now,
            updated_at: now,
        };
        self.storage.ai.upsert_model(&model).await.unwrap();
        for (index, (provider_id, upstream_model, priority, weight)) in targets.iter().enumerate() {
            let metadata = self
                .model_metadata
                .lock()
                .unwrap()
                .get(&(provider_id.clone(), upstream_model.clone()))
                .cloned();
            let mut target = AiModelTarget {
                id: new_ai_id("target"),
                model_id: model.id.clone(),
                target_model_id: None,
                provider_id: Some(provider_id.clone()),
                upstream_model: Some(upstream_model.clone()),
                account_id: None,
                weight: *weight,
                priority: *priority,
                position: index as i64,
                config: json!({}),
                context_window: None,
                max_output_tokens: None,
                capabilities: json!({}),
                pricing: json!({}),
                semantic_description: None,
                enabled: true,
                created_at: now,
                updated_at: now,
            };
            if let Some(metadata) = metadata {
                apply_metadata(&mut target, &metadata);
            }
            self.storage.ai.upsert_target(&target).await.unwrap();
        }
        self.runtime.reload().await.unwrap();
    }

    /// Sets semantic descriptions on a virtual model's targets, in target
    /// order. Used by semantic routing tests.
    pub async fn describe_targets(&self, virtual_model: &str, descriptions: &[&str]) {
        let model = self
            .storage
            .ai
            .list_models()
            .await
            .unwrap()
            .into_iter()
            .find(|model| model.name == virtual_model)
            .unwrap_or_else(|| panic!("model {virtual_model} does not exist"));
        let mut targets = self.storage.ai.list_targets(Some(&model.id)).await.unwrap();
        targets.sort_by(|left, right| {
            left.priority
                .cmp(&right.priority)
                .then_with(|| left.position.cmp(&right.position))
                .then_with(|| left.id.cmp(&right.id))
        });
        for (target, description) in targets.iter_mut().zip(descriptions.iter()) {
            target.semantic_description = Some((*description).to_string());
            self.storage.ai.upsert_target(target).await.unwrap();
        }
        self.runtime.reload().await.unwrap();
    }

    pub async fn account_with_config(
        &self,
        provider_id: &str,
        name: &str,
        priority: i64,
        weight: i64,
        config_json: &str,
    ) -> String {
        let id = new_ai_id("account");
        let now = chrono::Utc::now().timestamp();
        self.storage
            .ai
            .upsert_account(&AiProviderAccount {
                id: id.clone(),
                provider_id: provider_id.to_string(),
                name: name.to_string(),
                credential_kind: "static".to_string(),
                secret_ref: Some(TEST_SECRET_REF.to_string()),
                oauth_ref: None,
                weight,
                priority,
                quota_limit: None,
                quota_unit: "requests".to_string(),
                quota_period: "none".to_string(),
                quota_used: 0,
                quota_reset_at: None,
                cooldown_until: None,
                enabled: true,
                oauth_status: "none".to_string(),
                oauth_expires_at: None,
                config: parse_json_object(config_json),
                created_at: now,
                updated_at: now,
            })
            .await
            .unwrap();
        self.runtime.reload().await.unwrap();
        id
    }

    /// Creates an AI-native provider account, optionally pre-consuming quota.
    pub async fn account(
        &self,
        provider_id: &str,
        name: &str,
        priority: i64,
        weight: i64,
        quota_limit: Option<i64>,
        quota_used: i64,
    ) -> String {
        let id = new_ai_id("account");
        let now = chrono::Utc::now().timestamp();
        self.storage
            .ai
            .upsert_account(&AiProviderAccount {
                id: id.clone(),
                provider_id: provider_id.to_string(),
                name: name.to_string(),
                credential_kind: "static".to_string(),
                secret_ref: Some(TEST_SECRET_REF.to_string()),
                oauth_ref: None,
                weight,
                priority,
                quota_limit,
                quota_unit: "requests".to_string(),
                quota_period: "none".to_string(),
                quota_used: 0,
                quota_reset_at: None,
                cooldown_until: None,
                enabled: true,
                oauth_status: "none".to_string(),
                oauth_expires_at: None,
                config: json!({}),
                created_at: now,
                updated_at: now,
            })
            .await
            .unwrap();
        if quota_used > 0 {
            self.storage
                .ai
                .consume_account_quota(&id, quota_used, 0, 0)
                .await
                .unwrap();
        }
        self.runtime.reload().await.unwrap();
        id
    }

    /// Creates an AI-native allowlist policy bound to a consumer (or global).
    pub async fn allowlist_policy(
        &self,
        name: &str,
        consumer_id: Option<&str>,
        document: Value,
    ) -> String {
        let now = chrono::Utc::now().timestamp();
        let policy = AiPolicy {
            id: new_ai_id("policy"),
            name: name.to_string(),
            policy_type: AiPolicyType::Allowlist,
            config: document,
            enabled: true,
            created_at: now,
            updated_at: now,
        };
        self.storage.ai.upsert_policy(&policy).await.unwrap();
        let (scope, scope_id) = match consumer_id {
            Some(consumer_id) => (AiPolicyScope::Consumer, Some(consumer_id.to_string())),
            None => (AiPolicyScope::Global, None),
        };
        let attachment = AiPolicyAttachment {
            id: new_ai_id("attachment"),
            policy_id: policy.id.clone(),
            scope,
            scope_id,
            order: 0,
            enabled: true,
            created_at: now,
            updated_at: now,
        };
        self.storage
            .ai
            .upsert_attachment(&attachment)
            .await
            .unwrap();
        self.runtime.reload().await.unwrap();
        policy.id
    }

    /// Replaces an AI-native policy document in place.
    pub async fn update_policy_document(&self, policy_id: &str, document: Value) {
        let mut policy = self
            .storage
            .ai
            .get_policy(policy_id)
            .await
            .unwrap()
            .unwrap();
        policy.config = document;
        policy.updated_at = chrono::Utc::now().timestamp();
        self.storage.ai.upsert_policy(&policy).await.unwrap();
        self.runtime.reload().await.unwrap();
    }

    pub async fn key(
        &self,
        name: &str,
        plaintext: &str,
        enabled: bool,
        expires_at: Option<String>,
    ) -> String {
        let parsed = expires_at.map(|value| {
            chrono::DateTime::parse_from_rfc3339(&value)
                .unwrap()
                .with_timezone(&chrono::Utc)
        });
        let record = self
            .storage
            .llm
            .create_virtual_key(&octra_storage::LlmVirtualKeyInput {
                name: name.to_string(),
                key_prefix: plaintext.chars().take(12).collect(),
                key_hash: octra_llm_gateway::keys::hash_key(plaintext),
                consumer_id: None,
                enabled,
                expires_at: parsed,
                permissions_json: "{}".to_string(),
                key_family: None,
            })
            .await
            .unwrap();
        self.runtime.reload().await.unwrap();
        record.id
    }

    pub async fn key_with_permissions(
        &self,
        name: &str,
        plaintext: &str,
        permissions: Value,
    ) -> String {
        let record = self
            .storage
            .llm
            .create_virtual_key(&octra_storage::LlmVirtualKeyInput {
                name: name.to_string(),
                key_prefix: plaintext.chars().take(12).collect(),
                key_hash: octra_llm_gateway::keys::hash_key(plaintext),
                consumer_id: None,
                enabled: true,
                expires_at: None,
                permissions_json: permissions.to_string(),
                key_family: None,
            })
            .await
            .unwrap();
        self.runtime.reload().await.unwrap();
        record.id
    }
}

/// Sends a request through a router and returns status, headers and body.
pub async fn send(
    app: Router,
    method: Method,
    path: &str,
    headers: &[(&str, &str)],
    body: Option<Value>,
) -> (StatusCode, HeaderMap, String) {
    let mut builder = Request::builder().method(method).uri(path);
    for (name, value) in headers {
        builder = builder.header(*name, *value);
    }
    let body = match body {
        Some(value) => {
            builder = builder.header(header::CONTENT_TYPE, "application/json");
            Body::from(value.to_string())
        }
        None => Body::empty(),
    };
    let response = app.oneshot(builder.body(body).unwrap()).await.unwrap();
    let status = response.status();
    let headers = response.headers().clone();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    (status, headers, String::from_utf8_lossy(&bytes).to_string())
}

pub async fn post_json(
    app: Router,
    path: &str,
    headers: &[(&str, &str)],
    body: Value,
) -> (StatusCode, HeaderMap, Value) {
    let (status, headers, text) = send(app, Method::POST, path, headers, Some(body)).await;
    let value = serde_json::from_str(&text).unwrap_or(Value::String(text));
    (status, headers, value)
}

pub async fn wait_for_usage(
    storage: &Storage,
    expected: usize,
) -> Vec<octra_storage::LlmUsageRecord> {
    for _ in 0..200 {
        let rows = storage.llm.list_usage(50).await.unwrap();
        if rows.len() >= expected {
            return rows;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    storage.llm.list_usage(50).await.unwrap()
}

pub fn openai_body(model: &str, stream: bool) -> Value {
    json!({
        "model": model,
        "messages": [{"role": "user", "content": "hello"}],
        "stream": stream
    })
}

pub fn anthropic_body(model: &str, stream: bool) -> Value {
    json!({
        "model": model,
        "max_tokens": 64,
        "messages": [{"role": "user", "content": [{"type": "text", "text": "hello"}]}],
        "stream": stream
    })
}
