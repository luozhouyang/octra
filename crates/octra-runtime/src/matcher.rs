//! Compiled route matcher.
//!
//! Routes are ordered by `regex_priority` (descending), longest path
//! (descending), then id (ascending). A request matches a route when:
//!
//! - one of the route protocols matches the request scheme (an empty list
//!   matches every scheme),
//! - the host is one of `hosts` (an empty list matches every host),
//! - the path matches one of `paths` (an empty list matches every path),
//! - one of `methods` matches (an empty list allows every method),
//! - every declared header has one of its expected values.

use std::collections::HashMap;
use std::time::Duration;

use http::HeaderMap;
use octra_api_core::{Route, Service};
use octra_core::Error;
use regex::Regex;

use crate::normalize_host;

/// Limits applied per request by the data plane.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ServiceLimits {
    pub connect_timeout: Duration,
    pub request_timeout: Duration,
    pub retries: u32,
}

impl ServiceLimits {
    pub fn from_service(service: &Service) -> Result<Self, Error> {
        if service.connect_timeout <= 0 || service.read_timeout <= 0 {
            return Err(Error::Runtime(format!(
                "service '{}' has non-positive timeouts",
                service.id
            )));
        }
        Ok(Self {
            connect_timeout: Duration::from_millis(service.connect_timeout as u64),
            request_timeout: Duration::from_millis(service.read_timeout as u64),
            retries: u32::try_from(service.retries.max(0)).unwrap_or(u32::MAX),
        })
    }
}

#[derive(Debug, Clone)]
pub struct CompiledService {
    pub service: Service,
    pub limits: ServiceLimits,
}

#[derive(Debug, Clone)]
enum CompiledPath {
    Prefix(String),
    Regex(String, Regex),
}

#[derive(Debug, Clone)]
pub struct CompiledRoute {
    pub route: Route,
    pub service_index: Option<usize>,
    hosts: Vec<String>,
    paths: Vec<CompiledPath>,
    methods: Vec<String>,
    headers: Vec<(String, Vec<String>)>,
    protocols: Vec<String>,
}

/// A matched route together with the service it dispatches to and the plain
/// path prefix that was matched (empty for regex paths).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RouteMatch {
    pub route_index: usize,
    pub service_index: Option<usize>,
    pub matched_path: String,
    /// Named groups captured by the matched regex path, in pattern order.
    /// Empty for prefix paths and for regex paths without named groups.
    pub uri_captures: Vec<(String, String)>,
}

/// Compiled route matcher. Build it with [`RouteMatcher::compile`].
#[derive(Debug, Default)]
pub struct RouteMatcher {
    services: Vec<CompiledService>,
    routes: Vec<CompiledRoute>,
    by_host: HashMap<String, Vec<usize>>,
}

/// Request attributes used for matching.
pub struct MatchInput<'a> {
    pub scheme: &'a str,
    pub host: Option<&'a str>,
    pub path: &'a str,
    pub method: &'a str,
    pub headers: &'a HeaderMap,
}

impl RouteMatcher {
    pub fn compile(services: &[Service], routes: &[Route]) -> Self {
        let compiled_services: Vec<CompiledService> = services
            .iter()
            .filter_map(|service| match ServiceLimits::from_service(service) {
                Ok(limits) => Some(CompiledService {
                    service: service.clone(),
                    limits,
                }),
                Err(err) => {
                    tracing::warn!(service = %service.id, %err, "skipping invalid service");
                    None
                }
            })
            .collect();

        let service_index: HashMap<&str, usize> = compiled_services
            .iter()
            .enumerate()
            .map(|(index, compiled)| (compiled.service.id.as_str(), index))
            .collect();

        let mut compiled_routes: Vec<CompiledRoute> = Vec::new();
        for route in routes {
            let index = match &route.service_id {
                Some(service_id) => match service_index.get(service_id.as_str()) {
                    Some(index) => Some(*index),
                    None => continue,
                },
                None => None,
            };
            match compile_route(route, index) {
                Some(compiled) => compiled_routes.push(compiled),
                None => tracing::warn!(route = %route.id, "skipping invalid route"),
            }
        }

        compiled_routes.sort_by(|a, b| {
            b.route
                .regex_priority
                .cmp(&a.route.regex_priority)
                .then_with(|| longest_path(b).cmp(&longest_path(a)))
                .then_with(|| a.route.id.cmp(&b.route.id))
        });

        let mut by_host: HashMap<String, Vec<usize>> = HashMap::new();
        let host_agnostic: Vec<usize> = compiled_routes
            .iter()
            .enumerate()
            .filter(|(_, route)| route.hosts.is_empty())
            .map(|(index, _)| index)
            .collect();
        for (index, route) in compiled_routes.iter().enumerate() {
            for host in &route.hosts {
                by_host.entry(host.clone()).or_default().push(index);
            }
        }
        by_host.insert(String::new(), host_agnostic);

        for host in by_host
            .keys()
            .filter(|host| !host.is_empty())
            .cloned()
            .collect::<Vec<_>>()
        {
            let mut merged = by_host.get(&host).cloned().unwrap_or_default();
            if let Some(agnostic) = by_host.get("") {
                merged.extend_from_slice(agnostic);
            }
            merged.sort_unstable();
            merged.dedup();
            by_host.insert(host, merged);
        }

        Self {
            services: compiled_services,
            routes: compiled_routes,
            by_host,
        }
    }

