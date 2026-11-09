//! Ignored lifecycle tests for installed WASM plugins.
//!
//! Build the reference component first:
//!
//!   cargo component build --release \
//!     --manifest-path examples/plugins/consumer-quota/Cargo.toml
//!
//! Then run:
//!
//!   cargo test -p octra-api-gateway --test plugins -- \
//!     --ignored wasm_admin:: --test-threads=1

use axum::http::StatusCode;
use octra_api_plugin_types::Scope;
use serde_json::{Value, json};

use crate::common::{WasmTestEnv, get, plugin_instance, seed_simple_service, spawn_mock};

const CAPABILITIES: &[&str] = &["request_headers", "log", "storage_entities", "admin_api"];

async fn install(env: &WasmTestEnv) -> String {
    let (status, _, artifact) = env.upload_example().await;
    assert_eq!(status, StatusCode::CREATED, "upload failed: {artifact}");
    let artifact_id = artifact["id"].as_str().expect("artifact id").to_string();
    let (status, _, body) = env
        .send_control_json(
            "POST",
            "/admin/plugins/consumer-quota/install",
            json!({
                "artifact_id": artifact_id,
                "approve_capabilities": CAPABILITIES,
            }),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "install failed: {body}");
    artifact_id
}

async fn configure_instance(env: &WasmTestEnv, service_id: &str) {
    plugin_instance(
        &env.gateway.storage,
        "consumer-quota",
        None,
        Scope::Service,
        Some(service_id),
        &[],
        None,
        json!({"limit": 1, "window_seconds": 60, "anonymous_limit": 1}),
    )
    .await;
    env.reload().await;
}

async fn health(env: &WasmTestEnv) -> (StatusCode, Value) {
    let (status, _, body) = env
        .send_control_json("GET", "/admin/plugins/consumer-quota/health", json!({}))
        .await;
    (status, body)
}

#[tokio::test]
#[ignore = "requires cargo component build in examples/plugins/consumer-quota"]
async fn installs_reports_health_and_uninstalls() {
    let env = WasmTestEnv::new().await;
    install(&env).await;

    let (status, body) = health(&env).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["healthy"], true);
    assert_eq!(body["installed"], true);
    assert_eq!(body["catalog"], true);
    assert_eq!(body["host"], true);

    let (status, _, body) = env
        .send_control_json(
            "POST",
            "/admin/plugins/consumer-quota/uninstall",
            json!({"purge_data": true}),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "uninstall failed: {body}");
    assert_eq!(body["uninstalled"], true);
    assert_eq!(body["purged"], true);

    assert_eq!(health(&env).await.0, StatusCode::NOT_FOUND);
    assert!(!env.wasm.catalog.contains("consumer-quota"));
    assert!(!env.wasm.host_provider.contains("consumer-quota"));
}

