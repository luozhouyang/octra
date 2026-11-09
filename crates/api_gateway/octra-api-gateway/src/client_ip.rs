//! Client address resolution with an explicit trusted-proxy allowlist.
//!
//! Forwarded headers are honored only when the immediate transport peer is a
//! trusted proxy. With an empty `gateway.trusted_ips`, headers are ignored and
//! the peer address is used, so a client cannot spoof its address.

use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;

use axum::http::{HeaderMap, HeaderName};
use ipnet::{IpNet, Ipv4Net, Ipv6Net};
use octra_config::GatewayConfig;

const DEFAULT_HEADER: &str = "x-forwarded-for";

#[derive(Debug, Clone)]
pub struct ClientIpResolver {
    trusted: Arc<[IpNet]>,
    header: HeaderName,
    recursive: bool,
}

impl ClientIpResolver {
    pub fn from_config(config: &GatewayConfig) -> Self {
        let trusted = config
            .trusted_ips
            .iter()
            .filter_map(|rule| match parse_rule(rule) {
                Some(net) => Some(net),
                None => {
                    tracing::warn!(rule, "ignoring invalid gateway.trusted_ips entry");
                    None
                }
            })
            .collect::<Vec<_>>();
        let header = config
            .real_ip_header
            .parse::<HeaderName>()
            .unwrap_or_else(|_| HeaderName::from_static(DEFAULT_HEADER));
        Self {
            trusted: trusted.into(),
            header,
            recursive: config.real_ip_recursive,
        }
    }

    /// Returns the address plugins should treat as the client.
    ///
    /// A forwarded header only counts when the peer is trusted. Without a
    /// parseable forwarded address the peer address is returned unchanged.
    pub fn resolve(&self, peer: SocketAddr, headers: &HeaderMap) -> IpAddr {
        let peer_ip = normalize(peer.ip());
        if self.trusted.is_empty() || !self.is_trusted(peer_ip) {
            return peer_ip;
        }

        let forwarded = self.forwarded(headers);
        if forwarded.is_empty() {
            return peer_ip;
        }

        if self.recursive {
            forwarded
                .iter()
                .rev()
                .find(|ip| !self.is_trusted(**ip))
                .copied()
                .or_else(|| forwarded.first().copied())
                .unwrap_or(peer_ip)
        } else {
            forwarded.last().copied().unwrap_or(peer_ip)
        }
    }

    fn is_trusted(&self, ip: IpAddr) -> bool {
        self.trusted.iter().any(|net| net.contains(&ip))
    }

    fn forwarded(&self, headers: &HeaderMap) -> Vec<IpAddr> {
        headers
            .get_all(&self.header)
            .iter()
            .filter_map(|value| value.to_str().ok())
            .flat_map(|value| value.split(','))
            .filter_map(parse_address)
            .map(normalize)
            .collect()
    }
}

/// Parses a trusted rule as an address or CIDR. Single addresses normalize
/// IPv4-mapped IPv6 to IPv4 so both notations match the same peer.
fn parse_rule(rule: &str) -> Option<IpNet> {
    let rule = rule.trim();
    if let Ok(ip) = rule.parse::<IpAddr>() {
        return Some(match normalize(ip) {
            IpAddr::V4(ip) => IpNet::V4(Ipv4Net::new(ip, 32).ok()?),
            IpAddr::V6(ip) => IpNet::V6(Ipv6Net::new(ip, 128).ok()?),
        });
    }
    rule.parse::<IpNet>().ok()
}

/// Parses one forwarded entry, tolerating ports ("1.2.3.4:80",
/// "[2001:db8::1]:443"), bracketing, quotes, and surrounding whitespace.
fn parse_address(raw: &str) -> Option<IpAddr> {
    let raw = raw.trim().trim_matches('"');
    if raw.is_empty() {
        return None;
    }
    if let Ok(ip) = raw.parse::<IpAddr>() {
        return Some(ip);
    }
    if let Ok(addr) = raw.parse::<SocketAddr>() {
        return Some(addr.ip());
    }
    raw.strip_prefix('[')
        .and_then(|raw| raw.strip_suffix(']'))
        .and_then(|raw| raw.parse::<IpAddr>().ok())
}

