//! Test harness: mock upstreams, in-memory storage and gateway wiring.

#![allow(dead_code)]

use std::sync::{Arc, Mutex};

use axum::Router;
use axum::body::Body;
use axum::extract::State;
use axum::http::{HeaderMap, Method, Request, StatusCode, header};
use axum::response::Response;
use axum::routing::any;
use bytes::Bytes;
use http_body_util::BodyExt;
use octra_config::OctraConfig;
use octra_llm_gateway::{LlmControlState, LlmDataState, LlmRuntime, LlmUpstream, control_router};
use octra_storage::{
    LlmModelInput, LlmProviderAccountInput, LlmProviderInput, LlmRoutingTargetInput, Storage,
};
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
}

impl Harness {
    pub async fn new() -> Self {
        let storage = Arc::new(Storage::connect_in_memory().await.unwrap());
        let config = Arc::new(OctraConfig::default());
        let runtime = Arc::new(
            LlmRuntime::bootstrap(storage.clone(), config.clone())
                .await
                .unwrap(),
        );
        Self {
            storage,
            runtime,
            config,
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
        .into_router()
    }

    pub fn control_router(&self) -> Router {
        control_router(LlmControlState::new(
            self.runtime.clone(),
            self.storage.clone(),
        ))
    }

    /// Creates a provider pointing at a mock upstream.
    pub async fn provider(
        &self,
        name: &str,
        protocol: &str,
        base_url: &str,
        auth_type: &str,
        priority: i64,
    ) -> String {
        let record = self
            .storage
            .llm_create_provider(&LlmProviderInput {
                name: name.to_string(),
                kind: protocol.to_string(),
                protocol: protocol.to_string(),
                auth_type: auth_type.to_string(),
                base_url: Some(base_url.to_string()),
                secret_ref: Some("env:OCTRA_TEST_UNUSED_SECRET".to_string()),
                priority,
                config_json: "{}".to_string(),
                enabled: true,
            })
            .await
            .unwrap();
        record.id
    }

    pub async fn route_to(&self, virtual_model: &str, targets: &[(String, i64, i64)]) {
        let group = self
            .storage
            .llm_create_routing_group(&format!("{virtual_model}-group"), "priority")
            .await
            .unwrap();
        for (index, (provider_id, priority, weight)) in targets.iter().enumerate() {
            self.storage
                .llm_create_routing_target(
                    &group.id,
                    &LlmRoutingTargetInput {
                        provider_id: provider_id.clone(),
                        model_id: "m".to_string(),
                        account_id: None,
                        weight: *weight,
                        priority: *priority,
                        position: index as i64,
                    },
                )
                .await
                .unwrap();
        }
        self.storage
            .llm_create_route(virtual_model, Some(&group.id))
            .await
            .unwrap();
        self.runtime.reload().await.unwrap();
    }

    pub async fn model(&self, provider_id: &str, model_id: &str) {
        self.model_priced(provider_id, model_id, "{}").await;
    }

    pub async fn model_priced(&self, provider_id: &str, model_id: &str, pricing_json: &str) {
        self.storage
            .llm_create_model(&LlmModelInput {
                provider_id: provider_id.to_string(),
                model_id: model_id.to_string(),
                display_name: None,
                protocol: None,
                context_window: None,
                max_output_tokens: None,
                capabilities: "{}".to_string(),
                pricing_json: pricing_json.to_string(),
                enabled: true,
            })
            .await
            .unwrap();
        self.runtime.reload().await.unwrap();
    }

    /// Creates a provider account, optionally pre-consuming its quota.
    pub async fn account(
        &self,
        provider_id: &str,
        name: &str,
        priority: i64,
        weight: i64,
        quota_limit: Option<i64>,
        quota_used: i64,
    ) -> String {
        let record = self
            .storage
            .llm_create_account(
                provider_id,
                &LlmProviderAccountInput {
                    name: name.to_string(),
                    secret_ref: Some("env:OCTRA_TEST_UNUSED_SECRET".to_string()),
                    weight,
                    priority,
                    enabled: true,
                    quota_limit,
                    quota_unit: "requests".to_string(),
                    quota_period: "none".to_string(),
                    config_json: "{}".to_string(),
                },
            )
            .await
            .unwrap();
        if quota_used > 0 {
            self.storage
                .llm_consume_account_quota(&record.id, quota_used, 0, 0)
                .await
                .unwrap();
        }
        self.runtime.reload().await.unwrap();
        record.id
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
            .llm_create_virtual_key(&octra_storage::LlmVirtualKeyInput {
                name: name.to_string(),
                key_prefix: plaintext.chars().take(12).collect(),
                key_hash: octra_llm_gateway::keys::hash_key(plaintext),
                consumer_id: None,
                enabled,
                expires_at: parsed,
                permissions_json: "{}".to_string(),
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
        let rows = storage.llm_list_usage(50).await.unwrap();
        if rows.len() >= expected {
            return rows;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    storage.llm_list_usage(50).await.unwrap()
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
