//! Named application events, delivered when the writing transaction commits.
//!
//! `CALL drevo.events.post('invoice_approved', {invoice: $id})` signals
//! something that happened, so subscribers need not work it out from the
//! changed nodes. The event waits with the statement or transaction that
//! posted it and reaches subscribers only once that commits; a failed
//! statement or a rolled-back transaction posts nothing.
//!
//! Each database keeps its latest posted events in memory, numbered in the
//! order they were published, for subscribers to resume from (`GET /events`).
//! They are not stored on disk: a restart forgets them.

use std::cell::RefCell;
use std::collections::VecDeque;
use std::sync::Mutex;

/// An event posted by a statement, waiting for its commit.
#[derive(Debug, Clone, PartialEq)]
pub struct Pending {
    /// What happened, as the application names it.
    pub name: String,
    /// Whatever the application attached; `null` when nothing.
    pub payload: serde_json::Value,
}

/// A published event.
#[derive(Debug, Clone, PartialEq)]
pub struct Posted {
    /// Its number: events are numbered from 1 in the order they were
    /// published.
    pub seq: u64,
    /// The change-feed head when it was published: every change at or
    /// before this cursor committed first.
    pub cursor: u64,
    /// What happened.
    pub name: String,
    /// What the application attached.
    pub payload: serde_json::Value,
}

thread_local! {
    static COLLECTING: RefCell<Option<Vec<Pending>>> = const { RefCell::new(None) };
}

/// Run `work`, keeping the events it posts: the result and the events, in
/// the order they were posted. Nested calls keep their own events.
pub fn collect<R>(work: impl FnOnce() -> R) -> (R, Vec<Pending>) {
    struct Restore(Option<Vec<Pending>>);
    impl Drop for Restore {
        fn drop(&mut self) {
            let outer = self.0.take();
            COLLECTING.with(|c| *c.borrow_mut() = outer);
        }
    }
    let outer = COLLECTING.with(|c| c.borrow_mut().replace(Vec::new()));
    let restore = Restore(outer);
    let result = work();
    let posted = COLLECTING
        .with(|c| c.borrow_mut().take())
        .unwrap_or_default();
    drop(restore);
    (result, posted)
}

/// Post `event` to the innermost [`collect`]; `false`, and nothing kept,
/// when there is none.
pub fn post(event: Pending) -> bool {
    COLLECTING.with(|c| match c.borrow_mut().as_mut() {
        Some(events) => {
            events.push(event);
            true
        }
        None => false,
    })
}

/// What [`EventBus::after`] found.
#[derive(Debug, Clone, PartialEq)]
pub struct After {
    /// The events after the asked-for number, oldest first.
    pub events: Vec<Posted>,
    /// `true` when some events after that number are no longer kept, or the
    /// number was never issued (the server restarted): the subscriber
    /// missed events.
    pub lagged: bool,
    /// The number of the latest published event; 0 before the first.
    pub head: u64,
}

#[derive(Debug, Default)]
struct Inner {
    head: u64,
    events: VecDeque<Posted>,
    /// The cursor of the latest event dropped to keep the bus bounded.
    dropped_cursor: Option<u64>,
}

/// One database's published events.
#[derive(Debug, Default)]
pub struct EventBus {
    inner: Mutex<Inner>,
}

impl EventBus {
    /// An empty bus.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Publish `events` in order, stamped with the change-feed head that
    /// `cursor` reads under the bus lock, so an event's cursor is never
    /// below a change that committed before it. Keeps the latest `keep`.
    pub fn publish(&self, events: Vec<Pending>, cursor: impl FnOnce() -> u64, keep: u64) {
        if events.is_empty() {
            return;
        }
        let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let at = cursor();
        for event in events {
            inner.head += 1;
            let seq = inner.head;
            inner.events.push_back(Posted {
                seq,
                cursor: at,
                name: event.name,
                payload: event.payload,
            });
        }
        let keep = usize::try_from(keep.max(1)).unwrap_or(usize::MAX);
        while inner.events.len() > keep {
            if let Some(gone) = inner.events.pop_front() {
                inner.dropped_cursor = Some(gone.cursor);
            }
        }
    }

    /// The number of the latest published event; 0 before the first.
    #[must_use]
    pub fn head(&self) -> u64 {
        self.inner.lock().unwrap_or_else(|e| e.into_inner()).head
    }

    /// The events numbered after `seq`.
    #[must_use]
    pub fn after(&self, seq: u64) -> After {
        let inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let oldest = inner.events.front().map_or(inner.head + 1, |e| e.seq);
        let lagged = seq > inner.head || seq + 1 < oldest;
        let events = if lagged {
            Vec::new()
        } else {
            inner
                .events
                .iter()
                .filter(|e| e.seq > seq)
                .cloned()
                .collect()
        };
        After {
            events,
            lagged,
            head: inner.head,
        }
    }