fn normalize(ip: IpAddr) -> IpAddr {
    match ip {
        IpAddr::V6(ip) => ip
            .to_ipv4_mapped()
            .map(IpAddr::V4)
            .unwrap_or(IpAddr::V6(ip)),
        ip => ip,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderValue;

    fn peer(value: &str) -> SocketAddr {
        value.parse().expect("socket address")
    }

    fn headers(pairs: &[(&'static str, &str)]) -> HeaderMap {
        let mut map = HeaderMap::new();
        for (name, value) in pairs {
            map.append(*name, HeaderValue::from_str(value).expect("header value"));
        }
        map
    }

    fn config(trusted: &[&str], recursive: bool) -> GatewayConfig {
        GatewayConfig {
            trusted_ips: trusted.iter().map(|rule| (*rule).to_string()).collect(),
            real_ip_recursive: recursive,
            ..GatewayConfig::default()
        }
    }

    #[test]
    fn ignores_forwarded_headers_without_trusted_proxies() {
        let resolver = ClientIpResolver::from_config(&GatewayConfig::default());
        let headers = headers(&[("x-forwarded-for", "10.1.2.3")]);
        assert_eq!(
            resolver.resolve(peer("127.0.0.1:4242"), &headers),
            "127.0.0.1".parse::<IpAddr>().unwrap()
        );
    }

    #[test]
    fn ignores_forwarded_headers_from_untrusted_peers() {
        let resolver = ClientIpResolver::from_config(&config(&["10.0.0.0/8"], false));
        let headers = headers(&[("x-forwarded-for", "192.168.1.1")]);
        assert_eq!(
            resolver.resolve(peer("127.0.0.1:4242"), &headers),
            "127.0.0.1".parse::<IpAddr>().unwrap()
        );
    }

    #[test]
    fn uses_the_rightmost_value_from_a_trusted_peer() {
        let resolver = ClientIpResolver::from_config(&config(&["127.0.0.1"], false));
        let headers = headers(&[("x-forwarded-for", "203.0.113.1, 10.0.0.2")]);
        assert_eq!(
            resolver.resolve(peer("127.0.0.1:4242"), &headers),
            "10.0.0.2".parse::<IpAddr>().unwrap()
        );
    }

    #[test]
    fn recursive_walk_skips_trusted_hops() {
        let resolver = ClientIpResolver::from_config(&config(&["127.0.0.1", "10.0.0.0/8"], true));
        let headers = headers(&[("x-forwarded-for", "192.168.1.1, 10.0.0.5, 10.0.0.9")]);
        assert_eq!(
            resolver.resolve(peer("127.0.0.1:4242"), &headers),
            "192.168.1.1".parse::<IpAddr>().unwrap()
        );
    }

    #[test]
    fn recursive_walk_falls_back_to_the_leftmost_when_every_hop_is_trusted() {
        let resolver = ClientIpResolver::from_config(&config(&["127.0.0.1", "10.0.0.0/8"], true));
        let headers = headers(&[("x-forwarded-for", "10.0.0.5, 10.0.0.9")]);
        assert_eq!(
            resolver.resolve(peer("127.0.0.1:4242"), &headers),
            "10.0.0.5".parse::<IpAddr>().unwrap()
        );
    }

    #[test]
    fn reads_every_header_line_and_skips_unparseable_entries() {
        let resolver = ClientIpResolver::from_config(&config(&["127.0.0.1"], false));
        let headers = headers(&[
            ("x-forwarded-for", "203.0.113.1, unknown"),
            ("x-forwarded-for", " 198.51.100.7 ,"),
        ]);
        assert_eq!(
            resolver.resolve(peer("127.0.0.1:4242"), &headers),
            "198.51.100.7".parse::<IpAddr>().unwrap()
        );
    }

    #[test]
    fn accepts_ports_brackets_and_ipv6() {
        let resolver = ClientIpResolver::from_config(&config(&["127.0.0.1"], false));

        let with_port = headers(&[("x-forwarded-for", "203.0.113.9:8080")]);
        assert_eq!(
            resolver.resolve(peer("127.0.0.1:4242"), &with_port),
            "203.0.113.9".parse::<IpAddr>().unwrap()
        );

        let bracketed = headers(&[("x-forwarded-for", "[2001:db8::1]:443")]);
        assert_eq!(
            resolver.resolve(peer("127.0.0.1:4242"), &bracketed),
            "2001:db8::1".parse::<IpAddr>().unwrap()
        );

        let bare = headers(&[("x-forwarded-for", "2001:db8::2")]);
        assert_eq!(
            resolver.resolve(peer("127.0.0.1:4242"), &bare),
            "2001:db8::2".parse::<IpAddr>().unwrap()
        );
    }

    #[test]
    fn normalizes_ipv4_mapped_peers_and_values() {
        let resolver = ClientIpResolver::from_config(&config(&["127.0.0.1/32"], false));
        let headers = headers(&[("x-forwarded-for", "10.1.2.3")]);
        assert_eq!(
            resolver.resolve(peer("[::ffff:127.0.0.1]:4242"), &headers),
            "10.1.2.3".parse::<IpAddr>().unwrap()
        );
    }

    #[test]
    fn honors_a_custom_header() {
        let mut config = config(&["127.0.0.1"], false);
        config.real_ip_header = "x-real-ip".into();
        let resolver = ClientIpResolver::from_config(&config);

        let custom = headers(&[("x-real-ip", "198.51.100.4")]);
        assert_eq!(
            resolver.resolve(peer("127.0.0.1:4242"), &custom),
            "198.51.100.4".parse::<IpAddr>().unwrap()
        );

        let ignored = headers(&[("x-forwarded-for", "198.51.100.5")]);
        assert_eq!(
            resolver.resolve(peer("127.0.0.1:4242"), &ignored),
            "127.0.0.1".parse::<IpAddr>().unwrap()
        );
    }

    #[test]
    fn falls_back_to_the_peer_for_an_empty_header() {
        let resolver = ClientIpResolver::from_config(&config(&["127.0.0.1"], false));
        let headers = headers(&[("x-forwarded-for", " , ")]);
        assert_eq!(
            resolver.resolve(peer("127.0.0.1:4242"), &headers),
            "127.0.0.1".parse::<IpAddr>().unwrap()
        );
    }
}
