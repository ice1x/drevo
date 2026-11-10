//! The LLM summariser: a community's extractive view — its best-ranked
//! members, keywords and heaviest relationship types — handed to the chat
//! model `drevo.extract.*` uses, which answers with a prose summary within
//! the token budget.
//!
//! The model is never asked while a write is under way. A statement that
//! opens with `drevo.community.summarize` runs in three phases:
//!
//! 1. **Plan** — the statement runs read-only under the statement timeout
//!    ([`planning`]): the graph is partitioned and ranked, and each
//!    community's request is recorded instead of sent ([`Ask`]).
//! 2. **Ask** — outside any transaction, lock or statement timeout, one
//!    model call per planned community ([`ask_planned`]), each bounded by
//!    the endpoint's own timeout (`DREVO_EXTRACT_TIMEOUT_MS`). The answers
//!    are remembered by the community's member set and request.
//! 3. **Write** — the statement runs as its transaction with the answers
//!    ([`with_prepared`]): a retry after a commit conflict reads them again
//!    instead of asking. A community the plan did not see (the graph changed
//!    in between) is reported ([`with_prepared`] returns `true`) so the
//!    caller plans again.
//!
//! A background community job runs the same phases, the write under the
//! scheduler's gate. Also here: the process-wide limit on synchronous LLM
//! summaries (`DREVO_COMMUNITY_MAX_LLM_SYNC`), which does not apply inside a
//! job ([`as_job`]).

use std::cell::{Cell, RefCell};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use super::extractive::ExtractiveSummarizer;
use super::{CommunitySummarizer, SummaryInput, SummaryMode};
use crate::llm::{ChatCompleter, ChatMessage, ChatRequest};
use crate::rag::serialize::cut;
use crate::rag::tokens::Tokenizer;

/// How many communities one synchronous `mode: 'llm'` call may summarise
/// unless `DREVO_COMMUNITY_MAX_LLM_SYNC` says otherwise.
pub(crate) const DEFAULT_MAX_LLM_SYNC: usize = 20;

static MAX_LLM_SYNC: AtomicUsize = AtomicUsize::new(DEFAULT_MAX_LLM_SYNC);

/// The most communities a synchronous `mode: 'llm'` call may summarise in
/// this process.
pub(crate) fn max_llm_sync() -> usize {
    MAX_LLM_SYNC.load(Ordering::Relaxed)
}

/// Hold synchronous `mode: 'llm'` calls in this process to `max`
/// communities from now on; 0 counts as 1. The server sets it at start.
#[cfg_attr(not(feature = "http"), allow(dead_code))]
pub(crate) fn set_max_llm_sync(max: usize) {
    MAX_LLM_SYNC.store(max.max(1), Ordering::Relaxed);
}

/// One planned model call: the community's member set
/// (`member_ids_hash`) and the request for its summary.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Ask {
    /// The member-set hash of the community.
    pub(crate) members: String,
    /// The request, which carries the view, the budget and the model.
    pub(crate) request: ChatRequest,
    /// The most tokens the summary may take.
    pub(crate) token_budget: usize,
}

/// The answers of the model by [`Ask`].
pub(crate) type Answers = Vec<(Ask, String)>;

/// What a planning run found.
#[derive(Debug, Default)]
struct Plan {
    asks: Vec<Ask>,
    /// `Some(true)`: the call needs the model for `asks`; `Some(false)`: it
    /// needs no model (extractive, async); `None`: it did not get that far.
    outcome: Option<bool>,
}

thread_local! {
    /// Whether this thread runs a background community job.
    static IN_JOB: Cell<bool> = const { Cell::new(false) };
    /// The plan being recorded on this thread (phase 1).
    static PLAN: RefCell<Option<Plan>> = const { RefCell::new(None) };
    /// The answers the write reads (phase 3).
    static PREPARED: RefCell<Option<Answers>> = const { RefCell::new(None) };
    /// Whether the write met a community without an answer.
    static UNPREPARED: Cell<bool> = const { Cell::new(false) };
}

/// Run `work` as a background community job on this thread: the limit on
/// synchronous LLM summaries does not apply. Used by the ingest scheduler,
/// which wasm builds do not have.
#[cfg_attr(target_arch = "wasm32", allow(dead_code))]
pub(crate) fn as_job<R>(work: impl FnOnce() -> R) -> R {
    struct Restore(bool);
    impl Drop for Restore {
        fn drop(&mut self) {
            IN_JOB.with(|j| j.set(self.0));
        }
    }
    let _restore = Restore(IN_JOB.with(|j| j.replace(true)));
    work()
}

