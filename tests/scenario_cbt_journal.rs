//! Scenario integration test: CBT Journal (Cognitive Behavioral Therapy)
//!
//! This test validates drevo against a real-world CBT journal use case.
//!
//! Domain model:
//! - Node kinds: `thought`, `emotion`, `situation`, `cognitive_distortion`, `rational_response`
//! - Edge kinds: `triggered_by`, `leads_to`, `challenges`, `reframed_as`
//!
//! Tested workflows:
//! - Building a thought chain (situation → thought → emotion)
//! - Identifying cognitive distortions and linking them
//! - Reframing: creating rational responses that challenge distortions
//! - Traversal: tracing chains from a situation to all consequences
//! - FTS: searching for recurring distortion patterns
//! - Subgraph: extracting the full context of a single CBT entry

use drevo::db::Drevo;
use drevo::model::*;
use std::collections::HashMap;

// =========================================================================
// Test helpers
// =========================================================================

fn memory_db() -> Drevo {
    Drevo::open_in_memory().expect("open in-memory DB")
}

#[cfg(feature = "redb-backend")]
fn redb_db(dir: &tempfile::TempDir) -> Drevo {
    let path = dir.path().join("cbt_test.db");
    Drevo::open(&path).expect("open redb DB")
}

fn make_node(kind: &str, title: &str, body: &str) -> NewNode {
    NewNode {
        kind: kind.to_string(),
        title: title.to_string(),
        body: body.to_string(),
        body_html: String::new(),
        properties: Properties::default(),
    }
}

fn make_node_with_props(
    kind: &str,
    title: &str,
    body: &str,
    props: HashMap<String, serde_json::Value>,
) -> NewNode {
    NewNode {
        kind: kind.to_string(),
        title: title.to_string(),
        body: body.to_string(),
        body_html: String::new(),
        properties: Properties::from(props),
    }
}

fn make_edge(from_id: u64, to_id: u64, kind: &str) -> NewEdge {
    NewEdge {
        from_id,
        to_id,
        kind: kind.to_string(),
        weight: 1.0,
        properties: Properties::default(),
    }
}

fn make_weighted_edge(from_id: u64, to_id: u64, kind: &str, weight: f32) -> NewEdge {
    NewEdge {
        from_id,
        to_id,
        kind: kind.to_string(),
        weight,
        properties: Properties::default(),
    }
}

/// Build a complete CBT journal entry and return all created node/edge IDs.
///
/// Graph structure:
///
/// ```text
/// situation: "Meeting with boss"
///   ──triggered_by──▶ thought: "I'll mess up the presentation"
///     ──leads_to──▶ emotion: "Anxiety (intensity: 8)"
///     ──leads_to──▶ emotion: "Fear of failure (intensity: 6)"
///     ──challenges──▶ distortion: "Fortune telling"
///     ──challenges──▶ distortion: "Catastrophizing"
///     ──reframed_as──▶ rational_response: "I've prepared well and handled meetings before"
///       ──leads_to──▶ emotion: "Calm confidence (intensity: 5)"
/// ```
struct CbtEntry {
    situation: u64,
    thought: u64,
    anxiety: u64,
    fear: u64,
    fortune_telling: u64,
    catastrophizing: u64,
    rational_response: u64,
    calm: u64,
}

