//! Provider account pool: per-account secrets, quota and cooldown state.
//!
//! Account records come from storage and are immutable per snapshot; the
//! counters below are live and shared across snapshots so a reload does not
//! forget a cooldown or consumed quota. Persisted counters stay authoritative
//! and are reconciled on bootstrap and reload.

use std::collections::BTreeMap;
use std::collections::HashSet;
use std::sync::Arc;
use std::sync::RwLock;
use std::sync::atomic::{AtomicI64, Ordering};

use chrono::{DateTime, Datelike, TimeZone, Utc};
use octra_storage::LlmProviderAccountRecord;

/// Live state for one account.
#[derive(Debug)]
pub struct AccountRuntime {
    pub record: LlmProviderAccountRecord,
    used: AtomicI64,
    cooldown_until_ms: AtomicI64,
    period_end_ms: AtomicI64,
    latency_ewma_ms: AtomicI64,
    failure_ewma: AtomicI64,
    pace_tat_ms: AtomicI64,
    pace: Option<Pace>,
}

/// Request pacing derived from `config_json`: `{"requests_per_minute": 60,
/// "burst": 10}`. Pacing is opt-in per account.
#[derive(Debug, Clone, Copy)]
struct Pace {
    interval_ms: i64,
    tolerance_ms: i64,
}

/// A quota counter that rolled over and should be persisted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QuotaReset {
    pub account_id: String,
    pub quota_reset_at: String,
}

impl AccountRuntime {
    fn new(record: LlmProviderAccountRecord) -> Self {
        let now = Utc::now();
        let period_end =
            stored_period_end(&record).unwrap_or_else(|| next_period_end(&record, now));
        let cooldown = record
            .cooldown_until
            .as_deref()
            .and_then(parse_ms)
            .unwrap_or(0);
        Self {
            used: AtomicI64::new(record.quota_used),
            cooldown_until_ms: AtomicI64::new(cooldown),
            period_end_ms: AtomicI64::new(period_end.timestamp_millis()),
            latency_ewma_ms: AtomicI64::new(0),
            failure_ewma: AtomicI64::new(0),
            pace_tat_ms: AtomicI64::new(0),
            pace: parse_pace(&record),
            record,
        }
    }

    pub fn id(&self) -> &str {
        &self.record.id
    }

    pub fn used(&self) -> i64 {
        self.used.load(Ordering::Relaxed)
    }

    pub fn quota_remaining(&self) -> Option<i64> {
        let limit = self.record.quota_limit?;
        Some((limit - self.used()).max(0))
    }

    pub fn is_available(&self, now_ms: i64) -> bool {
        self.record.enabled
            && now_ms >= self.cooldown_until_ms.load(Ordering::Relaxed)
            && self.quota_remaining().is_none_or(|remaining| remaining > 0)
    }

    pub fn cooldown_until(&self) -> i64 {
        self.cooldown_until_ms.load(Ordering::Relaxed)
    }

    /// Adds one request's consumption to the live counter for its unit.
    pub fn consume(&self, requests: i64, tokens: i64, cost_micros: i64) {
        let delta = match self.record.quota_unit.as_str() {
            "tokens" => tokens,
            "cost_micros" => cost_micros,
            _ => requests,
        };
        self.used.fetch_add(delta, Ordering::Relaxed);
    }

    pub fn mark_cooldown(&self, until_ms: i64) {
        self.cooldown_until_ms
            .fetch_max(until_ms, Ordering::Relaxed);
    }

    pub fn clear_cooldown(&self) {
        self.cooldown_until_ms.store(0, Ordering::Relaxed);
    }

    pub fn clear_usage(&self) {
        self.used.store(0, Ordering::Relaxed);
    }

