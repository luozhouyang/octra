//! Provisioning coverage for the LLM routes that front the in-process LLM
//! router.

use std::net::SocketAddr;
use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use octra_api_core::{CreatePluginInstance, CreateRoute, LLM_INTERNAL_TAG, MANAGED_BY_LLM_TAG};
use octra_api_gateway::{ApiDataState, ApiMetrics};
use octra_api_plugin_runtime::{PluginRegistry, PluginRuntime};
use octra_config::OctraConfig;
use octra_daemon::llm_upstream;
use octra_llm_gateway::{LlmDataState, LlmRuntime};
use octra_runtime::GatewayRuntime;
use octra_storage::Storage;
use serde_json::json;
use tower::ServiceExt;

#[tokio::test]
async fn provisions_and_reconciles_llm_routes() {
    let storage = Storage::connect_in_memory().await.unwrap();
    llm_upstream::ensure_llm_routes(&storage).await.unwrap();
    llm_upstream::ensure_llm_routes(&storage).await.unwrap();

    let services = storage.api.list_services().await.unwrap();
    let service = services
        .iter()
        .find(|service| service.id == "svc_llm_gateway")
        .expect("llm gateway service");
    assert!(service.tags.iter().any(|tag| tag == LLM_INTERNAL_TAG));
    assert!(service.tags.iter().any(|tag| tag == MANAGED_BY_LLM_TAG));
    assert_eq!(service.retries, 0);

    let routes = storage.api.list_routes(None).await.unwrap();
    let managed: Vec<_> = routes
        .iter()
        .filter(|route| route.service_id.as_deref() == Some("svc_llm_gateway"))
        .collect();
    assert_eq!(managed.len(), llm_upstream::LLM_ROUTE_PATHS.len());
    for route in &managed {
        // Every route is an exact regex; no broad `/v1` or `/v1beta` prefix
        // can capture unrelated API traffic.
        assert!(route.paths.iter().all(|path| path.starts_with('~')));
        assert!(
            route
                .paths
                .iter()
                .all(|path| path != "/v1" && path != "/v1beta")
        );
        // Routes must accept every method so the CORS preflight reaches the
        // API gateway's access phase.
        assert!(route.methods.is_empty());
        assert!(!route.strip_path);
    }
}

