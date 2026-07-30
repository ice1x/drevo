//! Scenario integration test: Agent Memory Graph (issue #48)
//!
//! Validates drevo as a first-class **persistent memory backend for agent
//! orchestrators** (LangGraph / Temporal / Claude Code / Cursor / CrewAI …),
//! alongside the existing CBT-journal / story-editor / task-manager / ERP /
//! bug-tracker scenarios. Everything here runs on drevo's *existing*
//! primitives — no new engine features — which is the point: the use case is
//! already served by the graph + FTS + traversal surface; this test pins the
//! orchestrator hot-path so it cannot regress.
//!
//! Canonical schema exercised (from #48):
//! - Node kinds: `agent`, `session`, `observation`, `preference`, `tool_call`.
//! - Edge kinds: `observed_by`, `performed_in_session`, `supersedes`,
//!   `honours_preference`, `produced_by_tool`.
//!
//! Orchestrator flow: bootstrap → record_observation → recall (FTS + kind
//! filter) → context_subgraph → supersede (stale memory) → compact (prune
//! low-confidence observations). Each step maps to a drevo call:
//! - record_observation  → `create_node` + `create_edge`
//! - recall              → `search_fts` filtered by kind
//! - context_subgraph    → `subgraph`
//! - supersede           → a `supersedes` edge
//! - compact             → `delete_node` for low-confidence, stale rows

use drevo::db::Drevo;
use drevo::model::*;
use std::collections::HashMap;

// =========================================================================
// Helpers — the tiny "agent memory API" an orchestrator would wrap.
// =========================================================================

fn memory_db() -> Drevo {
    Drevo::open_in_memory().expect("open in-memory DB")
}

fn props(pairs: &[(&str, serde_json::Value)]) -> Properties {
    let mut m: HashMap<String, serde_json::Value> = HashMap::new();
    for (k, v) in pairs {
        m.insert((*k).to_string(), v.clone());
    }
    Properties::from(m)
}

fn node(kind: &str, title: &str, body: &str, p: Properties) -> NewNode {
    NewNode {
        kind: kind.to_string(),
        title: title.to_string(),
        body: body.to_string(),
        body_html: String::new(),
        properties: p,
    }
}

fn edge(from_id: u64, to_id: u64, kind: &str) -> NewEdge {
    NewEdge {
        from_id,
        to_id,
        kind: kind.to_string(),
        weight: 1.0,
        properties: Properties::default(),
    }
}

/// `record_observation(session, content, source, confidence) -> node_id`,
/// wired to the `agent` (observed_by) and the `session` (performed_in_session).
fn record_observation(
    db: &Drevo,
    agent_id: u64,
    session_id: u64,
    title: &str,
    content: &str,
    source: &str,
    confidence: f64,
) -> u64 {
    let obs = db
        .create_node(node(
            "observation",
            title,
            content,
            props(&[
                ("source", serde_json::json!(source)),
                ("confidence", serde_json::json!(confidence)),
            ]),
        ))
        .expect("create observation");
    db.create_edge(edge(obs.id, agent_id, "observed_by"))
        .expect("observed_by");
    db.create_edge(edge(obs.id, session_id, "performed_in_session"))
        .expect("performed_in_session");
    obs.id
}

/// `recall(query, limit)` restricted to observations — the retrieval hot path.
fn recall_observations(db: &Drevo, query: &str, limit: usize) -> Vec<Node> {
    db.search_fts(query, limit)
        .expect("fts")
        .into_iter()
        .map(|s| s.node)
        .filter(|n| n.kind == "observation")
        .collect()
}

fn confidence_of(n: &Node) -> f64 {
    n.properties
        .get("confidence")
        .and_then(|v| v.as_f64())
        .unwrap_or(0.0)
}

/// `compact` candidates: every `observation` whose confidence is below
/// `threshold`. A scan by kind (not a search) — compaction inspects the whole
/// memory surface, not just what matches a query.
fn stale_observations(db: &Drevo, threshold: f64) -> Vec<u64> {
    db.list_nodes_by_kind("observation", 10_000, 0)
        .expect("list observations")
        .into_iter()
        .filter(|n| confidence_of(n) < threshold)
        .map(|n| n.id)
        .collect()
}

// =========================================================================
// The full orchestrator flow.
// =========================================================================

