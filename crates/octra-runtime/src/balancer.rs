//! Load balancing over upstream targets.
//!
//! Supports round-robin, weighted round-robin, and Kong-style consistent
//! hashing (with `hash_on` / `hash_fallback`), plus passive and active health
//! checking driven by the upstream `healthchecks` configuration.

use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use http::HeaderMap;
use octra_api_core::{LoadBalanceAlgorithm, Target, Upstream};
use serde_json::Value;

#[derive(Debug, Clone)]
pub struct Endpoint {
    pub id: String,
    pub url: String,
    pub weight: u32,
    pub eligible: bool,
    slot_weight: u32,
    health: Option<Arc<TargetHealth>>,
    passive: PassiveConfig,
}

impl Endpoint {
    pub fn direct(id: impl Into<String>, url: impl Into<String>) -> Arc<Self> {
        Arc::new(Self {
            id: id.into(),
            url: url.into(),
            weight: 1,
            eligible: true,
            slot_weight: 1,
            health: None,
            passive: PassiveConfig::default(),
        })
    }

    pub fn is_healthy(&self) -> bool {
        self.health
            .as_ref()
            .is_none_or(|health| !health.is_unhealthy())
    }

    pub fn report_status(&self, status: u16) {
        if let Some(health) = &self.health {
            health.report_status(&self.passive, status);
        }
    }

    pub fn report_timeout(&self) {
        if let Some(health) = &self.health {
            health.report_timeout(&self.passive);
        }
    }

    pub fn report_tcp_failure(&self) {
        if let Some(health) = &self.health {
            health.report_tcp_failure(&self.passive);
        }
    }
}

/// Shared health state for one upstream target, keyed by target id so it
/// survives runtime reloads.
#[derive(Debug, Default)]
pub struct TargetHealth {
    unhealthy: AtomicBool,
    successes: AtomicU32,
    http_failures: AtomicU32,
    tcp_failures: AtomicU32,
    timeouts: AtomicU32,
    next_check_ms: AtomicU64,
}

impl TargetHealth {
    pub fn is_unhealthy(&self) -> bool {
        self.unhealthy.load(Ordering::Relaxed)
    }

    fn clear(&self) {
        self.unhealthy.store(false, Ordering::Relaxed);
        self.reset_counters();
    }

    fn reset_counters(&self) {
        self.successes.store(0, Ordering::Relaxed);
        self.http_failures.store(0, Ordering::Relaxed);
        self.tcp_failures.store(0, Ordering::Relaxed);
        self.timeouts.store(0, Ordering::Relaxed);
    }

    fn record_success(&self, threshold: u32) {
        let successes = self.successes.fetch_add(1, Ordering::Relaxed) + 1;
        if threshold == 0 || successes >= threshold {
            self.clear();
        }
    }

    fn record_failure(&self, counter: &AtomicU32, threshold: u32) {
        self.successes.store(0, Ordering::Relaxed);
        let failures = counter.fetch_add(1, Ordering::Relaxed) + 1;
        if threshold > 0 && failures >= threshold {
            self.unhealthy.store(true, Ordering::Relaxed);
        }
    }

    fn report_status(&self, config: &PassiveConfig, status: u16) {
        if config.healthy_statuses.contains(&status) {
            self.record_success(config.healthy_successes);
        } else if config.http_statuses.contains(&status) {
            self.record_failure(&self.http_failures, config.http_failures);
        }
    }

    fn report_timeout(&self, config: &PassiveConfig) {
        self.record_failure(&self.timeouts, config.timeouts);
    }

    fn report_tcp_failure(&self, config: &PassiveConfig) {
        self.record_failure(&self.tcp_failures, config.tcp_failures);
    }

    fn report_active(&self, config: &ActiveConfig, outcome: ActiveOutcome) {
        match outcome {
            ActiveOutcome::Healthy => self.record_success(config.healthy_successes),
            ActiveOutcome::UnhealthyStatus => {
                self.record_failure(&self.http_failures, config.http_failures)
            }
            ActiveOutcome::Timeout => self.record_failure(&self.timeouts, config.timeouts),
            ActiveOutcome::TcpFailure => {
                self.record_failure(&self.tcp_failures, config.tcp_failures)
            }
            ActiveOutcome::Ignored => {}
        }
    }

