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
        vec![PluginPhase::Request, PluginPhase::Log]
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

    async fn on_request(&self, cx: &mut RequestContext<'_>) -> PluginResult {
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
