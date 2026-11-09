//! Daemon-level lifecycle test: register an LLM WASM component through the
//! same bridge the admin installer uses, configure it, serve a request, and
//! uninstall it.
//!
//! Build the component first:
//!
//!   cargo component build --release \
//!     --manifest-path examples/plugins/llm-demo/Cargo.toml
//!
//! Then run:
//!
//!   cargo test -p octra-daemon --test llm_wasm_lifecycle -- --ignored

use std::path::{Path, PathBuf};
use std::sync::Arc;

use octra_api_gateway::LlmWasmLifecycle;
use octra_api_plugin_types::{PluginKind, PluginManifest, Scope};
use octra_api_plugin_wasm::{WasmEngine, WasmLimits};
use octra_daemon::wasm_lifecycle::LlmWasmRuntime;
use octra_llm_core::{ChatRequest, Message, Protocol};
use octra_llm_gateway::LlmPluginRuntime;
use octra_llm_plugin_types::{LlmRequestMeta, LlmRequestState, LlmResponseHeaders};
use octra_storage::Storage;

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

fn meta() -> LlmRequestMeta<'static> {
    LlmRequestMeta {
        request_id: "req-daemon",
        ingress: Protocol::Openai,
        consumer_id: Some("consumer-1"),
        virtual_key_id: None,
        virtual_key_family: None,
        consumer_group_ids: &[],
        session_id: None,
        client_ip: Some("127.0.0.1".parse().unwrap()),
        user_agent: None,
        intent: None,
    }
}

#[tokio::test]
#[ignore = "requires cargo component build in examples/plugins/llm-demo"]
async fn installs_configures_serves_and_uninstalls_an_llm_component() {
    let temp = tempfile::tempdir().unwrap();
    let artifact = stage_artifact(&temp.path().join("artifact"));
    let layout = octra_plugin_storage::validate_artifact_dir_for_kind(&artifact, PluginKind::Llm)
        .expect("artifact layout");
    let manifest: PluginManifest =
        serde_json::from_str(&std::fs::read_to_string(example_dir().join("plugin.json")).unwrap())
            .unwrap();

    let storage = Arc::new(Storage::connect_in_memory().await.unwrap());
    let registry = Arc::new(octra_llm_plugin_builtin::registry());
    let runtime = Arc::new(
        LlmPluginRuntime::bootstrap(storage.clone(), registry.clone())
            .await
            .unwrap(),
    );
    let engine = Arc::new(WasmEngine::new(WasmLimits::default()).unwrap());
    let lifecycle = LlmWasmRuntime::new(registry.clone(), runtime.clone(), engine.clone());

    // Install through the same bridge the admin handler uses.
    lifecycle
        .install(manifest, artifact, None)
        .await
        .expect("install");
    assert!(registry.factory("llm-demo").is_some());

    // A global config makes the plugin part of every request chain.
    storage
        .llm
        .create_plugin_config(
            "plg_llm-demo",
            Scope::Global,
            None,
            true,
            r#"{"reject_models":["blocked-model"]}"#,
        )
        .await
        .unwrap();
    runtime.reload().await.unwrap();
    assert!(runtime.has_plugins());

    let meta = meta();

    // The configured component adds its response header to an allowed model.
    let mut headers = LlmResponseHeaders::new();
    let mut request = ChatRequest::new("allowed-model", vec![Message::user("hi")]);
    let mut state = LlmRequestState::new();
    let error = runtime
        .request_chain(None, None, &[], "allowed-model")
        .on_request(&meta, &mut headers, &mut request, &mut state)
        .await
        .unwrap();
    assert!(error.is_none());
    assert!(
        headers
            .iter()
            .any(|(name, value)| name.as_str() == "x-llm-demo" && value == "active")
    );

    // The guest rejects a blocked model through the JSON error envelope.
    let mut headers = LlmResponseHeaders::new();
    let mut request = ChatRequest::new("blocked-model", vec![Message::user("hi")]);
    let mut state = LlmRequestState::new();
    let error = runtime
        .request_chain(None, None, &[], "blocked-model")
        .on_request(&meta, &mut headers, &mut request, &mut state)
        .await
        .unwrap()
        .expect("blocked model must be rejected");
    assert_eq!(error.status, 429);
    assert_eq!(error.code.as_deref(), Some("model_blocked"));

    // Uninstalling removes the factory, evicts the component and leaves the
    // chain compiling (orphaned configs are skipped).
    lifecycle
        .uninstall("llm-demo", Some(layout.hash.as_str()))
        .await
        .expect("uninstall");
    assert!(registry.factory("llm-demo").is_none());
    assert_eq!(engine.cached_components(), 0);
    runtime.reload().await.unwrap();
    assert!(!runtime.has_plugins());
    let mut headers = LlmResponseHeaders::new();
    let mut request = ChatRequest::new("allowed-model", vec![Message::user("hi")]);
    let mut state = LlmRequestState::new();
    let error = runtime
        .request_chain(None, None, &[], "allowed-model")
        .on_request(&meta, &mut headers, &mut request, &mut state)
        .await
        .unwrap();
    assert!(error.is_none());
    assert!(headers.iter().next().is_none());
}
