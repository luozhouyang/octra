//! ACL group authorization.

use std::sync::Arc;

use async_trait::async_trait;
use http::{HeaderName, HeaderValue, StatusCode};
use octra_api_plugin_types::{
    ApiGatewayPlugin, ApiGatewayPluginFactory, Capability, PluginAction, PluginError,
    PluginManifest, PluginPhase, PluginResult, RequestContext,
};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::{manifest, plugin_response};

pub struct AclFactory;

impl ApiGatewayPluginFactory for AclFactory {
    fn manifest(&self) -> PluginManifest {
        manifest(
            "acl",
            950,
            vec![Capability::RequestHeaders],
            "Allows or denies requests by consumer group membership",
            json!({
                "type": "object",
                "properties": {
                    "allow": { "type": "array", "items": { "type": "string" } },
                    "deny": { "type": "array", "items": { "type": "string" } },
                    "hide_groups_header": { "type": "boolean" },
                    "include_consumer_groups": { "type": "boolean" }
                }
            }),
        )
    }

    fn create(&self, _id: &str, config: Value) -> Result<Arc<dyn ApiGatewayPlugin>, PluginError> {
        let config: Config = serde_json::from_value(config)
            .map_err(|error| PluginError::fail_closed(format!("invalid acl config: {error}")))?;
        if !config.allow.is_empty() && !config.deny.is_empty() {
            return Err(PluginError::fail_closed(
                "acl requires exactly one of allow or deny",
            ));
        }
        Ok(Arc::new(AclPlugin {
            manifest: self.manifest(),
            config,
        }))
    }

    fn phases(&self) -> Vec<PluginPhase> {
        vec![PluginPhase::Access, PluginPhase::Log]
    }
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
struct Config {
    allow: Vec<String>,
    deny: Vec<String>,
    hide_groups_header: bool,
    include_consumer_groups: bool,
}

struct AclPlugin {
    manifest: PluginManifest,
    config: Config,
}

#[async_trait]
impl ApiGatewayPlugin for AclPlugin {
    fn manifest(&self) -> &PluginManifest {
        &self.manifest
    }

    async fn access(&self, cx: &mut RequestContext<'_>) -> PluginResult {
        let Some(consumer) = cx.state.consumer.as_ref() else {
            return Ok(PluginAction::ShortCircuit(plugin_response(
                StatusCode::UNAUTHORIZED,
                "Unauthorized",
            )));
        };

        let mut groups = consumer.acl_groups.clone();
        if self.config.include_consumer_groups {
            groups.extend(consumer.groups.iter().cloned());
        }
        groups.sort();
        groups.dedup();

        let denied = if !self.config.allow.is_empty() {
            !groups.iter().any(|group| self.config.allow.contains(group))
        } else if !self.config.deny.is_empty() {
            groups.iter().any(|group| self.config.deny.contains(group))
        } else {
            false
        };

        if denied {
            return Ok(PluginAction::ShortCircuit(plugin_response(
                StatusCode::FORBIDDEN,
                "You cannot consume this service",
            )));
        }

        if !self.config.hide_groups_header
            && !groups.is_empty()
            && let Ok(value) = HeaderValue::from_str(&groups.join(", "))
        {
            cx.headers
                .insert(HeaderName::from_static("x-consumer-groups"), value);
        }
        Ok(PluginAction::Continue)
    }
}

impl octra_api_plugin_types::ApiGatewayBackgroundPlugin for AclPlugin {}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{RequestFixture, response_parts, short_circuit};
    use octra_api_plugin_types::ConsumerInfo;

    fn plugin(config: Value) -> Arc<dyn ApiGatewayPlugin> {
        AclFactory.create("acl", config).expect("acl config")
    }

    fn consumer(acl_groups: &[&str]) -> ConsumerInfo {
        ConsumerInfo {
            id: "consumer-1".into(),
            acl_groups: acl_groups
                .iter()
                .map(|group| (*group).to_string())
                .collect(),
            ..ConsumerInfo::default()
        }
    }

    async fn message(response: axum::response::Response) -> (StatusCode, Value) {
        let (status, _, body) = response_parts(response).await;
        (status, serde_json::from_slice(&body).expect("json body"))
    }