/// Whether this thread runs a background community job.
pub(crate) fn in_job() -> bool {
    IN_JOB.with(Cell::get)
}

/// Run `work` recording a plan (phase 1): the summariser records each
/// request instead of asking. Returns `work`'s result and, when the call got
/// as far as deciding, whether it needs the model and for what.
pub(crate) fn planning<R>(work: impl FnOnce() -> R) -> (R, Option<Option<Vec<Ask>>>) {
    struct Restore(Option<Plan>);
    impl Drop for Restore {
        fn drop(&mut self) {
            let outer = self.0.take();
            PLAN.with(|p| *p.borrow_mut() = outer);
        }
    }
    let restore = Restore(PLAN.with(|p| p.borrow_mut().replace(Plan::default())));
    let out = work();
    let plan = PLAN.with(|p| p.borrow_mut().take()).unwrap_or_default();
    drop(restore);
    let found = plan.outcome.map(|needs| needs.then_some(plan.asks));
    (out, found)
}

/// Whether this thread records a plan.
pub(crate) fn is_planning() -> bool {
    PLAN.with(|p| p.borrow().is_some())
}

/// End the plan being recorded: the call needs the model (`needs_model`)
/// for the requests recorded so far, or no model at all.
pub(crate) fn finish_plan(needs_model: bool) {
    PLAN.with(|p| {
        if let Some(plan) = p.borrow_mut().as_mut() {
            plan.outcome = Some(needs_model);
        }
    });
}

/// Run `work` (phase 3) with `answers` as the model's summaries. Also
/// returns whether a community without an answer was met.
pub(crate) fn with_prepared<R>(answers: &Answers, work: impl FnOnce() -> R) -> (R, bool) {
    struct Restore(Option<Answers>, bool);
    impl Drop for Restore {
        fn drop(&mut self) {
            let outer = self.0.take();
            PREPARED.with(|p| *p.borrow_mut() = outer);
            UNPREPARED.with(|u| u.set(self.1));
        }
    }
    let restore = Restore(
        PREPARED.with(|p| p.borrow_mut().replace(answers.clone())),
        UNPREPARED.with(|u| u.replace(false)),
    );
    let out = work();
    let missed = UNPREPARED.with(Cell::get);
    drop(restore);
    (out, missed)
}

/// Whether answers are in place on this thread ([`with_prepared`]).
pub(crate) fn is_prepared() -> bool {
    PREPARED.with(|p| p.borrow().is_some())
}

/// Phase 2: ask the installed model each of `asks` that `answers` has no
/// answer for yet, adding the answers. Each call is bounded by the
/// endpoint's own timeout.
///
/// # Errors
///
/// What went wrong and whether asking again later may succeed.
pub(crate) fn ask_planned(asks: &[Ask], answers: &mut Answers) -> Result<(), (String, bool)> {
    let pending: Vec<&Ask> = asks
        .iter()
        .filter(|a| !answers.iter().any(|(known, _)| known == *a))
        .collect();
    if pending.is_empty() {
        return Ok(());
    }
    let completer = crate::extract::installed().ok_or_else(|| {
        (
            "no chat upstream is configured: set DREVO_EXTRACT_UPSTREAM or \
             DREVO_TEXT2CYPHER_UPSTREAM; see CALL drevo.extract.status()"
                .to_string(),
            true,
        )
    })?;
    for ask in pending {
        let answer = completer.complete(&ask.request).map_err(|e| {
            (
                format!("the chat model failed to summarise a community: {e}"),
                e.is_retryable(),
            )
        })?;
        let summary = summary_of(&answer, ask.token_budget).map_err(|m| (m, true))?;
        answers.push((ask.clone(), summary));
    }
    Ok(())
}

/// Writes summaries by asking a chat model, given the community's
/// extractive view: while planning it records the request, while writing it
/// reads the answer, and elsewhere it asks.
pub(crate) struct LlmSummarizer {
    completer: Arc<dyn ChatCompleter>,
    model: Option<String>,
}

impl LlmSummarizer {
    /// A summariser asking `completer`, with `model` instead of the
    /// endpoint's own when given.
    pub(crate) fn new(completer: Arc<dyn ChatCompleter>, model: Option<String>) -> Self {
        Self { completer, model }
    }
}

/// What the model is told before the community.
fn system_prompt(token_budget: usize) -> String {
    format!(
        "You summarise one community of a knowledge graph: a group of closely linked nodes. \
         Say what the group is about and how its members relate, in plain prose, using only \
         the facts given. Answer with the summary alone, at most {token_budget} tokens."
    )
}

