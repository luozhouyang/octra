//! Local-first outbound URL policy.
//!
//! Loopback and private targets are intentional for an on-device gateway, but
//! cloud metadata and link-local endpoints are never valid MCP/OpenAPI
//! targets. This guard rejects them before a request is sent; it is applied to
//! configured MCP servers and to OpenAPI documents/external `$ref` fetches.

use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::time::Duration;

use reqwest::Url;

const RESOLVE_TIMEOUT: Duration = Duration::from_secs(5);
const PINNED_CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const PINNED_REQUEST_TIMEOUT: Duration = Duration::from_secs(60);

/// Validates a URL's scheme, credentials, host, and resolved addresses.
pub async fn ensure_safe_url(raw: &str) -> Result<(), String> {
    resolve_safe(raw).await.map(|_| ())
}

/// Builds a client pinned to the address that was validated for this URL.
///
/// This closes the DNS-rebinding window between validation and connect: the
/// request cannot resolve the hostname again to a different address. Redirects
/// remain disabled so a validated host cannot bounce to a blocked one.
pub async fn pinned_client(raw: &str) -> Result<reqwest::Client, String> {
    let target = resolve_safe(raw).await?;
    let mut builder = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .connect_timeout(PINNED_CONNECT_TIMEOUT)
        .timeout(PINNED_REQUEST_TIMEOUT);
    if let Some((host, address)) = target.pin {
        builder = builder.resolve(&host, address);
    }
    builder
        .build()
        .map_err(|error| format!("cannot build pinned HTTP client: {error}"))
}

/// Validates syntax and literal metadata addresses without requiring DNS.
/// Used when an operator saves a server that may be offline.
pub async fn ensure_safe_url_syntax(raw: &str) -> Result<(), String> {
    check_url(raw, false).await
}

async fn check_url(raw: &str, resolve: bool) -> Result<(), String> {
    if resolve {
        resolve_safe(raw).await.map(|_| ())
    } else {
        validate_url(raw, false).await.map(|_| ())
    }
}

struct SafeTarget {
    pin: Option<(String, SocketAddr)>,
}

async fn resolve_safe(raw: &str) -> Result<SafeTarget, String> {
    let (url, host, host_lower) = validate_url(raw, true).await?;
    if let Ok(ip) = host.parse::<IpAddr>() {
        if blocked_ip(ip) {
            return Err(format!(
                "URL address '{ip}' is not an allowed egress target"
            ));
        }
        return Ok(SafeTarget { pin: None });
    }

    let port = url
        .port_or_known_default()
        .ok_or_else(|| "URL has no usable port".to_string())?;
    let lookup = tokio::time::timeout(
        RESOLVE_TIMEOUT,
        tokio::net::lookup_host((host.as_str(), port)),
    )
    .await
    .map_err(|_| format!("timed out resolving '{host}'"))?
    .map_err(|error| format!("cannot resolve '{host}': {error}"))?;
    let mut any = false;
    let mut fallback = None;
    let mut preferred = None;
    for address in lookup {
        any = true;
        if blocked_ip(address.ip()) {
            return Err(format!(
                "URL host '{host}' resolves to a blocked address '{}'",
                address.ip()
            ));
        }
        if fallback.is_none() {
            fallback = Some(address);
        }
        if preferred.is_none() && address.ip().is_ipv4() {
            preferred = Some(address);
        }
    }
    if !any {
        return Err(format!("URL host '{host}' did not resolve"));
    }
    let address = preferred.or(fallback).expect("resolved address exists");
    let _ = host_lower;
    Ok(SafeTarget {
        pin: Some((host, address)),
    })
}

async fn validate_url(raw: &str, resolve: bool) -> Result<(Url, String, String), String> {
    let url = Url::parse(raw).map_err(|error| format!("invalid URL '{raw}': {error}"))?;
    if !matches!(url.scheme(), "http" | "https") {
        return Err(format!("unsupported URL scheme '{}'", url.scheme()));
    }
    if !url.username().is_empty() || url.password().is_some() {
        return Err("URLs with embedded credentials are not allowed".into());
    }
    let host = url
        .host_str()
        .ok_or_else(|| "URL has no host".to_string())?
        .to_string();
    let host_lower = host.trim_end_matches('.').to_ascii_lowercase();
    if is_metadata_host(&host_lower) {
        return Err(format!("URL host '{host}' is a metadata endpoint"));
    }
    if let Ok(ip) = host.parse::<IpAddr>() {
        if blocked_ip(ip) {
            return Err(format!(
                "URL address '{ip}' is not an allowed egress target"
            ));
        }
        return Ok((url, host, host_lower));
    }
    if !resolve {
        return Ok((url, host, host_lower));
    }
    Ok((url, host, host_lower))
}

fn is_metadata_host(host: &str) -> bool {
    matches!(
        host,
        "metadata.google.internal" | "metadata.goog" | "instance-data" | "metadata.azure.internal"
    )
}

fn blocked_ip(ip: IpAddr) -> bool {
    let ip = match ip {
        IpAddr::V6(v6) => v6.to_ipv4_mapped().map(IpAddr::V4).unwrap_or(ip),
        IpAddr::V4(_) => ip,
    };
    match ip {
        IpAddr::V4(v4) => {
            v4.is_link_local()
                || v4.is_unspecified()
                || v4.is_multicast()
                || v4.is_broadcast()
                || v4 == Ipv4Addr::new(100, 100, 100, 200)
                || v4 == Ipv4Addr::new(168, 63, 129, 16)
        }
        IpAddr::V6(v6) => {
            v6.is_unspecified()
                || v6.is_multicast()
                || v6.is_unicast_link_local()
                || v6.segments()[..4] == [0xfd00, 0x0ec2, 0, 0]
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn blocks_metadata_and_link_local_addresses() {
        assert!(blocked_ip("169.254.169.254".parse().unwrap()));
        assert!(blocked_ip("100.100.100.200".parse().unwrap()));
        assert!(blocked_ip("168.63.129.16".parse().unwrap()));
        assert!(blocked_ip("fe80::1".parse().unwrap()));
        assert!(blocked_ip("::ffff:169.254.169.254".parse().unwrap()));
    }

    #[test]
    fn allows_loopback_and_private_addresses() {
        assert!(!blocked_ip("127.0.0.1".parse().unwrap()));
        assert!(!blocked_ip("10.0.0.5".parse().unwrap()));
        assert!(!blocked_ip("192.168.1.5".parse().unwrap()));
    }

    #[tokio::test]
    async fn pinned_client_rejects_metadata_and_builds_for_loopback() {
        assert!(
            pinned_client("http://169.254.169.254/latest")
                .await
                .is_err()
        );
        assert!(pinned_client("http://127.0.0.1:9/mcp").await.is_ok());
    }

    #[tokio::test]
    async fn rejects_metadata_hosts_without_dns() {
        assert!(
            ensure_safe_url("http://metadata.google.internal/latest")
                .await
                .is_err()
        );
        assert!(
            ensure_safe_url("http://169.254.169.254/latest")
                .await
                .is_err()
        );
        assert!(ensure_safe_url("http://127.0.0.1:8080/mcp").await.is_ok());
    }
}
