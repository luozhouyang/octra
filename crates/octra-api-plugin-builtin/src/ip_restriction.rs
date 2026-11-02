//! IP/CIDR restrictions.

use std::net::IpAddr;
use std::sync::Arc;

use async_trait::async_trait;
use ipnet::IpNet;
use octra_api_plugin_types::{
    ApiGatewayPlugin, ApiGatewayPluginFactory, PluginAction, PluginError, PluginManifest,
    PluginPhase, PluginResult, RequestContext,
};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::{manifest, plugin_response};

pub struct IpRestrictionFactory;

impl ApiGatewayPluginFactory for IpRestrictionFactory {
    fn manifest(&self) -> PluginManifest {
        manifest(
            "ip-restriction",
            990,
            vec![],
            "Allows or denies requests by client IP or CIDR",
            json!({
                "type": "object",
                "properties": {
                    "allow": { "type": "array", "items": { "type": "string" } },
                    "deny": { "type": "array", "items": { "type": "string" } },
                    "status": { "type": ["integer", "null"] },
                    "message": { "type": "string" }
                }
            }),
        )
    }

    fn create(&self, _id: &str, config: Value) -> Result<Arc<dyn ApiGatewayPlugin>, PluginError> {
        let config: Config = serde_json::from_value(config).map_err(|error| {
            PluginError::fail_closed(format!("invalid ip-restriction config: {error}"))
        })?;
        if config.allow.is_empty() && config.deny.is_empty() {
            return Err(PluginError::fail_closed(
                "ip-restriction requires allow or deny",
            ));
        }
        Ok(Arc::new(IpRestrictionPlugin {
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
    status: Option<u16>,
    message: Option<String>,
}

struct IpRestrictionPlugin {
    manifest: PluginManifest,
    config: Config,
}

#[async_trait]
impl ApiGatewayPlugin for IpRestrictionPlugin {
    fn manifest(&self) -> &PluginManifest {
        &self.manifest
    }

    async fn on_request(&self, cx: &mut RequestContext<'_>) -> PluginResult {
        let Some(ip) = cx.client_ip else {
            return Ok(PluginAction::Continue);
        };

        let allowed = if self.config.allow.is_empty() {
            true
        } else {
            matches_any(ip, &self.config.allow)
        };
        let denied = !self.config.deny.is_empty() && matches_any(ip, &self.config.deny);

        if allowed && !denied {
            return Ok(PluginAction::Continue);
        }

        let status = self
            .config
            .status
            .and_then(|status| http::StatusCode::from_u16(status).ok())
            .unwrap_or(http::StatusCode::FORBIDDEN);
        let message = self
            .config
            .message
            .as_deref()
            .unwrap_or("Your IP address is not allowed");
        Ok(PluginAction::ShortCircuit(plugin_response(status, message)))
    }
}

fn matches_any(ip: IpAddr, rules: &[String]) -> bool {
    rules.iter().any(|rule| {
        rule.parse::<IpAddr>()
            .map(|candidate| candidate == ip)
            .or_else(|_| rule.parse::<IpNet>().map(|net| net.contains(&ip)))
            .unwrap_or(false)
    })
}