    fn due(&self, now_ms: u64) -> bool {
        self.next_check_ms.load(Ordering::Relaxed) <= now_ms
    }

    fn schedule(&self, now_ms: u64, interval: Duration) {
        let next = if interval.is_zero() {
            u64::MAX
        } else {
            now_ms.saturating_add(interval.as_millis() as u64)
        };
        self.next_check_ms.store(next, Ordering::Relaxed);
    }
}

/// Process-wide target health, shared across runtime reloads.
#[derive(Debug, Clone, Default)]
pub struct HealthStore {
    inner: Arc<Mutex<HashMap<String, Arc<TargetHealth>>>>,
}

impl HealthStore {
    pub fn get(&self, target_id: &str) -> Arc<TargetHealth> {
        let mut guard = self.inner.lock().unwrap_or_else(|error| error.into_inner());
        guard
            .entry(target_id.to_string())
            .or_insert_with(|| Arc::new(TargetHealth::default()))
            .clone()
    }

    pub fn remove(&self, target_id: &str) {
        let mut guard = self.inner.lock().unwrap_or_else(|error| error.into_inner());
        guard.remove(target_id);
    }
}

#[derive(Debug, Clone)]
pub struct PassiveConfig {
    healthy_statuses: Vec<u16>,
    healthy_successes: u32,
    http_failures: u32,
    http_statuses: Vec<u16>,
    tcp_failures: u32,
    timeouts: u32,
}

impl Default for PassiveConfig {
    fn default() -> Self {
        Self {
            healthy_statuses: vec![200, 302],
            healthy_successes: 0,
            http_failures: 0,
            http_statuses: vec![429, 500, 503],
            tcp_failures: 0,
            timeouts: 0,
        }
    }
}

#[derive(Debug, Clone)]
pub struct ActiveConfig {
    enabled: bool,
    http_path: String,
    timeout: Duration,
    healthy_interval: Duration,
    unhealthy_interval: Duration,
    healthy_statuses: Vec<u16>,
    healthy_successes: u32,
    http_failures: u32,
    http_statuses: Vec<u16>,
    tcp_failures: u32,
    timeouts: u32,
}

impl Default for ActiveConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            http_path: "/".into(),
            timeout: Duration::from_secs(1),
            healthy_interval: Duration::ZERO,
            unhealthy_interval: Duration::ZERO,
            healthy_statuses: vec![200, 302],
            healthy_successes: 0,
            http_failures: 0,
            http_statuses: vec![429, 500, 503],
            tcp_failures: 0,
            timeouts: 0,
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct HealthConfig {
    pub passive: PassiveConfig,
    pub active: ActiveConfig,
}

impl HealthConfig {
    pub fn from_value(value: &Value) -> Self {
        let passive = value.get("passive");
        let active = value.get("active");
        let healthy_interval = duration_secs(
            active
                .and_then(|value| value.pointer("/healthy/interval"))
                .unwrap_or(&Value::Null),
        );
        let unhealthy_interval = duration_secs(
            active
                .and_then(|value| value.pointer("/unhealthy/interval"))
                .unwrap_or(&Value::Null),
        );
        Self {
            passive: PassiveConfig {
                healthy_statuses: statuses(
                    passive.and_then(|value| value.pointer("/healthy/http_statuses")),
                    &[200, 302],
                ),
                healthy_successes: unsigned(
                    passive.and_then(|value| value.pointer("/healthy/successes")),
                ),
                http_failures: unsigned(
                    passive.and_then(|value| value.pointer("/unhealthy/http_failures")),
                ),
                http_statuses: statuses(
                    passive.and_then(|value| value.pointer("/unhealthy/http_statuses")),
                    &[429, 500, 503],
                ),
                tcp_failures: unsigned(
                    passive.and_then(|value| value.pointer("/unhealthy/tcp_failures")),
                ),
                timeouts: unsigned(passive.and_then(|value| value.pointer("/unhealthy/timeouts"))),
            },
            active: ActiveConfig {
                enabled: !healthy_interval.is_zero() || !unhealthy_interval.is_zero(),
                http_path: active
                    .and_then(|value| value.get("http_path"))
                    .and_then(Value::as_str)
                    .unwrap_or("/")
                    .to_string(),
                timeout: duration_secs(
                    active
                        .and_then(|value| value.get("timeout"))
                        .unwrap_or(&Value::Null),
                )
                .max(Duration::from_millis(1)),
                healthy_interval,
                unhealthy_interval,
                healthy_statuses: statuses(
                    active.and_then(|value| value.pointer("/healthy/http_statuses")),
                    &[200, 302],
                ),
                healthy_successes: unsigned(
                    active.and_then(|value| value.pointer("/healthy/successes")),
                ),
                http_failures: unsigned(
                    active.and_then(|value| value.pointer("/unhealthy/http_failures")),
                ),
                http_statuses: statuses(
                    active.and_then(|value| value.pointer("/unhealthy/http_statuses")),
                    &[429, 500, 503],
                ),
                tcp_failures: unsigned(
                    active.and_then(|value| value.pointer("/unhealthy/tcp_failures")),
                ),
                timeouts: unsigned(active.and_then(|value| value.pointer("/unhealthy/timeouts"))),
            },
        }
    }