    pub fn services(&self) -> &[CompiledService] {
        &self.services
    }

    pub fn routes(&self) -> &[CompiledRoute] {
        &self.routes
    }

    pub fn service(&self, index: usize) -> Option<&CompiledService> {
        self.services.get(index)
    }

    pub fn route(&self, index: usize) -> Option<&CompiledRoute> {
        self.routes.get(index)
    }

    /// Finds the best enabled route for a request.
    pub fn match_route(&self, input: &MatchInput<'_>) -> Option<RouteMatch> {
        let host = input.host.map(normalize_host).unwrap_or_default();
        let candidates = self
            .by_host
            .get(host.as_str())
            .or_else(|| self.by_host.get(""))?;

        candidates
            .iter()
            .copied()
            .filter_map(|index| self.routes.get(index).map(|route| (index, route)))
            .find_map(|(index, route)| {
                (route.route.enabled)
                    .then(|| route.matches(input))
                    .flatten()
                    .map(|(matched_path, uri_captures)| RouteMatch {
                        route_index: index,
                        service_index: route.service_index,
                        matched_path,
                        uri_captures,
                    })
            })
    }

    /// Matches and returns the borrowed route and service.
    pub fn resolve<'a>(
        &'a self,
        input: &MatchInput<'_>,
    ) -> Option<(&'a CompiledRoute, Option<&'a CompiledService>)> {
        let matched = self.match_route(input)?;
        let route = self.routes.get(matched.route_index)?;
        let service = matched
            .service_index
            .and_then(|index| self.services.get(index));
        Some((route, service))
    }
}

fn longest_path(route: &CompiledRoute) -> usize {
    route
        .paths
        .iter()
        .map(|path| match path {
            CompiledPath::Prefix(prefix) => prefix.len(),
            CompiledPath::Regex(raw, _) => raw.len(),
        })
        .max()
        .unwrap_or(0)
}

fn compile_route(route: &Route, service_index: Option<usize>) -> Option<CompiledRoute> {
    let mut methods = Vec::with_capacity(route.methods.len());
    for method in &route.methods {
        methods.push(octra_api_core::parse_method(method)?);
    }

    let hosts: Vec<String> = route
        .hosts
        .iter()
        .map(|host| normalize_host(host))
        .filter(|host| !host.is_empty())
        .collect();

    let mut paths = Vec::with_capacity(route.paths.len());
    for path in &route.paths {
        let path = path.trim();
        if let Some(pattern) = path.strip_prefix('~') {
            let regex = Regex::new(&format!("^(?:{pattern})"))
                .map_err(|err| tracing::warn!(route = %route.id, %err, "invalid route regex"))
                .ok()?;
            paths.push(CompiledPath::Regex(path.to_string(), regex));
        } else if path.starts_with('/') {
            paths.push(CompiledPath::Prefix(path.to_string()));
        } else {
            return None;
        }
    }

    let headers = route
        .headers
        .iter()
        .map(|(name, values)| (name.to_ascii_lowercase(), values.clone()))
        .collect();

    let protocols = route
        .protocols
        .iter()
        .map(|protocol| protocol.to_ascii_lowercase())
        .collect();

    Some(CompiledRoute {
        route: route.clone(),
        service_index,
        hosts,
        paths,
        methods,
        headers,
        protocols,
    })
}

