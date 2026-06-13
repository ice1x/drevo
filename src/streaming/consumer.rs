//! The ingestion engine — poll, decode, apply, commit.
//!
//! [`IngestConsumer`] glues a [`StreamSource`] (the broker) to an
//! [`IngestSink`] (the graph). One [`run_once`](IngestConsumer::run_once) pulls
//! a batch, decodes each payload into an [`IngestEvent`], applies it to the
//! sink, and — on success — commits the batch's high-water offset back to the
//! source. [`run_to_idle`](IngestConsumer::run_to_idle) repeats that until the
//! partition drains.
//!
//! # Error handling
//!
//! Two failure modes can arise per message: a payload that won't decode
//! ([`StreamError::Parse`]) and a well-formed event the sink rejects
//! ([`StreamError::Sink`]). The [`ErrorPolicy`] chooses what happens:
//!
//! * [`ErrorPolicy::Halt`] — stop immediately, returning the error. Offsets up
//!   to (but not including) the bad message are still committed, so a fixed /
//!   skipped message can be resumed past.
//! * [`ErrorPolicy::DeadLetter`] — record the bad message in
//!   [`dead_letters`](IngestConsumer::dead_letters) and carry on. The graph
//!   keeps ingesting good events; an operator inspects the dead-letter queue
//!   out of band. This is the production default for an unattended consumer.
//!
//! # Idempotent resume
//!
//! The consumer tracks a `resume_after` watermark and silently skips any
//! message whose offset is at or below it. Combined with the at-least-once
//! re-delivery of [`StreamSource`], a message processed but not yet committed
//! before a crash is re-delivered and — if it slips past the watermark — re-
//! applied harmlessly, because every [`IngestEvent`] is idempotent.

use crate::streaming::error::{Result, StreamError};
use crate::streaming::event::IngestEvent;
use crate::streaming::sink::IngestSink;
use crate::streaming::source::{Offset, StreamSource};

/// What the consumer does when a message cannot be decoded or applied.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ErrorPolicy {
    /// Stop on the first bad message, returning its [`StreamError`]. Offsets
    /// before the failure are still committed.
    Halt,
    /// Record the bad message in the dead-letter queue and continue. The
    /// production default for an unattended consumer.
    #[default]
    DeadLetter,
}

/// A message that could not be ingested, captured under
/// [`ErrorPolicy::DeadLetter`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeadLetter {
    /// The broker offset of the failed message.
    pub offset: Offset,
    /// The raw, undecoded payload (preserved for replay / inspection).
    pub payload: Vec<u8>,
    /// Why the message failed, rendered to text.
    pub reason: String,
}

/// The outcome of a single [`run_once`](IngestConsumer::run_once) or a full
/// [`run_to_idle`](IngestConsumer::run_to_idle).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct IngestReport {
    /// Messages pulled from the source.
    pub polled: usize,
    /// Events successfully applied to the sink.
    pub applied: usize,
    /// Messages skipped because their offset was at or below the resume
    /// watermark (idempotent re-delivery).
    pub skipped: usize,
    /// Messages routed to the dead-letter queue this call.
    pub dead_lettered: usize,
    /// The highest offset successfully applied (or skipped) so far.
    pub last_offset: Offset,
}

/// Drives a [`StreamSource`] into an [`IngestSink`] under a chosen
/// [`ErrorPolicy`], tracking offsets for at-least-once, idempotent ingestion.
#[derive(Debug)]
pub struct IngestConsumer {
    policy: ErrorPolicy,
    batch_size: usize,
    resume_after: Offset,
    dead_letters: Vec<DeadLetter>,
    applied_total: usize,
}

impl IngestConsumer {
    /// The default batch size used by [`IngestConsumer::new`] when none is
    /// specified.
    pub const DEFAULT_BATCH_SIZE: usize = 256;