/// The request for `input`'s summary: its extractive view, then the ask.
pub(crate) fn request(
    input: &SummaryInput<'_>,
    model: Option<&str>,
) -> Result<ChatRequest, String> {
    // The view the model reads gets more room than the summary it writes.
    let view = ExtractiveSummarizer.summarize(&SummaryInput {
        token_budget: input.token_budget.saturating_mul(4),
        ..input.clone()
    })?;
    Ok(ChatRequest {
        model: model.map(str::to_string),
        messages: vec![
            ChatMessage::system(system_prompt(input.token_budget)),
            ChatMessage::user(format!(
                "Community:\n{view}\n\nSummarise this community in at most {} tokens.",
                input.token_budget
            )),
        ],
    })
}

/// The model's `answer` as a summary: trimmed, cut to `token_budget`.
pub(crate) fn summary_of(answer: &str, token_budget: usize) -> Result<String, String> {
    let answer = answer.trim();
    if answer.is_empty() {
        return Err("the chat model answered with an empty summary".into());
    }
    if Tokenizer::Approx.count(answer) <= token_budget {
        return Ok(answer.to_string());
    }
    Ok(cut(
        answer,
        token_budget.saturating_mul(4).saturating_sub(1),
    ))
}

/// Why the write met a community the plan did not see.
const UNPLANNED: &str = "the graph changed while the model was summarising; run the call again";

impl CommunitySummarizer for LlmSummarizer {
    fn mode(&self) -> SummaryMode {
        SummaryMode::Llm
    }