    /// Smooths observed upstream latency into the account health signal.
    pub fn observe_success(&self, latency_ms: u64) {
        let sample = latency_ms.min(i64::MAX as u64) as i64;
        let current = self.latency_ewma_ms.load(Ordering::Relaxed);
        let next = if current == 0 {
            sample
        } else {
            (current * 7 + sample) / 8
        };
        self.latency_ewma_ms.store(next, Ordering::Relaxed);
        let failures = self.failure_ewma.load(Ordering::Relaxed);
        if failures > 0 {
            self.failure_ewma
                .store(failures - failures / 4, Ordering::Relaxed);
        }
    }

    /// Raises the failure signal; it decays on later successes.
    pub fn observe_failure(&self) {
        let current = self.failure_ewma.load(Ordering::Relaxed);
        let next = current + (1000 - current) / 2;
        self.failure_ewma.store(next.min(1000), Ordering::Relaxed);
    }

    pub fn latency_ewma_ms(&self) -> u64 {
        self.latency_ewma_ms.load(Ordering::Relaxed).max(0) as u64
    }

    pub fn failure_ewma(&self) -> u64 {
        self.failure_ewma.load(Ordering::Relaxed).max(0) as u64
    }

    /// Consumes one pacing slot. Returns false when the account is paced out.
    /// Only call this immediately before making an upstream attempt.
    pub fn try_acquire(&self, now_ms: i64) -> bool {
        let Some(pace) = self.pace else {
            return true;
        };
        let mut tat = self.pace_tat_ms.load(Ordering::Relaxed);
        loop {
            if tat - pace.tolerance_ms > now_ms {
                return false;
            }
            let next = tat.max(now_ms).saturating_add(pace.interval_ms);
            match self.pace_tat_ms.compare_exchange_weak(
                tat,
                next,
                Ordering::Relaxed,
                Ordering::Relaxed,
            ) {
                Ok(_) => return true,
                Err(current) => tat = current,
            }
        }
    }

    /// Milliseconds until this account can accept another paced request.
    pub fn pace_wait_ms(&self, now_ms: i64) -> u64 {
        let Some(pace) = self.pace else {
            return 0;
        };
        let tat = self.pace_tat_ms.load(Ordering::Relaxed);
        (tat - pace.tolerance_ms - now_ms).max(0) as u64
    }

    /// Resets the live counter when its quota period has elapsed.
    fn roll_period(&self, now: DateTime<Utc>) -> Option<QuotaReset> {
        if self.record.quota_period == "none" {
            return None;
        }
        let end = self.period_end_ms.load(Ordering::Relaxed);
        if end != 0 && now.timestamp_millis() < end {
            return None;
        }
        let next = next_period_end(&self.record, now);
        self.period_end_ms
            .store(next.timestamp_millis(), Ordering::Relaxed);
        self.used.store(0, Ordering::Relaxed);
        Some(QuotaReset {
            account_id: self.record.id.clone(),
            quota_reset_at: next.to_rfc3339(),
        })
    }
}

/// Shared account state keyed by account id.
#[derive(Debug, Clone, Default)]
pub struct AccountPool {
    entries: Arc<RwLock<BTreeMap<String, Arc<AccountRuntime>>>>,
}

impl AccountPool {
    pub fn new() -> Self {
        Self::default()
    }

