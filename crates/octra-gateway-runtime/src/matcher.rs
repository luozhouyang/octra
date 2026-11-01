//! Compiled route matcher.
//!
//! Routes are ordered by priority (descending), matched prefix length
//! (descending), then id (ascending). Requests match a route when the host is
//! an exact (case-insensitive) match or the route has no host, the path falls
//! under the route prefix at a segment boundary, the method is allowed (an
//! empty method list allows all methods), and every declared header equals the
//! request header value.

use std::collections::HashMap;
use std::time::Duration;

use http::HeaderMap;
use octra_api_core::{Route, Service};
use octra_core::Error;

use crate::normalize_host;

/// Limits applied per request by the data plane.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ServiceLimits {
    pub connect_timeout: Duration,
    pub request_timeout: Duration,
    pub retries: u32,
    pub require_auth: bool,
}

impl ServiceLimits {
    pub fn from_service(service: &Service) -> Result<Self, Error> {
        if service.connect_timeout_ms <= 0 || service.request_timeout_ms <= 0 {
            return Err(Error::Runtime(format!(
                "service '{}' has non-positive timeouts",
                service.id
            )));
        }
        Ok(Self {
            connect_timeout: Duration::from_millis(service.connect_timeout_ms as u64),
            request_timeout: Duration::from_millis(service.request_timeout_ms as u64),
            retries: u32::try_from(service.retries.max(0)).unwrap_or(u32::MAX),
            require_auth: service.require_auth,
        })
    }
}

#[derive(Debug, Clone)]
pub struct CompiledService {
    pub service: Service,
    pub limits: ServiceLimits,
}

#[derive(Debug, Clone)]
pub struct CompiledRoute {
    pub route: Route,
    pub service_index: usize,
    normalized_host: Option<String>,
    methods: Vec<String>,
    headers: Vec<(String, String)>,
    path_prefix: String,
}

/// A matched route together with the service it dispatches to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RouteMatch {
    pub route_index: usize,
    pub service_index: usize,
}

/// Compiled route matcher. Build it with [`RouteMatcher::compile`].
#[derive(Debug, Default)]
pub struct RouteMatcher {
    services: Vec<CompiledService>,
    routes: Vec<CompiledRoute>,
    /// Candidate route indices in priority order for a normalized host,
    /// including host-agnostic routes for a specific host. The empty key holds
    /// host-agnostic routes.
    by_host: HashMap<String, Vec<usize>>,
}

/// Request attributes used for matching.
pub struct MatchInput<'a> {
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
            let Some(&index) = service_index.get(route.service_id.as_str()) else {
                continue;
            };
            match compile_route(route, index) {
                Some(compiled) => compiled_routes.push(compiled),
                None => tracing::warn!(route = %route.id, "skipping invalid route"),
            }
        }

        // Priority desc -> prefix length desc -> id.
        compiled_routes.sort_by(|a, b| {
            b.route
                .priority
                .cmp(&a.route.priority)
                .then_with(|| b.path_prefix.len().cmp(&a.path_prefix.len()))
                .then_with(|| a.route.id.cmp(&b.route.id))
        });

        let mut by_host: HashMap<String, Vec<usize>> = HashMap::new();
        let host_agnostic: Vec<usize> = compiled_routes
            .iter()
            .enumerate()
            .filter(|(_, route)| route.normalized_host.is_none())
            .map(|(index, _)| index)
            .collect();
        for (index, route) in compiled_routes.iter().enumerate() {
            if let Some(host) = &route.normalized_host {
                by_host.entry(host.clone()).or_default().push(index);
            }
        }
        by_host.insert(String::new(), host_agnostic.clone());

        for host in by_host
            .keys()
            .filter(|host| !host.is_empty())
            .cloned()
            .collect::<Vec<_>>()
        {
            let mut merged = by_host.get(&host).cloned().unwrap_or_default();
            merged.extend_from_slice(&host_agnostic);
            merged.sort_unstable();
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
            .find(|(_, route)| route.route.enabled && route.matches(input))
            .map(|(index, route)| RouteMatch {
                route_index: index,
                service_index: route.service_index,
            })
    }

    /// Matches and returns the borrowed route and service.
    pub fn resolve<'a>(
        &'a self,
        input: &MatchInput<'_>,
    ) -> Option<(&'a CompiledRoute, &'a CompiledService)> {
        let matched = self.match_route(input)?;
        let route = self.routes.get(matched.route_index)?;
        let service = self.services.get(matched.service_index)?;
        Some((route, service))
    }
}

fn compile_route(route: &Route, service_index: usize) -> Option<CompiledRoute> {
    if !route.path_prefix.starts_with('/') {
        return None;
    }

    let mut methods = Vec::with_capacity(route.methods.len());
    for method in &route.methods {
        let parsed = octra_api_core::parse_method(method)?;
        methods.push(parsed);
    }

    let headers = route
        .headers
        .iter()
        .map(|(name, value)| (name.to_ascii_lowercase(), value.clone()))
        .collect();

    Some(CompiledRoute {
        route: route.clone(),
        service_index,
        normalized_host: route
            .host
            .as_deref()
            .map(normalize_host)
            .filter(|host| !host.is_empty()),
        methods,
        headers,
        path_prefix: route.path_prefix.clone(),
    })
}

