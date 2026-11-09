//! The extractive summariser: a community described by its best-ranked
//! members grouped by label, its keywords and its heaviest relationship
//! types — deterministic, no model, within a token budget.
//!
//! A summary reads
//!
//! ```text
//! 6 members. Person: Alice, Bob, Carol. Ticket: BE-142 Login token expires early, ….
//! Keywords: token, api, …. Relations: WORKS_ON (7), MENTORS (2), BLOCKS (1).
//! ```
//!
//! (one line). Members are named best-ranked first, at most `maxMembers` of
//! them, as many as fit the budget; labels appear in the order of their
//! best-ranked member.

use super::{CommunitySummarizer, Member, SummaryInput, SummaryMode};
use crate::rag::serialize::cut;
use crate::rag::tokens::Tokenizer;

/// Writes summaries from the members' names, the keywords and the
/// relationship types.
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct ExtractiveSummarizer;

impl CommunitySummarizer for ExtractiveSummarizer {
    fn mode(&self) -> SummaryMode {
        SummaryMode::Extractive
    }

    fn summarize(&self, input: &SummaryInput<'_>) -> Result<String, String> {
        let fits = |text: &str| Tokenizer::Approx.count(text) <= input.token_budget;
        let named = input.max_members.min(input.members.len());
        // The most members that fit with every keyword and relation type.
        for taken in (0..=named).rev() {
            let text = compose(
                input,
                &input.members[..taken],
                input.keywords,
                input.rel_types,
            );
            if fits(&text) {
                return Ok(text);
            }
        }
        // Not even the frame fits: drop the relations, then keywords.
        for kept in (0..=input.keywords.len()).rev() {
            let text = compose(input, &[], &input.keywords[..kept], &[]);
            if fits(&text) {
                return Ok(text);
            }
        }
        let frame = compose(input, &[], &[], &[]);
        let max_chars = input.token_budget.saturating_mul(4).saturating_sub(1);
        Ok(cut(&frame, max_chars))
    }
}

/// The summary naming `members` (best first), with `keywords` and
/// `rel_types`.
fn compose(
    input: &SummaryInput<'_>,
    members: &[Member],
    keywords: &[String],
    rel_types: &[(String, usize)],
) -> String {
    let mut groups: Vec<(&str, Vec<&str>)> = Vec::new();
    for member in members {
        match groups.iter_mut().find(|(label, _)| *label == member.label) {
            Some((_, names)) => names.push(&member.name),
            None => groups.push((&member.label, vec![&member.name])),
        }
    }
    let mut text = format!("{} members.", input.size);
    for (label, names) in groups {
        text.push_str(&format!(" {label}: {}.", names.join(", ")));
    }
    if !keywords.is_empty() {
        text.push_str(&format!(" Keywords: {}.", keywords.join(", ")));
    }
    if !rel_types.is_empty() {
        let rels: Vec<String> = rel_types
            .iter()
            .map(|(kind, count)| format!("{kind} ({count})"))
            .collect();
        text.push_str(&format!(" Relations: {}.", rels.join(", ")));
    }
    text
}
