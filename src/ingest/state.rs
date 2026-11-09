//! The states an ingest job and its items go through, and which moves
//! between them are allowed.
//!
//! A job is `queued` until its first item starts, then `running`; `pause`
//! and `resume` stop and restart the picking of new items; it ends `done`
//! when every item is done, `failed` when the items left are all `dead`,
//! or `cancelled`. `retryFailed` brings a failed job back to `running`.
//!
//! An item is `queued`, `running` while a worker has it, `done`, `failed`
//! while it waits to be tried again, or `dead` once it has used its
//! attempts or failed in a way another attempt cannot fix. Anything else is
//! refused with a message naming both states.

// Only the scheduler, which wasm builds leave out, uses some of this.
#![cfg_attr(target_arch = "wasm32", allow(dead_code))]

/// Where an ingest job is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum JobState {
    Queued,
    Running,
    Paused,
    Done,
    Failed,
    Cancelled,
}

/// Where an ingest item is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ItemState {
    Queued,
    Running,
    Done,
    Failed,
    Dead,
}

/// What happens to a job.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum JobAction {
    /// A worker starts its first item.
    Start,
    /// `drevo.ingest.pause`.
    Pause,
    /// `drevo.ingest.resume`; `started` when an item has run before.
    Resume { started: bool },
    /// `drevo.ingest.cancel`.
    Cancel,
    /// No item is left to run; `dead` when some of them are dead.
    Finish { dead: bool },
    /// `drevo.ingest.retryFailed` requeued `requeued` dead items.
    RetryFailed { requeued: bool },
}

/// What happens to an item.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ItemAction {
    /// A worker takes it.
    Pick,
    /// Its extraction was written.
    Succeed,
    /// Its attempt failed; `retry` when another attempt is allowed.
    Fail { retry: bool },
    /// The database reopened while it was running.
    Recover,
    /// `drevo.ingest.retryFailed`.
    Requeue,
}

impl JobState {
    /// The name stored on the `:IngestJob` node.
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Queued => "queued",
            Self::Running => "running",
            Self::Paused => "paused",
            Self::Done => "done",
            Self::Failed => "failed",
            Self::Cancelled => "cancelled",
        }
    }

    /// The state named `name`.
    pub(crate) fn parse(name: &str) -> Option<Self> {
        [
            Self::Queued,
            Self::Running,
            Self::Paused,
            Self::Done,
            Self::Failed,
            Self::Cancelled,
        ]
        .into_iter()
        .find(|s| s.as_str() == name)
    }

    /// Whether the job has ended: done, failed or cancelled.
    pub(crate) fn finished(self) -> bool {
        matches!(self, Self::Done | Self::Failed | Self::Cancelled)
    }
}

impl ItemState {
    /// The name stored on the `:IngestItem` node.
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Queued => "queued",
            Self::Running => "running",
            Self::Done => "done",
            Self::Failed => "failed",
            Self::Dead => "dead",
        }
    }

    /// The state named `name`.
    pub(crate) fn parse(name: &str) -> Option<Self> {
        [
            Self::Queued,
            Self::Running,
            Self::Done,
            Self::Failed,
            Self::Dead,
        ]
        .into_iter()
        .find(|s| s.as_str() == name)
    }
}

/// The state `action` moves a job in `from` to.
///
/// # Errors
///
/// A message naming the action and the state when the move is not allowed.
pub(crate) fn job(from: JobState, action: JobAction) -> Result<JobState, String> {
    use JobAction as A;
    use JobState as J;
    let state = from.as_str();
    match (from, action) {
        (J::Queued | J::Running, A::Start) => Ok(J::Running),
        (_, A::Start) => Err(format!("cannot start a job that is {state}")),
        (J::Queued | J::Running, A::Pause) => Ok(J::Paused),
        (J::Paused, A::Pause) => Err("the job is already paused".into()),
        (_, A::Pause) => Err(format!("cannot pause a job that is {state}")),
        (J::Paused, A::Resume { started }) => Ok(if started { J::Running } else { J::Queued }),
        (_, A::Resume { .. }) => Err(format!(
            "cannot resume a job that is {state}: it is not paused"
        )),
        (J::Queued | J::Running | J::Paused | J::Failed, A::Cancel) => Ok(J::Cancelled),
        (_, A::Cancel) => Err(format!("cannot cancel a job that is {state}")),
        (J::Running, A::Finish { dead }) => Ok(if dead { J::Failed } else { J::Done }),
        (_, A::Finish { .. }) => Err(format!("cannot finish a job that is {state}")),
        (J::Cancelled, A::RetryFailed { .. }) => Err(format!("cannot retry a job that is {state}")),
        (J::Failed | J::Done, A::RetryFailed { requeued: true }) => Ok(J::Running),
        (_, A::RetryFailed { .. }) => Ok(from),
    }
}

