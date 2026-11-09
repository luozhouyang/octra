//! In-memory reservations for virtual-key token and cost limits.
//!
//! Usage is persisted asynchronously, so a pure read-then-check admits every
//! concurrent request that starts before the first usage row lands. A request
//! reserves a pessimistic token/cost budget before it starts; admissions see
//! persisted usage plus every active reservation, and the guard releases the
//! reservation when the request (or stream) is dropped.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

/// One limit-window reservation.
#[derive(Debug, Clone)]
pub struct ReservationRequest {
    pub key: String,
    pub tokens: u64,
    pub cost_micros: u64,
    pub usage_tokens: u64,
    pub usage_cost_micros: u64,
    pub limit_tokens: Option<u64>,
    pub limit_cost_micros: Option<u64>,
}

#[derive(Debug, Default)]
pub struct VkReservationLedger {
    active: Mutex<HashMap<String, (u64, u64)>>,
}

#[derive(Debug)]
pub struct VkReservation {
    ledger: Arc<VkReservationLedger>,
    entries: Vec<(String, u64, u64)>,
}

impl Drop for VkReservation {
    fn drop(&mut self) {
        let mut active = self
            .ledger
            .active
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        for (key, tokens, cost) in &self.entries {
            let Some((reserved_tokens, reserved_cost)) = active.get_mut(key) else {
                continue;
            };
            *reserved_tokens = reserved_tokens.saturating_sub(*tokens);
            *reserved_cost = reserved_cost.saturating_sub(*cost);
            if *reserved_tokens == 0 && *reserved_cost == 0 {
                active.remove(key);
            }
        }
    }
}

impl VkReservationLedger {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// Atomically checks every window and records the reservation. Returns
    /// `None` when any window would exceed its limit.
    pub fn try_reserve(
        self: &Arc<Self>,
        requests: Vec<ReservationRequest>,
    ) -> Option<VkReservation> {
        if requests.is_empty() {
            return None;
        }
        let mut active = self
            .active
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        for request in &requests {
            let (reserved_tokens, reserved_cost) =
                active.get(&request.key).copied().unwrap_or_default();
            if let Some(limit) = request.limit_tokens {
                let used = request.usage_tokens.saturating_add(reserved_tokens);
                if used > limit || (used == limit && request.tokens == 0) {
                    return None;
                }
            }
            if let Some(limit) = request.limit_cost_micros {
                let used = request.usage_cost_micros.saturating_add(reserved_cost);
                if used > limit || (used == limit && request.cost_micros == 0) {
                    return None;
                }
            }
        }

        let mut entries = Vec::with_capacity(requests.len());
        for request in requests {
            let entry = active.entry(request.key.clone()).or_default();
            entry.0 = entry.0.saturating_add(request.tokens);
            entry.1 = entry.1.saturating_add(request.cost_micros);
            entries.push((request.key, request.tokens, request.cost_micros));
        }
        Some(VkReservation {
            ledger: self.clone(),
            entries,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request(key: &str, tokens: u64, usage: u64, limit: u64) -> ReservationRequest {
        ReservationRequest {
            key: key.into(),
            tokens,
            cost_micros: 0,
            usage_tokens: usage,
            usage_cost_micros: 0,
            limit_tokens: Some(limit),
            limit_cost_micros: None,
        }
    }

    #[test]
    fn concurrent_admissions_see_reservations() {
        let ledger = VkReservationLedger::new();
        let first = ledger
            .try_reserve(vec![request("k", 60, 0, 100)])
            .expect("first request fits");
        assert!(ledger.try_reserve(vec![request("k", 60, 0, 100)]).is_none());
        drop(first);
        assert!(ledger.try_reserve(vec![request("k", 60, 0, 100)]).is_some());
    }
}
