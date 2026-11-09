//! The progress of ingest jobs as `ingest` events, at most one per second
//! per job.
//!
//! [`Coalescer::offer`] is told every job-level change. The first change of
//! a job goes out at once; changes within a second of the last one sent
//! wait, the newest replacing older ones, until [`Coalescer::due`] releases
//! them. `done` never goes down in what is sent, even when two changes are
//! offered out of order.

// Only the scheduler, which wasm builds leave out, uses some of this.
#![cfg_attr(target_arch = "wasm32", allow(dead_code))]

use std::collections::HashMap;
use std::time::{Duration, Instant};

/// The shortest time between two events of one job.
pub(crate) const EVERY: Duration = Duration::from_secs(1);

/// One job's progress, as an `ingest` event carries it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Progress {
    /// The job's id.
    pub(crate) job_id: String,
    /// Its state name.
    pub(crate) state: String,
    /// Items done.
    pub(crate) done: u64,
    /// Items dead.
    pub(crate) failed: u64,
    /// Items in all.
    pub(crate) total: u64,
}

impl Progress {
    /// Read a payload [`Progress::payload`] wrote; `None` when it is not one.
    pub(crate) fn from_payload(payload: &serde_json::Value) -> Option<Self> {
        let count = |key: &str| payload.get(key).and_then(serde_json::Value::as_u64);
        Some(Self {
            job_id: payload.get("jobId")?.as_str()?.to_string(),
            state: payload.get("state")?.as_str()?.to_string(),
            done: count("done")?,
            failed: count("failed")?,
            total: count("total")?,
        })
    }

    /// The event's JSON payload.
    pub(crate) fn payload(&self) -> serde_json::Value {
        serde_json::json!({
            "jobId": self.job_id,
            "state": self.state,
            "done": self.done,
            "failed": self.failed,
            "total": self.total,
        })
    }
}

/// Holds back job changes so each job sends at most one event a second.
#[derive(Debug, Default)]
pub(crate) struct Coalescer {
    /// When each job's last event went out, and its `done`.
    sent: HashMap<String, (Instant, u64)>,
    /// The newest change of each job that has not gone out.
    waiting: HashMap<String, Progress>,
}

impl Coalescer {
    /// Take in a change of a job at `now`: the event to send now, or `None`
    /// when it has to wait.
    pub(crate) fn offer(&mut self, progress: Progress, now: Instant) -> Option<Progress> {
        match self.sent.get(&progress.job_id) {
            Some((at, _)) if now.saturating_duration_since(*at) < EVERY => {
                self.waiting.insert(progress.job_id.clone(), progress);
                None
            }
            _ => {
                self.waiting.remove(&progress.job_id);
                Some(self.send(progress, now))
            }
        }
    }

    /// Record `progress` as sent at `now`, `done` raised to what was sent.
    fn send(&mut self, mut progress: Progress, now: Instant) -> Progress {
        if let Some((_, done)) = self.sent.get(&progress.job_id) {
            progress.done = progress.done.max(*done);
        }
        self.sent
            .insert(progress.job_id.clone(), (now, progress.done));
        progress
    }

    /// The waiting changes whose second is up at `now`, to send.
    pub(crate) fn due(&mut self, now: Instant) -> Vec<Progress> {
        let ready: Vec<String> = self
            .waiting
            .keys()
            .filter(|job| {
                self.sent
                    .get(*job)
                    .is_none_or(|(at, _)| now.saturating_duration_since(*at) >= EVERY)
            })
            .cloned()
            .collect();
        let mut out = Vec::with_capacity(ready.len());
        for job in ready {
            if let Some(progress) = self.waiting.remove(&job) {
                out.push(self.send(progress, now));
            }
        }
        out
    }

    /// When the next waiting change is due; `None` when none waits.
    pub(crate) fn next_due(&self) -> Option<Instant> {
        self.waiting
            .keys()
            .filter_map(|job| self.sent.get(job).map(|(at, _)| *at + EVERY))
            .min()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(job: &str, state: &str, done: u64) -> Progress {
        Progress {
            job_id: job.into(),
            state: state.into(),
            done,
            failed: 0,
            total: 10,
        }
    }

    #[test]
    fn the_first_change_goes_out_at_once() {
        let mut c = Coalescer::default();
        let t0 = Instant::now();
        assert_eq!(
            c.offer(p("a", "running", 1), t0),
            Some(p("a", "running", 1))
        );
        assert_eq!(c.next_due(), None);
    }

    #[test]
    fn changes_within_a_second_wait_and_the_newest_wins() {
        let mut c = Coalescer::default();
        let t0 = Instant::now();
        c.offer(p("a", "running", 1), t0);
        assert_eq!(
            c.offer(p("a", "running", 2), t0 + Duration::from_millis(100)),
            None
        );
        assert_eq!(
            c.offer(p("a", "running", 3), t0 + Duration::from_millis(300)),
            None
        );
        assert_eq!(c.next_due(), Some(t0 + EVERY));
        assert!(c.due(t0 + Duration::from_millis(999)).is_empty());
        assert_eq!(c.due(t0 + EVERY), vec![p("a", "running", 3)]);
        assert_eq!(c.next_due(), None);
        assert!(c.due(t0 + Duration::from_secs(5)).is_empty());
        // The next change waits for a second after that one.
        assert_eq!(
            c.offer(p("a", "done", 10), t0 + Duration::from_millis(1500)),
            None
        );
        assert_eq!(c.next_due(), Some(t0 + EVERY + EVERY));
        assert_eq!(c.due(t0 + Duration::from_secs(2)), vec![p("a", "done", 10)]);
    }

    #[test]
    fn jobs_are_held_back_separately() {
        let mut c = Coalescer::default();
        let t0 = Instant::now();
        assert!(c.offer(p("a", "running", 1), t0).is_some());
        assert!(c.offer(p("b", "running", 1), t0).is_some());
        assert!(c.offer(p("a", "running", 2), t0).is_none());
        let later = t0 + Duration::from_millis(1200);
        assert_eq!(
            c.offer(p("b", "running", 2), later),
            Some(p("b", "running", 2))
        );
        assert_eq!(c.due(later), vec![p("a", "running", 2)]);
    }

    #[test]
    fn done_never_goes_down() {
        let mut c = Coalescer::default();
        let t0 = Instant::now();
        c.offer(p("a", "running", 5), t0);
        let late = t0 + Duration::from_secs(2);
        let sent = c.offer(p("a", "paused", 4), late).expect("due");
        assert_eq!(sent.done, 5);
        assert_eq!(sent.state, "paused");
    }

    #[test]
    fn a_payload_reads_back() {
        let progress = p("job-1", "paused", 3);
        assert_eq!(Progress::from_payload(&progress.payload()), Some(progress));
        assert_eq!(
            Progress::from_payload(&serde_json::json!({"jobId": "x"})),
            None
        );
        assert_eq!(Progress::from_payload(&serde_json::json!("ingest")), None);
    }

    #[test]
    fn the_payload_names_the_fields() {
        assert_eq!(
            p("job-1", "running", 2).payload(),
            serde_json::json!({"jobId": "job-1", "state": "running", "done": 2, "failed": 0, "total": 10})
        );
    }
}