impl CompiledRoute {
    fn matches(&self, input: &MatchInput<'_>) -> Option<(String, Vec<(String, String)>)> {
        if !self.protocols.is_empty()
            && !self
                .protocols
                .iter()
                .any(|protocol| protocol.eq_ignore_ascii_case(input.scheme))
        {
            return None;
        }

        if !self.hosts.is_empty() {
            let host = input.host.map(normalize_host).unwrap_or_default();
            if !self.hosts.iter().any(|expected| expected == &host) {
                return None;
            }
        }

        let (matched_path, uri_captures) = if self.paths.is_empty() {
            (String::new(), Vec::new())
        } else {
            self.paths.iter().find_map(|path| match path {
                CompiledPath::Prefix(prefix) if path_matches_prefix(input.path, prefix) => {
                    Some((prefix.clone(), Vec::new()))
                }
                CompiledPath::Regex(_, regex) => regex.captures(input.path).map(|captures| {
                    let values = regex
                        .capture_names()
                        .flatten()
                        .filter_map(|name| {
                            captures
                                .name(name)
                                .map(|value| (name.to_string(), value.as_str().to_string()))
                        })
                        .collect();
                    (String::new(), values)
                }),
                _ => None,
            })?
        };

        if !self.methods.is_empty()
            && !self
                .methods
                .iter()
                .any(|method| method.eq_ignore_ascii_case(input.method))
        {
            return None;
        }

        for (name, expected) in &self.headers {
            let value = input
                .headers
                .get(name.as_str())
                .and_then(|value| value.to_str().ok())?;
            if !expected.iter().any(|expected| expected == value) {
                return None;
            }
        }

        Some((matched_path, uri_captures))
    }
}

/// True when `path` equals `prefix` or continues it at a segment boundary.
pub fn path_matches_prefix(path: &str, prefix: &str) -> bool {
    if prefix == "/" {
        return true;
    }
    let prefix = prefix.trim_end_matches('/');
    match path.strip_prefix(prefix) {
        Some(rest) => rest.is_empty() || rest.starts_with('/'),
        None => false,
    }
}

