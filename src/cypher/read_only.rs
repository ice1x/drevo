//! Read-only classification of a parsed Cypher query for the engine flip
//! (RFC `docs/rfc-native-core.md` #307, Phase 6 slice A).
//!
//! The native read mirror ([`crate::native_mirror::NativeMirror`]) may only
//! serve a query that (a) performs **no writes** — writes must land on the
//! durable KV store — and (b) calls **no procedure the mirror cannot
//! answer**: the mirror carries the native label / property indexes and the
//! value cache, but not the FTS, vector, or semantic subsystems, so those
//! procedures stay on the KV path where they are implemented.
//!
//! The classifier is deliberately conservative: anything it does not
//! recognise routes to the KV engine, which can execute everything. A false
//! `false` costs speed; a false `true` would cost correctness — so unknown
//! procedures are never allowlisted.

use crate::cypher::ast::{Clause, Query};

/// Procedures the native mirror can answer itself. Everything else —
/// including every unknown name — routes to the KV engine. Names are matched
/// exactly as the executor resolves them (dot-joined, case-sensitive).
const MIRROR_PROCEDURES: [&str; 4] = [
    "db.labels",
    "db.propertyKeys",
    "db.relationshipTypes",
    "drevo.info",
];

/// `true` when every clause of every `UNION` arm is a read the native mirror
/// can serve; `false` for any write clause (`CREATE` / `MERGE` / `SET` /
/// `DELETE` / `REMOVE` / `FOREACH`) or any procedure call outside the
/// mirror's allowlist.
pub fn mirror_can_serve(query: &Query) -> bool {
    query
        .parts
        .iter()
        .flat_map(|part| part.query.clauses.iter())
        .all(clause_is_mirror_read)
}

/// Per-clause arm of [`mirror_can_serve`].
fn clause_is_mirror_read(clause: &Clause) -> bool {
    match clause {
        Clause::Match(_) | Clause::With(_) | Clause::Return(_) | Clause::Unwind(_) => true,
        // FOREACH exists only to apply updates, so it is a write even
        // before looking inside its body.
        Clause::Create(_)
        | Clause::Merge(_)
        | Clause::Delete(_)
        | Clause::Set(_)
        | Clause::Remove(_)
        | Clause::Foreach(_) => false,
        Clause::Call(call) => MIRROR_PROCEDURES.contains(&call.name.join(".").as_str()),
    }
}
