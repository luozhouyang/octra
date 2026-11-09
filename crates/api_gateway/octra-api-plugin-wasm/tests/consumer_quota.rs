//! End-to-end test for the reference `consumer-quota` component.
//!
//! Ignored by default because it needs a built artifact:
//!
//!   cargo component build --release \
//!     --manifest-path examples/plugins/consumer-quota/Cargo.toml

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use http::{HeaderMap, HeaderValue, Method, StatusCode, Uri};
use octra_api_core::{Route, Service};
use octra_api_plugin_types::{
    APIGatewayPluginFactory, Capability, EntityQuery, GatewayState, LogLevel, PluginAction,
    PluginHost, PluginStorage, StorageError,
};
use octra_api_plugin_wasm::{APIGatewayWasmPluginFactory, WasmEngine, WasmLimits};
use serde_json::{Value, json};

#[derive(Default)]
struct Counters {
    values: Mutex<HashMap<String, i64>>,
}

impl PluginStorage for Counters {
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
        entity: &str,
        key: &Value,
        field: &str,
        delta: i64,
        _extra: Option<&Value>,
    ) -> Result<i64, StorageError> {
        let slot = format!("{entity}:{}:{field}", key);
        let mut values = self
            .values
            .lock()
            .map_err(|error| StorageError::Backend(error.to_string()))?;
        let value = values.entry(slot).or_insert(0);
        *value += delta;
        Ok(*value)
    }

    fn delete(&self, _entity: &str, _key: &Value) -> Result<bool, StorageError> {
        Ok(false)
    }

    fn query(&self, _entity: &str, _query: &EntityQuery) -> Result<Vec<Value>, StorageError> {
        Ok(Vec::new())
    }
}

struct TestHost {
    name: String,
    counters: Arc<Counters>,
}

impl PluginHost for TestHost {
    fn plugin_name(&self) -> &str {
        &self.name
    }

    fn plugin_instance_id(&self) -> &str {
        "test-instance"
    }

    fn capabilities(&self) -> &[Capability] {
        &[
            Capability::RequestHeaders,
            Capability::Log,
            Capability::StorageEntities,
        ]
    }

    fn storage(&self) -> &dyn PluginStorage {
        self.counters.as_ref()
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

fn staged_artifact() -> Option<PathBuf> {
    octra_api_plugin_wasm::testing::example_wasm()?;
    let staging = tempfile::tempdir().ok()?;
    let artifact = staging.keep();
    octra_api_plugin_wasm::testing::stage_example_artifact(&artifact).ok()?;
    Some(artifact)
}

#[tokio::test]
#[ignore = "requires cargo component build in examples/plugins/consumer-quota"]
async fn consumer_quota_enforces_the_limit_end_to_end() {
    let Some(artifact) = staged_artifact() else {
        panic!(
            "component artifact not found; run `cargo component build --release \
             --manifest-path examples/plugins/consumer-quota/Cargo.toml`"
        );
    };

    let engine = WasmEngine::new(WasmLimits::default()).unwrap();
    let factory = APIGatewayWasmPluginFactory::from_artifact(&engine, &artifact).unwrap();
    let counters = Arc::new(Counters::default());
    let host = Arc::new(TestHost {
        name: "consumer-quota".to_string(),
        counters,
    });
    let plugin = factory
        .create_with_host(host, "quota-1", json!({ "limit": 1, "window_seconds": 60 }))
        .unwrap();

    let route = route();
    let service = service();
    let method = Method::GET;

    let mut first_uri: Uri = "/v1/chat".parse().unwrap();
    let mut first_headers = HeaderMap::new();
    let mut first_state = GatewayState::default();
    {
        let mut cx = octra_api_plugin_types::RequestContext {
            request_id: "req-1",
            state: &mut first_state,
            matched_path: "/v1/chat",
            method: &method,
            uri: &mut first_uri,
            headers: &mut first_headers,
            client_ip: Some("127.0.0.1".parse().unwrap()),
            consumer_id: Some("consumer-1"),
            credential_id: None,
            route: &route,
            service: &service,
        };
        match plugin.access(&mut cx).await.unwrap() {
            PluginAction::Continue => {}
            PluginAction::ShortCircuit(response) => panic!("unexpected rejection: {response:?}"),
        }
    }
    assert_eq!(
        first_headers.get("x-quota-limit").unwrap(),
        HeaderValue::from_static("1")
    );
    assert_eq!(
        first_headers.get("x-quota-remaining").unwrap(),
        HeaderValue::from_static("0")
    );

    let mut second_uri: Uri = "/v1/chat".parse().unwrap();
    let mut second_headers = HeaderMap::new();
    let mut second_state = GatewayState::default();
    let mut cx = octra_api_plugin_types::RequestContext {
        request_id: "req-2",
        state: &mut second_state,
        matched_path: "/v1/chat",
        method: &method,
        uri: &mut second_uri,
        headers: &mut second_headers,
        client_ip: Some("127.0.0.1".parse().unwrap()),
        consumer_id: Some("consumer-1"),
        credential_id: None,
        route: &route,
        service: &service,
    };
    match plugin.access(&mut cx).await.unwrap() {
        PluginAction::ShortCircuit(response) => {
            assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
            let retry_after: u64 = response
                .headers()
                .get("retry-after")
                .unwrap()
                .to_str()
                .unwrap()
                .parse()
                .unwrap();
            assert!((1..=60).contains(&retry_after));
        }
        PluginAction::Continue => panic!("quota was not enforced"),
    }

    let metrics = engine
        .metrics()
        .snapshot("consumer-quota")
        .expect("metrics recorded");
    assert_eq!(metrics.calls, 2);
    assert_eq!(metrics.rejections, 1);
    assert_eq!(metrics.failures, 0);
}
