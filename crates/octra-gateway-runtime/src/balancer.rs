//! Load balancing over eligible targets.
//!
//! A pool only considers targets that are enabled and healthy. Round-robin
//! advances a shared atomic cursor; weighted round-robin maps the same cursor
//! onto a weighted slot schedule, so targets with larger weights receive a
//! proportionally larger share of requests.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use octra_api_core::{LoadBalanceAlgorithm, Target};

/// One selectable upstream destination.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Endpoint {
    pub id: String,
    pub url: String,
    pub weight: u32,
    /// Whether the backing target is enabled and healthy.
    pub eligible: bool,
    /// Effective weight used by weighted selection (at least 1).
    slot_weight: u32,
}

#[derive(Debug)]
pub struct TargetPool {
    algorithm: LoadBalanceAlgorithm,
    targets: Vec<Arc<Endpoint>>,
    cursor: AtomicU64,
}

impl TargetPool {
    /// Builds a pool from all targets of one upstream. `seed` offsets the
    /// cursor so independently created pools do not march in lockstep.
    pub fn new(algorithm: LoadBalanceAlgorithm, targets: &[&Target], seed: u64) -> Self {
        let endpoints: Vec<Arc<Endpoint>> = targets
            .iter()
            .map(|target| {
                Arc::new(Endpoint {
                    id: target.id.clone(),
                    url: target.url.trim_end_matches('/').to_string(),
                    weight: target.weight,
                    eligible: target.enabled && target.healthy,
                    slot_weight: target.weight.max(1),
                })
            })
            .collect();

        Self {
            algorithm,
            targets: endpoints,
            cursor: AtomicU64::new(seed),
        }
    }

    pub fn algorithm(&self) -> LoadBalanceAlgorithm {
        self.algorithm
    }

    /// Resets the rotation cursor. Intended for tests.
    #[doc(hidden)]
    pub fn reset_cursor(&self) {
        self.cursor.store(0, Ordering::Relaxed);
    }

    pub fn len(&self) -> usize {
        self.targets.len()
    }

    pub fn is_empty(&self) -> bool {
        self.targets.is_empty()
    }

    /// Targets that may receive traffic.
    pub fn eligible(&self) -> Vec<Arc<Endpoint>> {
        self.eligible_refs().into_iter().map(Arc::clone).collect()
    }

    /// Selects the next eligible target, or `None` when none qualify.
    pub fn select(&self) -> Option<Arc<Endpoint>> {
        self.select_excluding(|_| false)
    }

    /// Selects the next eligible target that `excluded` does not reject. Used
    /// to retry across distinct targets.
    pub fn select_excluding(&self, excluded: impl Fn(&str) -> bool) -> Option<Arc<Endpoint>> {
        let eligible = self.eligible_refs();
        let eligible: Vec<&Arc<Endpoint>> = eligible
            .into_iter()
            .filter(|endpoint| !excluded(endpoint.id.as_str()))
            .collect();
        if eligible.is_empty() {
            return None;
        }

        let start = self.cursor.fetch_add(1, Ordering::Relaxed);
        match self.algorithm {
            LoadBalanceAlgorithm::RoundRobin => {
                let index = (start % eligible.len() as u64) as usize;
                Some(Arc::clone(eligible[index]))
            }
            LoadBalanceAlgorithm::WeightedRoundRobin => {
                let total: u64 = eligible
                    .iter()
                    .map(|endpoint| u64::from(endpoint.slot_weight))
                    .sum();
                let mut point = start % total;
                for endpoint in &eligible {
                    let weight = u64::from(endpoint.slot_weight);
                    if point < weight {
                        return Some(Arc::clone(endpoint));
                    }
                    point -= weight;
                }
                // Unreachable because `point < total`; fall back to the first.
                eligible.first().map(|endpoint| Arc::clone(endpoint))
            }
        }
    }

