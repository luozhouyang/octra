//! End-to-end coverage for the `ip-restriction` plugin.
//!
//! The harness attaches `ConnectInfo(127.0.0.1:4242)` to every request.

use axum::http::StatusCode;
use octra_config::OctraConfig;
use serde_json::json;

use crate::common::{
    Gateway, gateway, gateway_with_config, get, seed_service, service_plugin, spawn_mock,
    with_peer, with_xff,
};

async fn fixture_on(gateway: Gateway, config: serde_json::Value) -> Gateway {
    let (addr, _calls) = spawn_mock("ip").await;
    seed_service(
        &gateway.storage,
        "svc-ip",
        &format!("http://{addr}"),
        false,
        "/ip",
        true,
    )
    .await;
    service_plugin(&gateway.storage, "svc-ip", "ip-restriction", config).await;
    gateway.reload().await;
    gateway
}

async fn fixture(config: serde_json::Value) -> Gateway {
    fixture_on(gateway().await, config).await
}

#[tokio::test]
async fn allows_an_ip_inside_an_allowlist() {
    let gateway = fixture(json!({"allow": ["127.0.0.0/8"]})).await;
    assert_eq!(gateway.send(get("/ip/items")).await.0, StatusCode::CREATED);
}

#[tokio::test]
async fn rejects_an_ip_inside_a_denylist() {
    let gateway = fixture(json!({"deny": ["127.0.0.0/8"], "message": "blocked"})).await;
    let (status, _, json) = gateway.send(get("/ip/items")).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_eq!(json["message"], "blocked");
}

#[tokio::test]
async fn rejects_an_ip_outside_an_allowlist_with_a_custom_status() {
    let gateway = fixture(json!({"allow": ["10.0.0.0/8"], "status": 418})).await;
    assert_eq!(
        gateway.send(get("/ip/items")).await.0,
        StatusCode::IM_A_TEAPOT
    );
}

#[tokio::test]
async fn deny_rules_win_over_allow_rules() {
    let gateway = fixture(json!({
        "allow": ["127.0.0.0/8"],
        "deny": ["127.0.0.1"]
    }))
    .await;
    assert_eq!(
        gateway.send(get("/ip/items")).await.0,
        StatusCode::FORBIDDEN
    );
}

#[tokio::test]
async fn applies_ipv6_rules_to_the_peer_address() {
    let gateway = fixture(json!({"allow": ["2001:db8::/32"]})).await;

    let allowed = with_peer(get("/ip/items"), "[2001:db8::1]:4242");
    assert_eq!(gateway.send(allowed).await.0, StatusCode::CREATED);
    assert_eq!(
        gateway.send(get("/ip/items")).await.0,
        StatusCode::FORBIDDEN
    );
}

#[tokio::test]
async fn ignores_x_forwarded_for() {
    // Without trusted proxies the client address comes from the transport peer,
    // so the header cannot widen the allowlist.
    let gateway = fixture(json!({"allow": ["127.0.0.0/8"]})).await;
    let request = with_xff(get("/ip/items"), "10.1.2.3");
    assert_eq!(gateway.send(request).await.0, StatusCode::CREATED);
}

#[tokio::test]
async fn honors_forwarded_for_from_a_trusted_proxy() {
    let mut config = OctraConfig::default();
    config.gateway.trusted_ips = vec!["127.0.0.1/32".into()];
    let gateway = fixture_on(
        gateway_with_config(config).await,
        json!({"allow": ["10.0.0.0/8"]}),
    )
    .await;

    let allowed = with_xff(get("/ip/items"), "10.1.2.3");
    assert_eq!(gateway.send(allowed).await.0, StatusCode::CREATED);

    let denied = with_xff(get("/ip/items"), "192.168.1.1");
    assert_eq!(gateway.send(denied).await.0, StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn an_untrusted_peer_cannot_spoof_forwarded_for() {
    let mut config = OctraConfig::default();
    // The harness peer is 127.0.0.1, which is outside this trusted range.
    config.gateway.trusted_ips = vec!["10.0.0.0/8".into()];
    let gateway = fixture_on(
        gateway_with_config(config).await,
        json!({"allow": ["192.168.0.0/16"]}),
    )
    .await;

    let spoofed = with_xff(get("/ip/items"), "192.168.1.1");
    assert_eq!(gateway.send(spoofed).await.0, StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn recursive_forwarded_for_skips_trusted_proxies() {
    let mut config = OctraConfig::default();
    config.gateway.trusted_ips = vec!["127.0.0.1/32".into(), "10.0.0.0/8".into()];
    config.gateway.real_ip_recursive = true;
    let gateway = fixture_on(
        gateway_with_config(config).await,
        json!({"allow": ["192.168.0.0/16"]}),
    )
    .await;

    // The rightmost hop (10.0.0.5) is a trusted proxy, so the client is the
    // next address to the left.
    let request = with_xff(get("/ip/items"), "192.168.1.1, 10.0.0.5");
    assert_eq!(gateway.send(request).await.0, StatusCode::CREATED);

    let denied = with_xff(get("/ip/items"), "203.0.113.1, 10.0.0.5");
    assert_eq!(gateway.send(denied).await.0, StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn honors_a_custom_real_ip_header() {
    let mut config = OctraConfig::default();
    config.gateway.trusted_ips = vec!["127.0.0.1/32".into()];
    config.gateway.real_ip_header = "x-real-ip".into();
    let gateway = fixture_on(
        gateway_with_config(config).await,
        json!({"allow": ["10.0.0.0/8"]}),
    )
    .await;

    let request = crate::common::with_header(get("/ip/items"), "x-real-ip", "10.9.9.9");
    assert_eq!(gateway.send(request).await.0, StatusCode::CREATED);

    let ignored = with_xff(get("/ip/items"), "10.9.9.9");
    assert_eq!(gateway.send(ignored).await.0, StatusCode::FORBIDDEN);
}