    fn summarize(&self, input: &SummaryInput<'_>) -> Result<String, String> {
        let ask = Ask {
            members: input.member_ids_hash.to_string(),
            request: request(input, self.model.as_deref())?,
            token_budget: input.token_budget,
        };
        if is_planning() {
            PLAN.with(|p| {
                if let Some(plan) = p.borrow_mut().as_mut() {
                    plan.asks.push(ask);
                }
            });
            return Ok(String::new());
        }
        let prepared = PREPARED.with(|p| {
            p.borrow().as_ref().map(|answers| {
                answers
                    .iter()
                    .find(|(a, _)| *a == ask)
                    .map(|(_, s)| s.clone())
            })
        });
        match prepared {
            Some(Some(summary)) => Ok(summary),
            Some(None) => {
                UNPREPARED.with(|u| u.set(true));
                Err(UNPLANNED.to_string())
            }
            None => {
                let answer = self
                    .completer
                    .complete(&ask.request)
                    .map_err(|e| format!("the chat model failed to summarise a community: {e}"))?;
                summary_of(&answer, input.token_budget)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::community::Member;
    use crate::llm::LlmError;
    use std::sync::Mutex;

    /// Answers every request with `answer` and keeps the requests.
    struct Canned {
        answer: Result<String, LlmError>,
        seen: Mutex<Vec<ChatRequest>>,
    }

    impl ChatCompleter for Canned {
        fn complete(&self, request: &ChatRequest) -> Result<String, LlmError> {
            self.seen
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .push(request.clone());
            self.answer.clone()
        }
    }

    fn canned(answer: Result<&str, LlmError>) -> Arc<Canned> {
        Arc::new(Canned {
            answer: answer.map(str::to_string),
            seen: Mutex::new(Vec::new()),
        })
    }

    fn members() -> Vec<Member> {
        ["Alice", "Bob", "Carol"]
            .iter()
            .enumerate()
            .map(|(i, name)| Member {
                name: (*name).to_string(),
                label: "Person".into(),
                rank: 1.0 - i as f64 * 0.1,
            })
            .collect()
    }

    fn input<'a>(
        members: &'a [Member],
        keywords: &'a [String],
        rel_types: &'a [(String, usize)],
        token_budget: usize,
    ) -> SummaryInput<'a> {
        SummaryInput {
            size: members.len(),
            member_ids_hash: "h1",
            members,
            keywords,
            rel_types,
            max_members: 40,
            token_budget,
        }
    }

    #[test]
    fn the_request_carries_the_extractive_view_the_budget_and_the_model() {
        let members = members();
        let keywords = vec!["login".to_string(), "token".to_string()];
        let rel_types = vec![("WORKS_ON".to_string(), 4)];
        let request = request(&input(&members, &keywords, &rel_types, 120), Some("m2")).unwrap();
        assert_eq!(request.model.as_deref(), Some("m2"));
        assert_eq!(request.messages.len(), 2);
        assert_eq!(request.messages[0].role, "system");
        assert!(request.messages[0].content.contains("at most 120 tokens"));
        let user = &request.messages[1].content;
        for part in [
            "Alice",
            "Bob",
            "Carol",
            "login",
            "token",
            "WORKS_ON",
            "120 tokens",
        ] {
            assert!(user.contains(part), "{part} missing from {user}");
        }
    }

    #[test]
    fn the_summary_is_the_trimmed_answer_within_the_budget() {
        assert_eq!(summary_of("  A team.\n", 50).unwrap(), "A team.");
        assert_eq!(
            summary_of(" \n ", 50).unwrap_err(),
            "the chat model answered with an empty summary"
        );
        let long = "word ".repeat(400);
        let cut = summary_of(&long, 20).unwrap();
        assert!(cut.chars().count() <= 80, "{}", cut.chars().count());
        assert!(cut.ends_with('…'));
    }

    #[test]
    fn the_summariser_asks_the_model_once_per_community() {
        let model = canned(Ok("The backend team."));
        let summarizer = LlmSummarizer::new(model.clone(), None);
        assert_eq!(summarizer.mode(), SummaryMode::Llm);
        let members = members();
        let got = summarizer
            .summarize(&input(&members, &[], &[], 300))
            .unwrap();
        assert_eq!(got, "The backend team.");
        let seen = model.seen.lock().unwrap_or_else(|e| e.into_inner());
        assert_eq!(seen.len(), 1);
        assert_eq!(seen[0].model, None);
    }

    #[test]
    fn planning_records_the_requests_and_asks_nothing() {
        let model = canned(Ok("never"));
        let summarizer = LlmSummarizer::new(model.clone(), Some("m".into()));
        let members = members();
        assert!(!is_planning());
        let (out, plan) = planning(|| {
            let got = summarizer.summarize(&input(&members, &[], &[], 50));
            finish_plan(true);
            got
        });
        assert_eq!(out.unwrap(), "");
        let plan = plan.unwrap().unwrap();
        assert_eq!(plan.len(), 1);
        assert_eq!(plan[0].members, "h1");
        assert_eq!(plan[0].token_budget, 50);
        assert_eq!(plan[0].request.model.as_deref(), Some("m"));
        assert!(model
            .seen
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .is_empty());
        assert!(!is_planning(), "the plan leaked out of its scope");
        // A call that needs no model, and one that never decided.
        assert_eq!(planning(|| finish_plan(false)).1, Some(None));
        assert_eq!(planning(|| ()).1, None);
    }

    #[test]
    fn the_write_reads_the_answers_and_reports_a_community_it_has_none_for() {
        let model = canned(Ok("never"));
        let summarizer = LlmSummarizer::new(model.clone(), None);
        let members = members();
        let planned = input(&members, &[], &[], 50);
        let (_, plan) = planning(|| {
            summarizer.summarize(&planned).unwrap();
            finish_plan(true);
        });
        let ask = plan.unwrap().unwrap().remove(0);
        let answers = vec![(ask, "The backend team.".to_string())];
        let (got, missed) = with_prepared(&answers, || summarizer.summarize(&planned));
        assert_eq!(got.unwrap(), "The backend team.");
        assert!(!missed);
        // Another member set, or another budget, was not planned.
        let other = SummaryInput {
            member_ids_hash: "h2",
            ..planned.clone()
        };
        let (got, missed) = with_prepared(&answers, || summarizer.summarize(&other));
        assert_eq!(got.unwrap_err(), UNPLANNED);
        assert!(missed);
        let (_, missed) = with_prepared(&answers, || {
            summarizer.summarize(&SummaryInput {
                token_budget: 60,
                ..planned.clone()
            })
        });
        assert!(missed);
        assert!(!is_prepared(), "the answers leaked out of their scope");
        assert!(model
            .seen
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .is_empty());
    }

    #[test]
    fn asking_skips_what_is_already_answered() {
        let members = members();
        let summarizer = LlmSummarizer::new(canned(Ok("x")), None);
        let (_, plan) = planning(|| {
            summarizer
                .summarize(&input(&members, &[], &[], 50))
                .unwrap();
            finish_plan(true);
        });
        let asks = plan.unwrap().unwrap();
        let mut answers = vec![(asks[0].clone(), "known".to_string())];
        // Nothing pending: no model is needed, none is looked up.
        assert_eq!(ask_planned(&asks, &mut answers), Ok(()));
        assert_eq!(answers.len(), 1);
    }

    #[test]
    fn only_a_job_lifts_the_sync_limit() {
        assert!(!in_job());
        assert!(as_job(in_job));
        assert!(!in_job(), "the job flag leaked out of its scope");
    }

    #[test]
    fn the_sync_limit_defaults_to_twenty_and_is_at_least_one() {
        assert_eq!(DEFAULT_MAX_LLM_SYNC, 20);
        assert!(max_llm_sync() >= 1);
    }
}