#[tokio::test]
#[ignore = "requires cargo component build in examples/plugins/consumer-quota"]
async fn rejects_duplicate_installs_and_unknown_upgrades() {
    let env = WasmTestEnv::new().await;
    let artifact_id = install(&env).await;

    let (status, _, body) = env
        .send_control_json(
            "POST",
            "/admin/plugins/consumer-quota/install",
            json!({
                "artifact_id": artifact_id,
                "approve_capabilities": CAPABILITIES,
            }),
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(
        body["message"]
            .as_str()
            .expect("message")
            .contains("already installed")
    );

    let (status, _, _) = env
        .send_control_json(
            "POST",
            "/admin/plugins/unknown/upgrade",
            json!({
                "artifact_id": artifact_id,
                "approve_capabilities": CAPABILITIES,
            }),
        )
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
#[ignore = "requires cargo component build in examples/plugins/consumer-quota"]
async fn rejects_mismatched_capability_approvals() {
    let env = WasmTestEnv::new().await;
    let (status, _, artifact) = env.upload_example().await;
    assert_eq!(status, StatusCode::CREATED);
    let artifact_id = artifact["id"].as_str().expect("artifact id");

    for approve in [
        json!([]),
        json!(["request_headers", "log", "storage_entities"]),
        json!([
            "request_headers",
            "log",
            "storage_entities",
            "admin_api",
            "log"
        ]),
    ] {
        let (status, _, body) = env
            .send_control_json(
                "POST",
                "/admin/plugins/consumer-quota/install",
                json!({
                    "artifact_id": artifact_id,
                    "approve_capabilities": approve,
                }),
            )
            .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "approval: {approve}");
        assert!(
            body["message"]
                .as_str()
                .expect("message")
                .contains("approve_capabilities")
        );
    }
    assert!(!env.wasm.catalog.contains("consumer-quota"));
}

#[tokio::test]
#[ignore = "requires cargo component build in examples/plugins/consumer-quota"]
async fn rejects_oversized_and_missing_artifacts() {
    let env = WasmTestEnv::with_max_artifact_bytes(1).await;
    let (status, _, body) = env.upload_example().await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(body["message"].as_str().expect("message").contains("limit"));

    let env = WasmTestEnv::new().await;
    let (status, _, _) = env
        .send_control_json(
            "POST",
            "/admin/plugin-artifacts",
            json!({"path": "/definitely/not/a/plugin"}),
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

#[tokio::test]
#[ignore = "requires cargo component build in examples/plugins/consumer-quota"]
async fn enforces_the_plugin_migrations_entities_and_metrics() {
    let env = WasmTestEnv::new().await;
    install(&env).await;

    let (addr, _calls) = spawn_mock("quota").await;
    let seeded = seed_simple_service(
        &env.gateway.storage,
        "svc-quota",
        &format!("http://{addr}"),
        "/quota",
    )
    .await;
    configure_instance(&env, &seeded.service_id).await;

    let (status, _, body) = env.gateway.send(get("/quota/items")).await;
    assert_eq!(status, StatusCode::CREATED);
    assert_eq!(body["headers"]["x-quota-limit"], "1");

    let (status, _, body) = env.gateway.send(get("/quota/items")).await;
    assert_eq!(status, StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(body["message"], "quota exceeded");

    let (status, _, metrics) = env
        .send_control_json("GET", "/admin/plugins/consumer-quota/metrics", json!({}))
        .await;
    assert_eq!(status, StatusCode::OK);
    assert!(metrics["metrics"]["calls"].as_u64().unwrap() >= 2);
    assert!(metrics["metrics"]["rejections"].as_u64().unwrap() >= 1);

    let (status, _, migrations) = env
        .send_control_json("GET", "/admin/plugins/consumer-quota/migrations", json!({}))
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(migrations["migrations"][0]["version"], "000_base");

    let (status, _, rows) = env
        .send_control_json(
            "GET",
            "/admin/plugins/consumer-quota/entities/quota_usage",
            json!({}),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    let data = rows["data"].as_array().expect("rows");
    assert_eq!(data.len(), 1);
    assert_eq!(data[0]["consumer_id"], "ip:127.0.0.1");
    assert_eq!(data[0]["count"], 2);
}

#[tokio::test]
#[ignore = "requires cargo component build in examples/plugins/consumer-quota"]
async fn dispatches_plugin_admin_routes() {
    let env = WasmTestEnv::new().await;
    install(&env).await;
    plugin_instance(
        &env.gateway.storage,
        "consumer-quota",
        None,
        Scope::Global,
        None,
        &[],
        None,
        json!({"limit": 5, "window_seconds": 60}),
    )
    .await;
    env.reload().await;

    let (status, _, routes) = env
        .send_control_json(
            "GET",
            "/admin/plugins/consumer-quota/admin-routes",
            json!({}),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(routes["approved"], true);
    assert!(
        routes["routes"]
            .as_array()
            .expect("routes")
            .iter()
            .any(|route| route["path"] == "/status")
    );

    let (status, _, body) = env
        .send_control_json("GET", "/admin/plugins/consumer-quota/api/status", json!({}))
        .await;
    assert_eq!(status, StatusCode::OK, "dispatch failed: {body}");
    assert_eq!(body["limit"], 5);

    let (status, _, body) = env
        .send_control_json(
            "GET",
            "/admin/plugins/consumer-quota/api/usage/c1",
            json!({}),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["consumer"], "c1");
}

#[tokio::test]
#[ignore = "requires cargo component build in examples/plugins/consumer-quota"]
async fn upgrade_keeps_plugin_storage() {
    let env = WasmTestEnv::new().await;
    let artifact_id = install(&env).await;

    let (addr, _calls) = spawn_mock("quota-upgrade").await;
    let seeded = seed_simple_service(
        &env.gateway.storage,
        "svc-quota-upgrade",
        &format!("http://{addr}"),
        "/quota-upgrade",
    )
    .await;
    configure_instance(&env, &seeded.service_id).await;
    assert_eq!(
        env.gateway.send(get("/quota-upgrade/items")).await.0,
        StatusCode::CREATED
    );

    let (status, _, body) = env
        .send_control_json(
            "POST",
            "/admin/plugins/consumer-quota/upgrade",
            json!({
                "artifact_id": artifact_id,
                "approve_capabilities": CAPABILITIES,
            }),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "upgrade failed: {body}");
    assert!(
        body["migrations"]["skipped"]
            .as_array()
            .expect("skipped migrations")
            .iter()
            .any(|version| version == "000_base")
    );

    let (status, _, rows) = env
        .send_control_json(
            "GET",
            "/admin/plugins/consumer-quota/entities/quota_usage",
            json!({}),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(rows["data"].as_array().expect("rows").len(), 1);
}

struct RecordingLifecycle {
    installed: std::sync::Mutex<Vec<String>>,
    removed: std::sync::Mutex<Vec<String>>,
}

#[async_trait::async_trait]
impl octra_api_gateway::LlmWasmLifecycle for RecordingLifecycle {
    async fn install(
        &self,
        manifest: octra_api_plugin_types::PluginManifest,
        _artifact_dir: std::path::PathBuf,
        _replaces_hash: Option<&str>,
    ) -> Result<(), String> {
        self.installed.lock().unwrap().push(manifest.name);
        Ok(())
    }

    async fn uninstall(&self, name: &str, _artifact_hash: Option<&str>) -> Result<(), String> {
        self.removed.lock().unwrap().push(name.to_string());
        Ok(())
    }

    fn registered(&self, name: &str) -> bool {
        self.installed
            .lock()
            .unwrap()
            .iter()
            .any(|installed| installed == name)
    }
}

fn artifact(dir: &std::path::Path, kind: &str, version: &str) -> std::path::PathBuf {
    std::fs::create_dir_all(dir).unwrap();
    std::fs::write(
        dir.join("plugin.json"),
        json!({
            "api_version": "3",
            "name": "llm-demo",
            "version": version,
            "kind": kind,
            "runtime": "wasm",
            "priority": 0,
            "description": "demo",
            "capabilities": [],
            "phases": ["access"]
        })
        .to_string(),
    )
    .unwrap();
    std::fs::write(
        dir.join("plugin.wasm"),
        [0x00, 0x61, 0x73, 0x6d, 0x0d, 0x00, 0x01, 0x00],
    )
    .unwrap();
    dir.to_path_buf()
}

fn llm_artifact(dir: &std::path::Path) -> std::path::PathBuf {
    artifact(dir, "llm", "1.0.0")
}

#[tokio::test]
async fn installs_and_uninstalls_an_llm_artifact_through_the_lifecycle_hook() {
    let lifecycle = std::sync::Arc::new(RecordingLifecycle {
        installed: std::sync::Mutex::new(Vec::new()),
        removed: std::sync::Mutex::new(Vec::new()),
    });
    let env = WasmTestEnv::with_llm_lifecycle(lifecycle.clone()).await;
    let artifact = llm_artifact(&env.dir.path().join("llm-demo-artifact"));

    let (status, _, uploaded) = env
        .send_control_json(
            "POST",
            "/admin/plugin-artifacts",
            json!({"path": artifact.to_string_lossy()}),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "upload failed: {uploaded}");
    let artifact_id = uploaded["id"].as_str().expect("artifact id");

    let (status, _, installed) = env
        .send_control_json(
            "POST",
            "/admin/plugins/llm-demo/install",
            json!({"artifact_id": artifact_id}),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "install failed: {installed}");
    assert_eq!(lifecycle.installed.lock().unwrap().as_slice(), ["llm-demo"]);

    let (status, _, health) = env
        .send_control_json("GET", "/admin/plugins/llm-demo/health", json!({}))
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(health["healthy"], true, "health: {health}");
    assert_eq!(health["kind"], "llm");

    let (status, _, uninstalled) = env
        .send_control_json("POST", "/admin/plugins/llm-demo/uninstall", json!({}))
        .await;
    assert_eq!(status, StatusCode::OK, "uninstall failed: {uninstalled}");
    assert_eq!(lifecycle.removed.lock().unwrap().as_slice(), ["llm-demo"]);
}

struct FailingLifecycle;

#[async_trait::async_trait]
impl octra_api_gateway::LlmWasmLifecycle for FailingLifecycle {
    async fn install(
        &self,
        _manifest: octra_api_plugin_types::PluginManifest,
        _artifact_dir: std::path::PathBuf,
        _replaces_hash: Option<&str>,
    ) -> Result<(), String> {
        Err("cannot activate".to_string())
    }

    async fn uninstall(&self, _name: &str, _artifact_hash: Option<&str>) -> Result<(), String> {
        Ok(())
    }

    fn registered(&self, _name: &str) -> bool {
        false
    }
}

#[tokio::test]
async fn failed_llm_activation_removes_the_catalog_row() {
    let env = WasmTestEnv::with_llm_lifecycle(std::sync::Arc::new(FailingLifecycle)).await;
    let artifact = llm_artifact(&env.dir.path().join("llm-demo-artifact"));
    let (status, _, uploaded) = env
        .send_control_json(
            "POST",
            "/admin/plugin-artifacts",
            json!({"path": artifact.to_string_lossy()}),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED);
    let artifact_id = uploaded["id"].as_str().expect("artifact id");

    let (status, _, body) = env
        .send_control_json(
            "POST",
            "/admin/plugins/llm-demo/install",
            json!({"artifact_id": artifact_id}),
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "install: {body}");
    assert!(
        body["message"]
            .as_str()
            .unwrap()
            .contains("cannot activate")
    );

    let (status, _, _) = env
        .send_control_json("GET", "/admin/plugins/llm-demo/health", json!({}))
        .await;
    assert_eq!(
        status,
        StatusCode::NOT_FOUND,
        "catalog row was not rolled back"
    );
}

#[tokio::test]
async fn deletes_unused_artifacts_and_cascades_llm_configs_on_uninstall() {
    let lifecycle = std::sync::Arc::new(RecordingLifecycle {
        installed: std::sync::Mutex::new(Vec::new()),
        removed: std::sync::Mutex::new(Vec::new()),
    });
    let env = WasmTestEnv::with_llm_lifecycle(lifecycle).await;
    let source = llm_artifact(&env.dir.path().join("llm-demo-artifact"));

    // Upload and delete an artifact that no plugin uses.
    let (status, _, uploaded) = env
        .send_control_json(
            "POST",
            "/admin/plugin-artifacts",
            json!({"path": source.to_string_lossy()}),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED);
    let artifact_id = uploaded["id"].as_str().unwrap().to_string();

    let (status, _, body) = env
        .send_control_json(
            "DELETE",
            &format!("/admin/plugin-artifacts/{artifact_id}"),
            json!({}),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "delete failed: {body}");
    assert_eq!(body["deleted"], true);
    let (status, _, _) = env
        .send_control_json(
            "GET",
            &format!("/admin/plugin-artifacts/{artifact_id}"),
            json!({}),
        )
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    // Install a second copy, then refuse to delete the artifact in use.
    let (status, _, uploaded) = env
        .send_control_json(
            "POST",
            "/admin/plugin-artifacts",
            json!({"path": source.to_string_lossy()}),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED);
    let artifact_id = uploaded["id"].as_str().unwrap().to_string();
    let (status, _, body) = env
        .send_control_json(
            "POST",
            "/admin/plugins/llm-demo/install",
            json!({"artifact_id": artifact_id}),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "install failed: {body}");
    let (status, _, _) = env
        .send_control_json(
            "DELETE",
            &format!("/admin/plugin-artifacts/{artifact_id}"),
            json!({}),
        )
        .await;
    assert_eq!(status, StatusCode::CONFLICT);

    // Uninstall cascades the plugin configuration rows.
    env.gateway
        .storage
        .llm
        .create_plugin_config("plg_llm-demo", Scope::Global, None, true, "{}")
        .await
        .unwrap();
    assert_eq!(
        env.gateway
            .storage
            .llm
            .list_plugin_configs("plg_llm-demo")
            .await
            .unwrap()
            .len(),
        1
    );
    let (status, _, body) = env
        .send_control_json("POST", "/admin/plugins/llm-demo/uninstall", json!({}))
        .await;
    assert_eq!(status, StatusCode::OK, "uninstall failed: {body}");
    assert!(
        env.gateway
            .storage
            .llm
            .list_plugin_configs("plg_llm-demo")
            .await
            .unwrap()
            .is_empty()
    );
}

#[tokio::test]
async fn rejects_cross_kind_upgrades() {
    let lifecycle = std::sync::Arc::new(RecordingLifecycle {
        installed: std::sync::Mutex::new(Vec::new()),
        removed: std::sync::Mutex::new(Vec::new()),
    });
    let env = WasmTestEnv::with_llm_lifecycle(lifecycle).await;
    let api = artifact(&env.dir.path().join("api-artifact"), "api", "1.0.0");
    let (status, _, uploaded) = env
        .send_control_json(
            "POST",
            "/admin/plugin-artifacts",
            json!({"path": api.to_string_lossy()}),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "upload: {uploaded}");
    let artifact_id = uploaded["id"].as_str().unwrap();
    let (status, _, body) = env
        .send_control_json(
            "POST",
            "/admin/plugins/llm-demo/install",
            json!({"artifact_id": artifact_id}),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "install: {body}");

    let llm = artifact(&env.dir.path().join("llm-artifact"), "llm", "2.0.0");
    let (status, _, uploaded) = env
        .send_control_json(
            "POST",
            "/admin/plugin-artifacts",
            json!({"path": llm.to_string_lossy()}),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "upload: {uploaded}");
    let artifact_id = uploaded["id"].as_str().unwrap();
    let (status, _, body) = env
        .send_control_json(
            "POST",
            "/admin/plugins/llm-demo/upgrade",
            json!({"artifact_id": artifact_id}),
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "upgrade: {body}");
    assert!(
        body["message"]
            .as_str()
            .unwrap()
            .contains("cannot upgrade a api plugin with a llm artifact")
    );
}
