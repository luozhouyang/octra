//! End-to-end ABI coverage for the Kong seven-phase WASM lifecycle.
//!
//! Ignored by default because it needs the built probe component:
//!
//!   cargo component build --release \
//!     --manifest-path examples/plugins/lifecycle-probe/Cargo.toml

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use http::{HeaderMap, StatusCode};
use octra_api_core::{Route, Service};
use octra_api_plugin_types::{
    ApiGatewayPlugin, ApiGatewayPluginFactory, BodyFilterContext, Capability, EntityQuery,
    GatewayEvent, GatewayState, HeaderFilterContext, JobInvocation, LogLevel, PluginContext,
    PluginHost, PluginStorage, StorageError,
};
use octra_api_plugin_wasm::{WasmEngine, WasmLimits, WasmPluginFactory};
use serde_json::{Value, json};

struct EmptyStorage;

impl PluginStorage for EmptyStorage {
    fn kv_get(&self, _key: &str) -> Result<Option<Vec<u8>>, StorageError> {
        Ok(None)
    }
    fn kv_put(
        &self,
        _key: &str,
        _value: &[u8],
        _ttl: Option<Duration>,
    ) -> Result<(), StorageError> {
        Ok(())
    }
    fn kv_delete(&self, _key: &str) -> Result<bool, StorageError> {
        Ok(false)
    }
    fn kv_incr(
        &self,
        _key: &str,
        _delta: i64,
        _ttl: Option<Duration>,
    ) -> Result<i64, StorageError> {
        Ok(0)
    }
    fn get(&self, _entity: &str, _key: &Value) -> Result<Option<Value>, StorageError> {
        Ok(None)
    }
    fn upsert(&self, _entity: &str, _values: &Value) -> Result<(), StorageError> {
        Ok(())
    }
    fn increment(
        &self,
        _entity: &str,
        _key: &Value,
        _field: &str,
        _delta: i64,
        _extra: Option<&Value>,
    ) -> Result<i64, StorageError> {
        Ok(0)
    }
    fn delete(&self, _entity: &str, _key: &Value) -> Result<bool, StorageError> {
        Ok(false)
    }
    fn query(&self, _entity: &str, _query: &EntityQuery) -> Result<Vec<Value>, StorageError> {
        Ok(Vec::new())
    }
}

struct ProbeHost {
    storage: EmptyStorage,
    capabilities: Vec<Capability>,
}

impl PluginHost for ProbeHost {
    fn plugin_name(&self) -> &str {
        "lifecycle-probe"
    }
    fn plugin_instance_id(&self) -> &str {
        "probe-instance"
    }
    fn capabilities(&self) -> &[Capability] {
        &self.capabilities
    }
    fn storage(&self) -> &dyn PluginStorage {
        &self.storage
    }
    fn log(&self, _level: LogLevel, _message: &str) {}
}

fn route() -> Route {
    Route {
        id: "route".to_string(),
        name: None,
        protocols: Vec::new(),
        methods: Vec::new(),
        hosts: Vec::new(),
        paths: vec!["/".to_string()],
        headers: Default::default(),
        strip_path: false,
        preserve_host: false,
        regex_priority: 0,
        service_id: None,
        enabled: true,
        tags: Vec::new(),
        created_at: 0,
        updated_at: 0,
    }
}

fn service() -> Service {
    Service {
        id: "service".to_string(),
        name: "service".to_string(),
        protocol: "http".to_string(),
        host: "localhost".to_string(),
        port: 80,
        path: None,
        retries: 0,
        connect_timeout: 0,
        write_timeout: 0,
        read_timeout: 0,
        enabled: true,
        description: String::new(),
        tags: Vec::new(),
        created_at: 0,
        updated_at: 0,
    }
}

fn probe_artifact() -> Option<(tempfile::TempDir, PathBuf)> {
    let source =
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../examples/plugins/lifecycle-probe");
    let wasm = source.join("target/wasm32-wasip1/release/lifecycle_probe.wasm");
    if !wasm.is_file() {
        return None;
    }
    let dir = tempfile::tempdir().ok()?;
    let path = dir.path().to_path_buf();
    std::fs::copy(source.join("plugin.json"), path.join("plugin.json")).ok()?;
    std::fs::copy(wasm, path.join("plugin.wasm")).ok()?;
    Some((dir, path))
}