    /// Create a consumer with the [`ErrorPolicy::DeadLetter`] default and the
    /// default batch size ([`DEFAULT_BATCH_SIZE`](Self::DEFAULT_BATCH_SIZE)),
    /// resuming from [`Offset::ZERO`].
    #[must_use]
    pub fn new() -> Self {
        Self {
            policy: ErrorPolicy::default(),
            batch_size: Self::DEFAULT_BATCH_SIZE,
            resume_after: Offset::ZERO,
            dead_letters: Vec::new(),
            applied_total: 0,
        }
    }

    /// Set the error policy (builder style).
    #[must_use]
    pub fn with_policy(mut self, policy: ErrorPolicy) -> Self {
        self.policy = policy;
        self
    }

    /// Set the maximum number of messages polled per [`run_once`](Self::run_once)
    /// (builder style). A `0` batch size is clamped to `1` so the consumer
    /// always makes progress.
    #[must_use]
    pub fn with_batch_size(mut self, batch_size: usize) -> Self {
        self.batch_size = batch_size.max(1);
        self
    }

    /// Start ingesting after `offset`, skipping any re-delivered message at or
    /// below it (builder style). Use this to resume a consumer from a source's
    /// [`committed`](StreamSource::committed) mark.
    #[must_use]
    pub fn resume_after(mut self, offset: Offset) -> Self {
        self.resume_after = offset;
        self
    }

    /// The configured error policy.
    #[must_use]
    pub fn policy(&self) -> ErrorPolicy {
        self.policy
    }

    /// The dead-letter queue accumulated across every run under
    /// [`ErrorPolicy::DeadLetter`].
    #[must_use]
    pub fn dead_letters(&self) -> &[DeadLetter] {
        &self.dead_letters
    }

    /// The total number of events successfully applied across every run.
    #[must_use]
    pub fn applied_total(&self) -> usize {
        self.applied_total
    }

    /// The current resume watermark — messages at or below this offset are
    /// skipped as already-applied.
    #[must_use]
    pub fn resume_watermark(&self) -> Offset {
        self.resume_after
    }

    /// Poll one batch and process it, committing the high-water offset on
    /// success.
    ///
    /// Returns the per-call [`IngestReport`]. Under [`ErrorPolicy::Halt`] a bad
    /// message ends the call with the corresponding [`StreamError`] *after*
    /// committing every good offset that preceded it.
    ///
    /// # Errors
    ///
    /// Under [`ErrorPolicy::Halt`], returns [`StreamError::Parse`] for an
    /// undecodable payload or [`StreamError::Sink`] when the sink rejects an
    /// event. Under [`ErrorPolicy::DeadLetter`] this never returns `Err`.
    pub fn run_once<S, K>(&mut self, source: &mut S, sink: &mut K) -> Result<IngestReport>
    where
        S: StreamSource,
        K: IngestSink,
    {
        let batch = source.poll(self.batch_size);
        let mut report = IngestReport {
            polled: batch.len(),
            last_offset: self.resume_after,
            ..IngestReport::default()
        };

        // Highest offset safe to commit: advances only across messages we have
        // fully accounted for (applied, skipped, or dead-lettered).
        let mut commit_through: Option<Offset> = None;

        for msg in batch {
            // Idempotent resume: drop anything already accounted for.
            if msg.offset <= self.resume_after {
                report.skipped += 1;
                commit_through = Some(msg.offset);
                report.last_offset = msg.offset;
                continue;
            }

            match self.process_one(&msg, sink) {
                Ok(()) => {
                    self.resume_after = msg.offset;
                    report.applied += 1;
                    self.applied_total += 1;
                    commit_through = Some(msg.offset);
                    report.last_offset = msg.offset;
                }
                Err(err) => match self.policy {
                    ErrorPolicy::Halt => {
                        if let Some(through) = commit_through {
                            source.commit(through);
                        }
                        return Err(err);
                    }
                    ErrorPolicy::DeadLetter => {
                        self.dead_letters.push(DeadLetter {
                            offset: msg.offset,
                            payload: msg.payload,
                            reason: err.to_string(),
                        });
                        // The bad message is accounted for: advance past it so a
                        // resume does not re-deliver it forever.
                        self.resume_after = msg.offset;
                        report.dead_lettered += 1;
                        commit_through = Some(msg.offset);
                        report.last_offset = msg.offset;
                    }
                },
            }
        }

        if let Some(through) = commit_through {
            source.commit(through);
        }
        Ok(report)
    }

