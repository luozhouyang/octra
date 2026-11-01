//! Routing group compilation: priority rounds with weighted round-robin.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use octra_llm_core::Protocol;
use octra_storage::{LlmRouteRecord, LlmRoutingTargetRecord, LlmRuntimeData};

use crate::config::{ProviderConfig, model_protocol};

/// One compiled routing target.
#[derive(Debug, Clone)]
pub struct Target {
    pub provider_id: String,
    pub provider_name: String,
    pub model_id: String,
    pub protocol: Protocol,
    pub weight: u32,
    pub priority: i64,
}

impl Target {
    pub fn label(&self) -> String {
        format!("{}/{}", self.provider_name, self.model_id)
    }
}

/// Ordered targets for one virtual model. Rounds are ordered by ascending
/// priority; within a round, a weighted round-robin picks the next target.
#[derive(Debug, Clone)]
pub struct RoutePlan {
    pub virtual_model_id: String,
    pub route_id: String,
    pub group_id: Option<String>,
    pub strategy: String,
    rounds: Vec<Vec<Target>>,
    cycles: Vec<Vec<usize>>,
    fallback: Vec<Target>,
    group_slot: usize,
}

/// Stable slot per routing group, so round-robin counters are shared by all
/// snapshots of the same runtime.
#[derive(Debug, Clone, Default)]
pub struct RoundRobin {
    counters: Arc<Vec<AtomicU64>>,
}

const MAX_SLOTS: usize = 4096;

impl RoundRobin {
    pub fn new() -> Self {
        Self {
            counters: Arc::new((0..MAX_SLOTS).map(|_| AtomicU64::new(0)).collect()),
        }
    }

    /// Advances the counter for a group and returns the previous value.
    pub fn next(&self, slot: usize) -> u64 {
        self.counters
            .get(slot % MAX_SLOTS)
            .map(|counter| counter.fetch_add(1, Ordering::Relaxed))
            .unwrap_or(0)
    }
}

impl RoutePlan {
    pub fn compile(
        data: &LlmRuntimeData,
        route: &LlmRouteRecord,
        providers: &BTreeMap<String, Arc<ProviderConfig>>,
    ) -> Result<Self, String> {
        let Some(group_id) = route.routing_group_id.as_deref() else {
            return Err(format!(
                "route {} has no routing group",
                route.virtual_model_id
            ));
        };
        let group = data
            .groups
            .iter()
            .find(|group| group.id == group_id)
            .ok_or_else(|| format!("routing group {group_id} does not exist"))?;

        let mut targets: Vec<&LlmRoutingTargetRecord> = data
            .targets
            .iter()
            .filter(|target| target.group_id == group_id)
            .collect();
        targets.sort_by(|a, b| {
            a.priority
                .cmp(&b.priority)
                .then_with(|| a.position.cmp(&b.position))
                .then_with(|| a.id.cmp(&b.id))
        });

        let mut rounds: BTreeMap<i64, Vec<Target>> = BTreeMap::new();
        for record in targets {
            let Some(provider) = providers.get(&record.provider_id) else {
                tracing::warn!(
                    provider_id = %record.provider_id,
                    group_id = %group_id,
                    "routing target skipped: provider is missing or disabled"
                );
                continue;
            };
            let protocol = model_protocol(data, &record.provider_id, &record.model_id)
                .unwrap_or(provider.protocol);
            rounds.entry(record.priority).or_default().push(Target {
                provider_id: record.provider_id.clone(),
                provider_name: provider.name.clone(),
                model_id: record.model_id.clone(),
                protocol,
                weight: record.weight.clamp(1, 1000) as u32,
                priority: record.priority,
            });
        }

        let mut ordered = Vec::new();
        let mut cycles = Vec::new();
        for (_, group_targets) in rounds {
            cycles.push(weighted_cycle(&group_targets));
            ordered.push(group_targets);
        }
        let fallback: Vec<Target> = ordered
            .first()
            .and_then(|round| round.first().cloned())
            .into_iter()
            .collect();

        Ok(Self {
            virtual_model_id: route.virtual_model_id.clone(),
            route_id: route.id.clone(),
            group_id: Some(group_id.to_string()),
            strategy: group.strategy.clone(),
            rounds: ordered,
            cycles,
            fallback,
            group_slot: slot_for(group_id),
        })
    }

    pub fn is_empty(&self) -> bool {
        self.rounds.iter().all(Vec::is_empty)
    }

    /// Attempt order for one request: every round in priority order, then the
    /// highest-priority target once more to satisfy the retry/fallback budget.
    pub fn attempts(&self, tick: u64) -> Vec<Target> {
        let mut attempts = Vec::new();
        for (round_index, round) in self.rounds.iter().enumerate() {
            let cycle = &self.cycles[round_index];
            if cycle.is_empty() {
                continue;
            }
            let start = if round_index == 0 {
                (tick % cycle.len() as u64) as usize
            } else {
                0
            };
            let mut seen = std::collections::HashSet::new();
            for offset in 0..cycle.len() {
                let index = cycle[(start + offset) % cycle.len()];
                if seen.insert(index) {
                    attempts.push(round[index].clone());
                }
            }
        }
        attempts.extend(self.fallback.iter().cloned());
        attempts
    }

    pub fn rounds(&self) -> &[Vec<Target>] {
        &self.rounds
    }

    pub fn group_slot(&self) -> usize {
        self.group_slot
    }
}