    /// At most `len` distinct selections, in cursor order, for retries.
    pub fn ordered_attempts(&self) -> Vec<Arc<Endpoint>> {
        let eligible = self.eligible_refs();
        if eligible.is_empty() {
            return Vec::new();
        }
        let start = self.cursor.fetch_add(1, Ordering::Relaxed);
        let len = eligible.len();

        match self.algorithm {
            LoadBalanceAlgorithm::RoundRobin => {
                let offset = (start % len as u64) as usize;
                (0..len)
                    .map(|step| Arc::clone(eligible[(offset + step) % len]))
                    .collect()
            }
            LoadBalanceAlgorithm::WeightedRoundRobin => {
                let total: u64 = eligible
                    .iter()
                    .map(|endpoint| u64::from(endpoint.slot_weight))
                    .sum();
                let mut point = start % total;
                let mut ordered = Vec::with_capacity(len);
                let mut seen = 0;

                // First pass: the chosen weighted slot.
                for (index, endpoint) in eligible.iter().enumerate() {
                    let weight = u64::from(endpoint.slot_weight);
                    if point < weight {
                        ordered.push(Arc::clone(endpoint));
                        seen = index;
                        break;
                    }
                    point -= weight;
                }

                // Remaining targets rotate after the chosen one.
                for step in 1..len {
                    let endpoint = &eligible[(seen + step) % len];
                    ordered.push(Arc::clone(endpoint));
                }
                ordered
            }
        }
    }

    fn eligible_refs(&self) -> Vec<&Arc<Endpoint>> {
        self.targets
            .iter()
            .filter(|endpoint| endpoint.eligible)
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;

    fn target(id: &str, weight: u32, enabled: bool, healthy: bool) -> Target {
        let now = Utc::now();
        Target {
            id: id.into(),
            upstream_id: "up".into(),
            url: format!("http://{id}.test"),
            weight,
            healthy,
            enabled,
            created_at: now,
            updated_at: now,
        }
    }

    fn pool(algorithm: LoadBalanceAlgorithm, targets: &[Target]) -> TargetPool {
        let refs: Vec<&Target> = targets.iter().collect();
        TargetPool::new(algorithm, &refs, 0)
    }

    #[test]
    fn round_robin_cycles_in_order() {
        let targets = vec![target("a", 1, true, true), target("b", 1, true, true)];
        let pool = pool(LoadBalanceAlgorithm::RoundRobin, &targets);
        let picks: Vec<String> = (0..4).map(|_| pool.select().unwrap().id.clone()).collect();
        assert_eq!(picks, vec!["a", "b", "a", "b"]);
    }

    #[test]
    fn weighted_round_robin_respects_weights() {
        let targets = vec![target("a", 1, true, true), target("b", 3, true, true)];
        let pool = pool(LoadBalanceAlgorithm::WeightedRoundRobin, &targets);
        let mut counts = std::collections::HashMap::new();
        for _ in 0..40 {
            *counts.entry(pool.select().unwrap().id.clone()).or_insert(0) += 1;
        }
        assert_eq!(counts["a"], 10);
        assert_eq!(counts["b"], 30);
    }

    #[test]
    fn filters_disabled_and_unhealthy() {
        let targets = vec![
            target("a", 1, false, true),
            target("b", 1, true, false),
            target("c", 1, true, true),
        ];
        let pool = pool(LoadBalanceAlgorithm::RoundRobin, &targets);
        for _ in 0..3 {
            assert_eq!(pool.select().unwrap().id, "c");
        }
    }

    #[test]
    fn empty_pool_returns_none() {
        let pool = pool(LoadBalanceAlgorithm::RoundRobin, &[]);
        assert!(pool.select().is_none());
    }

    #[test]
    fn retry_attempts_are_distinct() {
        let targets = vec![target("a", 1, true, true), target("b", 1, true, true)];
        let pool = pool(LoadBalanceAlgorithm::RoundRobin, &targets);
        let attempts = pool.ordered_attempts();
        assert_eq!(attempts.len(), 2);
        assert_ne!(attempts[0].id, attempts[1].id);
    }
}