async fn plugin() -> Arc<dyn ApiGatewayPlugin> {
    let Some((_dir, artifact)) = probe_artifact() else {
        panic!(
            "probe component not built; run `cargo component build --release \
             --manifest-path examples/plugins/lifecycle-probe/Cargo.toml`"
        );
    };
    let engine = WasmEngine::new(WasmLimits::default()).unwrap();
    let factory = WasmPluginFactory::from_artifact(&engine, &artifact).unwrap();
    let host = Arc::new(ProbeHost {
        storage: EmptyStorage,
        capabilities: vec![
            Capability::ResponseHeaders,
            Capability::ResponseBody,
            Capability::Log,
        ],
    });
    factory
        .create_with_host(host, "probe-1", json!({}))
        .unwrap()
}

async fn body_event(
    plugin: &dyn ApiGatewayPlugin,
    request_id: &str,
    chunk: Option<&[u8]>,
    eof: bool,
) -> Option<Vec<u8>> {
    let route = route();
    let service = service();
    let mut shared = BTreeMap::new();
    let mut ctx = PluginContext::default();
    let headers = HeaderMap::new();
    let mut cx = BodyFilterContext {
        chunk: chunk.map(bytes::Bytes::copy_from_slice),
        eof,
        request_id,
        status: StatusCode::OK,
        headers: &headers,
        consumer_id: None,
        route: &route,
        service: &service,
        target_url: "http://upstream",
        shared: &mut shared,
        ctx: &mut ctx,
    };
    plugin.body_filter(&mut cx).await.unwrap();
    cx.chunk.map(|chunk| chunk.to_vec())
}

#[tokio::test]
#[ignore = "requires cargo component build in examples/plugins/lifecycle-probe"]
async fn full_body_filter_keeps_request_contexts_separate() {
    let plugin = plugin().await;

    assert!(
        body_event(plugin.as_ref(), "A", Some(b"a1"), false)
            .await
            .is_none()
    );
    assert!(
        body_event(plugin.as_ref(), "B", Some(b"b1"), false)
            .await
            .is_none()
    );

    let a = body_event(plugin.as_ref(), "A", Some(b"a2"), true).await;
    let b = body_event(plugin.as_ref(), "B", Some(b"b2"), true).await;

    assert_eq!(a.as_deref(), Some(b"A:a1a2".as_slice()));
    assert_eq!(b.as_deref(), Some(b"B:b1b2".as_slice()));
}

#[tokio::test]
#[ignore = "requires cargo component build in examples/plugins/lifecycle-probe"]
async fn empty_body_filter_emits_one_eof_event() {
    let plugin = plugin().await;
    let output = body_event(plugin.as_ref(), "empty", None, true).await;
    assert_eq!(output.as_deref(), Some(b"empty:".as_slice()));
}

#[tokio::test]
#[ignore = "requires cargo component build in examples/plugins/lifecycle-probe"]
async fn header_filter_reaches_the_guest() {
    let plugin = plugin().await;
    let route = route();
    let service = service();
    let mut state = GatewayState::default();
    let request_headers = HeaderMap::new();
    let mut headers = HeaderMap::new();
    let mut status = StatusCode::OK;
    let mut cx = HeaderFilterContext {
        request_id: "header-1",
        state: &mut state,
        request_headers: &request_headers,
        status: &mut status,
        headers: &mut headers,
        consumer_id: None,
        route: &route,
        service: &service,
        target_url: "http://upstream",
    };
    plugin.header_filter(&mut cx).await.unwrap();
    assert_eq!(headers.get("x-lifecycle-probe").unwrap(), "header-filter");
}

#[tokio::test]
#[ignore = "requires cargo component build in examples/plugins/lifecycle-probe"]
async fn background_events_and_jobs_reach_the_guest() {
    let plugin = plugin().await;
    plugin
        .on_event(&GatewayEvent {
            id: "event-1".to_string(),
            topic: "probe.ok".to_string(),
            source: "test".to_string(),
            timestamp: 0,
            payload: json!({}),
        })
        .await
        .unwrap();
    let failed = plugin
        .on_event(&GatewayEvent {
            id: "event-2".to_string(),
            topic: "probe.fail".to_string(),
            source: "test".to_string(),
            timestamp: 0,
            payload: json!({}),
        })
        .await;
    assert!(failed.is_err());

    plugin
        .on_job(&JobInvocation {
            id: "job-1".to_string(),
            name: "probe-ok".to_string(),
            scheduled_for: 0,
            attempt: 0,
        })
        .await
        .unwrap();
    let failed = plugin
        .on_job(&JobInvocation {
            id: "job-2".to_string(),
            name: "probe-fail".to_string(),
            scheduled_for: 0,
            attempt: 0,
        })
        .await;
    assert!(failed.is_err());
}