    pub fn active_enabled(&self) -> bool {
        self.active.enabled
    }
}

fn unsigned(value: Option<&Value>) -> u32 {
    value
        .and_then(Value::as_u64)
        .and_then(|value| u32::try_from(value).ok())
        .unwrap_or(0)
}

fn duration_secs(value: &Value) -> Duration {
    match value {
        Value::Number(number) => number
            .as_f64()
            .map(|seconds| Duration::from_secs_f64(seconds.max(0.0)))
            .unwrap_or(Duration::ZERO),
        Value::String(text) => text
            .parse::<f64>()
            .map(|seconds| Duration::from_secs_f64(seconds.max(0.0)))
            .unwrap_or(Duration::ZERO),
        _ => Duration::ZERO,
    }
}

fn statuses(value: Option<&Value>, default: &[u16]) -> Vec<u16> {
    let Some(items) = value.and_then(Value::as_array) else {
        return default.to_vec();
    };
    items
        .iter()
        .filter_map(Value::as_u64)
        .filter_map(|item| u16::try_from(item).ok())
        .collect()
}

/// Hash configuration copied from the upstream entity.
#[derive(Debug, Clone)]
pub struct HashConfig {
    pub hash_on: String,
    pub hash_fallback: String,
    pub hash_on_header: String,
    pub hash_fallback_header: String,
    pub hash_on_cookie: String,
    pub hash_on_cookie_path: String,
    pub hash_on_query_arg: String,
    pub hash_fallback_query_arg: String,
    pub hash_on_uri_capture: String,
    pub hash_fallback_uri_capture: String,
    pub slots: u64,
}

impl Default for HashConfig {
    fn default() -> Self {
        Self {
            hash_on: "none".into(),
            hash_fallback: "none".into(),
            hash_on_header: "X-Consumer-ID".into(),
            hash_fallback_header: "X-Consumer-ID".into(),
            hash_on_cookie: "kong".into(),
            hash_on_cookie_path: "/".into(),
            hash_on_query_arg: String::new(),
            hash_fallback_query_arg: String::new(),
            hash_on_uri_capture: String::new(),
            hash_fallback_uri_capture: String::new(),
            slots: 10_000,
        }
    }
}

impl HashConfig {
    pub fn from_upstream(upstream: &Upstream) -> Self {
        Self {
            hash_on: upstream.hash_on.clone(),
            hash_fallback: upstream.hash_fallback.clone(),
            hash_on_header: upstream.hash_on_header.clone(),
            hash_fallback_header: upstream.hash_fallback_header.clone(),
            hash_on_cookie: upstream.hash_on_cookie.clone(),
            hash_on_cookie_path: upstream.hash_on_cookie_path.clone(),
            hash_on_query_arg: upstream.hash_on_query_arg.clone(),
            hash_fallback_query_arg: upstream.hash_fallback_query_arg.clone(),
            hash_on_uri_capture: upstream.hash_on_uri_capture.clone(),
            hash_fallback_uri_capture: upstream.hash_fallback_uri_capture.clone(),
            slots: u64::try_from(upstream.slots).unwrap_or(10_000).max(1),
        }
    }