    /// Repeatedly [`run_once`](Self::run_once) until the source yields an empty
    /// batch, aggregating the per-call reports.
    ///
    /// # Errors
    ///
    /// Propagates the first [`StreamError`] under [`ErrorPolicy::Halt`]; never
    /// errors under [`ErrorPolicy::DeadLetter`].
    pub fn run_to_idle<S, K>(&mut self, source: &mut S, sink: &mut K) -> Result<IngestReport>
    where
        S: StreamSource,
        K: IngestSink,
    {
        let mut total = IngestReport {
            last_offset: self.resume_after,
            ..IngestReport::default()
        };
        loop {
            let report = self.run_once(source, sink)?;
            if report.polled == 0 {
                break;
            }
            total.polled += report.polled;
            total.applied += report.applied;
            total.skipped += report.skipped;
            total.dead_lettered += report.dead_lettered;
            total.last_offset = report.last_offset;
        }
        Ok(total)
    }

    /// Decode and apply a single message, mapping failures into [`StreamError`].
    fn process_one<K: IngestSink>(
        &self,
        msg: &crate::streaming::source::StreamMessage,
        sink: &mut K,
    ) -> Result<()> {
        let event = IngestEvent::from_json(&msg.payload).map_err(|e| StreamError::Parse {
            offset: msg.offset,
            reason: e.to_string(),
        })?;
        sink.apply(&event).map_err(|reason| StreamError::Sink {
            offset: msg.offset,
            reason,
        })
    }
}

impl Default for IngestConsumer {
    /// Equivalent to [`IngestConsumer::new`] — the [`ErrorPolicy::DeadLetter`]
    /// default with the default batch size. (A derived `Default` would set a
    /// `0` batch size and silently stall, so it is delegated to `new`.)
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::streaming::event::EventProperties;
    use crate::streaming::sink::MemoryGraphSink;
    use crate::streaming::source::MemorySource;

    fn node_event(key: &str, title: &str) -> IngestEvent {
        IngestEvent::UpsertNode {
            key: key.into(),
            kind: "note".into(),
            title: title.into(),
            body: String::new(),
            properties: EventProperties::new(),
        }
    }

    #[test]
    fn run_to_idle_applies_every_event_and_commits() {
        let mut source = MemorySource::new();
        for i in 0..3 {
            source
                .push_event(&node_event(&format!("n{i}"), &format!("T{i}")))
                .unwrap();
        }
        let mut sink = MemoryGraphSink::new();
        let mut consumer = IngestConsumer::new();

        let report = consumer.run_to_idle(&mut source, &mut sink).unwrap();
        assert_eq!(report.applied, 3);
        assert_eq!(report.polled, 3);
        assert_eq!(sink.node_count(), 3);
        assert_eq!(source.committed(), Offset(3));
        assert_eq!(consumer.resume_watermark(), Offset(3));
    }

    #[test]
    fn batching_processes_in_multiple_rounds() {
        let mut source = MemorySource::new();
        for i in 0..5 {
            source
                .push_event(&node_event(&format!("n{i}"), &format!("T{i}")))
                .unwrap();
        }
        let mut sink = MemoryGraphSink::new();
        let mut consumer = IngestConsumer::new().with_batch_size(2);

        let first = consumer.run_once(&mut source, &mut sink).unwrap();
        assert_eq!(first.applied, 2);
        assert_eq!(source.committed(), Offset(2));

        let total = consumer.run_to_idle(&mut source, &mut sink).unwrap();
        assert_eq!(total.applied, 3); // remaining
        assert_eq!(sink.node_count(), 5);
        assert_eq!(source.committed(), Offset(5));
    }

