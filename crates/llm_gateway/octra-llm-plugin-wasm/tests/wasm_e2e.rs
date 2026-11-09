//! End-to-end test against the reference `llm-demo` component.
//!
//! Build the component first:
//!
//!   cargo component build --release \
//!     --manifest-path examples/plugins/llm-demo/Cargo.toml
//!
//! Then run:
//!
//!   cargo test -p octra-llm-plugin-wasm --test wasm_e2e -- --ignored

use std::path::{Path, PathBuf};
use std::sync::Arc;

use octra_api_plugin_types::PluginStorage;
use octra_api_plugin_wasm::{WasmEngine, WasmLimits};
use octra_llm_core::{ChatRequest, ChatResponse, Message, Protocol, StreamEvent, Usage};
use octra_llm_plugin_types::{
    LlmAction, LlmFinalResponseContext, LlmLogContext, LlmLogEvent, LlmPluginCreateContext,
    LlmPluginFactory, LlmPluginStateStore, LlmRequestContext, LlmRequestMeta, LlmResponseContext,
    LlmResponseHeaders, LlmStreamEndAction, LlmStreamEndContext,
};
use octra_llm_plugin_wasm::LlmWasmPluginFactory;
use octra_plugin_storage::PluginStorageRoot;
use serde_json::{Value, json};

#[derive(Default)]
struct MemoryState;

impl LlmPluginStateStore for MemoryState {
    fn get(&self, _key: &str) -> Option<Value> {
        None
    }

    fn put(&self, _key: &str, _value: Value) {}
}

fn example_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../../examples/plugins/llm-demo")
}

fn stage_artifact(destination: &Path) -> PathBuf {
    let example = example_dir();
    let wasm = example.join("target/wasm32-wasip1/release/llm_demo.wasm");
    assert!(
        wasm.is_file(),
        "reference component not built; run `cargo component build --release \
         --manifest-path examples/plugins/llm-demo/Cargo.toml`"
    );
    std::fs::create_dir_all(destination).unwrap();
    std::fs::copy(example.join("plugin.json"), destination.join("plugin.json")).unwrap();
    std::fs::copy(wasm, destination.join("plugin.wasm")).unwrap();
    destination.to_path_buf()
}

fn meta(groups: &[String]) -> LlmRequestMeta<'_> {
    LlmRequestMeta {
        request_id: "req-e2e",
        ingress: Protocol::Openai,
        consumer_id: Some("consumer-1"),
        virtual_key_id: Some("key-1"),
        virtual_key_family: Some("family-1"),
        consumer_group_ids: groups,
        session_id: None,
        client_ip: Some("127.0.0.1".parse().unwrap()),
        user_agent: Some("e2e/1.0"),
        intent: None,
    }
}

