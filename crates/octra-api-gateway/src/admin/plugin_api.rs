//! Custom Admin API routes declared by installed WASM plugins.
//!
//! Plugins list their routes in `plugin.json`; requests are mounted under
//! `/admin/plugins/{name}/api` and only dispatched when the operator approved
//! the `admin_api` capability.

use axum::Json;
use axum::extract::{Path, Request, State};
use axum::http::HeaderMap;
use axum::response::Response;
use octra_api_core::PluginInstance;
use octra_api_plugin_types::{
    AdminRouteMatch, Capability, PluginError, PluginHostProvider, PluginManifest, match_admin_route,
};
use octra_api_plugin_wasm::AdminDispatch;
use octra_data_plane::new_request_id;
use serde_json::{Value, json};

use crate::ApiControlState;

use super::error::{AdminError, AdminResult};
use super::installed_plugins::wasm;

pub async fn admin_routes(
    State(state): State<ApiControlState>,
    Path(name): Path<String>,
) -> AdminResult<Json<Value>> {
    wasm(&state)?;
    let record = load_record(&state, &name).await?;
    let manifest = parse_manifest(&record.manifest_json)?;
    let approved = record.approved_capabilities.contains(&Capability::AdminApi);
    let routes: Vec<Value> = manifest
        .admin_routes
        .iter()
        .map(|route| {
            json!({
                "method": route.method.to_ascii_uppercase(),
                "path": route.path,
                "full_path": full_path(&name, &route.path),
                "summary": route.summary,
                "description": route.description,
            })
        })
        .collect();
    Ok(Json(json!({
        "name": name,
        "installed": true,
        "enabled": record.enabled,
        "approved": approved,
        "routes": routes,
    })))
}

pub async fn dispatch_root(
    State(state): State<ApiControlState>,
    Path(name): Path<String>,
    request: Request,
) -> AdminResult<Response> {
    dispatch_inner(state, name, "/".to_string(), request).await
}

pub async fn dispatch(
    State(state): State<ApiControlState>,
    Path((name, path)): Path<(String, String)>,
    request: Request,
) -> AdminResult<Response> {
    let path = if path.is_empty() {
        "/".to_string()
    } else {
        format!("/{path}")
    };
    dispatch_inner(state, name, path, request).await
}

async fn dispatch_inner(
    state: ApiControlState,
    name: String,
    path: String,
    request: Request,
) -> AdminResult<Response> {
    let wasm = wasm(&state)?;
    let record = load_record(&state, &name).await?;
    if record.runtime_type != "wasm" {
        return Err(AdminError::not_found(format!(
            "plugin '{name}' is not a wasm plugin"
        )));
    }
    if !record.enabled {
        return Err(AdminError::unavailable(format!(
            "plugin '{name}' is disabled"
        )));
    }
    let manifest = parse_manifest(&record.manifest_json)?;
    let method = request.method().as_str().to_ascii_uppercase();
    let params = match match_admin_route(&manifest.admin_routes, &method, &path) {
        AdminRouteMatch::Found { params, .. } => params,
        AdminRouteMatch::MethodNotAllowed => {
            return Err(AdminError::method_not_allowed(format!(
                "plugin '{name}' has no admin route {method} {path}"
            )));
        }
        AdminRouteMatch::NotFound => {
            return Err(AdminError::not_found(format!(
                "plugin '{name}' has no admin route {method} {path}"
            )));
        }
    };

    let query = request.uri().query().unwrap_or("").to_string();
    let headers = plugin_headers(request.headers());
    let max_body = wasm.engine.limits().max_body_bytes;
    let body = axum::body::to_bytes(request.into_body(), max_body)
        .await
        .map_err(|_| AdminError::payload_too_large("request body is too large"))?;

    let config = plugin_config(&state, &name).await?;
    let instance = PluginInstance {
        id: format!("{name}:admin"),
        name: name.clone(),
        instance_name: None,
        config: config.clone(),
        enabled: true,
        protocols: Vec::new(),
        ordering: None,
        service_id: None,
        route_id: None,
        consumer_id: None,
        consumer_group_id: None,
        tags: Vec::new(),
        created_at: 0,
        updated_at: 0,
    };
    let host = wasm.host_provider.host_for(&instance);
    if !host.capabilities().contains(&Capability::AdminApi) {
        return Err(AdminError::forbidden(format!(
            "the admin_api capability is not approved for plugin '{name}'"
        )));
    }
    let factory = wasm
        .catalog
        .get(&name)
        .ok_or_else(|| AdminError::unavailable(format!("plugin '{name}' is not loaded")))?;
    let plugin = tokio::task::spawn_blocking(move || factory.create_admin(host, config))
        .await
        .map_err(|error| AdminError::internal(format!("plugin task failed: {error}")))?
        .map_err(plugin_error)?;

    plugin
        .dispatch_admin(AdminDispatch {
            request_id: new_request_id(),
            method,
            path,
            query,
            headers,
            params,
            body,
        })
        .await
        .map_err(plugin_error)
}

