//! End-to-end coverage for the `jwt` plugin.

use std::time::{SystemTime, UNIX_EPOCH};

use axum::http::{StatusCode, header};
use jsonwebtoken::{Algorithm, EncodingKey, Header, encode};
use octra_api_core::{CreateConsumer, CreateJwtSecret};
use serde_json::json;

use crate::common::{
    Gateway, gateway, get, header_default, seed_service, service_plugin, spawn_mock,
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
    service_plugin(&gateway.storage, "svc-jwt", "jwt", json!({})).await;

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
            algorithm: Some("HS256".into()),
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