fn build_cbt_entry(db: &Drevo) -> CbtEntry {
    // Situation
    let situation = db
        .create_node(make_node(
            "situation",
            "Meeting with boss",
            "Upcoming performance review meeting with direct manager",
        ))
        .unwrap();

    // Automatic thought triggered by the situation
    let thought = db
        .create_node(make_node(
            "thought",
            "I'll mess up the presentation",
            "Automatic negative thought about performance review",
        ))
        .unwrap();
    db.create_edge(make_edge(situation.id, thought.id, "triggered_by"))
        .unwrap();

    // Emotions resulting from the thought
    let mut anxiety_props = HashMap::new();
    anxiety_props.insert("intensity".to_string(), serde_json::json!(8));
    let anxiety = db
        .create_node(make_node_with_props(
            "emotion",
            "Anxiety",
            "Physical tension, racing heart",
            anxiety_props,
        ))
        .unwrap();
    db.create_edge(make_edge(thought.id, anxiety.id, "leads_to"))
        .unwrap();

    let mut fear_props = HashMap::new();
    fear_props.insert("intensity".to_string(), serde_json::json!(6));
    let fear = db
        .create_node(make_node_with_props(
            "emotion",
            "Fear of failure",
            "Dread about being judged negatively",
            fear_props,
        ))
        .unwrap();
    db.create_edge(make_edge(thought.id, fear.id, "leads_to"))
        .unwrap();

    // Cognitive distortions identified
    let fortune_telling = db
        .create_node(make_node(
            "cognitive_distortion",
            "Fortune telling",
            "Predicting negative outcomes without evidence",
        ))
        .unwrap();
    db.create_edge(make_edge(thought.id, fortune_telling.id, "challenges"))
        .unwrap();

    let catastrophizing = db
        .create_node(make_node(
            "cognitive_distortion",
            "Catastrophizing",
            "Imagining the worst possible outcome",
        ))
        .unwrap();
    db.create_edge(make_edge(thought.id, catastrophizing.id, "challenges"))
        .unwrap();

    // Rational response (reframing)
    let rational_response = db
        .create_node(make_node(
            "rational_response",
            "I've prepared well",
            "I've prepared well and handled meetings before. My boss gave positive feedback last quarter.",
        ))
        .unwrap();
    db.create_edge(make_edge(thought.id, rational_response.id, "reframed_as"))
        .unwrap();

    // New emotion after reframing
    let mut calm_props = HashMap::new();
    calm_props.insert("intensity".to_string(), serde_json::json!(5));
    let calm = db
        .create_node(make_node_with_props(
            "emotion",
            "Calm confidence",
            "Reduced anxiety after rational reframing",
            calm_props,
        ))
        .unwrap();
    db.create_edge(make_edge(rational_response.id, calm.id, "leads_to"))
        .unwrap();

    CbtEntry {
        situation: situation.id,
        thought: thought.id,
        anxiety: anxiety.id,
        fear: fear.id,
        fortune_telling: fortune_telling.id,
        catastrophizing: catastrophizing.id,
        rational_response: rational_response.id,
        calm: calm.id,
    }
}

// =========================================================================
// Tests — run against both backends via macro
// =========================================================================

