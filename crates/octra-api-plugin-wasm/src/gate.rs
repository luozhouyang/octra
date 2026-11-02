//! Process-wide bound on concurrent plugin invocations.
//!
//! Calls run on blocking threads; without a cap a burst of requests could
//! occupy the whole blocking pool. A zero capacity means "unbounded".

use std::sync::{Arc, Condvar, Mutex};

pub(crate) struct InflightGate {
    capacity: usize,
    active: Mutex<usize>,
    released: Condvar,
}

impl InflightGate {
    pub(crate) fn new(capacity: usize) -> Arc<Self> {
        Arc::new(Self {
            capacity,
            active: Mutex::new(0),
            released: Condvar::new(),
        })
    }

    pub(crate) fn acquire(self: &Arc<Self>) -> InflightPermit {
        if self.capacity == 0 {
            return InflightPermit { gate: None };
        }

        let mut active = self
            .active
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        while *active >= self.capacity {
            active = self
                .released
                .wait(active)
                .unwrap_or_else(|poisoned| poisoned.into_inner());
        }
        *active += 1;
        drop(active);
        InflightPermit {
            gate: Some(self.clone()),
        }
    }
}

pub(crate) struct InflightPermit {
    gate: Option<Arc<InflightGate>>,
}

impl Drop for InflightPermit {
    fn drop(&mut self) {
        let Some(gate) = self.gate.take() else {
            return;
        };
        let mut active = gate
            .active
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        *active = active.saturating_sub(1);
        drop(active);
        gate.released.notify_one();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;
    use std::thread;
    use std::time::Duration;

    #[test]
    fn bounds_concurrent_invocations() {
        let gate = InflightGate::new(1);
        let first = gate.acquire();
        let (sender, receiver) = mpsc::channel();
        let blocked = gate.clone();
        thread::spawn(move || {
            let _permit = blocked.acquire();
            sender.send(()).unwrap();
        });
        assert!(receiver.recv_timeout(Duration::from_millis(50)).is_err());
        drop(first);
        receiver.recv_timeout(Duration::from_secs(1)).unwrap();
    }

    #[test]
    fn zero_capacity_is_unbounded() {
        let gate = InflightGate::new(0);
        let _first = gate.acquire();
        let _second = gate.acquire();
    }
}