#[test]
fn agent_memory_full_orchestrator_flow() {
    let db = memory_db();

    // --- bootstrap: an agent instance + a session -----------------------
    let agent = db
        .create_node(node(
            "agent",
            "claude-code",
            "",
            props(&[
                ("model", serde_json::json!("claude-opus-4-8")),
                ("platform", serde_json::json!("cli")),
            ]),
        ))
        .expect("create agent");
    let session = db
        .create_node(node(
            "session",
            "session-2026-07-30",
            "",
            props(&[("workspace", serde_json::json!("/repo/drevo"))]),
        ))
        .expect("create session");
    db.create_edge(edge(agent.id, session.id, "performed_in_session"))
        .expect("agent ran session");

    // --- record_observation: three memories of varying confidence -------
    let dark_mode = record_observation(
        &db,
        agent.id,
        session.id,
        "ui preference",
        "the user prefers a dark mode colour scheme",
        "chat",
        0.9,
    );
    record_observation(
        &db,
        agent.id,
        session.id,
        "build tool",
        "the project builds with cargo and uses redb for storage",
        "code",
        0.8,
    );
    let flaky = record_observation(
        &db,
        agent.id,
        session.id,
        "hunch",
        "maybe the dark mode toggle is flaky but unsure",
        "guess",
        0.2, // low confidence — a compact() candidate
    );

    // --- recall: FTS + kind filter finds the dark-mode memory -----------
    let hits = recall_observations(&db, "dark mode", 10);
    assert!(
        hits.iter().any(|n| n.id == dark_mode),
        "recall must surface the dark-mode observation, got {:?}",
        hits.iter().map(|n| n.id).collect::<Vec<_>>()
    );

    // --- context_subgraph: the bounded window around a memory -----------
    // 1 hop from the observation reaches the agent + session it belongs to.
    let ctx = db.subgraph(dark_mode, 1).expect("subgraph");
    let ctx_ids: Vec<u64> = ctx.nodes.iter().map(|n| n.id).collect();
    assert!(ctx_ids.contains(&dark_mode));
    assert!(
        ctx_ids.contains(&agent.id),
        "context must include the agent"
    );
    assert!(
        ctx_ids.contains(&session.id),
        "context must include the session"
    );

    // --- supersede: the user changes their mind -------------------------
    let light_mode = record_observation(
        &db,
        agent.id,
        session.id,
        "ui preference updated",
        "the user now prefers a light mode colour scheme",
        "chat",
        0.95,
    );
    db.create_edge(edge(light_mode, dark_mode, "supersedes"))
        .expect("supersedes");
    // The supersedes edge is traversable from the new memory to the old.
    let out = db
        .edges_of(light_mode, Direction::Outgoing)
        .expect("edges_of");
    assert!(
        out.iter()
            .any(|e| e.kind == "supersedes" && e.to_id == dark_mode),
        "the new memory must supersede the old one"
    );

    // --- compact: prune low-confidence observations ---------------------
    // compact(session) is a *scan* of the observations (by kind), not a
    // search: drop every memory whose confidence is below a threshold. Here
    // that is the 0.2-confidence hunch.
    let threshold = 0.5;
    let stale: Vec<u64> = stale_observations(&db, threshold);
    assert!(stale.contains(&flaky), "the hunch is a compact candidate");
    for id in &stale {
        db.delete_node(*id).expect("compact delete");
    }
    assert!(
        db.get_node(flaky).expect("get").is_none(),
        "compacted observation must be gone"
    );
    // High-confidence memories survive compaction.
    assert!(db.get_node(dark_mode).expect("get").is_some());
    assert!(db.get_node(light_mode).expect("get").is_some());
}

#[test]
fn recall_ranks_and_filters_by_kind() {
    let db = memory_db();
    let agent = db
        .create_node(node("agent", "a", "", Properties::default()))
        .unwrap();
    let session = db
        .create_node(node("session", "s", "", Properties::default()))
        .unwrap();
    // A same-text `preference` node must NOT come back from an observation
    // recall — kind filtering is what keeps recall on the intended surface.
    db.create_node(node(
        "preference",
        "pref",
        "the user prefers zorptastic layouts",
        Properties::default(),
    ))
    .unwrap();
    let obs = record_observation(
        &db,
        agent.id,
        session.id,
        "obs",
        "the user mentioned zorptastic layouts in passing",
        "chat",
        0.7,
    );

    let hits = recall_observations(&db, "zorptastic", 10);
    assert_eq!(hits.len(), 1, "only the observation, not the preference");
    assert_eq!(hits[0].id, obs);
}

#[test]
fn compact_is_a_no_op_when_nothing_is_stale() {
    let db = memory_db();
    let agent = db
        .create_node(node("agent", "a", "", Properties::default()))
        .unwrap();
    let session = db
        .create_node(node("session", "s", "", Properties::default()))
        .unwrap();
    let keep = record_observation(
        &db,
        agent.id,
        session.id,
        "solid",
        "a solid confident fact",
        "code",
        0.9,
    );
    assert!(stale_observations(&db, 0.5).is_empty());
    assert!(db.get_node(keep).unwrap().is_some());
}