/// Removes the matched prefix, preserving the remainder; empty becomes `/`.
pub fn strip_matched_prefix(path: &str, prefix: &str) -> String {
    if prefix.is_empty() {
        return path.to_string();
    }
    if prefix == "/" {
        return if path.is_empty() {
            "/".to_string()
        } else {
            path.to_string()
        };
    }
    let prefix = prefix.trim_end_matches('/');
    let rest = path.strip_prefix(prefix).unwrap_or(path);
    if rest.is_empty() {
        "/".to_string()
    } else if rest.starts_with('/') {
        rest.to_string()
    } else {
        format!("/{rest}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    fn service(id: &str) -> Service {
        Service {
            id: id.into(),
            name: id.into(),
            protocol: "http".into(),
            host: "127.0.0.1".into(),
            port: 9000,
            path: None,
            retries: 0,
            connect_timeout: 1000,
            write_timeout: 5000,
            read_timeout: 5000,
            enabled: true,
            description: String::new(),
            tags: Vec::new(),
            created_at: 0,
            updated_at: 0,
        }
    }

    fn route(id: &str, service_id: Option<&str>) -> Route {
        Route {
            id: id.into(),
            name: None,
            protocols: vec!["http".into()],
            methods: Vec::new(),
            hosts: Vec::new(),
            paths: Vec::new(),
            headers: BTreeMap::new(),
            strip_path: false,
            preserve_host: false,
            request_buffering: true,
            response_buffering: true,
            regex_priority: 0,
            service_id: service_id.map(str::to_string),
            enabled: true,
            tags: Vec::new(),
            created_at: 0,
            updated_at: 0,
        }
    }

    fn matched_id(
        matcher: &RouteMatcher,
        host: Option<&str>,
        path: &str,
        method: &str,
    ) -> Option<String> {
        let headers = HeaderMap::new();
        matcher
            .match_route(&MatchInput {
                scheme: "http",
                host,
                path,
                method,
                headers: &headers,
            })
            .map(|matched| matcher.routes[matched.route_index].route.id.clone())
    }

    #[test]
    fn host_is_exact_case_insensitive_and_port_is_ignored() {
        let services = vec![service("s1")];
        let mut r = route("r1", Some("s1"));
        r.hosts = vec!["API.Example.com".into()];
        let matcher = RouteMatcher::compile(&services, &[r]);

        assert_eq!(
            matched_id(&matcher, Some("api.example.com:8080"), "/x", "GET").as_deref(),
            Some("r1")
        );
        assert_eq!(
            matched_id(&matcher, Some("other.example.com"), "/x", "GET"),
            None
        );
    }

    #[test]
    fn path_prefix_respects_segment_boundaries() {
        let services = vec![service("s1")];
        let mut r = route("api", Some("s1"));
        r.paths = vec!["/api".into()];
        let matcher = RouteMatcher::compile(&services, &[r]);

        assert_eq!(
            matched_id(&matcher, None, "/api", "GET").as_deref(),
            Some("api")
        );
        assert_eq!(
            matched_id(&matcher, None, "/api/v1", "GET").as_deref(),
            Some("api")
        );
        assert_eq!(matched_id(&matcher, None, "/apix", "GET"), None);
    }

    #[test]
    fn regex_path_matches() {
        let services = vec![service("s1")];
        let mut r = route("re", Some("s1"));
        r.paths = vec!["~/v[0-9]+/".into()];
        let matcher = RouteMatcher::compile(&services, &[r]);
        assert_eq!(
            matched_id(&matcher, None, "/v1/users", "GET").as_deref(),
            Some("re")
        );
        assert_eq!(matched_id(&matcher, None, "/users", "GET"), None);
    }

    #[test]
    fn regex_path_records_named_captures() {
        let services = vec![service("s1")];
        let mut r = route("re", Some("s1"));
        r.paths = vec!["~/tenants/(?<tenant>[^/]+)/items".into()];
        let matcher = RouteMatcher::compile(&services, &[r]);
        let headers = HeaderMap::new();
        let matched = matcher
            .match_route(&MatchInput {
                scheme: "http",
                host: None,
                path: "/tenants/acme/items",
                method: "GET",
                headers: &headers,
            })
            .expect("regex route matches");
        assert_eq!(
            matched.uri_captures,
            vec![("tenant".to_string(), "acme".to_string())]
        );
    }

    #[test]
    fn prefix_path_has_no_uri_captures() {
        let services = vec![service("s1")];
        let mut r = route("prefix", Some("s1"));
        r.paths = vec!["/api".into()];
        let matcher = RouteMatcher::compile(&services, &[r]);
        let headers = HeaderMap::new();
        let matched = matcher
            .match_route(&MatchInput {
                scheme: "http",
                host: None,
                path: "/api/users",
                method: "GET",
                headers: &headers,
            })
            .expect("prefix route matches");
        assert!(matched.uri_captures.is_empty());
    }

    #[test]
    fn longest_path_wins_and_priority_overrides() {
        let services = vec![service("s1"), service("s2")];
        let mut short = route("short", Some("s1"));
        short.paths = vec!["/".into()];
        let mut long = route("long", Some("s2"));
        long.paths = vec!["/api/v1".into()];
        let matcher = RouteMatcher::compile(&services, &[short.clone(), long.clone()]);
        assert_eq!(
            matched_id(&matcher, None, "/api/v1/users", "GET").as_deref(),
            Some("long")
        );

        short.regex_priority = 10;
        let matcher = RouteMatcher::compile(&services, &[short, long]);
        assert_eq!(
            matched_id(&matcher, None, "/api/v1/users", "GET").as_deref(),
            Some("short")
        );
    }

    #[test]
    fn methods_headers_and_protocols_filter_routes() {
        let services = vec![service("s1")];
        let mut with_methods = route("get-only", Some("s1"));
        with_methods.methods = vec!["get".into()];

        let mut with_headers = route("hdr", Some("s1"));
        with_headers.headers = BTreeMap::from([("x-env".into(), vec!["prod".into()])]);
        with_headers.regex_priority = 1;

        let matcher = RouteMatcher::compile(&services, &[with_methods, with_headers]);

        let mut headers = HeaderMap::new();
        headers.insert("x-env", "prod".parse().unwrap());
        let hit = matcher.match_route(&MatchInput {
            scheme: "http",
            host: None,
            path: "/",
            method: "GET",
            headers: &headers,
        });
        assert_eq!(
            hit.map(|matched| matcher.routes[matched.route_index].route.id.as_str()),
            Some("hdr")
        );

        let empty = HeaderMap::new();
        let hit = matcher.match_route(&MatchInput {
            scheme: "http",
            host: None,
            path: "/",
            method: "GET",
            headers: &empty,
        });
        assert_eq!(
            hit.map(|matched| matcher.routes[matched.route_index].route.id.as_str()),
            Some("get-only")
        );

        let hit = matcher.match_route(&MatchInput {
            scheme: "http",
            host: None,
            path: "/",
            method: "POST",
            headers: &empty,
        });
        assert!(hit.is_none());
    }

    #[test]
    fn disabled_routes_are_skipped() {
        let services = vec![service("s1")];
        let mut disabled = route("off", Some("s1"));
        disabled.enabled = false;
        let matcher = RouteMatcher::compile(&services, &[disabled]);
        assert_eq!(matched_id(&matcher, None, "/", "GET"), None);
    }

    #[test]
    fn strip_prefix_preserves_remainder() {
        assert_eq!(strip_matched_prefix("/api/v1/users", "/api"), "/v1/users");
        assert_eq!(strip_matched_prefix("/api", "/api"), "/");
        assert_eq!(strip_matched_prefix("/api/", "/api"), "/");
        assert_eq!(strip_matched_prefix("/x", "/"), "/x");
        assert_eq!(strip_matched_prefix("/x", ""), "/x");
    }
}
