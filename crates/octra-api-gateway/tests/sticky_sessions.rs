//! Sticky cookie behavior for cookie-hashed upstreams.

mod common;

use std::net::SocketAddr;

use axum::Router;
use axum::body::Body;
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::Response;
use axum::routing::any;
use octra_api_core::{CreateRoute, CreateService, CreateTarget, CreateUpstream};

use common::*;

/// Upstream that answers like the shared mock but also emits its own cookie.
async fn spawn_set_cookie_mock(name: &'static str) -> SocketAddr {
    let app = Router::new().fallback(any(move || async move {
        Response::builder()
            .status(StatusCode::CREATED)
            .header(header::CONTENT_TYPE, "application/json")
            .header("x-upstream", name)
            .header(header::SET_COOKIE, "upstream=keep; Path=/; HttpOnly")
            .body(Body::from("{}"))
            .unwrap()
    }));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    addr
}

async fn seed_cookie_service(
    gateway: &Gateway,
    service_id: &str,
    cookie_name: &str,
    cookie_path: &str,
    targets: &[(&str, SocketAddr)],
) {
    let service = gateway
        .storage
        .api
        .create_service(&CreateService {
            id: Some(service_id.into()),
            name: service_id.into(),
            ..Default::default()
        })
        .await
        .unwrap();
    let upstream = gateway
        .storage
        .api
        .create_upstream(&CreateUpstream {
            id: Some(format!("{service_id}-up")),
            name: service_id.into(),
            algorithm: Some("consistent-hashing".into()),
            hash_on: Some("cookie".into()),
            hash_on_cookie: Some(cookie_name.into()),
            hash_on_cookie_path: Some(cookie_path.into()),
            ..Default::default()
        })
        .await
        .unwrap();
    for (id, addr) in targets {
        gateway
            .storage
            .api
            .create_target(&CreateTarget {
                id: Some((*id).into()),
                upstream_id: upstream.id.clone(),
                target: addr.to_string(),
                ..Default::default()
            })
            .await
            .unwrap();
    }
    gateway
        .storage
        .api
        .create_route(&CreateRoute {
            id: Some(format!("{service_id}-route")),
            service_id: Some(service.id),
            ..Default::default()
        })
        .await
        .unwrap();
}

fn set_cookies(headers: &HeaderMap) -> Vec<String> {
    headers
        .get_all(header::SET_COOKIE)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .map(str::to_string)
        .collect()
}

fn cookie_pair(cookies: &[String], name: &str) -> (String, String) {
    let value = |suffix: &str| {
        cookies
            .iter()
            .find_map(|cookie| {
                cookie
                    .strip_prefix(&format!("{name}{suffix}="))
                    .and_then(|rest| rest.split(';').next())
                    .map(str::to_string)
            })
            .unwrap_or_else(|| panic!("missing cookie {name}{suffix}: {cookies:?}"))
    };
    (value(""), value(".sig"))
}

#[tokio::test]
async fn sets_sticky_cookie_and_honors_it_on_follow_up_requests() {
    let (addr_a, _calls_a) = spawn_mock("sticky-a").await;
    let (addr_b, _calls_b) = spawn_mock("sticky-b").await;
    let gateway = gateway().await;
    seed_cookie_service(
        &gateway,
        "svc-sticky",
        "session",
        "/api",
        &[("sticky-a", addr_a), ("sticky-b", addr_b)],
    )
    .await;
    gateway.reload().await;

    let (status, headers, json) = gateway.send(get("/")).await;
    assert_eq!(status, StatusCode::CREATED);
    let selected = json["server"]
        .as_str()
        .expect("upstream name in response")
        .to_string();
    assert!(
        selected == "sticky-a" || selected == "sticky-b",
        "unexpected upstream {selected}"
    );
    let cookies = set_cookies(&headers);
    assert_eq!(cookies.len(), 2);
    assert!(
        cookies
            .iter()
            .any(|cookie| cookie.ends_with("; Path=/api; HttpOnly; SameSite=Lax"))
    );
    let (target, signature) = cookie_pair(&cookies, "session");
    assert_eq!(target, selected);

    let mut follow_up = get("/");
    follow_up.headers_mut().insert(
        header::COOKIE,
        header_default(&format!("session={target}; session.sig={signature}")),
    );
    let (status, headers, json) = gateway.send(follow_up).await;
    assert_eq!(status, StatusCode::CREATED);
    assert_eq!(json["server"].as_str(), Some(selected.as_str()));
    assert!(
        set_cookies(&headers).is_empty(),
        "a valid sticky cookie should not be reset"
    );
}

