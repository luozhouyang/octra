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
        vec![PluginPhase::Access, PluginPhase::Log]
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

    async fn access(&self, cx: &mut RequestContext<'_>) -> PluginResult {
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{RequestFixture, response_parts, short_circuit};
    use http::StatusCode;

    fn plugin(config: Value) -> Arc<dyn ApiGatewayPlugin> {
        IpRestrictionFactory
            .create("ip-restriction", config)
            .expect("ip-restriction config")
    }

    async fn request(
        plugin: &Arc<dyn ApiGatewayPlugin>,
        client_ip: Option<&str>,
    ) -> (PluginAction, RequestFixture) {
        let mut fixture = RequestFixture {
            client_ip: client_ip.map(|ip| ip.parse().expect("ip address")),
            ..RequestFixture::default()
        };
        let action = plugin.access(&mut fixture.context()).await.unwrap();
        (action, fixture)
    }

    #[test]
    fn validates_config() {
        assert!(IpRestrictionFactory.create("ip", json!({})).is_err());
        assert!(
            IpRestrictionFactory
                .create("ip", json!({"allow": ["10.0.0.1"]}))
                .is_ok()
        );
        assert!(
            IpRestrictionFactory
                .create("ip", json!({"deny": ["10.0.0.0/8"]}))
                .is_ok()
        );
        assert!(
            IpRestrictionFactory
                .create("ip", json!({"allow": ["2001:db8::/32"]}))
                .is_ok()
        );
        assert!(
            IpRestrictionFactory
                .create("ip", json!({"allow": ["10.0.0.1"], "status": 200}))
                .is_ok()
        );
        assert!(
            IpRestrictionFactory
                .create("ip", json!({"allow": ["10.0.0.1"], "unexpected": true}))
                .is_ok()
        );
        assert!(
            IpRestrictionFactory
                .create("ip", json!({"allow": ["10.0.0.1"], "status": "403"}))
                .is_err()
        );
    }

    #[test]
    fn declares_request_and_log_phases() {
        assert_eq!(
            IpRestrictionFactory.phases(),
            vec![PluginPhase::Access, PluginPhase::Log]
        );
    }

    #[test]
    fn matches_exact_addresses_cidrs_and_ipv6() {
        let v4: IpAddr = "10.1.2.3".parse().expect("ipv4");
        let v6: IpAddr = "2001:db8::1".parse().expect("ipv6");

        assert!(matches_any(v4, &["10.1.2.3".to_string()]));
        assert!(matches_any(v4, &["10.0.0.0/8".to_string()]));
        assert!(!matches_any(v4, &["192.168.0.0/16".to_string()]));
        assert!(matches_any(v6, &["2001:db8::1".to_string()]));
        assert!(matches_any(v6, &["2001:db8::/32".to_string()]));
        assert!(!matches_any(v6, &["2001:db9::/32".to_string()]));
        assert!(!matches_any(v4, &["not-an-address".to_string()]));
        assert!(!matches_any(v4, &["10.0.0.0/33".to_string()]));
        assert!(matches_any(
            v4,
            &["192.168.0.0/16".to_string(), "10.0.0.0/8".to_string()]
        ));
    }

    #[tokio::test]
    async fn allows_permitted_addresses_and_rejects_others() {
        let plugin = plugin(json!({"allow": ["10.0.0.0/8"]}));

        let (action, _) = request(&plugin, Some("10.1.2.3")).await;
        assert!(matches!(action, PluginAction::Continue));

        let (action, _) = request(&plugin, Some("192.168.1.1")).await;
        let (status, _, body) = response_parts(short_circuit(action)).await;
        assert_eq!(status, StatusCode::FORBIDDEN);
        let body: Value = serde_json::from_slice(&body).expect("json body");
        assert_eq!(body["message"], "Your IP address is not allowed");
    }

    #[tokio::test]
    async fn deny_rules_win_over_allow_rules() {
        let plugin = plugin(json!({"allow": ["10.0.0.0/8"], "deny": ["10.1.2.3"]}));

        let (action, _) = request(&plugin, Some("10.1.2.3")).await;
        let (status, _, _) = response_parts(short_circuit(action)).await;
        assert_eq!(status, StatusCode::FORBIDDEN);

        let (action, _) = request(&plugin, Some("10.9.9.9")).await;
        assert!(matches!(action, PluginAction::Continue));
    }

    #[tokio::test]
    async fn supports_custom_status_and_message() {
        let plugin = plugin(json!({
            "deny": ["192.168.0.0/16"],
            "status": 404,
            "message": "go away"
        }));

        let (action, _) = request(&plugin, Some("192.168.1.1")).await;
        let (status, _, body) = response_parts(short_circuit(action)).await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        let body: Value = serde_json::from_slice(&body).expect("json body");
        assert_eq!(body["message"], "go away");
    }

    #[tokio::test]
    async fn accepts_non_error_statuses_and_skips_missing_client_ips() {
        let plugin = plugin(json!({"deny": ["192.168.0.0/16"], "status": 200}));
        let (action, _) = request(&plugin, Some("192.168.1.1")).await;
        let (status, _, _) = response_parts(short_circuit(action)).await;
        assert_eq!(status, StatusCode::OK);

        let (action, _) = request(&plugin, None).await;
        assert!(matches!(action, PluginAction::Continue));
    }

    #[tokio::test]
    async fn handles_ipv6_rules() {
        let plugin = plugin(json!({"deny": ["2001:db8::/32"]}));
        let (action, _) = request(&plugin, Some("2001:db8::dead")).await;
        let (status, _, _) = response_parts(short_circuit(action)).await;
        assert_eq!(status, StatusCode::FORBIDDEN);

        let (action, _) = request(&plugin, Some("2001:db9::1")).await;
        assert!(matches!(action, PluginAction::Continue));
    }
}