/// Expands weights into a repeating cycle of target indexes so `tick % len`
/// yields the right distribution without per-target counters. Repetition is
/// capped so a heavy weight does not explode the cycle.
fn weighted_cycle(targets: &[Target]) -> Vec<usize> {
    let mut cycle = Vec::new();
    for (index, target) in targets.iter().enumerate() {
        for _ in 0..target.weight.clamp(1, 20) {
            cycle.push(index);
        }
    }
    cycle
}

fn slot_for(key: &str) -> usize {
    let mut hash: u64 = 1469598103934665603;
    for byte in key.as_bytes() {
        hash ^= *byte as u64;
        hash = hash.wrapping_mul(1099511628211);
    }
    (hash % MAX_SLOTS as u64) as usize
}

#[cfg(test)]
mod tests {
    use super::*;
    use octra_storage::{
        LlmModelRecord, LlmProviderRecord, LlmRoutingGroupRecord, LlmRoutingTargetRecord,
    };

    fn provider(id: &str, protocol: &str) -> LlmProviderRecord {
        LlmProviderRecord {
            id: id.into(),
            name: id.into(),
            kind: protocol.into(),
            protocol: protocol.into(),
            auth_type: "none".into(),
            base_url: Some("http://127.0.0.1:9/v1".into()),
            secret_ref: None,
            priority: 0,
            config_json: "{}".into(),
            enabled: true,
            created_at: String::new(),
            updated_at: String::new(),
        }
    }

    fn target(id: &str, provider: &str, priority: i64, weight: i64) -> LlmRoutingTargetRecord {
        LlmRoutingTargetRecord {
            id: id.into(),
            group_id: "g".into(),
            provider_id: provider.into(),
            model_id: format!("{provider}-model"),
            weight,
            priority,
            position: 0,
            created_at: String::new(),
            updated_at: String::new(),
        }
    }

    fn setup(
        targets: Vec<LlmRoutingTargetRecord>,
        providers: Vec<LlmProviderRecord>,
    ) -> (LlmRuntimeData, BTreeMap<String, Arc<ProviderConfig>>) {
        let data = LlmRuntimeData {
            providers: providers.clone(),
            policies: Vec::new(),
            models: vec![LlmModelRecord {
                id: "m".into(),
                provider_id: "anthropic".into(),
                model_id: "anthropic-model".into(),
                display_name: None,
                protocol: Some("anthropic".into()),
                context_window: None,
                max_output_tokens: None,
                capabilities: "{}".into(),
                pricing_json: "{}".into(),
                enabled: true,
                created_at: String::new(),
            }],
            groups: vec![LlmRoutingGroupRecord {
                id: "g".into(),
                name: "g".into(),
                strategy: "priority".into(),
                created_at: String::new(),
            }],
            routes: Vec::new(),
            targets,
            virtual_keys: Vec::new(),
        };
        let providers = providers
            .iter()
            .filter(|provider| provider.enabled)
            .map(|record| {
                (
                    record.id.clone(),
                    Arc::new(ProviderConfig::from_record(record).unwrap()),
                )
            })
            .collect();
        (data, providers)
    }

    fn route() -> LlmRouteRecord {
        LlmRouteRecord {
            id: "r".into(),
            virtual_model_id: "v".into(),
            routing_group_id: Some("g".into()),
            created_at: String::new(),
            updated_at: String::new(),
        }
    }

    #[test]
    fn priority_beats_weight() {
        let (data, providers) = setup(
            vec![target("t1", "low", 10, 100), target("t2", "high", 0, 1)],
            vec![provider("low", "openai"), provider("high", "openai")],
        );
        let plan = RoutePlan::compile(&data, &route(), &providers).unwrap();
        let attempts = plan.attempts(0);
        assert_eq!(attempts[0].provider_id, "high");
        assert_eq!(attempts[1].provider_id, "low");
        // Fallback retries the best target at the end.
        assert_eq!(attempts.last().unwrap().provider_id, "high");
    }

    #[test]
    fn weight_controls_distribution_within_a_priority() {
        let (data, providers) = setup(
            vec![target("t1", "a", 0, 3), target("t2", "b", 0, 1)],
            vec![provider("a", "openai"), provider("b", "openai")],
        );
        let plan = RoutePlan::compile(&data, &route(), &providers).unwrap();
        let first: Vec<String> = (0..4)
            .map(|tick| plan.attempts(tick)[0].provider_id.clone())
            .collect();
        assert_eq!(first, vec!["a", "a", "a", "b"]);
        // Every tick still tries both targets on fallback.
        assert_eq!(plan.attempts(0)[1].provider_id, "b");
    }

    #[test]
    fn disabled_provider_is_skipped() {
        let mut disabled = provider("b", "openai");
        disabled.enabled = false;
        let (data, providers) = setup(
            vec![target("t1", "a", 0, 1), target("t2", "b", 0, 1)],
            vec![provider("a", "openai"), disabled],
        );
        let plan = RoutePlan::compile(&data, &route(), &providers).unwrap();
        assert_eq!(plan.targets_len_for_test(), 1);
        assert_eq!(plan.attempts(0)[0].provider_id, "a");
    }

    #[test]
    fn model_override_decides_protocol() {
        let (data, providers) = setup(
            vec![target("t1", "anthropic", 0, 1)],
            vec![provider("anthropic", "openai")],
        );
        let plan = RoutePlan::compile(&data, &route(), &providers).unwrap();
        assert_eq!(plan.attempts(0)[0].protocol, Protocol::Anthropic);
    }

    impl RoutePlan {
        fn targets_len_for_test(&self) -> usize {
            self.rounds.iter().map(Vec::len).sum()
        }
    }
}