    /// Where a subscriber resuming after the change at `cursor` follows the
    /// events from: it was sent the events published before that change
    /// (stamped below `cursor`), not the ones from it on. `None` when some of
    /// those it was not sent are no longer kept.
    #[must_use]
    pub fn seq_before_cursor(&self, cursor: u64) -> Option<u64> {
        let inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        if inner.dropped_cursor.is_some_and(|c| c >= cursor) {
            return None;
        }
        let oldest = inner.events.front().map_or(inner.head, |e| e.seq - 1);
        Some(
            inner
                .events
                .iter()
                .take_while(|e| e.cursor < cursor)
                .last()
                .map_or(oldest, |e| e.seq),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pending(name: &str) -> Pending {
        Pending {
            name: name.to_string(),
            payload: serde_json::json!({ "n": name }),
        }
    }

    #[test]
    fn collect_keeps_what_the_work_posts_in_order() {
        let ((), events) = collect(|| {
            assert!(post(pending("a")));
            assert!(post(pending("b")));
        });
        assert_eq!(events, vec![pending("a"), pending("b")]);
    }

    #[test]
    fn posting_outside_collect_keeps_nothing() {
        assert!(!post(pending("lost")));
        let ((), events) = collect(|| {});
        assert!(events.is_empty());
    }

    #[test]
    fn nested_collects_keep_their_own_events() {
        let (inner, outer) = collect(|| {
            post(pending("outer-1"));
            let ((), inner) = collect(|| {
                post(pending("inner"));
            });
            post(pending("outer-2"));
            inner
        });
        assert_eq!(inner, vec![pending("inner")]);
        assert_eq!(outer, vec![pending("outer-1"), pending("outer-2")]);
    }

    #[test]
    fn a_panic_in_the_work_does_not_leave_collecting_on() {
        let caught = std::panic::catch_unwind(|| {
            collect(|| {
                post(pending("x"));
                panic!("boom");
            })
        });
        assert!(caught.is_err());
        assert!(!post(pending("after")));
    }

    #[test]
    fn published_events_are_numbered_and_stamped_with_the_cursor() {
        let bus = EventBus::new();
        bus.publish(vec![pending("a"), pending("b")], || 7, 100);
        bus.publish(vec![pending("c")], || 9, 100);
        let after = bus.after(0);
        assert!(!after.lagged);
        assert_eq!(after.head, 3);
        let got: Vec<(u64, u64, &str)> = after
            .events
            .iter()
            .map(|e| (e.seq, e.cursor, e.name.as_str()))
            .collect();
        assert_eq!(got, vec![(1, 7, "a"), (2, 7, "b"), (3, 9, "c")]);
        assert_eq!(bus.after(2).events.len(), 1);
        assert!(bus.after(3).events.is_empty());
    }

    #[test]
    fn publishing_nothing_changes_nothing() {
        let bus = EventBus::new();
        bus.publish(Vec::new(), || panic!("not read"), 10);
        assert_eq!(bus.head(), 0);
    }

    #[test]
    fn the_bus_keeps_the_latest_and_reports_older_numbers_as_lagged() {
        let bus = EventBus::new();
        for i in 0..5 {
            bus.publish(vec![pending(&format!("e{i}"))], || i, 3);
        }
        assert_eq!(bus.head(), 5);
        // 1 and 2 are gone; from 2 on nothing is missing.
        assert!(bus.after(0).lagged);
        assert!(bus.after(1).lagged);
        let after = bus.after(2);
        assert!(!after.lagged);
        assert_eq!(
            after.events.iter().map(|e| e.seq).collect::<Vec<_>>(),
            [3, 4, 5]
        );
    }

    #[test]
    fn a_number_never_issued_is_lagged() {
        let bus = EventBus::new();
        assert!(!bus.after(0).lagged);
        assert!(bus.after(4).lagged);
        bus.publish(vec![pending("a")], || 1, 10);
        assert!(bus.after(2).lagged);
        assert_eq!(bus.after(2).head, 1);
    }

    #[test]
    fn seq_before_cursor_finds_where_a_change_cursor_leaves_off() {
        let bus = EventBus::new();
        assert_eq!(bus.seq_before_cursor(5), Some(0));
        bus.publish(vec![pending("a")], || 3, 10);
        bus.publish(vec![pending("b"), pending("c")], || 6, 10);
        // Seen every change before 3: nothing published yet then.
        assert_eq!(bus.seq_before_cursor(3), Some(0));
        // Before 4..=6: "a" (published at 3) came before.
        assert_eq!(bus.seq_before_cursor(4), Some(1));
        assert_eq!(bus.seq_before_cursor(6), Some(1));
        assert_eq!(bus.seq_before_cursor(7), Some(3));
    }

    #[test]
    fn seq_before_cursor_is_none_when_needed_events_were_dropped() {
        let bus = EventBus::new();
        bus.publish(vec![pending("a")], || 3, 1);
        bus.publish(vec![pending("b")], || 6, 1);
        // "a" (cursor 3) was dropped: a subscriber that stopped before 3
        // would miss it.
        assert_eq!(bus.seq_before_cursor(2), None);
        assert_eq!(bus.seq_before_cursor(3), None);
        // After 3 only "b" matters, and it is kept.
        assert_eq!(bus.seq_before_cursor(4), Some(1));
        assert_eq!(bus.seq_before_cursor(7), Some(2));
    }
}
