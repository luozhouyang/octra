//! Kong-compatible Admin API mounted at `/admin` on the control plane.
//!
//! Lists use the `{data, next, offset}` envelope, entities expose foreign keys
//! as `{"id": ...}` objects, and every mutation reloads the runtime snapshot
//! and plugin chains before returning.

mod consumer_groups;
mod consumers;
mod credentials;
mod error;
mod installed_plugins;
mod list;
mod operations;
mod plugin_api;
mod plugins;
mod policies;
mod routes;
mod schemas;
mod services;
mod status;
mod upstreams;

use axum::Router;
use axum::routing::{any, get, post};

use crate::ApiControlState;

use error::{AdminError, AdminResult};

/// Routes for the Kong-style Admin API, mounted inside the control router so
/// the daemon's bearer authentication and audit layer still apply.
pub fn router() -> Router<ApiControlState> {
    Router::new()
        .route(
            "/admin/services",
            get(services::list).post(services::create),
        )
        .route(
            "/admin/services/{id}",
            get(services::get)
                .patch(services::update)
                .put(services::upsert)
                .delete(services::delete),
        )
        .route(
            "/admin/services/{id}/routes",
            get(routes::list_for_service).post(routes::create_for_service),
        )
        .route(
            "/admin/services/{id}/plugins",
            get(plugins::list_for_service).post(plugins::create_for_service),
        )
        .route("/admin/routes", get(routes::list).post(routes::create))
        .route(
            "/admin/routes/{id}",
            get(routes::get)
                .patch(routes::update)
                .put(routes::upsert)
                .delete(routes::delete),
        )
        .route(
            "/admin/routes/{id}/plugins",
            get(plugins::list_for_route).post(plugins::create_for_route),
        )
        .route(
            "/admin/upstreams",
            get(upstreams::list).post(upstreams::create),
        )
        .route(
            "/admin/upstreams/{id}",
            get(upstreams::get)
                .patch(upstreams::update)
                .put(upstreams::upsert)
                .delete(upstreams::delete),
        )
        .route(
            "/admin/upstreams/{id}/targets",
            get(upstreams::list_targets).post(upstreams::create_target),
        )
        .route(
            "/admin/targets",
            get(upstreams::list_all_targets).post(upstreams::create_any_target),
        )
        .route(
            "/admin/targets/{id}",
            get(upstreams::get_target)
                .patch(upstreams::update_target)
                .put(upstreams::upsert_target)
                .delete(upstreams::delete_target),
        )
        .route(
            "/admin/consumers",
            get(consumers::list).post(consumers::create),
        )
        .route(
            "/admin/consumers/{id}",
            get(consumers::get)
                .patch(consumers::update)
                .put(consumers::upsert)
                .delete(consumers::delete),
        )
        .route(
            "/admin/consumers/{id}/plugins",
            get(plugins::list_for_consumer).post(plugins::create_for_consumer),
        )
        .route(
            "/admin/consumers/{id}/key-auth",
            get(credentials::list_keyauth_for_consumer)
                .post(credentials::create_keyauth_for_consumer),
        )
        .route(
            "/admin/consumers/{id}/basic-auth",
            get(credentials::list_basicauth_for_consumer)
                .post(credentials::create_basicauth_for_consumer),
        )
        .route(
            "/admin/consumers/{id}/jwt",
            get(credentials::list_jwt_for_consumer).post(credentials::create_jwt_for_consumer),
        )
        .route(
            "/admin/consumers/{id}/acls",
            get(credentials::list_acls_for_consumer).post(credentials::create_acl_for_consumer),
        )
        .route(
            "/admin/consumers/{id}/consumer_groups",
            get(consumer_groups::list_for_consumer),
        )
        .route(
            "/admin/consumer_groups",
            get(consumer_groups::list).post(consumer_groups::create),
        )
        .route(
            "/admin/consumer_groups/{id}",
            get(consumer_groups::get)
                .patch(consumer_groups::update)
                .put(consumer_groups::upsert)
                .delete(consumer_groups::delete),
        )
        .route(
            "/admin/consumer_groups/{id}/consumers",
            get(consumer_groups::list_members).post(consumer_groups::add_member),
        )
        .route(
            "/admin/consumer_groups/{id}/consumers/{consumer_id}",
            axum::routing::delete(consumer_groups::remove_member),
        )
        .route(
            "/admin/consumer_groups/{id}/plugins",
            get(plugins::list_for_consumer_group).post(plugins::create_for_consumer_group),
        )
        .route(
            "/admin/policies",
            get(policies::list).post(policies::create),
        )
        .route(
            "/admin/policies/{id}",
            get(policies::get)
                .patch(policies::update)
                .put(policies::upsert)
                .delete(policies::delete),
        )
        .route("/admin/plugins", get(plugins::list).post(plugins::create))
        .route("/admin/plugins/enabled", get(plugins::enabled))
        .route(
            "/admin/plugin-artifacts",
            get(installed_plugins::list_artifacts).post(installed_plugins::upload_artifact),
        )
        .route(
            "/admin/plugin-artifacts/{id}",
            get(installed_plugins::get_artifact).delete(installed_plugins::delete_artifact),
        )
        .route("/admin/plugin-metrics", get(installed_plugins::all_metrics))
        .route(
            "/admin/plugins/{name}/health",
            get(installed_plugins::health),
        )
        .route(
            "/admin/plugins/{name}/metrics",
            get(installed_plugins::metrics),
        )
        .route(
            "/admin/plugins/{name}/install",
            post(installed_plugins::install),
        )
        .route(
            "/admin/plugins/{name}/upgrade",
            post(installed_plugins::upgrade),
        )
        .route(
            "/admin/plugins/{name}/uninstall",
            post(installed_plugins::uninstall),
        )
        .route(
            "/admin/plugins/{name}/migrations",
            get(installed_plugins::migrations),
        )
        .route(
            "/admin/plugins/{name}/entities/{entity}",
            get(installed_plugins::entity_rows),
        )
        .route(
            "/admin/plugins/{name}/admin-routes",
            get(plugin_api::admin_routes),
        )
        .route("/admin/plugins/{name}/api", any(plugin_api::dispatch_root))
        .route(
            "/admin/plugins/{name}/api/{*path}",
            any(plugin_api::dispatch),
        )
        .route(
            "/admin/plugins/{id}",
            get(plugins::get)
                .patch(plugins::update)
                .put(plugins::upsert)
                .delete(plugins::delete),
        )
        .route(
            "/admin/key-auths",
            get(credentials::list_keyauth).post(credentials::create_keyauth),
        )
        .route(
            "/admin/key-auths/{id}",
            get(credentials::get_keyauth)
                .patch(credentials::update_keyauth)
                .put(credentials::upsert_keyauth)
                .delete(credentials::delete_keyauth),
        )
        .route(
            "/admin/basic-auths",
            get(credentials::list_basicauth).post(credentials::create_basicauth),
        )
        .route(
            "/admin/basic-auths/{id}",
            get(credentials::get_basicauth)
                .patch(credentials::update_basicauth)
                .put(credentials::upsert_basicauth)
                .delete(credentials::delete_basicauth),
        )
        .route(
            "/admin/jwts",
            get(credentials::list_jwt).post(credentials::create_jwt),
        )
        .route(
            "/admin/jwts/{id}",
            get(credentials::get_jwt)
                .patch(credentials::update_jwt)
                .put(credentials::upsert_jwt)
                .delete(credentials::delete_jwt),
        )
        .route(
            "/admin/acls",
            get(credentials::list_acls).post(credentials::create_acl),
        )
        .route(
            "/admin/acls/{id}",
            get(credentials::get_acl)
                .patch(credentials::update_acl)
                .put(credentials::upsert_acl)
                .delete(credentials::delete_acl),
        )
        .route("/admin/schemas/plugins", get(schemas::list))
        .route("/admin/schemas/plugins/validate", post(schemas::validate))
        .route("/admin/schemas/plugins/{name}", get(schemas::get))
        .route("/admin/status", get(status::status))
        .route("/admin/metrics", get(operations::metrics))
        .route(
            "/admin/metrics/prometheus",
            get(operations::prometheus_metrics),
        )
        .route("/admin/logs", get(operations::logs))
        .route("/admin/audit-logs", get(operations::audit_logs))
        .route("/admin/reload", post(operations::reload))
}