    /// Primary hash key, falling back to `hash_fallback` when the primary
    /// source is absent from the request (as Kong does).
    pub fn key(&self, input: &HashInput<'_>) -> Option<String> {
        self.extract(
            &self.hash_on,
            &self.hash_on_header,
            &self.hash_on_query_arg,
            &self.hash_on_uri_capture,
            input,
        )
        .or_else(|| {
            self.extract(
                &self.hash_fallback,
                &self.hash_fallback_header,
                &self.hash_fallback_query_arg,
                &self.hash_fallback_uri_capture,
                input,
            )
        })
    }

    fn extract(
        &self,
        mode: &str,
        header: &str,
        query_arg: &str,
        uri_capture: &str,
        input: &HashInput<'_>,
    ) -> Option<String> {
        let value = match mode {
            "consumer" => input.consumer_id.map(str::to_string),
            "ip" => input.client_ip.map(|ip| ip.to_string()),
            "header" => join_header(input.headers, header),
            "cookie" => cookie_value(input.headers, &self.hash_on_cookie),
            "path" => Some(input.path.to_string()),
            "query_arg" => query_value(input.path, query_arg),
            "uri_capture" if !uri_capture.is_empty() => input
                .uri_captures
                .iter()
                .find(|(name, _)| name == uri_capture)
                .map(|(_, value)| value.clone()),
            _ => None,
        };
        value.filter(|value| !value.is_empty())
    }
}

/// Request-derived inputs used to derive the consistent-hash key.
#[derive(Debug, Clone, Copy)]
pub struct HashInput<'a> {
    pub headers: &'a HeaderMap,
    pub path: &'a str,
    pub client_ip: Option<IpAddr>,
    pub consumer_id: Option<&'a str>,
    /// Named groups captured by the matched route regex path.
    pub uri_captures: &'a [(String, String)],
}

fn join_header(headers: &HeaderMap, name: &str) -> Option<String> {
    let values: Vec<&str> = headers
        .get_all(name)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .collect();
    if values.is_empty() {
        None
    } else {
        Some(values.join(","))
    }
}

fn cookie_value(headers: &HeaderMap, name: &str) -> Option<String> {
    let header = headers.get(http::header::COOKIE)?.to_str().ok()?;
    header.split(';').find_map(|pair| {
        let (key, value) = pair.split_once('=')?;
        (key.trim() == name).then(|| value.trim().to_string())
    })
}

fn query_value(path: &str, name: &str) -> Option<String> {
    if name.is_empty() {
        return None;
    }
    let query = path.split_once('?')?.1;
    url::form_urlencoded::parse(query.as_bytes())
        .find_map(|(key, value)| (key == name).then(|| value.into_owned()))
}

/// FNV-1a, stable across processes and Rust releases.
fn fnv1a(value: &str) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in value.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

#[derive(Debug)]
pub struct TargetPool {
    algorithm: LoadBalanceAlgorithm,
    targets: Vec<Arc<Endpoint>>,
    cursor: AtomicU64,
    hash: HashConfig,
    health: HealthConfig,
}

impl TargetPool {
    pub fn new(
        algorithm: LoadBalanceAlgorithm,
        targets: &[Target],
        scheme: &str,
        seed: u64,
    ) -> Self {
        Self::with_config(
            algorithm,
            targets,
            scheme,
            seed,
            HashConfig::default(),
            HealthConfig::default(),
            &HealthStore::default(),
        )
    }

    pub fn from_upstream(
        upstream: &Upstream,
        targets: &[Target],
        scheme: &str,
        seed: u64,
        health_store: &HealthStore,
    ) -> Self {
        Self::with_config(
            algorithm_from_str(upstream.algorithm.as_str()),
            targets,
            scheme,
            seed,
            HashConfig::from_upstream(upstream),
            HealthConfig::from_value(&upstream.healthchecks),
            health_store,
        )
    }