    /// Reconciles the pool with storage, preserving live counters. Returns
    /// quota periods that rolled over and need persisting.
    pub fn sync(&self, records: &[LlmProviderAccountRecord]) -> Vec<QuotaReset> {
        let now = Utc::now();
        let mut guard = self
            .entries
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let mut resets = Vec::new();
        let mut seen = HashSet::new();
        for record in records {
            seen.insert(record.id.clone());
            let entry = match guard.get(&record.id) {
                Some(existing) => {
                    let cooldown = existing.cooldown_until().max(
                        record
                            .cooldown_until
                            .as_deref()
                            .and_then(parse_ms)
                            .unwrap_or(0),
                    );
                    Arc::new(AccountRuntime {
                        record: record.clone(),
                        // Persisted usage may include streamed requests that
                        // completed after the live counter was read.
                        used: AtomicI64::new(existing.used().max(record.quota_used)),
                        cooldown_until_ms: AtomicI64::new(cooldown),
                        period_end_ms: AtomicI64::new(
                            existing.period_end_ms.load(Ordering::Relaxed),
                        ),
                        latency_ewma_ms: AtomicI64::new(
                            existing.latency_ewma_ms.load(Ordering::Relaxed),
                        ),
                        failure_ewma: AtomicI64::new(existing.failure_ewma.load(Ordering::Relaxed)),
                        pace_tat_ms: AtomicI64::new(existing.pace_tat_ms.load(Ordering::Relaxed)),
                        pace: parse_pace(record),
                    })
                }
                None => Arc::new(AccountRuntime::new(record.clone())),
            };
            if let Some(reset) = entry.roll_period(now) {
                resets.push(reset);
            }
            guard.insert(record.id.clone(), entry);
        }
        guard.retain(|id, _| seen.contains(id));
        resets
    }

    pub fn get(&self, id: &str) -> Option<Arc<AccountRuntime>> {
        self.entries
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get(id)
            .cloned()
    }

    /// Clears live counters and cooldown after an explicit quota reset.
    pub fn reset(&self, id: &str) {
        if let Some(entry) = self.get(id) {
            entry.clear_usage();
            entry.clear_cooldown();
        }
    }

    /// Records a cooldown after a rate-limit or quota failure.
    pub fn cool_down(&self, id: &str, until_ms: i64) {
        if let Some(entry) = self.get(id) {
            entry.mark_cooldown(until_ms);
        }
    }
}

/// Ordered candidate accounts for one routing attempt.
///
/// Tiers follow priority; within a tier the routing strategy orders the
/// accounts. A session id rotates the first tier so repeated requests from
/// one caller stick to the same account.
pub fn candidates(
    accounts: &[Arc<AccountRuntime>],
    pinned: Option<&str>,
    strategy: &str,
    session: Option<&str>,
    tick: u64,
    now_ms: i64,
) -> Vec<Arc<AccountRuntime>> {
    let mut available: Vec<Arc<AccountRuntime>> = accounts
        .iter()
        .filter(|account| account.is_available(now_ms))
        .cloned()
        .collect();
    if let Some(pinned) = pinned {
        available.retain(|account| account.id() == pinned);
    }
    if available.is_empty() {
        return available;
    }

    available.sort_by(|a, b| {
        a.record
            .priority
            .cmp(&b.record.priority)
            .then_with(|| a.record.name.cmp(&b.record.name))
            .then_with(|| a.id().cmp(b.id()))
    });

    let mut tiers: Vec<Vec<Arc<AccountRuntime>>> = Vec::new();
    for account in available {
        match tiers.last_mut() {
            Some(tier) if tier[0].record.priority == account.record.priority => tier.push(account),
            _ => tiers.push(vec![account]),
        }
    }

    let mut ordered = Vec::new();
    for (index, tier) in tiers.iter().enumerate() {
        let mut ordered_tier = order_tier(tier, strategy, tick, now_ms);
        if index == 0
            && let Some(session) = session
            && !session.is_empty()
            && !ordered_tier.is_empty()
        {
            let start = (hash(session) as usize) % ordered_tier.len();
            ordered_tier.rotate_left(start);
        }
        ordered.extend(ordered_tier);
    }
    ordered
}

