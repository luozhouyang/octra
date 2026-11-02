//! Load balancing over upstream targets.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use octra_api_core::{LoadBalanceAlgorithm, Target};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Endpoint {
    pub id: String,
    pub url: String,
    pub weight: u32,
    pub eligible: bool,
    slot_weight: u32,
}

impl Endpoint {
    pub fn direct(id: impl Into<String>, url: impl Into<String>) -> Arc<Self> {
        Arc::new(Self {
            id: id.into(),
            url: url.into(),
            weight: 1,
            eligible: true,
            slot_weight: 1,
        })
    }
}

#[derive(Debug)]
pub struct TargetPool {
    algorithm: LoadBalanceAlgorithm,
    targets: Vec<Arc<Endpoint>>,
    cursor: AtomicU64,
}

impl TargetPool {
    pub fn new(
        algorithm: LoadBalanceAlgorithm,
        targets: &[Target],
        scheme: &str,
        seed: u64,
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
        self.targets.iter().any(|endpoint| endpoint.eligible)
    }

    pub fn select(&self) -> Option<Arc<Endpoint>> {
        self.select_excluding(|_| false)
    }

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
                eligible.first().map(|endpoint| Arc::clone(endpoint))
            }
            _ => {
                let index = (start % eligible.len() as u64) as usize;
                Some(Arc::clone(eligible[index]))
            }
        }
    }

    pub fn ordered_attempts(&self) -> Vec<Arc<Endpoint>> {
        let eligible = self.eligible_refs();
        if eligible.is_empty() {
            return Vec::new();
        }
        let start = self.cursor.fetch_add(1, Ordering::Relaxed);
        let len = eligible.len();

        match self.algorithm {
            LoadBalanceAlgorithm::WeightedRoundRobin => {
                let total: u64 = eligible
                    .iter()
                    .map(|endpoint| u64::from(endpoint.slot_weight))
                    .sum();
                let mut point = start % total;
                let mut ordered = Vec::with_capacity(len);
                let mut seen = 0;

                for (index, endpoint) in eligible.iter().enumerate() {
                    let weight = u64::from(endpoint.slot_weight);
                    if point < weight {
                        ordered.push(Arc::clone(endpoint));
                        seen = index;
                        break;
                    }
                    point -= weight;
                }

                for step in 1..len {
                    let endpoint = &eligible[(seen + step) % len];
                    ordered.push(Arc::clone(endpoint));
                }
                ordered
            }
            _ => {
                let offset = (start % len as u64) as usize;
                (0..len)
                    .map(|step| Arc::clone(eligible[(offset + step) % len]))
                    .collect()
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

/// Maps a stored algorithm name to the local implementation. Only round-robin
/// and weighted-round-robin are implemented; other strategies fall back to
/// round-robin.
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
}
