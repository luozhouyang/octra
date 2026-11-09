//! End-to-end coverage for the `jwt` plugin.

use std::time::{SystemTime, UNIX_EPOCH};

use axum::http::{StatusCode, header};
use jsonwebtoken::{Algorithm, EncodingKey, Header, encode};
use octra_api_core::{CreateConsumer, CreateJwtSecret};
use serde_json::json;

use crate::common::{
    Gateway, gateway, get, header_default, request, seed_service, service_plugin, spawn_mock,
};

const ISSUER: &str = "issuer-one";
const SECRET: &str = "jwt-test-secret";

fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64
}

fn token(issuer: &str, secret: &str, exp: i64) -> String {
    let claims = json!({"iss": issuer, "exp": exp});
    encode(
        &Header::new(Algorithm::HS256),
        &claims,
        &EncodingKey::from_secret(secret.as_bytes()),
    )
    .unwrap()
}

async fn fixture() -> Gateway {
    fixture_with(json!({})).await
}

async fn fixture_with(config: serde_json::Value) -> Gateway {
    fixture_with_algorithm(config, "HS256").await
}

async fn fixture_with_algorithm(config: serde_json::Value, algorithm: &str) -> Gateway {
    let (addr, _calls) = spawn_mock("jwt").await;
    let gateway = gateway().await;
    seed_service(
        &gateway.storage,
        "svc-jwt",
        &format!("http://{addr}"),
        false,
        "/jwt",
        true,
    )
    .await;
    service_plugin(&gateway.storage, "svc-jwt", "jwt", config).await;

    let consumer = gateway
        .storage
        .api
        .create_consumer(&CreateConsumer {
            username: Some("jwt-user".into()),
            ..Default::default()
        })
        .await
        .unwrap();
    gateway
        .storage
        .api
        .create_jwt(&CreateJwtSecret {
            consumer_id: consumer.id,
            key: ISSUER.into(),
            secret: SECRET.into(),
            algorithm: Some(algorithm.into()),
            ..Default::default()
        })
        .await
        .unwrap();
    gateway.reload().await;
    gateway
}

fn bearer(value: &str) -> (header::HeaderName, axum::http::HeaderValue) {
    (
        header::AUTHORIZATION,
        header_default(&format!("Bearer {value}")),
    )
}

