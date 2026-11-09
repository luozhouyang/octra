//! End-to-end test for plugin-declared Admin API routes.
//!
//! Ignored by default because it needs a built artifact:
//!
//!   cargo component build --release \
//!     --manifest-path examples/plugins/consumer-quota/Cargo.toml

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use http::StatusCode;
use octra_api_plugin_types::{
    Capability, EntityQuery, LogLevel, PluginHost, PluginStorage, StorageError,
};
use octra_api_plugin_wasm::{APIGatewayWasmPluginFactory, AdminDispatch, WasmEngine, WasmLimits};
use serde_json::{Value, json};

struct EmptyStorage;

impl PluginStorage for EmptyStorage {
    fn kv_get(&self, _key: &str) -> Result<Option<Vec<u8>>, StorageError> {
        Err(StorageError::Unavailable)
    }

    fn kv_put(
        &self,
        _key: &str,
        _value: &[u8],
        _ttl: Option<Duration>,
    ) -> Result<(), StorageError> {
        Err(StorageError::Unavailable)
    }

    fn kv_delete(&self, _key: &str) -> Result<bool, StorageError> {
        Err(StorageError::Unavailable)
    }

    fn kv_incr(
        &self,
        _key: &str,
        _delta: i64,
        _ttl: Option<Duration>,
    ) -> Result<i64, StorageError> {
        Err(StorageError::Unavailable)
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
        Ok(1)
    }

    fn delete(&self, _entity: &str, _key: &Value) -> Result<bool, StorageError> {
        Ok(false)
    }

    fn query(&self, _entity: &str, _query: &EntityQuery) -> Result<Vec<Value>, StorageError> {
        Ok(Vec::new())
    }
}

static EMPTY: EmptyStorage = EmptyStorage;

struct TestHost {
    capabilities: Vec<Capability>,
}

impl PluginHost for TestHost {
    fn plugin_name(&self) -> &str {
        "consumer-quota"
    }

    fn plugin_instance_id(&self) -> &str {
        "admin-test"
    }

    fn capabilities(&self) -> &[Capability] {
        &self.capabilities
    }

    fn storage(&self) -> &dyn PluginStorage {
        &EMPTY
    }

    fn log(&self, _level: LogLevel, _message: &str) {}
}

fn staged_artifact() -> Option<PathBuf> {
    octra_api_plugin_wasm::testing::example_wasm()?;
    let staging = tempfile::tempdir().ok()?;
    let artifact = staging.keep();
    octra_api_plugin_wasm::testing::stage_example_artifact(&artifact).ok()?;
    Some(artifact)
}

fn admin_request(method: &str, path: &str, params: Vec<(String, String)>) -> AdminDispatch {
    AdminDispatch {
        request_id: format!("admin-{method}"),
        method: method.to_string(),
        path: path.to_string(),
        query: String::new(),
        headers: Vec::new(),
        params,
        body: Bytes::new(),
    }
}

#[tokio::test]
#[ignore = "requires cargo component build in examples/plugins/consumer-quota"]
async fn dispatches_declared_admin_routes() {
    let Some(artifact) = staged_artifact() else {
        panic!(
            "component artifact not found; run `cargo component build --release \
             --manifest-path examples/plugins/consumer-quota/Cargo.toml`"
        );
    };
    let engine = WasmEngine::new(WasmLimits::default()).unwrap();
    let factory = APIGatewayWasmPluginFactory::from_artifact(&engine, &artifact).unwrap();
    let host = Arc::new(TestHost {
        capabilities: vec![
            Capability::AdminApi,
            Capability::StorageEntities,
            Capability::Log,
        ],
    });
    let plugin = factory
        .create_admin(host, json!({ "limit": 5, "window_seconds": 60 }))
        .unwrap();

    let response = plugin
        .dispatch_admin(admin_request("GET", "/status", Vec::new()))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response.headers().get("content-type").unwrap(),
        "application/json"
    );
    let body = axum::body::to_bytes(response.into_body(), 64 * 1024)
        .await
        .unwrap();
    let body: Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(body["limit"], 5);
    assert_eq!(body["window_seconds"], 60);

    let response = plugin
        .dispatch_admin(admin_request(
            "GET",
            "/usage/consumer-1",
            vec![("consumer".to_string(), "consumer-1".to_string())],
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let body = axum::body::to_bytes(response.into_body(), 64 * 1024)
        .await
        .unwrap();
    let body: Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(body["consumer"], "consumer-1");
}

#[tokio::test]
#[ignore = "requires cargo component build in examples/plugins/consumer-quota"]
async fn requires_the_admin_api_capability() {
    let Some(artifact) = staged_artifact() else {
        panic!(
            "component artifact not found; run `cargo component build --release \
             --manifest-path examples/plugins/consumer-quota/Cargo.toml`"
        );
    };
    let engine = WasmEngine::new(WasmLimits::default()).unwrap();
    let factory = APIGatewayWasmPluginFactory::from_artifact(&engine, &artifact).unwrap();
    let host = Arc::new(TestHost {
        capabilities: vec![Capability::StorageEntities],
    });
    let plugin = factory
        .create_admin(host, json!({ "limit": 5, "window_seconds": 60 }))
        .unwrap();
    assert!(
        plugin
            .dispatch_admin(admin_request("GET", "/status", Vec::new()))
            .await
            .is_err()
    );
}
