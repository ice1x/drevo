//! Connection limits: how many Bolt connections and HTTP requests a server
//! serves at once.
//!
//! A [`Gate`](crate::limits::Gate) counts what is inside it and refuses entry
//! past its maximum (`0` means no maximum). A [`Pass`](crate::limits::Pass) is
//! one admitted connection or request; dropping it frees the slot.
//! [`ConnectionLimits`](crate::limits::ConnectionLimits) holds the two gates a
//! server has, one for Bolt connections (`DREVO_MAX_CONNECTIONS`) and one for
//! HTTP requests in flight (`DREVO_HTTP_MAX_IN_FLIGHT`).
//!
//! ```
//! use std::sync::Arc;
//! use drevo::limits::Gate;
//!
//! let gate = Arc::new(Gate::new(1));
//! let first = gate.try_enter().expect("room for one");
//! assert!(gate.try_enter().is_none(), "full");
//! assert_eq!(gate.rejected(), 1);
//! drop(first);
//! assert!(gate.try_enter().is_some(), "room again");
//! ```

use std::sync::atomic::{AtomicI64, AtomicU64, AtomicUsize, Ordering};
use std::sync::Arc;

/// Bolt connections served at once unless `DREVO_MAX_CONNECTIONS` says
/// otherwise.
pub const DEFAULT_MAX_CONNECTIONS: usize = 512;

/// HTTP requests served at once unless `DREVO_HTTP_MAX_IN_FLIGHT` says
/// otherwise.
pub const DEFAULT_HTTP_MAX_IN_FLIGHT: usize = 256;

/// How many writes one explicit transaction may buffer before it commits,
/// unless `DREVO_MAX_TX_WRITES` says otherwise.
pub const DEFAULT_MAX_TX_WRITES: usize = 1_000_000;

/// The Bolt status a refused connection gets in reply to `HELLO`. Neo4j sends
/// it when it has no worker left for a connection; drivers treat it as
/// transient and try again.
pub const BOLT_REFUSED_CODE: &str = "Neo.TransientError.Request.NoThreadsAvailable";

/// A counted set of slots with an optional maximum.
#[derive(Debug, Default)]
pub struct Gate {
    max: AtomicUsize,
    active: AtomicUsize,
    rejected: AtomicU64,
    last_warned_ms: AtomicI64,
}

impl Gate {
    /// A gate that admits at most `max` at once (`0`: no maximum).
    pub fn new(max: usize) -> Self {
        let gate = Self::default();
        gate.set_max(max);
        gate
    }

    /// Admit at most `max` at once from now on (`0`: no maximum). Those
    /// already inside stay.
    pub fn set_max(&self, max: usize) {
        self.max.store(max, Ordering::Relaxed);
    }

    /// The maximum (`0`: none).
    pub fn max(&self) -> usize {
        self.max.load(Ordering::Relaxed)
    }

    /// How many are inside now.
    pub fn active(&self) -> usize {
        self.active.load(Ordering::Relaxed)
    }

    /// How many were refused since the gate was made.
    pub fn rejected(&self) -> u64 {
        self.rejected.load(Ordering::Relaxed)
    }

    /// Take a slot, or count a refusal and return `None` when the gate is
    /// full.
    pub fn try_enter(self: &Arc<Self>) -> Option<Pass> {
        let max = self.max();
        // Compare-and-swap, so the count never exceeds the maximum, not even
        // for an instant (it is also what `/metrics` reports).
        let mut current = self.active.load(Ordering::Acquire);
        loop {
            if max != 0 && current >= max {
                self.rejected.fetch_add(1, Ordering::Relaxed);
                return None;
            }
            match self.active.compare_exchange_weak(
                current,
                current + 1,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => break,
                Err(seen) => current = seen,
            }
        }
        Some(Pass {
            gate: Arc::clone(self),
        })
    }

    /// `true` at most once a minute: whether a refusal should be logged, so a
    /// flood of refused clients does not flood the log.
    pub fn should_warn(&self) -> bool {
        let now = crate::model::now_ms();
        let last = self.last_warned_ms.load(Ordering::Relaxed);
        now.saturating_sub(last) >= 60_000
            && self
                .last_warned_ms
                .compare_exchange(last, now, Ordering::Relaxed, Ordering::Relaxed)
                .is_ok()
    }
}

