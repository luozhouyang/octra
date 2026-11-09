//! Upstream HTTP client construction.
//!
//! The shared client sets the connection pool and a global connect timeout.
//! Per request we apply the service's own connect and request timeouts on top.

use std::time::Duration;

use octra_config::OctraConfig;

pub fn build(config: &OctraConfig) -> reqwest::Client {
    reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(
            config.gateway.connect_timeout_secs.max(1),
        ))
        .pool_idle_timeout(Duration::from_secs(90))
        .build()
        .unwrap_or_else(|error| {
            tracing::warn!(%error, "failed to build the API gateway HTTP client; using defaults");
            reqwest::Client::new()
        })
}