    #[test]
    fn validates_allow_or_deny_exclusively() {
        assert!(
            AclFactory
                .create("acl", json!({"allow": ["admins"]}))
                .is_ok()
        );
        assert!(
            AclFactory
                .create("acl", json!({"deny": ["banned"]}))
                .is_ok()
        );
        assert!(
            AclFactory
                .create("acl", json!({"allow": ["admins"], "deny": ["banned"]}))
                .is_err()
        );
        assert!(
            AclFactory
                .create(
                    "acl",
                    json!({"allow": ["admins"], "hide_groups_header": true})
                )
                .is_ok()
        );
        assert!(
            AclFactory
                .create("acl", json!({"unexpected": true}))
                .is_ok()
        );
        assert!(
            AclFactory
                .create("acl", json!({"allow": "admins"}))
                .is_err()
        );
    }

    #[test]
    fn declares_request_and_log_phases() {
        assert_eq!(
            AclFactory.phases(),
            vec![PluginPhase::Access, PluginPhase::Log]
        );
    }

    #[tokio::test]
    async fn unauthenticated_requests_are_unauthorized() {
        let plugin = plugin(json!({"allow": ["admins"]}));
        let mut fixture = RequestFixture::default();
        let action = plugin.access(&mut fixture.context()).await.unwrap();
        let (status, body) = message(short_circuit(action)).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
        assert_eq!(body["message"], "Unauthorized");
    }

    #[tokio::test]
    async fn allow_lists_require_membership_and_emit_the_groups_header() {
        let plugin = plugin(json!({"allow": ["admins"]}));

        let mut fixture = RequestFixture::default();
        fixture.state.consumer = Some(consumer(&["admins", "users", "admins"]));
        let action = plugin.access(&mut fixture.context()).await.unwrap();
        assert!(matches!(action, PluginAction::Continue));
        assert_eq!(
            fixture.headers.get("x-consumer-groups").unwrap(),
            "admins, users"
        );

        let mut fixture = RequestFixture::default();
        fixture.state.consumer = Some(consumer(&["users"]));
        let action = plugin.access(&mut fixture.context()).await.unwrap();
        let (status, body) = message(short_circuit(action)).await;
        assert_eq!(status, StatusCode::FORBIDDEN);
        assert_eq!(body["message"], "You cannot consume this service");
    }

    #[tokio::test]
    async fn deny_lists_win_and_can_hide_the_groups_header() {
        let plugin = plugin(json!({"deny": ["banned"], "hide_groups_header": true}));

        let mut fixture = RequestFixture::default();
        fixture.state.consumer = Some(consumer(&["banned"]));
        let action = plugin.access(&mut fixture.context()).await.unwrap();
        let (status, _) = message(short_circuit(action)).await;
        assert_eq!(status, StatusCode::FORBIDDEN);

        let mut fixture = RequestFixture::default();
        fixture.state.consumer = Some(consumer(&["users"]));
        let action = plugin.access(&mut fixture.context()).await.unwrap();
        assert!(matches!(action, PluginAction::Continue));
        assert!(!fixture.headers.contains_key("x-consumer-groups"));
    }

    #[tokio::test]
    async fn consumer_groups_are_opt_in() {
        let strict = plugin(json!({"allow": ["admins"]}));
        let relaxed = plugin(json!({"allow": ["admins"], "include_consumer_groups": true}));

        let mut fixture = RequestFixture::default();
        fixture.state.consumer = Some(ConsumerInfo {
            id: "consumer-1".into(),
            groups: vec!["admins".into()],
            ..ConsumerInfo::default()
        });
        let action = strict.access(&mut fixture.context()).await.unwrap();
        let (status, _) = message(short_circuit(action)).await;
        assert_eq!(status, StatusCode::FORBIDDEN);

        let mut fixture = RequestFixture::default();
        fixture.state.consumer = Some(ConsumerInfo {
            id: "consumer-1".into(),
            groups: vec!["admins".into()],
            ..ConsumerInfo::default()
        });
        let action = relaxed.access(&mut fixture.context()).await.unwrap();
        assert!(matches!(action, PluginAction::Continue));
    }
}

