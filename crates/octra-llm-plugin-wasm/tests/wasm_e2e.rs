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

use octra_api_plugin_wasm::{WasmEngine, WasmLimits};
use octra_llm_core::{ChatRequest, Message, Protocol, Usage};
use octra_llm_plugin_types::{
    LlmAction, LlmLogContext, LlmLogEvent, LlmPluginCreateContext, LlmPluginFactory,
    LlmPluginStateStore, LlmRequestContext, LlmRequestMeta, LlmResponseHeaders,
};
use octra_llm_plugin_wasm::LlmWasmPluginFactory;
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
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../examples/plugins/llm-demo")
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
    let factory = LlmWasmPluginFactory::from_artifact(&engine, &artifact).unwrap();
    let context = LlmPluginCreateContext {
        instance_key: "cfg-e2e",
        plugin_name: "llm-demo",
        scope: "global",
        scope_id: "",
        state: Arc::new(MemoryState),
        counters: Arc::new(octra_llm_plugin_types::MemoryCounterStore::default()),
    };
    let plugin = factory
        .create_with(json!({ "reject_models": ["blocked-model"] }), &context)
        .unwrap();

    let groups: Vec<String> = Vec::new();
    let meta = meta(&groups);

    // Allowed model: the plugin leaves the request untouched, records state
    // and contributes a response header.
    let mut headers = LlmResponseHeaders::new();
    let mut request = ChatRequest::new("allowed-model", vec![Message::user("hi")]);
    let mut state = Value::Null;
    let action = plugin
        .on_request(&mut LlmRequestContext {
            meta: &meta,
            headers: &mut headers,
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

    // Blocked model: the guest rejects through the JSON error envelope.
    let mut headers = LlmResponseHeaders::new();
    let mut request = ChatRequest::new("blocked-model", vec![Message::user("hi")]);
    let mut state = Value::Null;
    let action = plugin
        .on_request(&mut LlmRequestContext {
            meta: &meta,
            headers: &mut headers,
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
    };
    let mut state = Value::Null;
    plugin
        .on_log(&mut LlmLogContext {
            meta: &meta,
            event: &event,
            state: &mut state,
        })
        .await;
}