/// One admitted connection or request; dropping it frees its slot.
#[derive(Debug)]
pub struct Pass {
    gate: Arc<Gate>,
}

impl Drop for Pass {
    fn drop(&mut self) {
        self.gate.active.fetch_sub(1, Ordering::AcqRel);
    }
}

/// The gates of one server.
#[derive(Debug)]
pub struct ConnectionLimits {
    /// Open Bolt connections (`DREVO_MAX_CONNECTIONS`).
    pub bolt: Arc<Gate>,
    /// HTTP requests being served (`DREVO_HTTP_MAX_IN_FLIGHT`).
    pub http: Arc<Gate>,
}

impl Default for ConnectionLimits {
    fn default() -> Self {
        Self {
            bolt: Arc::new(Gate::new(DEFAULT_MAX_CONNECTIONS)),
            http: Arc::new(Gate::new(DEFAULT_HTTP_MAX_IN_FLIGHT)),
        }
    }
}

/// The message a refused Bolt client gets.
pub fn bolt_refused_message(max: usize) -> String {
    format!(
        "the server already serves {max} Bolt connections (DREVO_MAX_CONNECTIONS); \
         retry later or close idle connections"
    )
}

/// The message a refused HTTP client gets.
pub fn http_refused_message(max: usize) -> String {
    format!("the server already serves {max} requests (DREVO_HTTP_MAX_IN_FLIGHT); retry shortly")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_full_gate_refuses_and_counts() {
        let gate = Arc::new(Gate::new(2));
        let a = gate.try_enter().unwrap();
        let _b = gate.try_enter().unwrap();
        assert_eq!(gate.active(), 2);
        assert!(gate.try_enter().is_none());
        assert!(gate.try_enter().is_none());
        assert_eq!(gate.rejected(), 2);
        assert_eq!(gate.active(), 2, "a refusal leaves the count alone");
        drop(a);
        assert_eq!(gate.active(), 1);
        assert!(gate.try_enter().is_some());
    }

    #[test]
    fn zero_means_no_maximum() {
        let gate = Arc::new(Gate::new(0));
        let passes: Vec<_> = (0..1000).map(|_| gate.try_enter().unwrap()).collect();
        assert_eq!(gate.active(), 1000);
        assert_eq!(gate.rejected(), 0);
        drop(passes);
        assert_eq!(gate.active(), 0);
    }

    #[test]
    fn lowering_the_maximum_keeps_those_inside() {
        let gate = Arc::new(Gate::new(3));
        let held: Vec<_> = (0..3).map(|_| gate.try_enter().unwrap()).collect();
        gate.set_max(1);
        assert_eq!(gate.active(), 3);
        assert!(gate.try_enter().is_none());
        drop(held);
        assert!(gate.try_enter().is_some());
    }

    #[test]
    fn concurrent_entries_never_exceed_the_maximum() {
        let gate = Arc::new(Gate::new(8));
        let peak = Arc::new(AtomicUsize::new(0));
        std::thread::scope(|s| {
            for _ in 0..16 {
                let gate = Arc::clone(&gate);
                let peak = Arc::clone(&peak);
                s.spawn(move || {
                    for _ in 0..2000 {
                        if let Some(pass) = gate.try_enter() {
                            peak.fetch_max(gate.active(), Ordering::Relaxed);
                            drop(pass);
                        }
                    }
                });
            }
        });
        assert!(peak.load(Ordering::Relaxed) <= 8);
        assert_eq!(gate.active(), 0);
    }

    #[test]
    fn warnings_are_rate_limited() {
        let gate = Gate::new(1);
        assert!(gate.should_warn());
        assert!(!gate.should_warn(), "not twice in a minute");
    }

    #[test]
    fn the_defaults_apply() {
        let limits = ConnectionLimits::default();
        assert_eq!(limits.bolt.max(), DEFAULT_MAX_CONNECTIONS);
        assert_eq!(limits.http.max(), DEFAULT_HTTP_MAX_IN_FLIGHT);
        assert!(bolt_refused_message(7).contains("7 Bolt connections"));
        assert!(http_refused_message(3).contains("DREVO_HTTP_MAX_IN_FLIGHT"));
    }
}