    fn with_config(
        algorithm: LoadBalanceAlgorithm,
        targets: &[Target],
        scheme: &str,
        seed: u64,
        hash: HashConfig,
        health: HealthConfig,
        health_store: &HealthStore,
    ) -> Self {
        let endpoints: Vec<Arc<Endpoint>> = targets
            .iter()
            .map(|target| {
                let weight = target.weight;
                Arc::new(Endpoint {
                    id: target.id.clone(),
                    url: format!("{scheme}://{}", target.target),
                    weight,
                    eligible: weight > 0,
                    slot_weight: weight.max(1),
                    health: Some(health_store.get(&target.id)),
                    passive: health.passive.clone(),
                })
            })
            .collect();

        Self {
            algorithm,
            targets: endpoints,
            cursor: AtomicU64::new(seed),
            hash,
            health,
        }
    }

    pub fn algorithm(&self) -> LoadBalanceAlgorithm {
        self.algorithm
    }

    pub fn reset_cursor(&self) {
        self.cursor.store(0, Ordering::Relaxed);
    }

    pub fn len(&self) -> usize {
        self.targets.len()
    }

    pub fn is_empty(&self) -> bool {
        self.targets.is_empty()
    }

    pub fn eligible(&self) -> Vec<Arc<Endpoint>> {
        self.eligible_refs().into_iter().map(Arc::clone).collect()
    }

    pub fn has_eligible(&self) -> bool {
        self.targets
            .iter()
            .any(|endpoint| endpoint.eligible && endpoint.is_healthy())
    }

    fn eligible_refs(&self) -> Vec<&Arc<Endpoint>> {
        self.targets
            .iter()
            .filter(|endpoint| endpoint.eligible && endpoint.is_healthy())
            .collect()
    }

    pub fn select(&self) -> Option<Arc<Endpoint>> {
        self.select_with(None)
    }

    pub fn select_with(&self, hash: Option<&HashInput<'_>>) -> Option<Arc<Endpoint>> {
        let ordered = self.ordered_attempts(hash);
        ordered.into_iter().next()
    }

    pub fn ordered_attempts(&self, hash: Option<&HashInput<'_>>) -> Vec<Arc<Endpoint>> {
        let eligible = self.eligible_refs();
        if eligible.is_empty() {
            return Vec::new();
        }
        let len = eligible.len();

        let start = match self.hashing_index(&eligible, hash) {
            Some(index) => index,
            None => {
                let cursor = self.cursor.fetch_add(1, Ordering::Relaxed);
                match self.algorithm {
                    LoadBalanceAlgorithm::WeightedRoundRobin => {
                        weighted_start(&eligible, cursor).unwrap_or(0)
                    }
                    _ => (cursor % len as u64) as usize,
                }
            }
        };

        (0..len)
            .map(|step| Arc::clone(eligible[(start + step) % len]))
            .collect()
    }

    fn hashing_index(
        &self,
        eligible: &[&Arc<Endpoint>],
        input: Option<&HashInput<'_>>,
    ) -> Option<usize> {
        if self.algorithm != LoadBalanceAlgorithm::ConsistentHashing {
            return None;
        }
        let key = self.hash.key(input?)?;
        let total: u64 = eligible
            .iter()
            .map(|endpoint| u64::from(endpoint.slot_weight))
            .sum();
        if total == 0 {
            return None;
        }
        let slots = self.hash.slots.max(1);
        let point = fnv1a(&key) % slots;
        // Mirror Kong's wheel: each target owns slots proportional to weight.
        let mut covered: u128 = 0;
        for (index, endpoint) in eligible.iter().enumerate() {
            covered += u128::from(endpoint.slot_weight) * u128::from(slots);
            let boundary = covered / u128::from(total);
            if u128::from(point) < boundary {
                return Some(index);
            }
        }
        eligible.len().checked_sub(1)
    }

    /// Runs one round of active health checks for every due target.
    pub async fn check_active(&self) {
        let config = &self.health.active;
        if !config.enabled {
            return;
        }
        let now = now_ms();
        for endpoint in &self.targets {
            let Some(health) = &endpoint.health else {
                continue;
            };
            if !health.due(now) {
                continue;
            }
            let outcome = probe(&endpoint.url, config).await;
            health.report_active(config, outcome);
            let unhealthy = health.is_unhealthy();
            let interval = if unhealthy {
                config.unhealthy_interval
            } else {
                config.healthy_interval
            };
            health.schedule(now_ms(), interval);
        }
    }