#[tokio::test]
#[ignore = "requires cargo component build in examples/plugins/llm-demo"]
async fn runs_the_reference_component_through_the_host() {
    let temp = tempfile::tempdir().unwrap();
    let artifact = stage_artifact(&temp.path().join("artifact"));
    let engine = WasmEngine::new(WasmLimits::default()).unwrap();
    let storage_root = Arc::new(PluginStorageRoot::new(temp.path().join("plugins")));
    let factory =
        LlmWasmPluginFactory::from_artifact(&engine, &artifact, Some(storage_root.clone()))
            .unwrap();
    let context = LlmPluginCreateContext {
        instance_key: "cfg-e2e",
        plugin_name: "llm-demo",
        scope: "global",
        scope_id: "",
        state: Arc::new(MemoryState),
        counters: Arc::new(octra_llm_plugin_types::MemoryCounterStore::default()),
    };
    let plugin = factory
        .create_with(
            json!({ "reject_models": ["blocked-model"], "counter": true }),
            &context,
        )
        .unwrap();

    let groups: Vec<String> = Vec::new();
    let meta = meta(&groups);
    assert!(plugin.restores_sensitive_content());

    // Allowed model: the plugin leaves the request untouched, records state
    // and contributes a response header.
    let mut headers = LlmResponseHeaders::new();
    let mut request = ChatRequest::new("allowed-model", vec![Message::user("hi")]);
    let mut state = Value::Null;
    let action = plugin
        .on_request(&mut LlmRequestContext {
            meta: &meta,
            response_headers: &mut headers,
            request: &mut request,
            state: &mut state,
        })
        .await
        .unwrap();
    assert!(matches!(action, LlmAction::Continue));
    assert_eq!(request.model, "allowed-model");
    assert_eq!(state["model"], "allowed-model");
    let header = headers
        .iter()
        .find(|(name, _)| name.as_str() == "x-llm-demo")
        .map(|(_, value)| value.to_str().unwrap().to_string());
    assert_eq!(header.as_deref(), Some("active"));
    let store = storage_root.open("llm-demo", Vec::new()).unwrap();
    assert_eq!(store.kv_get("requests").unwrap(), Some(b"1".to_vec()));

    // The response stage can read usage and add a header derived from it.
    let mut response = ChatResponse::new(
        "id",
        "allowed-model",
        Message::new(octra_llm_core::Role::Assistant),
    );
    response.usage.output_tokens = 7;
    let mut response_headers = LlmResponseHeaders::new();
    plugin
        .on_response(&mut LlmResponseContext {
            meta: &meta,
            request_model: "allowed-model",
            response: &mut response,
            response_headers: &mut response_headers,
            state: &mut Value::Null,
        })
        .await
        .unwrap();
    let usage_header = response_headers
        .iter()
        .find(|(name, _)| name.as_str() == "x-llm-demo-output-tokens")
        .map(|(_, value)| value.to_str().unwrap().to_string());
    assert_eq!(usage_header.as_deref(), Some("7"));

    // The stream-end stage appends an event before the terminal events.
    let mut stream_events = Vec::new();
    let stream_response = ChatResponse::new(
        "id",
        "allowed-model",
        Message::new(octra_llm_core::Role::Assistant),
    );
    let mut stream_state = Value::Null;
    let action = plugin
        .on_stream_end(&mut LlmStreamEndContext {
            meta: &meta,
            response: &stream_response,
            events: &mut stream_events,
            state: &mut stream_state,
        })
        .await
        .unwrap();
    let LlmStreamEndAction::Append(events) = action else {
        panic!("expected the guest to append a stream-end event");
    };
    assert_eq!(events.len(), 1);
    assert!(matches!(
        &events[0],
        StreamEvent::TextDelta { text } if text == " tail"
    ));

    // The final-response stage is dispatched to the guest without changing
    // the response.
    let mut final_state = Value::Null;
    plugin
        .on_final_response(&mut LlmFinalResponseContext {
            meta: &meta,
            request_model: "allowed-model",
            response: &stream_response,
            state: &mut final_state,
        })
        .await;

    // Blocked model: the guest rejects through the JSON error envelope.
    let mut headers = LlmResponseHeaders::new();
    let mut request = ChatRequest::new("blocked-model", vec![Message::user("hi")]);
    let mut state = Value::Null;
    let action = plugin
        .on_request(&mut LlmRequestContext {
            meta: &meta,
            response_headers: &mut headers,
            request: &mut request,
            state: &mut state,
        })
        .await
        .unwrap();
    match action {
        LlmAction::Reject(error) => {
            assert_eq!(error.status, 429);
            assert_eq!(error.code.as_deref(), Some("model_blocked"));
        }
        LlmAction::Continue => panic!("expected the guest to reject the model"),
        LlmAction::Respond { .. } => panic!("the guest must not short-circuit a response"),
    }
    assert!(headers.iter().next().is_none());

    // The log stage drops the per-request component instance.
    let usage = Usage::default();
    let event = LlmLogEvent {
        model: "allowed-model",
        requested_model: None,
        provider_id: None,
        provider_name: None,
        account_id: None,
        account_name: None,
        status: 200,
        latency_ms: 1,
        usage: &usage,
        finish_reason: Some("stop"),
        streaming: false,
        error: None,
        cache_status: None,
        cost_micros: None,
        response_source: None,
    };
    let state = Value::Null;
    plugin
        .on_log(&LlmLogContext {
            meta: &meta,
            event: &event,
            state: &state,
        })
        .await;
}

#[tokio::test]
#[ignore = "requires cargo component build in examples/plugins/llm-demo"]
async fn rejects_a_manifest_from_the_previous_contract() {
    let temp = tempfile::tempdir().unwrap();
    let artifact = stage_artifact(&temp.path().join("artifact"));
    let manifest_path = artifact.join("plugin.json");
    let mut manifest: Value =
        serde_json::from_str(&std::fs::read_to_string(&manifest_path).unwrap()).unwrap();
    manifest["api_version"] = json!("6");
    std::fs::write(&manifest_path, manifest.to_string()).unwrap();

    let engine = WasmEngine::new(WasmLimits::default()).unwrap();
    let error = match LlmWasmPluginFactory::from_artifact(&engine, &artifact, None) {
        Ok(_) => panic!("v6 manifests must not load on the v7 host"),
        Err(error) => error,
    };
    assert!(
        error.message.contains("api_version") && error.message.contains('7'),
        "unexpected error: {error}"
    );
}