pub(crate) async fn reload_all(state: &ApiControlState) -> AdminResult<()> {
    state.runtime.reload().await.map_err(AdminError::from)?;
    let plugins = state
        .storage
        .api
        .list_plugin_instances()
        .await
        .map_err(AdminError::from)?;
    state.plugins.reload(plugins).map_err(AdminError::from)?;
    Ok(())
}

async fn require_service(state: &ApiControlState, id: &str) -> AdminResult<()> {
    match state.storage.api.get_service(id).await? {
        Some(_) => Ok(()),
        None => Err(AdminError::not_found(format!("service '{id}' not found"))),
    }
}

async fn require_route(state: &ApiControlState, id: &str) -> AdminResult<()> {
    match state.storage.api.get_route(id).await? {
        Some(_) => Ok(()),
        None => Err(AdminError::not_found(format!("route '{id}' not found"))),
    }
}

async fn require_upstream(state: &ApiControlState, id: &str) -> AdminResult<()> {
    match state.storage.api.get_upstream(id).await? {
        Some(_) => Ok(()),
        None => Err(AdminError::not_found(format!("upstream '{id}' not found"))),
    }
}

async fn require_consumer(state: &ApiControlState, id: &str) -> AdminResult<()> {
    match state.storage.api.get_consumer(id).await? {
        Some(_) => Ok(()),
        None => Err(AdminError::not_found(format!("consumer '{id}' not found"))),
    }
}

pub(crate) async fn resolve_consumer_group_id(
    state: &ApiControlState,
    id_or_name: &str,
) -> AdminResult<String> {
    if let Some(group) = state.storage.api.get_consumer_group(id_or_name).await? {
        return Ok(group.id);
    }
    state
        .storage
        .api
        .get_consumer_group_by_name(id_or_name)
        .await?
        .map(|group| group.id)
        .ok_or_else(|| AdminError::not_found(format!("consumer group '{id_or_name}' not found")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn admin_routes_do_not_conflict() {
        let _ = router();
    }
}