fn order_tier(
    tier: &[Arc<AccountRuntime>],
    strategy: &str,
    tick: u64,
    _now_ms: i64,
) -> Vec<Arc<AccountRuntime>> {
    let mut ordered = tier.to_vec();
    match strategy {
        "weighted" => {
            let cycle = weighted_cycle(tier);
            if !cycle.is_empty() {
                let start = (tick % cycle.len() as u64) as usize;
                let mut seen = HashSet::new();
                let mut rotated = Vec::with_capacity(cycle.len());
                for offset in 0..cycle.len() {
                    let account = &tier[cycle[(start + offset) % cycle.len()]];
                    if seen.insert(account.id().to_string()) {
                        rotated.push(account.clone());
                    }
                }
                ordered = rotated;
            }
        }
        "rotate" | "round_robin" => {
            if !ordered.is_empty() {
                let start = (tick % ordered.len() as u64) as usize;
                ordered.rotate_left(start);
            }
        }
        "usage" => ordered.sort_by_key(|account| account.used()),
        "quota" => ordered.sort_by(|a, b| {
            let a = a.quota_remaining().unwrap_or(i64::MAX);
            let b = b.quota_remaining().unwrap_or(i64::MAX);
            b.cmp(&a)
        }),
        "smart" => ordered.sort_by(|a, b| {
            smart_key(a)
                .partial_cmp(&smart_key(b))
                .unwrap_or(std::cmp::Ordering::Equal)
        }),
        "pace" => ordered.sort_by_key(|account| account.pace_wait_ms(_now_ms)),
        _ => {}
    }
    ordered
}

/// Lower is better: latency plus failure and quota pressure.
fn smart_key(account: &AccountRuntime) -> f64 {
    let pressure = match account.quota_remaining() {
        Some(remaining) => {
            let limit = account.record.quota_limit.unwrap_or(0).max(1) as f64;
            (limit - remaining as f64) / limit
        }
        None => 0.0,
    };
    account.latency_ewma_ms() as f64 + account.failure_ewma() as f64 * 500.0 + pressure * 1000.0
}

fn weighted_cycle(tier: &[Arc<AccountRuntime>]) -> Vec<usize> {
    let mut cycle = Vec::new();
    for (index, account) in tier.iter().enumerate() {
        for _ in 0..account.record.weight.clamp(1, 20) {
            cycle.push(index);
        }
    }
    cycle
}

fn hash(key: &str) -> u64 {
    let mut hash: u64 = 1469598103934665603;
    for byte in key.as_bytes() {
        hash ^= *byte as u64;
        hash = hash.wrapping_mul(1099511628211);
    }
    hash
}

fn parse_ms(value: &str) -> Option<i64> {
    DateTime::parse_from_rfc3339(value)
        .ok()
        .map(|value| value.timestamp_millis())
}

fn parse_pace(record: &LlmProviderAccountRecord) -> Option<Pace> {
    let value: serde_json::Value = serde_json::from_str(&record.config_json).ok()?;
    let rpm = value
        .get("requests_per_minute")
        .or_else(|| value.get("rpm"))
        .and_then(serde_json::Value::as_i64)?;
    if rpm <= 0 {
        tracing::warn!(account_id = %record.id, rpm, "ignoring non-positive account pacing");
        return None;
    }
    let burst = value
        .get("burst")
        .and_then(serde_json::Value::as_i64)
        .unwrap_or(1)
        .max(1);
    let interval_ms = (60_000 / rpm).max(1);
    Some(Pace {
        interval_ms,
        tolerance_ms: interval_ms.saturating_mul(burst.saturating_sub(1)),
    })
}

fn stored_period_end(record: &LlmProviderAccountRecord) -> Option<DateTime<Utc>> {
    if !matches!(record.quota_period.as_str(), "day" | "month") {
        return None;
    }
    record
        .quota_reset_at
        .as_deref()
        .and_then(|value| DateTime::parse_from_rfc3339(value).ok())
        .map(|value| value.with_timezone(&Utc))
}