    pub fn active_health_enabled(&self) -> bool {
        self.health.active_enabled()
    }
}

fn weighted_start(eligible: &[&Arc<Endpoint>], start: u64) -> Option<usize> {
    let total: u64 = eligible
        .iter()
        .map(|endpoint| u64::from(endpoint.slot_weight))
        .sum();
    if total == 0 {
        return None;
    }
    let mut point = start % total;
    for (index, endpoint) in eligible.iter().enumerate() {
        let weight = u64::from(endpoint.slot_weight);
        if point < weight {
            return Some(index);
        }
        point -= weight;
    }
    Some(0)
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis() as u64)
        .unwrap_or(0)
}

enum ActiveOutcome {
    Healthy,
    UnhealthyStatus,
    Timeout,
    TcpFailure,
    Ignored,
}

async fn probe(base_url: &str, config: &ActiveConfig) -> ActiveOutcome {
    if base_url.starts_with("https://") || base_url.starts_with("http://") {
        let path = config.http_path.trim_start_matches('/');
        let url = format!("{base_url}/{path}");
        match active_client()
            .get(&url)
            .timeout(config.timeout)
            .send()
            .await
        {
            Ok(response) => {
                let status = response.status().as_u16();
                if config.healthy_statuses.contains(&status) {
                    ActiveOutcome::Healthy
                } else if config.http_statuses.contains(&status) {
                    ActiveOutcome::UnhealthyStatus
                } else {
                    ActiveOutcome::Ignored
                }
            }
            Err(error) if error.is_timeout() => ActiveOutcome::Timeout,
            Err(_) => ActiveOutcome::TcpFailure,
        }
    } else {
        ActiveOutcome::TcpFailure
    }
}

fn active_client() -> &'static reqwest::Client {
    static CLIENT: OnceLock<reqwest::Client> = OnceLock::new();
    CLIENT.get_or_init(|| {
        reqwest::Client::builder()
            .timeout(Duration::from_secs(5))
            .build()
            .unwrap_or_default()
    })
}