    #[test]
    fn dead_letter_policy_skips_bad_payloads_and_keeps_going() {
        let mut source = MemorySource::new();
        source.push_event(&node_event("good1", "A")).unwrap();
        source.push(b"this is not json".to_vec());
        source.push_event(&node_event("good2", "B")).unwrap();
        let mut sink = MemoryGraphSink::new();
        let mut consumer = IngestConsumer::new(); // DeadLetter default

        let report = consumer.run_to_idle(&mut source, &mut sink).unwrap();
        assert_eq!(report.applied, 2);
        assert_eq!(report.dead_lettered, 1);
        assert_eq!(sink.node_count(), 2);
        assert_eq!(consumer.dead_letters().len(), 1);
        assert_eq!(consumer.dead_letters()[0].offset, Offset(2));
        // Even the bad offset is committed so we resume past it.
        assert_eq!(source.committed(), Offset(3));
    }

    #[test]
    fn dead_letter_captures_sink_rejections() {
        let mut source = MemorySource::new();
        source.push_event(&node_event("ok", "A")).unwrap();
        source.push_event(&node_event("bad", "B")).unwrap();
        let mut sink = MemoryGraphSink::new().reject_keys(["bad"]);
        let mut consumer = IngestConsumer::new();

        let report = consumer.run_to_idle(&mut source, &mut sink).unwrap();
        assert_eq!(report.applied, 1);
        assert_eq!(report.dead_lettered, 1);
        assert!(consumer.dead_letters()[0].reason.contains("bad"));
    }

    #[test]
    fn halt_policy_stops_on_first_bad_message() {
        let mut source = MemorySource::new();
        source.push_event(&node_event("ok", "A")).unwrap();
        source.push(b"garbage".to_vec());
        source.push_event(&node_event("never", "C")).unwrap();
        let mut sink = MemoryGraphSink::new();
        let mut consumer = IngestConsumer::new().with_policy(ErrorPolicy::Halt);

        let err = consumer.run_once(&mut source, &mut sink).unwrap_err();
        assert!(matches!(err, StreamError::Parse { offset, .. } if offset == Offset(2)));
        // The good message before the failure was applied and committed.
        assert_eq!(sink.node_count(), 1);
        assert_eq!(source.committed(), Offset(1));
    }

    #[test]
    fn idempotent_replay_after_crash_does_not_double_apply() {
        let mut source = MemorySource::new();
        for i in 0..4 {
            source
                .push_event(&node_event(&format!("n{i}"), &format!("T{i}")))
                .unwrap();
        }
        let mut sink = MemoryGraphSink::new();

        // First consumer processes everything but only commits the first two
        // (simulate a crash mid-batch: it committed offset 2, then died).
        let mut c1 = IngestConsumer::new().with_batch_size(2);
        c1.run_once(&mut source, &mut sink).unwrap();
        assert_eq!(source.committed(), Offset(2));

        // Restart: a fresh consumer resumes from the committed mark, and the
        // source re-delivers everything after it.
        source.rewind_to_committed();
        let mut c2 = IngestConsumer::new().resume_after(source.committed());
        let report = c2.run_to_idle(&mut source, &mut sink).unwrap();

        // n0..n3 all present exactly once; nothing double-counted.
        assert_eq!(sink.node_count(), 4);
        assert_eq!(report.applied, 2); // only n2, n3 were new past the watermark
        assert_eq!(source.committed(), Offset(4));
    }

    #[test]
    fn resume_after_skips_redelivered_messages() {
        let mut source = MemorySource::new();
        source.push_event(&node_event("n1", "A")).unwrap();
        source.push_event(&node_event("n2", "B")).unwrap();
        let mut sink = MemoryGraphSink::new();
        let mut consumer = IngestConsumer::new().resume_after(Offset(1));

        let report = consumer.run_to_idle(&mut source, &mut sink).unwrap();
        assert_eq!(report.skipped, 1);
        assert_eq!(report.applied, 1);
        assert_eq!(sink.node_count(), 1);
        assert!(sink.node("n2").is_some());
    }

    #[test]
    fn empty_source_is_a_clean_noop() {
        let mut source = MemorySource::new();
        let mut sink = MemoryGraphSink::new();
        let mut consumer = IngestConsumer::new();
        let report = consumer.run_to_idle(&mut source, &mut sink).unwrap();
        assert_eq!(report, IngestReport::default());
    }
}