fn next_period_end(record: &LlmProviderAccountRecord, now: DateTime<Utc>) -> DateTime<Utc> {
    match record.quota_period.as_str() {
        "month" => {
            let (year, month) = if now.month() == 12 {
                (now.year() + 1, 1)
            } else {
                (now.year(), now.month() + 1)
            };
            Utc.with_ymd_and_hms(year, month, 1, 0, 0, 0)
                .single()
                .unwrap_or(now)
        }
        _ => {
            let today = now.date_naive();
            let tomorrow = today.succ_opt().unwrap_or(today);
            Utc.from_utc_datetime(&tomorrow.and_hms_opt(0, 0, 0).unwrap_or_default())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn account(
        id: &str,
        priority: i64,
        weight: i64,
        quota_limit: Option<i64>,
        quota_used: i64,
    ) -> LlmProviderAccountRecord {
        LlmProviderAccountRecord {
            id: id.into(),
            provider_id: "p".into(),
            name: id.into(),
            secret_ref: None,
            weight,
            priority,
            enabled: true,
            quota_limit,
            quota_unit: "requests".into(),
            quota_period: "none".into(),
            quota_used,
            quota_reset_at: None,
            cooldown_until: None,
            last_error: None,
            config_json: "{}".into(),
            created_at: String::new(),
            updated_at: String::new(),
        }
    }

    fn runtime(record: LlmProviderAccountRecord) -> Arc<AccountRuntime> {
        Arc::new(AccountRuntime::new(record))
    }

    #[test]
    fn priority_tiers_order_candidates() {
        let pool = vec![
            runtime(account("b", 1, 1, None, 0)),
            runtime(account("a", 0, 1, None, 0)),
        ];
        let ordered = candidates(&pool, None, "priority", None, 0, 0);
        assert_eq!(ordered[0].id(), "a");
        assert_eq!(ordered[1].id(), "b");
    }

    #[test]
    fn exhausted_and_cooling_accounts_are_skipped() {
        let exhausted = runtime(account("full", 0, 1, Some(10), 10));
        let cooling = runtime(account("cool", 1, 1, None, 0));
        cooling.mark_cooldown(10_000);
        let healthy = runtime(account("ok", 2, 1, None, 0));
        let ordered = candidates(&[exhausted, cooling, healthy], None, "priority", None, 0, 0);
        assert_eq!(ordered.len(), 1);
        assert_eq!(ordered[0].id(), "ok");
    }

    #[test]
    fn weighted_and_rotate_change_the_first_pick() {
        let pool = vec![
            runtime(account("a", 0, 3, None, 0)),
            runtime(account("b", 0, 1, None, 0)),
        ];
        let first: Vec<String> = (0..4)
            .map(|tick| {
                candidates(&pool, None, "weighted", None, tick, 0)[0]
                    .id()
                    .to_string()
            })
            .collect();
        assert_eq!(first, vec!["a", "a", "a", "b"]);
        let rotated: Vec<String> = (0..2)
            .map(|tick| {
                candidates(&pool, None, "rotate", None, tick, 0)[0]
                    .id()
                    .to_string()
            })
            .collect();
        assert_eq!(rotated, vec!["a", "b"]);
    }

    #[test]
    fn session_affinity_is_stable_within_a_tier() {
        let pool = vec![
            runtime(account("a", 0, 1, None, 0)),
            runtime(account("b", 0, 1, None, 0)),
            runtime(account("c", 0, 1, None, 0)),
        ];
        let picks: Vec<String> = (0..5)
            .map(|_| {
                candidates(&pool, None, "priority", Some("session-1"), 0, 0)[0]
                    .id()
                    .to_string()
            })
            .collect();
        assert!(picks.iter().all(|pick| pick == &picks[0]));
    }

    #[test]
    fn usage_and_quota_order_by_counters() {
        let pool = vec![
            runtime(account("used", 0, 1, None, 50)),
            runtime(account("fresh", 0, 1, None, 0)),
        ];
        assert_eq!(
            candidates(&pool, None, "usage", None, 0, 0)[0].id(),
            "fresh"
        );
        let limited = vec![
            runtime(account("low", 0, 1, Some(100), 90)),
            runtime(account("high", 0, 1, Some(100), 10)),
        ];
        assert_eq!(
            candidates(&limited, None, "quota", None, 0, 0)[0].id(),
            "high"
        );
    }

    #[test]
    fn smart_prefers_healthy_accounts() {
        let slow = runtime(account("slow", 0, 1, None, 0));
        slow.observe_success(1200);
        slow.observe_failure();
        let fast = runtime(account("fast", 0, 1, None, 0));
        fast.observe_success(80);
        let ordered = candidates(&[slow, fast], None, "smart", None, 0, 0);
        assert_eq!(ordered[0].id(), "fast");
    }

    #[test]
    fn gcra_pacing_limits_and_recovers() {
        let mut record = account("paced", 0, 1, None, 0);
        record.config_json = r#"{"requests_per_minute": 60, "burst": 1}"#.into();
        let paced = AccountRuntime::new(record);
        let now = 1_000_000;
        assert!(paced.try_acquire(now));
        assert!(!paced.try_acquire(now));
        assert_eq!(paced.pace_wait_ms(now), 1_000);
        assert_eq!(paced.pace_wait_ms(now + 1_000), 0);
        assert!(paced.try_acquire(now + 1_000));
    }

    #[test]
    fn pacing_burst_allows_short_bursts() {
        let mut record = account("bursty", 0, 1, None, 0);
        record.config_json = r#"{"requests_per_minute": 60, "burst": 3}"#.into();
        let paced = AccountRuntime::new(record);
        let now = 5_000;
        assert!(paced.try_acquire(now));
        assert!(paced.try_acquire(now));
        assert!(paced.try_acquire(now));
        assert!(!paced.try_acquire(now));
    }

    #[test]
    fn invalid_pacing_config_is_ignored() {
        let mut record = account("unpaced", 0, 1, None, 0);
        record.config_json = r#"{"requests_per_minute": 0}"#.into();
        let entry = AccountRuntime::new(record);
        for _ in 0..10 {
            assert!(entry.try_acquire(0));
        }
    }

    #[test]
    fn daily_quota_rolls_over() {
        let mut record = account("a", 0, 1, Some(10), 10);
        record.quota_period = "day".into();
        record.quota_reset_at = Some("2000-01-01T00:00:00Z".into());
        let entry = AccountRuntime::new(record);
        let now = Utc::now();
        let reset = entry.roll_period(now).expect("period should roll");
        assert_eq!(reset.account_id, "a");
        assert_eq!(entry.used(), 0);
        assert!(entry.is_available(now.timestamp_millis()));
    }

    #[test]
    fn pool_sync_preserves_live_counters() {
        let pool = AccountPool::new();
        let record = account("a", 0, 1, Some(10), 2);
        pool.sync(std::slice::from_ref(&record));
        let live = pool.get("a").unwrap();
        live.consume(1, 0, 0);
        assert_eq!(live.used(), 3);

        let mut updated = record.clone();
        updated.name = "renamed".into();
        pool.sync(std::slice::from_ref(&updated));
        let live = pool.get("a").unwrap();
        assert_eq!(live.used(), 3);
        assert_eq!(live.record.name, "renamed");

        pool.reset("a");
        assert_eq!(pool.get("a").unwrap().used(), 0);
    }

    #[test]
    fn next_period_end_is_aligned_to_boundaries() {
        let record = account("a", 0, 1, None, 0);
        let now = Utc.with_ymd_and_hms(2026, 2, 15, 10, 30, 0).unwrap();
        let day = next_period_end(&record, now);
        assert_eq!(day, Utc.with_ymd_and_hms(2026, 2, 16, 0, 0, 0).unwrap());

        let mut monthly = record;
        monthly.quota_period = "month".into();
        let month = next_period_end(&monthly, now);
        assert_eq!(month, Utc.with_ymd_and_hms(2026, 3, 1, 0, 0, 0).unwrap());
    }
}