async fn load_record(
    state: &ApiControlState,
    name: &str,
) -> AdminResult<octra_storage::PluginRecord> {
    state
        .storage
        .llm
        .get_plugin_by_name(name)
        .await?
        .ok_or_else(|| AdminError::not_found(format!("plugin '{name}' is not installed")))
}

fn parse_manifest(raw: &str) -> AdminResult<PluginManifest> {
    serde_json::from_str(raw)
        .map_err(|error| AdminError::internal(format!("invalid installed manifest: {error}")))
}

async fn plugin_config(state: &ApiControlState, name: &str) -> AdminResult<Value> {
    let instances = state.storage.api.list_plugin_instances().await?;
    Ok(instances
        .into_iter()
        .filter(|instance| instance.name == name && instance.enabled)
        .min_by(|left, right| left.id.cmp(&right.id))
        .map(|instance| instance.config)
        .unwrap_or_else(|| json!({})))
}

fn plugin_headers(headers: &HeaderMap) -> Vec<(String, String)> {
    headers
        .iter()
        .filter(|(name, _)| {
            !matches!(
                name.as_str(),
                "authorization" | "cookie" | "proxy-authorization"
            )
        })
        .filter_map(|(name, value)| {
            value
                .to_str()
                .ok()
                .map(|value| (name.as_str().to_string(), value.to_string()))
        })
        .collect()
}

fn full_path(name: &str, path: &str) -> String {
    if path == "/" {
        format!("/admin/plugins/{name}/api")
    } else {
        format!("/admin/plugins/{name}/api{path}")
    }
}

fn plugin_error(error: PluginError) -> AdminError {
    if error.message.contains("byte limit") {
        AdminError::payload_too_large(error.message)
    } else {
        AdminError::internal(error.message)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::{HeaderName, HeaderValue};

    #[test]
    fn strips_control_plane_credentials_from_plugin_headers() {
        let mut headers = HeaderMap::new();
        headers.insert(
            HeaderName::from_static("authorization"),
            HeaderValue::from_static("Bearer secret"),
        );
        headers.insert(
            HeaderName::from_static("cookie"),
            HeaderValue::from_static("session=secret"),
        );
        headers.insert(
            HeaderName::from_static("x-request-id"),
            HeaderValue::from_static("req-1"),
        );
        let pairs = plugin_headers(&headers);
        assert_eq!(
            pairs,
            vec![("x-request-id".to_string(), "req-1".to_string())]
        );
    }

    #[test]
    fn builds_full_plugin_paths() {
        assert_eq!(
            full_path("demo", "/status"),
            "/admin/plugins/demo/api/status"
        );
        assert_eq!(full_path("demo", "/"), "/admin/plugins/demo/api");
    }

    #[test]
    fn maps_body_limit_failures_to_413() {
        let error = plugin_error(PluginError::fail_closed(
            "plugin body of 10 bytes exceeds the 5 byte limit",
        ));
        assert_eq!(error.status, axum::http::StatusCode::PAYLOAD_TOO_LARGE);
    }
}