#[tokio::test]
async fn rejects_missing_expired_and_forged_tokens() {
    let gateway = fixture().await;

    assert_eq!(
        gateway.send(get("/jwt/items")).await.0,
        StatusCode::UNAUTHORIZED
    );

    let mut expired = get("/jwt/items");
    let (name, value) = bearer(&token(ISSUER, SECRET, now() - 3600));
    expired.headers_mut().insert(name, value);
    assert_eq!(gateway.send(expired).await.0, StatusCode::UNAUTHORIZED);

    let mut forged = get("/jwt/items");
    let (name, value) = bearer(&token(ISSUER, "wrong-secret", now() + 300));
    forged.headers_mut().insert(name, value);
    assert_eq!(gateway.send(forged).await.0, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn accepts_a_valid_token_and_resolves_the_consumer() {
    let gateway = fixture().await;

    let mut request = get("/jwt/items");
    let (name, value) = bearer(&token(ISSUER, SECRET, now() + 300));
    request.headers_mut().insert(name, value);
    let (status, _, json) = gateway.send(request).await;

    assert_eq!(status, StatusCode::CREATED);
    assert_eq!(json["path"], "/items");
    assert_eq!(json["x_consumer_username"], "jwt-user");
}

#[tokio::test]
async fn rejects_tokens_whose_expiration_exceeds_the_configured_window() {
    let gateway = fixture_with(json!({"maximum_expiration": 600})).await;

    let mut request = get("/jwt/items");
    let (name, value) = bearer(&token(ISSUER, SECRET, now() + 3600));
    request.headers_mut().insert(name, value);
    assert_eq!(gateway.send(request).await.0, StatusCode::UNAUTHORIZED);

    let mut request = get("/jwt/items");
    let (name, value) = bearer(&token(ISSUER, SECRET, now() + 300));
    request.headers_mut().insert(name, value);
    assert_eq!(gateway.send(request).await.0, StatusCode::CREATED);
}

#[tokio::test]
async fn includes_the_configured_realm() {
    let gateway = fixture_with(json!({"realm": "octra"})).await;
    let (status, headers, _) = gateway.send(get("/jwt/items")).await;

    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert_eq!(
        headers.get(header::WWW_AUTHENTICATE).unwrap(),
        "Bearer realm=\"octra\""
    );
}

fn token_with(claims: serde_json::Value) -> String {
    encode(
        &Header::new(Algorithm::HS256),
        &claims,
        &EncodingKey::from_secret(SECRET.as_bytes()),
    )
    .unwrap()
}

#[tokio::test]
async fn accepts_tokens_from_query_cookies_and_custom_headers() {
    let query = fixture_with(json!({"uri_param_names": ["jwt"]})).await;
    let query_token = token(ISSUER, SECRET, now() + 300);
    assert_eq!(
        query
            .send(get(&format!("/jwt/items?jwt={query_token}")))
            .await
            .0,
        StatusCode::CREATED
    );

    let cookie = fixture_with(json!({"cookie_names": ["session"]})).await;
    let cookie_token = token(ISSUER, SECRET, now() + 300);
    let mut request = get("/jwt/items");
    request.headers_mut().insert(
        header::COOKIE,
        header_default(&format!("session={cookie_token}")),
    );
    assert_eq!(cookie.send(request).await.0, StatusCode::CREATED);

    let custom = fixture_with(json!({"header_names": ["x-jwt"]})).await;
    let header_token = token(ISSUER, SECRET, now() + 300);
    let mut request = get("/jwt/items");
    request
        .headers_mut()
        .insert("x-jwt", header_default(&header_token));
    assert_eq!(custom.send(request).await.0, StatusCode::CREATED);
}

#[tokio::test]
async fn rejects_algorithm_mismatches() {
    let gateway = fixture_with_algorithm(json!({}), "HS512").await;

    let mut request = get("/jwt/items");
    let (name, value) = bearer(&token(ISSUER, SECRET, now() + 300));
    request.headers_mut().insert(name, value);
    assert_eq!(gateway.send(request).await.0, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn verifies_not_before_when_configured() {
    let gateway = fixture_with(json!({"claims_to_verify": ["exp", "nbf"]})).await;

    let future = token_with(json!({
        "iss": ISSUER,
        "exp": now() + 300,
        "nbf": now() + 120
    }));
    let mut request = get("/jwt/items");
    request.headers_mut().insert(
        header::AUTHORIZATION,
        header_default(&format!("Bearer {future}")),
    );
    assert_eq!(gateway.send(request).await.0, StatusCode::UNAUTHORIZED);

    let past = token_with(json!({
        "iss": ISSUER,
        "exp": now() + 300,
        "nbf": now() - 120
    }));
    let mut request = get("/jwt/items");
    request.headers_mut().insert(
        header::AUTHORIZATION,
        header_default(&format!("Bearer {past}")),
    );
    assert_eq!(gateway.send(request).await.0, StatusCode::CREATED);
}

#[tokio::test]
async fn skips_preflight_and_supports_anonymous_consumers() {
    let skipped = fixture_with(json!({"run_on_preflight": false})).await;
    assert_eq!(
        skipped.send(request("OPTIONS", "/jwt/items")).await.0,
        StatusCode::CREATED
    );

    let gateway = fixture_with(json!({"anonymous": "anon"})).await;
    gateway
        .storage
        .api
        .create_consumer(&CreateConsumer {
            username: Some("anon".into()),
            ..Default::default()
        })
        .await
        .unwrap();
    gateway.reload().await;

    let (status, _, json) = gateway.send(get("/jwt/items")).await;
    assert_eq!(status, StatusCode::CREATED);
    assert_eq!(json["x_consumer_username"], "anon");

    let unknown = fixture_with(json!({"anonymous": "ghost"})).await;
    assert_eq!(
        unknown.send(get("/jwt/items")).await.0,
        StatusCode::UNAUTHORIZED
    );
}