macro_rules! cbt_tests {
    ($db_expr:expr, $mod_name:ident) => {
        mod $mod_name {
            use super::*;

            // -----------------------------------------------------------------
            // 1. Basic graph construction — all CBT nodes and edges are created
            // -----------------------------------------------------------------

            #[test]
            fn build_complete_cbt_entry() {
                let db = $db_expr;
                let entry = build_cbt_entry(&db);

                // All 8 nodes should exist
                assert!(db.get_node(entry.situation).unwrap().is_some());
                assert!(db.get_node(entry.thought).unwrap().is_some());
                assert!(db.get_node(entry.anxiety).unwrap().is_some());
                assert!(db.get_node(entry.fear).unwrap().is_some());
                assert!(db.get_node(entry.fortune_telling).unwrap().is_some());
                assert!(db.get_node(entry.catastrophizing).unwrap().is_some());
                assert!(db.get_node(entry.rational_response).unwrap().is_some());
                assert!(db.get_node(entry.calm).unwrap().is_some());
            }

            #[test]
            fn node_kinds_are_correct() {
                let db = $db_expr;
                let entry = build_cbt_entry(&db);

                assert_eq!(
                    db.get_node(entry.situation).unwrap().unwrap().kind,
                    "situation"
                );
                assert_eq!(db.get_node(entry.thought).unwrap().unwrap().kind, "thought");
                assert_eq!(db.get_node(entry.anxiety).unwrap().unwrap().kind, "emotion");
                assert_eq!(
                    db.get_node(entry.fortune_telling).unwrap().unwrap().kind,
                    "cognitive_distortion"
                );
                assert_eq!(
                    db.get_node(entry.rational_response).unwrap().unwrap().kind,
                    "rational_response"
                );
            }

            // -----------------------------------------------------------------
            // 2. Thought chains — tracing from situation to consequences
            // -----------------------------------------------------------------

            #[test]
            fn trace_situation_to_thought() {
                let db = $db_expr;
                let entry = build_cbt_entry(&db);

                // Situation triggers thoughts (outgoing triggered_by edges)
                let triggered = db
                    .neighbors(entry.situation, Direction::Outgoing, Some("triggered_by"))
                    .unwrap();
                assert_eq!(triggered.len(), 1);
                assert_eq!(triggered[0].id, entry.thought);
            }

            #[test]
            fn trace_thought_to_emotions() {
                let db = $db_expr;
                let entry = build_cbt_entry(&db);

                let emotions = db
                    .neighbors(entry.thought, Direction::Outgoing, Some("leads_to"))
                    .unwrap();
                assert_eq!(emotions.len(), 2);
                let emotion_ids: Vec<u64> = emotions.iter().map(|n| n.id).collect();
                assert!(emotion_ids.contains(&entry.anxiety));
                assert!(emotion_ids.contains(&entry.fear));
            }

            #[test]
            fn trace_thought_to_distortions() {
                let db = $db_expr;
                let entry = build_cbt_entry(&db);

                let distortions = db
                    .neighbors(entry.thought, Direction::Outgoing, Some("challenges"))
                    .unwrap();
                assert_eq!(distortions.len(), 2);
                let ids: Vec<u64> = distortions.iter().map(|n| n.id).collect();
                assert!(ids.contains(&entry.fortune_telling));
                assert!(ids.contains(&entry.catastrophizing));
            }

            #[test]
            fn trace_thought_to_reframing() {
                let db = $db_expr;
                let entry = build_cbt_entry(&db);

                let reframed = db
                    .neighbors(entry.thought, Direction::Outgoing, Some("reframed_as"))
                    .unwrap();
                assert_eq!(reframed.len(), 1);
                assert_eq!(reframed[0].id, entry.rational_response);
            }

            #[test]
            fn reframing_leads_to_new_emotion() {
                let db = $db_expr;
                let entry = build_cbt_entry(&db);

                let new_emotions = db
                    .neighbors(
                        entry.rational_response,
                        Direction::Outgoing,
                        Some("leads_to"),
                    )
                    .unwrap();
                assert_eq!(new_emotions.len(), 1);
                assert_eq!(new_emotions[0].id, entry.calm);
            }

            // -----------------------------------------------------------------
            // 3. Reverse traversal — from emotion back to cause
            // -----------------------------------------------------------------

            #[test]
            fn trace_emotion_back_to_thought() {
                let db = $db_expr;
                let entry = build_cbt_entry(&db);

                let causes = db
                    .neighbors(entry.anxiety, Direction::Incoming, Some("leads_to"))
                    .unwrap();
                assert_eq!(causes.len(), 1);
                assert_eq!(causes[0].id, entry.thought);
            }

            #[test]
            fn trace_thought_back_to_situation() {
                let db = $db_expr;
                let entry = build_cbt_entry(&db);

                let situations = db
                    .neighbors(entry.thought, Direction::Incoming, Some("triggered_by"))
                    .unwrap();
                assert_eq!(situations.len(), 1);
                assert_eq!(situations[0].id, entry.situation);
            }

            // -----------------------------------------------------------------
            // 4. BFS — full chain traversal from situation
            // -----------------------------------------------------------------

            #[test]
            fn bfs_from_situation_depth_3_reaches_all_nodes() {
                let db = $db_expr;
                let entry = build_cbt_entry(&db);

                // BFS depth 3 from situation should reach: thought (1), emotions+distortions+rational (2), calm (3)
                let reachable = db
                    .bfs(entry.situation, 3, Direction::Outgoing, None)
                    .unwrap();
                let ids: Vec<u64> = reachable.iter().map(|n| n.id).collect();

                assert!(ids.contains(&entry.thought), "should reach thought");
                assert!(ids.contains(&entry.anxiety), "should reach anxiety");
                assert!(ids.contains(&entry.fear), "should reach fear");
                assert!(
                    ids.contains(&entry.fortune_telling),
                    "should reach fortune_telling"
                );
                assert!(
                    ids.contains(&entry.catastrophizing),
                    "should reach catastrophizing"
                );
                assert!(
                    ids.contains(&entry.rational_response),
                    "should reach rational_response"
                );
                assert!(ids.contains(&entry.calm), "should reach calm");
            }

            #[test]
            fn bfs_from_situation_depth_1_reaches_only_thought() {
                let db = $db_expr;
                let entry = build_cbt_entry(&db);

                let reachable = db
                    .bfs(entry.situation, 1, Direction::Outgoing, None)
                    .unwrap();
                assert_eq!(reachable.len(), 1);
                assert_eq!(reachable[0].id, entry.thought);
            }

            #[test]
            fn bfs_with_edge_filter_follows_only_leads_to() {
                let db = $db_expr;
                let entry = build_cbt_entry(&db);

                // From thought, following only "leads_to" edges should reach emotions, NOT distortions/rational_response
                let reachable = db
                    .bfs(entry.thought, 1, Direction::Outgoing, Some("leads_to"))
                    .unwrap();
                let ids: Vec<u64> = reachable.iter().map(|n| n.id).collect();
                assert!(ids.contains(&entry.anxiety));
                assert!(ids.contains(&entry.fear));
                assert!(!ids.contains(&entry.fortune_telling));
                assert!(!ids.contains(&entry.rational_response));
            }

            // -----------------------------------------------------------------
            // 5. Shortest path — finding connections between concepts
            // -----------------------------------------------------------------

            #[test]
            fn shortest_path_from_situation_to_calm() {
                let db = $db_expr;
                let entry = build_cbt_entry(&db);

                let path = db.shortest_path(entry.situation, entry.calm).unwrap();
                assert!(path.is_some(), "should find path from situation to calm");
                let path = path.unwrap();
                // situation → thought → rational_response → calm
                assert_eq!(path.len(), 4);
                assert_eq!(path[0], entry.situation);
                assert_eq!(*path.last().unwrap(), entry.calm);
            }

            #[test]
            fn shortest_path_from_situation_to_distortion() {
                let db = $db_expr;
                let entry = build_cbt_entry(&db);

                let path = db
                    .shortest_path(entry.situation, entry.fortune_telling)
                    .unwrap();
                assert!(path.is_some());
                let path = path.unwrap();
                // situation → thought → fortune_telling
                assert_eq!(path.len(), 3);
            }

            #[test]
            fn no_path_from_emotion_to_situation() {
                let db = $db_expr;
                let entry = build_cbt_entry(&db);

                // Emotions don't have outgoing edges to situations
                let path = db.shortest_path(entry.anxiety, entry.situation).unwrap();
                assert!(path.is_none(), "no outgoing path from emotion to situation");
            }

            // -----------------------------------------------------------------
            // 6. Subgraph — extracting full CBT entry context
            // -----------------------------------------------------------------

            #[test]
            fn subgraph_from_thought_depth_1_includes_immediate_neighbors() {
                let db = $db_expr;
                let entry = build_cbt_entry(&db);

                let sg = db.subgraph(entry.thought, 1).unwrap();
                let node_ids: Vec<u64> = sg.nodes.iter().map(|n| n.id).collect();

                // Thought + situation (incoming) + anxiety + fear + fortune_telling + catastrophizing + rational_response
                assert!(node_ids.contains(&entry.thought));
                assert!(node_ids.contains(&entry.situation));
                assert!(node_ids.contains(&entry.anxiety));
                assert!(node_ids.contains(&entry.fear));
                assert!(node_ids.contains(&entry.fortune_telling));
                assert!(node_ids.contains(&entry.catastrophizing));
                assert!(node_ids.contains(&entry.rational_response));
            }

            #[test]
            fn subgraph_from_situation_depth_3_is_complete() {
                let db = $db_expr;
                let entry = build_cbt_entry(&db);

                let sg = db.subgraph(entry.situation, 3).unwrap();
                // All 8 nodes should be in the subgraph
                assert_eq!(sg.nodes.len(), 8);
                // All 7 edges should be in the subgraph
                assert_eq!(sg.edges.len(), 7);
            }

            // -----------------------------------------------------------------
            // 7. Kind index — board views by node type
            // -----------------------------------------------------------------

            #[test]
            fn list_all_emotions() {
                let db = $db_expr;
                let entry = build_cbt_entry(&db);

                let emotions = db.list_nodes_by_kind("emotion", 100, 0).unwrap();
                assert_eq!(emotions.len(), 3); // anxiety, fear, calm
                let ids: Vec<u64> = emotions.iter().map(|n| n.id).collect();
                assert!(ids.contains(&entry.anxiety));
                assert!(ids.contains(&entry.fear));
                assert!(ids.contains(&entry.calm));
            }

            #[test]
            fn list_all_distortions() {
                let db = $db_expr;
                let entry = build_cbt_entry(&db);

                let distortions = db
                    .list_nodes_by_kind("cognitive_distortion", 100, 0)
                    .unwrap();
                assert_eq!(distortions.len(), 2);
                let ids: Vec<u64> = distortions.iter().map(|n| n.id).collect();
                assert!(ids.contains(&entry.fortune_telling));
                assert!(ids.contains(&entry.catastrophizing));
            }

            #[test]
            fn list_rational_responses() {
                let db = $db_expr;
                let entry = build_cbt_entry(&db);

                let responses = db.list_nodes_by_kind("rational_response", 100, 0).unwrap();
                assert_eq!(responses.len(), 1);
                assert_eq!(responses[0].id, entry.rational_response);
            }

            // -----------------------------------------------------------------
            // 8. FTS — searching for distortion patterns
            // -----------------------------------------------------------------

            #[test]
            fn fts_finds_distortion_by_name() {
                let db = $db_expr;
                build_cbt_entry(&db);

                let results = db.search_fts("fortune telling", 10).unwrap();
                assert!(!results.is_empty(), "should find 'fortune telling'");
                assert_eq!(results[0].node.kind, "cognitive_distortion");
                assert!(results[0].node.title.contains("Fortune telling"));
            }

            #[test]
            fn fts_finds_nodes_by_body_content() {
                let db = $db_expr;
                build_cbt_entry(&db);

                let results = db.search_fts("predicting negative outcomes", 10).unwrap();
                assert!(!results.is_empty(), "should find by body content");
            }

            #[test]
            fn fts_finds_rational_response() {
                let db = $db_expr;
                build_cbt_entry(&db);

                let results = db.search_fts("prepared well", 10).unwrap();
                assert!(!results.is_empty());
                let found_rational = results.iter().any(|r| r.node.kind == "rational_response");
                assert!(found_rational, "should find rational response");
            }

            #[test]
            fn fts_anxiety_returns_emotion_node() {
                let db = $db_expr;
                build_cbt_entry(&db);

                let results = db.search_fts("anxiety", 10).unwrap();
                assert!(!results.is_empty());
                // At least one result should be about anxiety/emotions
                let has_emotion = results.iter().any(|r| {
                    r.node.kind == "emotion" || r.node.body.to_lowercase().contains("anxiety")
                });
                assert!(has_emotion);
            }

            // -----------------------------------------------------------------
            // 9. Properties — emotion intensity tracking
            // -----------------------------------------------------------------

            #[test]
            fn emotion_intensity_is_stored() {
                let db = $db_expr;
                let entry = build_cbt_entry(&db);

                let anxiety = db.get_node(entry.anxiety).unwrap().unwrap();
                let intensity = anxiety.properties.get("intensity").unwrap();
                assert_eq!(intensity, &serde_json::json!(8));

                let calm = db.get_node(entry.calm).unwrap().unwrap();
                let calm_intensity = calm.properties.get("intensity").unwrap();
                assert_eq!(calm_intensity, &serde_json::json!(5));
            }

            // -----------------------------------------------------------------
            // 10. Multi-entry journal — recurring distortion patterns
            // -----------------------------------------------------------------

            #[test]
            fn multiple_entries_share_distortion_patterns() {
                let db = $db_expr;
                let entry1 = build_cbt_entry(&db);

                // Second entry: different situation, same distortion pattern
                let situation2 = db
                    .create_node(make_node(
                        "situation",
                        "Social gathering",
                        "Invited to a party where I don't know many people",
                    ))
                    .unwrap();

                let thought2 = db
                    .create_node(make_node(
                        "thought",
                        "Nobody will want to talk to me",
                        "Predicting social rejection without evidence",
                    ))
                    .unwrap();
                db.create_edge(make_edge(situation2.id, thought2.id, "triggered_by"))
                    .unwrap();

                // Reuse the same "fortune telling" distortion from entry 1
                db.create_edge(make_edge(thought2.id, entry1.fortune_telling, "challenges"))
                    .unwrap();

                // Now "fortune telling" should be reachable from both thoughts
                let ft_incoming = db
                    .neighbors(
                        entry1.fortune_telling,
                        Direction::Incoming,
                        Some("challenges"),
                    )
                    .unwrap();
                assert_eq!(
                    ft_incoming.len(),
                    2,
                    "fortune telling should be linked from 2 thoughts"
                );
                let thought_ids: Vec<u64> = ft_incoming.iter().map(|n| n.id).collect();
                assert!(thought_ids.contains(&entry1.thought));
                assert!(thought_ids.contains(&thought2.id));
            }

            #[test]
            fn find_most_common_distortions() {
                let db = $db_expr;
                let entry1 = build_cbt_entry(&db);

                // Add more entries linking to catastrophizing
                for i in 0..3 {
                    let thought = db
                        .create_node(make_node(
                            "thought",
                            &format!("Negative thought #{}", i),
                            "Another catastrophic prediction",
                        ))
                        .unwrap();
                    db.create_edge(make_edge(thought.id, entry1.catastrophizing, "challenges"))
                        .unwrap();
                }

                // Catastrophizing now has 4 incoming "challenges" edges (1 original + 3 new)
                let cat_incoming = db
                    .neighbors(
                        entry1.catastrophizing,
                        Direction::Incoming,
                        Some("challenges"),
                    )
                    .unwrap();
                assert_eq!(cat_incoming.len(), 4);

                // Fortune telling still has 1
                let ft_incoming = db
                    .neighbors(
                        entry1.fortune_telling,
                        Direction::Incoming,
                        Some("challenges"),
                    )
                    .unwrap();
                assert_eq!(ft_incoming.len(), 1);

                // This demonstrates: counting incoming "challenges" edges reveals the
                // most frequent distortion pattern
            }

            // -----------------------------------------------------------------
            // 11. Update workflow — adjusting emotions after therapy
            // -----------------------------------------------------------------

            #[test]
            fn update_emotion_intensity_after_reframing() {
                let db = $db_expr;
                let entry = build_cbt_entry(&db);

                // Anxiety intensity reduced from 8 to 3 after CBT exercise
                let mut new_props = HashMap::new();
                new_props.insert("intensity".to_string(), serde_json::json!(3));
                new_props.insert("post_reframe".to_string(), serde_json::json!(true));

                db.update_node(
                    entry.anxiety,
                    NodePatch {
                        properties: Some(Properties::from(new_props)),
                        ..Default::default()
                    },
                )
                .unwrap();

                let updated = db.get_node(entry.anxiety).unwrap().unwrap();
                assert_eq!(
                    updated.properties.get("intensity").unwrap(),
                    &serde_json::json!(3)
                );
                assert_eq!(
                    updated.properties.get("post_reframe").unwrap(),
                    &serde_json::json!(true)
                );
            }

            // -----------------------------------------------------------------
            // 12. Delete workflow — removing a thought chain
            // -----------------------------------------------------------------

            #[test]
            fn delete_thought_cascades_edges() {
                let db = $db_expr;
                let entry = build_cbt_entry(&db);

                // Delete the thought node — should cascade delete its edges
                db.delete_node(entry.thought).unwrap();

                // Thought should be gone
                assert!(db.get_node(entry.thought).unwrap().is_none());

                // Situation should still exist but have no outgoing triggered_by
                let triggered = db
                    .neighbors(entry.situation, Direction::Outgoing, Some("triggered_by"))
                    .unwrap();
                assert!(triggered.is_empty());

                // Emotions and distortions should still exist as nodes (no cascade to connected nodes)
                assert!(db.get_node(entry.anxiety).unwrap().is_some());
                assert!(db.get_node(entry.fortune_telling).unwrap().is_some());
            }

            // -----------------------------------------------------------------
            // 13. Edge weight — severity scoring for distortion links
            // -----------------------------------------------------------------

            #[test]
            fn weighted_distortion_links() {
                let db = $db_expr;

                let thought = db
                    .create_node(make_node(
                        "thought",
                        "Everything is terrible",
                        "Global negative thinking",
                    ))
                    .unwrap();

                let d1 = db
                    .create_node(make_node(
                        "cognitive_distortion",
                        "Overgeneralization",
                        "Drawing broad conclusions from single events",
                    ))
                    .unwrap();
                let d2 = db
                    .create_node(make_node(
                        "cognitive_distortion",
                        "All-or-nothing thinking",
                        "Seeing things in black and white",
                    ))
                    .unwrap();

                // Higher weight = stronger link to this distortion
                db.create_edge(make_weighted_edge(thought.id, d1.id, "challenges", 0.9))
                    .unwrap();
                db.create_edge(make_weighted_edge(thought.id, d2.id, "challenges", 0.4))
                    .unwrap();

                let edges = db.edges_of(thought.id, Direction::Outgoing).unwrap();
                let challenge_edges: Vec<&Edge> =
                    edges.iter().filter(|e| e.kind == "challenges").collect();
                assert_eq!(challenge_edges.len(), 2);

                // Find the strongest distortion link
                let strongest = challenge_edges
                    .iter()
                    .max_by(|a, b| a.weight.partial_cmp(&b.weight).unwrap())
                    .unwrap();
                assert_eq!(strongest.to_id, d1.id);
                assert!((strongest.weight - 0.9).abs() < f32::EPSILON);
            }

            // -----------------------------------------------------------------
            // 14. DFS — deep exploration of thought chains
            // -----------------------------------------------------------------

            #[test]
            fn dfs_from_situation_explores_full_chain() {
                let db = $db_expr;
                let entry = build_cbt_entry(&db);

                let visited = db
                    .dfs(entry.situation, 5, Direction::Outgoing, None)
                    .unwrap();
                let ids: Vec<u64> = visited.iter().map(|n| n.id).collect();

                // Should visit all downstream nodes
                assert!(ids.contains(&entry.thought));
                assert!(ids.contains(&entry.anxiety));
                assert!(ids.contains(&entry.calm));
            }

            // -----------------------------------------------------------------
            // 15. List recent — journal timeline
            // -----------------------------------------------------------------

            #[test]
            fn list_recent_shows_newest_entries_first() {
                let db = $db_expr;
                build_cbt_entry(&db);

                // Add a newer entry
                std::thread::sleep(std::time::Duration::from_millis(10));
                let newest = db
                    .create_node(make_node(
                        "situation",
                        "Latest journal entry",
                        "Most recent CBT exercise",
                    ))
                    .unwrap();

                let recent = db.list_recent(3).unwrap();
                assert!(!recent.is_empty());
                assert_eq!(recent[0].id, newest.id, "newest entry should be first");
            }
        }
    };
}