impl CompiledRoute {
    fn matches(&self, input: &MatchInput<'_>) -> bool {
        if let Some(expected_host) = &self.normalized_host {
            let Some(host) = input.host else {
                return false;
            };
            if &normalize_host(host) != expected_host {
                return false;
            }
        }

        if !path_matches_prefix(input.path, &self.path_prefix) {
            return false;
        }

        if !self.methods.is_empty()
            && !self
                .methods
                .iter()
                .any(|method| method.eq_ignore_ascii_case(input.method))
        {
            return false;
        }

        self.headers.iter().all(|(name, expected)| {
            input
                .headers
                .get(name.as_str())
                .and_then(|value| value.to_str().ok())
                == Some(expected.as_str())
        })
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
    use chrono::Utc;
    use http::{HeaderMap, HeaderValue};
    use std::collections::BTreeMap;

    fn service(id: &str) -> Service {
        let now = Utc::now();
        Service {
            id: id.into(),
            name: id.into(),
            protocol: "http".into(),
            description: String::new(),
            enabled: true,
            connect_timeout_ms: 1000,
            request_timeout_ms: 5000,
            retries: 0,
            require_auth: true,
            created_at: now,
            updated_at: now,
        }
    }

    fn route(id: &str, service_id: &str, host: Option<&str>, prefix: &str) -> Route {
        let now = Utc::now();
        Route {
            id: id.into(),
            service_id: service_id.into(),
            host: host.map(str::to_string),
            path_prefix: prefix.into(),
            methods: Vec::new(),
            headers: BTreeMap::new(),
            protocol: "http".into(),
            priority: 0,
            strip_prefix: false,
            enabled: true,
            created_at: now,
            updated_at: now,
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
            .resolve(&MatchInput {
                host,
                path,
                method,
                headers: &headers,
            })
            .map(|(route, _)| route.route.id.clone())
    }

    #[test]
    fn host_is_exact_case_insensitive_and_port_is_ignored() {
        let services = vec![service("s1")];
        let routes = vec![route("r1", "s1", Some("API.Example.com"), "/")];
        let matcher = RouteMatcher::compile(&services, &routes);

        assert_eq!(
            matched_id(&matcher, Some("api.example.com:8080"), "/x", "GET").as_deref(),
            Some("r1")
        );
        assert_eq!(
            matched_id(&matcher, Some("other.example.com"), "/x", "GET"),
            None
        );
        assert_eq!(matched_id(&matcher, None, "/x", "GET"), None);
    }

    #[test]
    fn path_prefix_respects_segment_boundaries() {
        let services = vec![service("s1")];
        let routes = vec![route("api", "s1", None, "/api")];
        let matcher = RouteMatcher::compile(&services, &routes);

        assert_eq!(
            matched_id(&matcher, None, "/api", "GET").as_deref(),
            Some("api")
        );
        assert_eq!(
            matched_id(&matcher, None, "/api/v1", "GET").as_deref(),
            Some("api")
        );
        assert_eq!(matched_id(&matcher, None, "/apix", "GET"), None);
        assert_eq!(matched_id(&matcher, None, "/", "GET"), None);
    }

    #[test]
    fn longest_prefix_wins_and_priority_overrides() {
        let services = vec![service("s1"), service("s2")];
        let mut routes = vec![
            route("short", "s1", None, "/"),
            route("long", "s2", None, "/api/v1"),
        ];
        routes[1].priority = 0;
        let matcher = RouteMatcher::compile(&services, &routes);
        assert_eq!(
            matched_id(&matcher, None, "/api/v1/users", "GET").as_deref(),
            Some("long")
        );

        // A high-priority route shadows a longer prefix.
        routes[0].priority = 10;
        let matcher = RouteMatcher::compile(&services, &routes);
        assert_eq!(
            matched_id(&matcher, None, "/api/v1/users", "GET").as_deref(),
            Some("short")
        );
    }

    #[test]
    fn methods_and_headers_filter_routes() {
        let services = vec![service("s1")];
        let mut with_methods = route("get-only", "s1", None, "/");
        with_methods.methods = vec!["get".into()];

        let mut with_headers = route("hdr", "s1", None, "/");
        with_headers.headers = BTreeMap::from([("x-env".into(), "prod".into())]);
        with_headers.priority = 1;

        let matcher = RouteMatcher::compile(&services, &[with_methods, with_headers]);

        let mut headers = HeaderMap::new();
        headers.insert("x-env", HeaderValue::from_static("prod"));
        let hit = matcher.resolve(&MatchInput {
            host: None,
            path: "/",
            method: "GET",
            headers: &headers,
        });
        assert_eq!(hit.map(|(route, _)| route.route.id.as_str()), Some("hdr"));

        let empty = HeaderMap::new();
        let hit = matcher.resolve(&MatchInput {
            host: None,
            path: "/",
            method: "GET",
            headers: &empty,
        });
        assert_eq!(
            hit.map(|(route, _)| route.route.id.as_str()),
            Some("get-only")
        );

        let hit = matcher.resolve(&MatchInput {
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
        let mut disabled = route("off", "s1", None, "/");
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
    }
}
