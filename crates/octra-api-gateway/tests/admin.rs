//! End-to-end tests for the Kong-compatible `/admin` API.

mod common;

use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use serde_json::{Value, json};

use crate::common::{Gateway, gateway, get, header_default, request};

fn json_request(method: &str, uri: &str, body: Value) -> Request<Body> {
    let mut request = request(method, uri);
    request
        .headers_mut()
        .insert(header::CONTENT_TYPE, header_default("application/json"));
    *request.body_mut() = Body::from(body.to_string());
    request
}

async fn create_service(gateway: &Gateway, body: Value) -> Value {
    let (status, _, service) = gateway
        .send_control(json_request("POST", "/admin/services", body))
        .await;
    assert_eq!(status, StatusCode::CREATED, "{service}");
    service
}

#[tokio::test]
async fn services_routes_and_nested_fk_objects() {
    let gateway = gateway().await;
    let service = create_service(
        &gateway,
        json!({"name": "svc", "host": "127.0.0.1", "port": 8080}),
    )
    .await;
    let service_id = service["id"].as_str().unwrap().to_string();

    let (status, _, route) = gateway
        .send_control(json_request(
            "POST",
            "/admin/routes",
            json!({"paths": ["/api"], "service": {"id": service_id}}),
        ))
        .await;
    assert_eq!(status, StatusCode::CREATED);
    assert_eq!(route["service"]["id"], service_id);

    let (status, _, fetched) = gateway
        .send_control(get(&format!(
            "/admin/routes/{}",
            route["id"].as_str().unwrap()
        )))
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(fetched["service"]["id"], service_id);

    let (status, _, routes) = gateway
        .send_control(get(&format!("/admin/services/{service_id}/routes")))
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(routes["data"].as_array().unwrap().len(), 1);
    assert!(routes.get("next").is_some());
}

#[tokio::test]
async fn list_envelope_and_cursor_pagination() {
    let gateway = gateway().await;
    for name in ["a", "b", "c"] {
        create_service(&gateway, json!({"name": name})).await;
    }

    let (status, _, page) = gateway.send_control(get("/admin/services?size=2")).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(page["data"].as_array().unwrap().len(), 2);
    let next = page["next"].as_str().unwrap();
    assert!(next.contains("offset="));

    let (status, _, rest) = gateway.send_control(get(next)).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(rest["data"].as_array().unwrap().len(), 1);
    assert!(rest["next"].is_null());
}

#[tokio::test]
async fn tag_filters_support_and_or() {
    let gateway = gateway().await;
    for (username, tags) in [
        ("a", json!(["prod", "eu"])),
        ("b", json!(["prod"])),
        ("c", json!(["dev"])),
    ] {
        let (status, _, _) = gateway
            .send_control(json_request(
                "POST",
                "/admin/consumers",
                json!({"username": username, "tags": tags}),
            ))
            .await;
        assert_eq!(status, StatusCode::CREATED);
    }

    let count = |page: &Value| page["data"].as_array().unwrap().len();
    let (_, _, page) = gateway
        .send_control(get("/admin/consumers?tags=prod"))
        .await;
    assert_eq!(count(&page), 2);
    let (_, _, page) = gateway
        .send_control(get("/admin/consumers?tags=prod,eu"))
        .await;
    assert_eq!(count(&page), 1);
    let (_, _, page) = gateway
        .send_control(get("/admin/consumers?tags=prod/dev"))
        .await;
    assert_eq!(count(&page), 3);
}

#[tokio::test]
async fn consumer_groups_and_group_scoped_plugins() {
    let gateway = gateway().await;
    let (_, _, consumer) = gateway
        .send_control(json_request(
            "POST",
            "/admin/consumers",
            json!({"username": "alice"}),
        ))
        .await;
    let consumer_id = consumer["id"].as_str().unwrap().to_string();
    let (_, _, group) = gateway
        .send_control(json_request(
            "POST",
            "/admin/consumer_groups",
            json!({"name": "admins"}),
        ))
        .await;
    let group_id = group["id"].as_str().unwrap().to_string();

    let (status, _, member) = gateway
        .send_control(json_request(
            "POST",
            &format!("/admin/consumer_groups/{group_id}/consumers"),
            json!({"consumer": {"id": consumer_id}}),
        ))
        .await;
    assert_eq!(status, StatusCode::CREATED);
    assert_eq!(member["consumer_id"], consumer_id);

    let (status, _, plugin) = gateway
        .send_control(json_request(
            "POST",
            &format!("/admin/consumer_groups/{group_id}/plugins"),
            json!({
                "name": "request-termination",
                "config": {"status_code": 418}
            }),
        ))
        .await;
    assert_eq!(status, StatusCode::CREATED);
    assert_eq!(plugin["consumer_group"]["id"], group_id);
    assert!(plugin["consumer"].is_null());

    let (status, _, plugins) = gateway
        .send_control(get(&format!("/admin/consumer_groups/{group_id}/plugins")))
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(plugins["data"].as_array().unwrap().len(), 1);

    let (status, _, duplicate) = gateway
        .send_control(json_request(
            "POST",
            &format!("/admin/consumer_groups/{group_id}/plugins"),
            json!({"name": "request-termination"}),
        ))
        .await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(duplicate["name"], "unique constraint violation");

    let (status, _, error) = gateway
        .send_control(json_request(
            "POST",
            &format!("/admin/consumers/{consumer_id}/plugins"),
            json!({"name": "key-auth"}),
        ))
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(error["name"], "schema violation");
}

#[tokio::test]
async fn plugin_schemas_and_status() {
    let gateway = gateway().await;

    let (status, _, schemas) = gateway.send_control(get("/admin/schemas/plugins")).await;
    assert_eq!(status, StatusCode::OK);
    assert!(schemas.get("key-auth").is_some());

    let (status, _, schema) = gateway
        .send_control(get("/admin/schemas/plugins/key-auth"))
        .await;
    assert_eq!(status, StatusCode::OK);
    assert!(schema["properties"].is_object());

    let (status, _, valid) = gateway
        .send_control(json_request(
            "POST",
            "/admin/schemas/plugins/validate",
            json!({"name": "key-auth", "config": {"key_names": ["apikey"]}}),
        ))
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(valid["valid"], true);

    let (_, _, invalid) = gateway
        .send_control(json_request(
            "POST",
            "/admin/schemas/plugins/validate",
            json!({"name": "key-auth", "config": {"key_names": 3}}),
        ))
        .await;
    assert_eq!(invalid["valid"], false);

    let (status, _, enabled) = gateway.send_control(get("/admin/plugins/enabled")).await;
    assert_eq!(status, StatusCode::OK);
    let enabled = enabled["enabled_plugins"].as_array().unwrap();
    assert!(enabled.iter().any(|name| name == "key-auth"));

    let (status, _, status_body) = gateway.send_control(get("/admin/status")).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(status_body["database"]["reachable"], true);
}

#[tokio::test]
async fn admin_errors_use_kong_shapes() {
    let gateway = gateway().await;
    let (status, _, missing) = gateway.send_control(get("/admin/services/missing")).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(missing["name"], "not found");

    let (status, _, invalid) = gateway
        .send_control(json_request("POST", "/admin/services", json!({})))
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(invalid["name"], "schema violation");
    assert_eq!(invalid["code"], 2);

    create_service(&gateway, json!({"name": "dup"})).await;
    let (status, _, conflict) = gateway
        .send_control(json_request(
            "POST",
            "/admin/services",
            json!({"name": "dup"}),
        ))
        .await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(conflict["name"], "unique constraint violation");
}