// Run all tests against MemoryBackend
cbt_tests!(memory_db(), memory);

// Run all tests against RedbBackend
#[cfg(feature = "redb-backend")]
mod redb {
    use super::*;

    macro_rules! cbt_redb_tests {
        ($mod_name:ident) => {
            mod $mod_name {
                use super::*;
                use tempfile::TempDir;

                fn setup() -> (TempDir, Drevo) {
                    let dir = TempDir::new().expect("create temp dir");
                    let db = redb_db(&dir);
                    (dir, db)
                }

                #[test]
                fn build_complete_cbt_entry() {
                    let (_dir, db) = setup();
                    let entry = build_cbt_entry(&db);
                    assert!(db.get_node(entry.situation).unwrap().is_some());
                    assert!(db.get_node(entry.calm).unwrap().is_some());
                }

                #[test]
                fn trace_situation_to_thought() {
                    let (_dir, db) = setup();
                    let entry = build_cbt_entry(&db);
                    let triggered = db
                        .neighbors(entry.situation, Direction::Outgoing, Some("triggered_by"))
                        .unwrap();
                    assert_eq!(triggered.len(), 1);
                    assert_eq!(triggered[0].id, entry.thought);
                }

                #[test]
                fn bfs_reaches_all_nodes() {
                    let (_dir, db) = setup();
                    let entry = build_cbt_entry(&db);
                    let reachable = db
                        .bfs(entry.situation, 3, Direction::Outgoing, None)
                        .unwrap();
                    assert_eq!(reachable.len(), 7); // all except situation itself
                }

                #[test]
                fn shortest_path_works() {
                    let (_dir, db) = setup();
                    let entry = build_cbt_entry(&db);
                    let path = db.shortest_path(entry.situation, entry.calm).unwrap();
                    assert!(path.is_some());
                    assert_eq!(path.unwrap().len(), 4);
                }

                #[test]
                fn subgraph_is_complete() {
                    let (_dir, db) = setup();
                    let entry = build_cbt_entry(&db);
                    let sg = db.subgraph(entry.situation, 3).unwrap();
                    assert_eq!(sg.nodes.len(), 8);
                    assert_eq!(sg.edges.len(), 7);
                }

                #[test]
                fn fts_works() {
                    let (_dir, db) = setup();
                    build_cbt_entry(&db);
                    let results = db.search_fts("fortune telling", 10).unwrap();
                    assert!(!results.is_empty());
                }

                #[test]
                fn kind_index_works() {
                    let (_dir, db) = setup();
                    build_cbt_entry(&db);
                    let emotions = db.list_nodes_by_kind("emotion", 100, 0).unwrap();
                    assert_eq!(emotions.len(), 3);
                }

                #[test]
                fn properties_persist() {
                    let (_dir, db) = setup();
                    let entry = build_cbt_entry(&db);
                    let anxiety = db.get_node(entry.anxiety).unwrap().unwrap();
                    assert_eq!(
                        anxiety.properties.get("intensity").unwrap(),
                        &serde_json::json!(8)
                    );
                }

                #[test]
                fn delete_cascades_edges() {
                    let (_dir, db) = setup();
                    let entry = build_cbt_entry(&db);
                    db.delete_node(entry.thought).unwrap();
                    assert!(db.get_node(entry.thought).unwrap().is_none());
                    let triggered = db
                        .neighbors(entry.situation, Direction::Outgoing, Some("triggered_by"))
                        .unwrap();
                    assert!(triggered.is_empty());
                }

                #[test]
                fn multiple_entries_share_distortions() {
                    let (_dir, db) = setup();
                    let entry1 = build_cbt_entry(&db);

                    let thought2 = db
                        .create_node(make_node(
                            "thought",
                            "Nobody cares about me",
                            "Social anxiety thought",
                        ))
                        .unwrap();
                    db.create_edge(make_edge(thought2.id, entry1.fortune_telling, "challenges"))
                        .unwrap();

                    let ft_incoming = db
                        .neighbors(
                            entry1.fortune_telling,
                            Direction::Incoming,
                            Some("challenges"),
                        )
                        .unwrap();
                    assert_eq!(ft_incoming.len(), 2);
                }
            }
        };
    }

    cbt_redb_tests!(redb_backend);
}