/// Maps a stored algorithm name to the local implementation. Only round-robin,
/// weighted-round-robin, and consistent-hashing are implemented; other
/// strategies fall back to round-robin.
pub fn algorithm_from_str(value: &str) -> LoadBalanceAlgorithm {
    LoadBalanceAlgorithm::parse(value).unwrap_or(LoadBalanceAlgorithm::RoundRobin)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn target(id: &str, weight: u32) -> Target {
        Target {
            id: id.into(),
            upstream_id: "u".into(),
            target: format!("{id}.test:80"),
            weight,
            tags: Vec::new(),
            created_at: 0,
            updated_at: 0,
        }
    }

    fn pool(algorithm: LoadBalanceAlgorithm, targets: &[Target]) -> TargetPool {
        TargetPool::new(algorithm, targets, "http", 0)
    }

    #[test]
    fn round_robin_cycles_in_order() {
        let targets = vec![target("a", 1), target("b", 1)];
        let pool = pool(LoadBalanceAlgorithm::RoundRobin, &targets);
        let picks: Vec<String> = (0..4).map(|_| pool.select().unwrap().id.clone()).collect();
        assert_eq!(picks, vec!["a", "b", "a", "b"]);
    }

    #[test]
    fn weighted_round_robin_respects_weights() {
        let targets = vec![target("a", 1), target("b", 3)];
        let pool = pool(LoadBalanceAlgorithm::WeightedRoundRobin, &targets);
        let mut counts = std::collections::HashMap::new();
        for _ in 0..40 {
            *counts.entry(pool.select().unwrap().id.clone()).or_insert(0) += 1;
        }
        assert_eq!(counts["a"], 10);
        assert_eq!(counts["b"], 30);
    }

    #[test]
    fn zero_weight_is_ineligible() {
        let targets = vec![target("a", 0), target("b", 1)];
        let pool = pool(LoadBalanceAlgorithm::RoundRobin, &targets);
        for _ in 0..10 {
            assert_eq!(pool.select().unwrap().id, "b");
        }
    }

    #[test]
    fn consistent_hashing_is_stable_per_key() {
        let targets = vec![target("a", 1), target("b", 1), target("c", 1)];
        let mut headers = HeaderMap::new();
        headers.insert("x-user", "alice".parse().unwrap());
        let hash = HashConfig {
            hash_on: "header".into(),
            hash_on_header: "x-user".into(),
            ..Default::default()
        };
        let pool = TargetPool::with_config(
            LoadBalanceAlgorithm::ConsistentHashing,
            &targets,
            "http",
            0,
            hash,
            HealthConfig::default(),
            &HealthStore::default(),
        );
        let input = HashInput {
            headers: &headers,
            path: "/",
            client_ip: None,
            consumer_id: None,
            uri_captures: &[],
        };
        let picks: Vec<String> = (0..5)
            .map(|_| pool.select_with(Some(&input)).unwrap().id.clone())
            .collect();
        assert!(picks.windows(2).all(|window| window[0] == window[1]));
    }

    #[test]
    fn hash_fallback_uses_ip_when_header_absent() {
        let targets = vec![target("a", 1), target("b", 1)];
        let hash = HashConfig {
            hash_on: "header".into(),
            hash_on_header: "x-user".into(),
            hash_fallback: "ip".into(),
            ..Default::default()
        };
        let pool = TargetPool::with_config(
            LoadBalanceAlgorithm::ConsistentHashing,
            &targets,
            "http",
            0,
            hash,
            HealthConfig::default(),
            &HealthStore::default(),
        );
        let input = HashInput {
            headers: &HeaderMap::new(),
            path: "/",
            client_ip: Some("10.0.0.7".parse().unwrap()),
            consumer_id: None,
            uri_captures: &[],
        };
        assert!(pool.select_with(Some(&input)).is_some());
    }

    #[test]
    fn passive_health_ejects_after_threshold() {
        let targets = vec![target("a", 1), target("b", 1)];
        let health = HealthConfig::from_value(&serde_json::json!({
            "passive": { "unhealthy": { "http_failures": 2, "http_statuses": [500] } }
        }));
        let store = HealthStore::default();
        let pool = TargetPool::with_config(
            LoadBalanceAlgorithm::RoundRobin,
            &targets,
            "http",
            0,
            HashConfig::default(),
            health,
            &store,
        );
        let first = pool.select().unwrap();
        first.report_status(500);
        assert!(first.is_healthy());
        first.report_status(500);
        assert!(!first.is_healthy());
        assert!(
            pool.ordered_attempts(None)
                .iter()
                .all(|endpoint| endpoint.id != first.id)
        );
    }

    #[test]
    fn hash_key_parses_cookies_and_query_args() {
        let mut headers = HeaderMap::new();
        headers.insert(
            http::header::COOKIE,
            "session=abc; kong=tenant-9".parse().unwrap(),
        );
        let hash = HashConfig {
            hash_on: "cookie".into(),
            ..Default::default()
        };
        let input = HashInput {
            headers: &headers,
            path: "/",
            client_ip: None,
            consumer_id: None,
            uri_captures: &[],
        };
        assert_eq!(hash.key(&input).as_deref(), Some("tenant-9"));

        let hash = HashConfig {
            hash_on: "query_arg".into(),
            hash_on_query_arg: "tenant".into(),
            ..Default::default()
        };
        let input = HashInput {
            headers: &HeaderMap::new(),
            path: "/items?tenant=blue",
            client_ip: None,
            consumer_id: None,
            uri_captures: &[],
        };
        assert_eq!(hash.key(&input).as_deref(), Some("blue"));

        let hash = HashConfig {
            hash_on: "uri_capture".into(),
            hash_on_uri_capture: "tenant".into(),
            ..Default::default()
        };
        let captures = vec![("tenant".to_string(), "blue".to_string())];
        let input = HashInput {
            headers: &HeaderMap::new(),
            path: "/tenants/blue/items",
            client_ip: None,
            consumer_id: None,
            uri_captures: &captures,
        };
        assert_eq!(hash.key(&input).as_deref(), Some("blue"));
    }
}