/// The state `action` moves an item in `from` to.
///
/// # Errors
///
/// A message naming the action and the state when the move is not allowed.
pub(crate) fn item(from: ItemState, action: ItemAction) -> Result<ItemState, String> {
    use ItemAction as A;
    use ItemState as I;
    let state = from.as_str();
    match (from, action) {
        (I::Queued | I::Failed, A::Pick) => Ok(I::Running),
        (_, A::Pick) => Err(format!("cannot pick an item that is {state}")),
        (I::Running, A::Succeed) => Ok(I::Done),
        (I::Running, A::Fail { retry }) => Ok(if retry { I::Failed } else { I::Dead }),
        (I::Running, A::Recover) => Ok(I::Queued),
        (_, A::Succeed) => Err(format!(
            "cannot complete an item that is {state}: it is not running"
        )),
        (_, A::Fail { .. }) => Err(format!(
            "cannot fail an item that is {state}: it is not running"
        )),
        (_, A::Recover) => Err(format!(
            "cannot recover an item that is {state}: it is not running"
        )),
        (I::Dead, A::Requeue) => Ok(I::Queued),
        (_, A::Requeue) => Err(format!(
            "cannot requeue an item that is {state}: it is not dead"
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ItemState as I;
    use JobState as J;

    const JOBS: [JobState; 6] = [
        J::Queued,
        J::Running,
        J::Paused,
        J::Done,
        J::Failed,
        J::Cancelled,
    ];
    const ITEMS: [ItemState; 5] = [I::Queued, I::Running, I::Done, I::Failed, I::Dead];

    #[test]
    fn names_round_trip() {
        for s in JOBS {
            assert_eq!(JobState::parse(s.as_str()), Some(s));
        }
        for s in ITEMS {
            assert_eq!(ItemState::parse(s.as_str()), Some(s));
        }
        assert_eq!(JobState::parse("sleeping"), None);
        assert_eq!(ItemState::parse("Done"), None);
    }

    #[test]
    fn only_done_failed_and_cancelled_are_finished() {
        let finished: Vec<JobState> = JOBS.into_iter().filter(|s| s.finished()).collect();
        assert_eq!(finished, [J::Done, J::Failed, J::Cancelled]);
    }

    /// Every job move: `(from, action, Ok(to) | Err(message))`. A move not
    /// listed is checked to be refused below.
    fn job_table() -> Vec<(JobState, JobAction, Result<JobState, &'static str>)> {
        use JobAction as A;
        vec![
            (J::Queued, A::Start, Ok(J::Running)),
            (J::Running, A::Start, Ok(J::Running)),
            (J::Queued, A::Pause, Ok(J::Paused)),
            (J::Running, A::Pause, Ok(J::Paused)),
            (J::Paused, A::Pause, Err("the job is already paused")),
            (J::Done, A::Pause, Err("cannot pause a job that is done")),
            (
                J::Failed,
                A::Pause,
                Err("cannot pause a job that is failed"),
            ),
            (
                J::Cancelled,
                A::Pause,
                Err("cannot pause a job that is cancelled"),
            ),
            (J::Paused, A::Resume { started: false }, Ok(J::Queued)),
            (J::Paused, A::Resume { started: true }, Ok(J::Running)),
            (
                J::Running,
                A::Resume { started: true },
                Err("cannot resume a job that is running: it is not paused"),
            ),
            (
                J::Queued,
                A::Resume { started: false },
                Err("cannot resume a job that is queued: it is not paused"),
            ),
            (
                J::Done,
                A::Resume { started: true },
                Err("cannot resume a job that is done: it is not paused"),
            ),
            (J::Queued, A::Cancel, Ok(J::Cancelled)),
            (J::Running, A::Cancel, Ok(J::Cancelled)),
            (J::Paused, A::Cancel, Ok(J::Cancelled)),
            (J::Failed, A::Cancel, Ok(J::Cancelled)),
            (J::Done, A::Cancel, Err("cannot cancel a job that is done")),
            (
                J::Cancelled,
                A::Cancel,
                Err("cannot cancel a job that is cancelled"),
            ),
            (J::Running, A::Finish { dead: false }, Ok(J::Done)),
            (J::Running, A::Finish { dead: true }, Ok(J::Failed)),
            (J::Failed, A::RetryFailed { requeued: true }, Ok(J::Running)),
            (J::Failed, A::RetryFailed { requeued: false }, Ok(J::Failed)),
            (J::Done, A::RetryFailed { requeued: true }, Ok(J::Running)),
            (
                J::Running,
                A::RetryFailed { requeued: true },
                Ok(J::Running),
            ),
            (
                J::Running,
                A::RetryFailed { requeued: false },
                Ok(J::Running),
            ),
            (J::Queued, A::RetryFailed { requeued: true }, Ok(J::Queued)),
            (J::Queued, A::RetryFailed { requeued: false }, Ok(J::Queued)),
            (J::Paused, A::RetryFailed { requeued: true }, Ok(J::Paused)),
            (J::Paused, A::RetryFailed { requeued: false }, Ok(J::Paused)),
            (J::Done, A::RetryFailed { requeued: false }, Ok(J::Done)),
            (
                J::Cancelled,
                A::RetryFailed { requeued: false },
                Err("cannot retry a job that is cancelled"),
            ),
            (
                J::Cancelled,
                A::RetryFailed { requeued: true },
                Err("cannot retry a job that is cancelled"),
            ),
        ]
    }

    fn all_job_actions() -> Vec<JobAction> {
        use JobAction as A;
        vec![
            A::Start,
            A::Pause,
            A::Resume { started: false },
            A::Resume { started: true },
            A::Cancel,
            A::Finish { dead: false },
            A::Finish { dead: true },
            A::RetryFailed { requeued: false },
            A::RetryFailed { requeued: true },
        ]
    }

    #[test]
    fn job_moves_follow_the_table() {
        for (from, action, want) in job_table() {
            let got = job(from, action);
            assert_eq!(got, want.map_err(str::to_string), "{from:?} {action:?}");
        }
    }

    #[test]
    fn job_moves_not_in_the_table_are_refused() {
        let table = job_table();
        for from in JOBS {
            for action in all_job_actions() {
                if table.iter().any(|(f, a, _)| *f == from && *a == action) {
                    continue;
                }
                let err = job(from, action).expect_err(&format!("{from:?} {action:?}"));
                assert!(err.contains(from.as_str()), "{from:?} {action:?}: {err}");
            }
        }
    }

    #[test]
    fn a_finished_job_cannot_start_or_finish_again() {
        use JobAction as A;
        assert_eq!(
            job(J::Done, A::Start),
            Err("cannot start a job that is done".to_string())
        );
        assert_eq!(
            job(J::Paused, A::Start),
            Err("cannot start a job that is paused".to_string())
        );
        assert_eq!(
            job(J::Paused, A::Finish { dead: false }),
            Err("cannot finish a job that is paused".to_string())
        );
        assert_eq!(
            job(J::Cancelled, A::Finish { dead: true }),
            Err("cannot finish a job that is cancelled".to_string())
        );
    }

    fn item_table() -> Vec<(ItemState, ItemAction, ItemState)> {
        use ItemAction as A;
        vec![
            (I::Queued, A::Pick, I::Running),
            (I::Failed, A::Pick, I::Running),
            (I::Running, A::Succeed, I::Done),
            (I::Running, A::Fail { retry: true }, I::Failed),
            (I::Running, A::Fail { retry: false }, I::Dead),
            (I::Running, A::Recover, I::Queued),
            (I::Dead, A::Requeue, I::Queued),
        ]
    }

    #[test]
    fn item_moves_follow_the_table_and_nothing_else_is_allowed() {
        use ItemAction as A;
        let actions = [
            A::Pick,
            A::Succeed,
            A::Fail { retry: true },
            A::Fail { retry: false },
            A::Recover,
            A::Requeue,
        ];
        let table = item_table();
        for from in ITEMS {
            for action in actions {
                let got = item(from, action);
                match table.iter().find(|(f, a, _)| *f == from && *a == action) {
                    Some((_, _, to)) => assert_eq!(got, Ok(*to), "{from:?} {action:?}"),
                    None => {
                        let err = got.expect_err(&format!("{from:?} {action:?}"));
                        assert!(err.contains(from.as_str()), "{err}");
                    }
                }
            }
        }
    }

    #[test]
    fn item_refusals_name_the_move() {
        use ItemAction as A;
        assert_eq!(
            item(I::Done, A::Pick),
            Err("cannot pick an item that is done".to_string())
        );
        assert_eq!(
            item(I::Queued, A::Requeue),
            Err("cannot requeue an item that is queued: it is not dead".to_string())
        );
        assert_eq!(
            item(I::Dead, A::Succeed),
            Err("cannot complete an item that is dead: it is not running".to_string())
        );
        assert_eq!(
            item(I::Queued, A::Fail { retry: true }),
            Err("cannot fail an item that is queued: it is not running".to_string())
        );
        assert_eq!(
            item(I::Done, A::Recover),
            Err("cannot recover an item that is done: it is not running".to_string())
        );
    }
}