#[tokio::test]
async fn real_llm_router_is_reached_through_the_api_gateway() {
    let storage = Arc::new(Storage::connect_in_memory().await.unwrap());
    let config = Arc::new(OctraConfig::default());
    llm_upstream::ensure_llm_routes(&storage).await.unwrap();
    // CORS is an ordinary API gateway plugin attached to the LLM service.
    storage
        .api
        .create_plugin_instance(&CreatePluginInstance {
            name: "cors".into(),
            service_id: Some("svc_llm_gateway".into()),
            config: Some(json!({"origins": ["https://app.example.com"]})),
            ..Default::default()
        })
        .await
        .unwrap();

    let api_runtime = Arc::new(GatewayRuntime::bootstrap(storage.clone()).await.unwrap());
    let llm_runtime = Arc::new(
        LlmRuntime::bootstrap(storage.clone(), config.clone())
            .await
            .unwrap(),
    );
    let llm_data = LlmDataState::new(llm_runtime, storage.clone(), config.clone());
    let internal = Arc::new(llm_upstream::LlmInternalUpstream::new(
        llm_data.into_router(),
    ));
    let mut registry = PluginRegistry::new();
    octra_api_plugin_builtin::register(&mut registry);
    let instances = storage.api.list_plugin_instances().await.unwrap();
    let plugins = Arc::new(
        PluginRuntime::with_plugins(Arc::new(registry), instances).expect("plugin chains compile"),
    );
    let api_data = ApiDataState::with_plugins(
        api_runtime,
        config,
        Arc::new(ApiMetrics::default()),
        plugins,
    )
    .with_internal_upstream(internal);
    let router = octra_api_gateway::data_plane_router(api_data);

    let mut request = Request::builder()
        .method("POST")
        .uri("/v1/chat/completions")
        .header(header::HOST, "localhost")
        .header(header::CONTENT_TYPE, "application/json")
        .header(header::ORIGIN, "https://app.example.com")
        .body(Body::from(
            r#"{"model":"definitely-not-a-model","messages":[]}"#,
        ))
        .unwrap();
    request.extensions_mut().insert(axum::extract::ConnectInfo(
        "127.0.0.1:4242".parse::<SocketAddr>().unwrap(),
    ));

    let response = router.clone().oneshot(request).await.unwrap();
    let status = response.status();
    let headers = response.headers().clone();
    let body = axum::body::to_bytes(response.into_body(), 1024 * 1024)
        .await
        .unwrap();
    let body = String::from_utf8_lossy(&body);

    // A 502 would mean the API gateway never reached the LLM router. The LLM
    // router should answer with its own protocol error for an unknown model.
    assert_ne!(status, StatusCode::BAD_GATEWAY, "body: {body}");
    assert!(
        body.contains("model") || body.contains("error"),
        "body: {body}"
    );
    assert_eq!(
        headers
            .get(header::ACCESS_CONTROL_ALLOW_ORIGIN)
            .and_then(|value| value.to_str().ok()),
        Some("https://app.example.com")
    );

    // The exact route must also match the CORS preflight, before LLM auth.
    let mut preflight = Request::builder()
        .method("OPTIONS")
        .uri("/v1/chat/completions")
        .header(header::HOST, "localhost")
        .header(header::ORIGIN, "https://app.example.com")
        .header("access-control-request-method", "POST")
        .body(Body::empty())
        .unwrap();
    preflight
        .extensions_mut()
        .insert(axum::extract::ConnectInfo(
            "127.0.0.1:4242".parse::<SocketAddr>().unwrap(),
        ));
    let response = router.oneshot(preflight).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response
            .headers()
            .get(header::ACCESS_CONTROL_ALLOW_ORIGIN)
            .and_then(|value| value.to_str().ok()),
        Some("https://app.example.com")
    );
}

#[tokio::test]
async fn removes_legacy_prefix_routes() {
    let storage = Storage::connect_in_memory().await.unwrap();
    llm_upstream::ensure_llm_routes(&storage).await.unwrap();
    storage
        .api
        .create_route(&CreateRoute {
            id: Some("rt_llm_v1".into()),
            service_id: Some("svc_llm_gateway".into()),
            paths: Some(vec!["/v1".into()]),
            tags: Some(vec![MANAGED_BY_LLM_TAG.into()]),
            ..Default::default()
        })
        .await
        .unwrap();
    assert!(storage.api.get_route("rt_llm_v1").await.unwrap().is_some());

    llm_upstream::ensure_llm_routes(&storage).await.unwrap();
    assert!(storage.api.get_route("rt_llm_v1").await.unwrap().is_none());
}

#[tokio::test]
async fn does_not_capture_unrelated_v1_paths() {
    let storage = Arc::new(Storage::connect_in_memory().await.unwrap());
    let config = Arc::new(OctraConfig::default());
    llm_upstream::ensure_llm_routes(&storage).await.unwrap();
    let api_runtime = Arc::new(GatewayRuntime::bootstrap(storage.clone()).await.unwrap());
    let api_data = ApiDataState::with_plugins(
        api_runtime,
        config,
        Arc::new(ApiMetrics::default()),
        Arc::new(PluginRuntime::empty()),
    );
    let router = octra_api_gateway::data_plane_router(api_data);

    let mut request = Request::builder()
        .method("GET")
        .uri("/v1/unrelated")
        .header(header::HOST, "localhost")
        .body(Body::empty())
        .unwrap();
    request.extensions_mut().insert(axum::extract::ConnectInfo(
        "127.0.0.1:4242".parse::<SocketAddr>().unwrap(),
    ));

    let response = router.oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    let body = axum::body::to_bytes(response.into_body(), 1024 * 1024)
        .await
        .unwrap();
    let body = String::from_utf8_lossy(&body);
    assert!(body.contains("no_route"), "body: {body}");
}