#[tokio::test]
async fn resets_invalid_sticky_cookie_to_the_selected_target() {
    let (addr_a, _calls_a) = spawn_mock("invalid-a").await;
    let (addr_b, _calls_b) = spawn_mock("invalid-b").await;
    let gateway = gateway().await;
    seed_cookie_service(
        &gateway,
        "svc-invalid",
        "session",
        "/sticky",
        &[("invalid-a", addr_a), ("invalid-b", addr_b)],
    )
    .await;
    gateway.reload().await;

    let mut request = get("/");
    request
        .headers_mut()
        .insert(header::COOKIE, header_default("session=not-a-target"));
    let (status, headers, json) = gateway.send(request).await;
    assert_eq!(status, StatusCode::CREATED);
    let selected = json["server"].as_str().expect("upstream name in response");
    let cookies = set_cookies(&headers);
    assert_eq!(cookies.len(), 2);
    let (target, signature) = cookie_pair(&cookies, "session");
    assert_eq!(target, selected);

    let mut follow_up = get("/");
    follow_up.headers_mut().insert(
        header::COOKIE,
        header_default(&format!("session={target}; session.sig={signature}")),
    );
    let (status, _, json) = gateway.send(follow_up).await;
    assert_eq!(status, StatusCode::CREATED);
    assert_eq!(json["server"].as_str(), Some(selected));

    // A tampered target id with the original signature is rejected and both
    // cookies are reset to the newly selected target.
    let mut tampered_request = get("/");
    tampered_request.headers_mut().insert(
        header::COOKIE,
        header_default(&format!(
            "session={selected}tampered; session.sig={signature}"
        )),
    );
    let (status, headers, _) = gateway.send(tampered_request).await;
    assert_eq!(status, StatusCode::CREATED);
    assert_eq!(set_cookies(&headers).len(), 2);
}

#[tokio::test]
async fn non_cookie_upstreams_do_not_set_sticky_cookies() {
    let (addr, _calls) = spawn_mock("plain").await;
    let gateway = gateway().await;
    seed_service(
        &gateway.storage,
        "svc-plain",
        &format!("http://{addr}"),
        false,
        "/",
        false,
    )
    .await;
    gateway.reload().await;

    let (status, headers, _) = gateway.send(get("/")).await;
    assert_eq!(status, StatusCode::CREATED);
    assert!(
        set_cookies(&headers).is_empty(),
        "round-robin upstreams must not set sticky cookies"
    );
}

#[tokio::test]
async fn preserves_upstream_set_cookie_headers() {
    let addr = spawn_set_cookie_mock("cookie-up").await;
    let gateway = gateway().await;
    seed_cookie_service(
        &gateway,
        "svc-cookie-up",
        "kong",
        "/",
        &[("cookie-target", addr)],
    )
    .await;
    gateway.reload().await;

    let (status, headers, _) = gateway.send(get("/")).await;
    assert_eq!(status, StatusCode::CREATED);
    let cookies = set_cookies(&headers);
    assert!(
        cookies.contains(&"upstream=keep; Path=/; HttpOnly".to_string()),
        "upstream cookie was not preserved: {cookies:?}"
    );
    assert!(
        cookies.contains(&"kong=cookie-target; Path=/; HttpOnly; SameSite=Lax".to_string()),
        "sticky target cookie was not appended: {cookies:?}"
    );
    assert!(
        cookies.iter().any(|cookie| cookie.starts_with("kong.sig=")),
        "sticky signature cookie was not appended: {cookies:?}"
    );
}

#[tokio::test]
async fn does_not_set_sticky_cookie_on_upstream_failure() {
    let gateway = gateway().await;
    let target: SocketAddr = "127.0.0.1:1".parse().unwrap();
    seed_cookie_service(
        &gateway,
        "svc-sticky-dead",
        "session",
        "/",
        &[("dead-target", target)],
    )
    .await;
    gateway.reload().await;

    let (status, headers, _) = gateway.send(get("/")).await;
    assert_eq!(status, StatusCode::BAD_GATEWAY);
    assert!(
        set_cookies(&headers).is_empty(),
        "failed upstream responses must not set sticky cookies"
    );
}
