//! Scenario integration test: Story / Book / Scenario Editor
//!
//! This test validates drevo against a real-world story editor use case.
//!
//! Domain model:
//! - Node kinds: `book`, `chapter`, `scene`, `character`, `location`, `plot_point`
//! - Edge kinds: `contains`, `follows`, `involves`, `takes_place_in`, `relates_to`
//!
//! Tested workflows:
//! - Building a tree-structured narrative (book → chapter → scene)
//! - Linking characters, locations, and plot points to scenes
//! - Traversal: navigating the narrative tree and character relationships
//! - Subgraph: extracting bounded context for AI co-authoring (MCP)
//! - FTS: searching across all narrative content
//! - Kind index: board views (all characters, all scenes, etc.)
//! - Scene ordering via edge weights and `follows` edges

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
    let path = dir.path().join("story_test.db");
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

// =========================================================================
// Domain builder — a small novel structure
// =========================================================================

/// A complete story structure for testing.
///
/// Graph structure:
///
/// ```text
/// book: "The Lost Garden"
///   ──contains──▶ chapter1: "The Discovery"
///     ──contains──▶ scene1: "Morning walk"
///       ──involves──▶ character: "Elena"
///       ──takes_place_in──▶ location: "Forest path"
///       ──relates_to──▶ plot_point: "Finding the map"
///     ──contains──▶ scene2: "The hidden gate"
///       ──involves──▶ character: "Elena"
///       ──involves──▶ character: "Marcus"
///       ──takes_place_in──▶ location: "Old garden wall"
///       ──relates_to──▶ plot_point: "Opening the gate"
///     scene1 ──follows──▶ scene2
///   ──contains──▶ chapter2: "Into the Garden"
///     ──contains──▶ scene3: "First steps inside"
///       ──involves──▶ character: "Elena"
///       ──involves──▶ character: "Marcus"
///       ──takes_place_in──▶ location: "The lost garden"
///       ──relates_to──▶ plot_point: "Garden is alive"
///     ──contains──▶ scene4: "The guardian appears"
///       ──involves──▶ character: "Elena"
///       ──involves──▶ character: "The Guardian"
///       ──takes_place_in──▶ location: "The lost garden"
///       ──relates_to──▶ plot_point: "Meeting the guardian"
///     scene3 ──follows──▶ scene4
///   chapter1 ──follows──▶ chapter2
///   character "Elena" ──relates_to──▶ character "Marcus"
/// ```
struct StoryData {
    book: u64,
    chapter1: u64,
    chapter2: u64,
    scene1: u64,
    scene2: u64,
    scene3: u64,
    scene4: u64,
    elena: u64,
    marcus: u64,
    guardian: u64,
    forest_path: u64,
    garden_wall: u64,
    lost_garden: u64,
    plot_find_map: u64,
    plot_open_gate: u64,
    plot_garden_alive: u64,
    plot_meet_guardian: u64,
}

fn build_story(db: &Drevo) -> StoryData {
    // --- Book ---
    let book = db
        .create_node(make_node(
            "book",
            "The Lost Garden",
            "A fantasy novel about two friends who discover an enchanted garden hidden behind an ancient wall.",
        ))
        .unwrap();

    // --- Characters ---
    let mut elena_props = HashMap::new();
    elena_props.insert("role".to_string(), serde_json::json!("protagonist"));
    elena_props.insert("age".to_string(), serde_json::json!(28));
    let elena = db
        .create_node(make_node_with_props(
            "character",
            "Elena",
            "A curious botanist who discovers the garden. Brave, methodical, deeply connected to nature.",
            elena_props,
        ))
        .unwrap();

    let mut marcus_props = HashMap::new();
    marcus_props.insert("role".to_string(), serde_json::json!("deuteragonist"));
    marcus_props.insert("age".to_string(), serde_json::json!(32));
    let marcus = db
        .create_node(make_node_with_props(
            "character",
            "Marcus",
            "Elena's childhood friend, a historian specializing in local legends. Skeptical but loyal.",
            marcus_props,
        ))
        .unwrap();

    let mut guardian_props = HashMap::new();
    guardian_props.insert("role".to_string(), serde_json::json!("antagonist"));
    guardian_props.insert("species".to_string(), serde_json::json!("spirit"));
    let guardian = db
        .create_node(make_node_with_props(
            "character",
            "The Guardian",
            "An ancient spirit bound to protect the garden. Neither friend nor foe.",
            guardian_props,
        ))
        .unwrap();

    // Character relationship
    db.create_edge(make_edge(elena.id, marcus.id, "relates_to"))
        .unwrap();

    // --- Locations ---
    let forest_path = db
        .create_node(make_node(
            "location",
            "Forest path",
            "A winding trail through old-growth forest on the outskirts of the village.",
        ))
        .unwrap();

    let garden_wall = db
        .create_node(make_node(
            "location",
            "Old garden wall",
            "A crumbling moss-covered stone wall hidden among overgrown hedges.",
        ))
        .unwrap();

    let lost_garden = db
        .create_node(make_node(
            "location",
            "The lost garden",
            "An impossibly lush garden that seems untouched by time. Flowers bloom in every season simultaneously.",
        ))
        .unwrap();

    // --- Plot points ---
    let plot_find_map = db
        .create_node(make_node(
            "plot_point",
            "Finding the map",
            "Elena discovers an old parchment map tucked inside a hollow tree during her morning walk.",
        ))
        .unwrap();

    let plot_open_gate = db
        .create_node(make_node(
            "plot_point",
            "Opening the gate",
            "Using the map's cipher, Elena and Marcus decode the mechanism that unlocks the hidden gate in the wall.",
        ))
        .unwrap();

    let plot_garden_alive = db
        .create_node(make_node(
            "plot_point",
            "Garden is alive",
            "The garden responds to their presence: paths shift, flowers turn toward them, and the air hums with energy.",
        ))
        .unwrap();

    let plot_meet_guardian = db
        .create_node(make_node(
            "plot_point",
            "Meeting the guardian",
            "A translucent figure emerges from the oldest tree, introducing itself as the garden's protector.",
        ))
        .unwrap();

    // --- Chapter 1: The Discovery ---
    let chapter1 = db
        .create_node(make_node(
            "chapter",
            "The Discovery",
            "Elena stumbles upon clues that lead to a hidden garden behind an ancient wall.",
        ))
        .unwrap();
    db.create_edge(make_edge(book.id, chapter1.id, "contains"))
        .unwrap();

    // Scene 1: Morning walk
    let scene1 = db
        .create_node(make_node(
            "scene",
            "Morning walk",
            "Elena takes her usual morning walk through the forest and finds a strange parchment in a hollow tree.",
        ))
        .unwrap();
    db.create_edge(make_edge(chapter1.id, scene1.id, "contains"))
        .unwrap();
    db.create_edge(make_edge(scene1.id, elena.id, "involves"))
        .unwrap();
    db.create_edge(make_edge(scene1.id, forest_path.id, "takes_place_in"))
        .unwrap();
    db.create_edge(make_edge(scene1.id, plot_find_map.id, "relates_to"))
        .unwrap();

    // Scene 2: The hidden gate
    let scene2 = db
        .create_node(make_node(
            "scene",
            "The hidden gate",
            "Elena brings Marcus to the wall. Together they decode the map and discover the gate mechanism.",
        ))
        .unwrap();
    db.create_edge(make_edge(chapter1.id, scene2.id, "contains"))
        .unwrap();
    db.create_edge(make_edge(scene2.id, elena.id, "involves"))
        .unwrap();
    db.create_edge(make_edge(scene2.id, marcus.id, "involves"))
        .unwrap();
    db.create_edge(make_edge(scene2.id, garden_wall.id, "takes_place_in"))
        .unwrap();
    db.create_edge(make_edge(scene2.id, plot_open_gate.id, "relates_to"))
        .unwrap();

    // Scene ordering within chapter 1
    db.create_edge(make_weighted_edge(scene1.id, scene2.id, "follows", 1.0))
        .unwrap();

    // --- Chapter 2: Into the Garden ---
    let chapter2 = db
        .create_node(make_node(
            "chapter",
            "Into the Garden",
            "The friends enter the garden and encounter its mysterious guardian.",
        ))
        .unwrap();
    db.create_edge(make_edge(book.id, chapter2.id, "contains"))
        .unwrap();

    // Chapter ordering
    db.create_edge(make_weighted_edge(chapter1.id, chapter2.id, "follows", 1.0))
        .unwrap();

    // Scene 3: First steps inside
    let scene3 = db
        .create_node(make_node(
            "scene",
            "First steps inside",
            "Elena and Marcus step through the gate into an impossibly beautiful garden where every season coexists.",
        ))
        .unwrap();
    db.create_edge(make_edge(chapter2.id, scene3.id, "contains"))
        .unwrap();
    db.create_edge(make_edge(scene3.id, elena.id, "involves"))
        .unwrap();
    db.create_edge(make_edge(scene3.id, marcus.id, "involves"))
        .unwrap();
    db.create_edge(make_edge(scene3.id, lost_garden.id, "takes_place_in"))
        .unwrap();
    db.create_edge(make_edge(scene3.id, plot_garden_alive.id, "relates_to"))
        .unwrap();

    // Scene 4: The guardian appears
    let scene4 = db
        .create_node(make_node(
            "scene",
            "The guardian appears",
            "A translucent figure materializes from the oldest tree and confronts Elena about her intentions.",
        ))
        .unwrap();
    db.create_edge(make_edge(chapter2.id, scene4.id, "contains"))
        .unwrap();
    db.create_edge(make_edge(scene4.id, elena.id, "involves"))
        .unwrap();
    db.create_edge(make_edge(scene4.id, guardian.id, "involves"))
        .unwrap();
    db.create_edge(make_edge(scene4.id, lost_garden.id, "takes_place_in"))
        .unwrap();
    db.create_edge(make_edge(scene4.id, plot_meet_guardian.id, "relates_to"))
        .unwrap();

    // Scene ordering within chapter 2
    db.create_edge(make_weighted_edge(scene3.id, scene4.id, "follows", 1.0))
        .unwrap();

    StoryData {
        book: book.id,
        chapter1: chapter1.id,
        chapter2: chapter2.id,
        scene1: scene1.id,
        scene2: scene2.id,
        scene3: scene3.id,
        scene4: scene4.id,
        elena: elena.id,
        marcus: marcus.id,
        guardian: guardian.id,
        forest_path: forest_path.id,
        garden_wall: garden_wall.id,
        lost_garden: lost_garden.id,
        plot_find_map: plot_find_map.id,
        plot_open_gate: plot_open_gate.id,
        plot_garden_alive: plot_garden_alive.id,
        plot_meet_guardian: plot_meet_guardian.id,
    }
}

// =========================================================================
// Tests — run against both backends via macro
// =========================================================================

macro_rules! story_tests {
    ($db_expr:expr, $mod_name:ident) => {
        mod $mod_name {
            use super::*;

            // -----------------------------------------------------------------
            // 1. Basic graph construction — all story nodes and edges exist
            // -----------------------------------------------------------------

            #[test]
            fn build_complete_story() {
                let db = $db_expr;
                let s = build_story(&db);

                // 17 nodes total
                assert!(db.get_node(s.book).unwrap().is_some());
                assert!(db.get_node(s.chapter1).unwrap().is_some());
                assert!(db.get_node(s.chapter2).unwrap().is_some());
                assert!(db.get_node(s.scene1).unwrap().is_some());
                assert!(db.get_node(s.scene2).unwrap().is_some());
                assert!(db.get_node(s.scene3).unwrap().is_some());
                assert!(db.get_node(s.scene4).unwrap().is_some());
                assert!(db.get_node(s.elena).unwrap().is_some());
                assert!(db.get_node(s.marcus).unwrap().is_some());
                assert!(db.get_node(s.guardian).unwrap().is_some());
                assert!(db.get_node(s.forest_path).unwrap().is_some());
                assert!(db.get_node(s.garden_wall).unwrap().is_some());
                assert!(db.get_node(s.lost_garden).unwrap().is_some());
                assert!(db.get_node(s.plot_find_map).unwrap().is_some());
                assert!(db.get_node(s.plot_open_gate).unwrap().is_some());
                assert!(db.get_node(s.plot_garden_alive).unwrap().is_some());
                assert!(db.get_node(s.plot_meet_guardian).unwrap().is_some());
            }

            #[test]
            fn node_kinds_are_correct() {
                let db = $db_expr;
                let s = build_story(&db);

                assert_eq!(db.get_node(s.book).unwrap().unwrap().kind, "book");
                assert_eq!(db.get_node(s.chapter1).unwrap().unwrap().kind, "chapter");
                assert_eq!(db.get_node(s.scene1).unwrap().unwrap().kind, "scene");
                assert_eq!(db.get_node(s.elena).unwrap().unwrap().kind, "character");
                assert_eq!(db.get_node(s.forest_path).unwrap().unwrap().kind, "location");
                assert_eq!(db.get_node(s.plot_find_map).unwrap().unwrap().kind, "plot_point");
            }

            // -----------------------------------------------------------------
            // 2. Tree structure — book → chapter → scene
            // -----------------------------------------------------------------

            #[test]
            fn book_contains_chapters() {
                let db = $db_expr;
                let s = build_story(&db);

                let chapters = db
                    .neighbors(s.book, Direction::Outgoing, Some("contains"))
                    .unwrap();
                assert_eq!(chapters.len(), 2);
                let ids: Vec<u64> = chapters.iter().map(|n| n.id).collect();
                assert!(ids.contains(&s.chapter1));
                assert!(ids.contains(&s.chapter2));
            }

            #[test]
            fn chapter1_contains_scenes() {
                let db = $db_expr;
                let s = build_story(&db);

                let scenes = db
                    .neighbors(s.chapter1, Direction::Outgoing, Some("contains"))
                    .unwrap();
                assert_eq!(scenes.len(), 2);
                let ids: Vec<u64> = scenes.iter().map(|n| n.id).collect();
                assert!(ids.contains(&s.scene1));
                assert!(ids.contains(&s.scene2));
            }

            #[test]
            fn chapter2_contains_scenes() {
                let db = $db_expr;
                let s = build_story(&db);

                let scenes = db
                    .neighbors(s.chapter2, Direction::Outgoing, Some("contains"))
                    .unwrap();
                assert_eq!(scenes.len(), 2);
                let ids: Vec<u64> = scenes.iter().map(|n| n.id).collect();
                assert!(ids.contains(&s.scene3));
                assert!(ids.contains(&s.scene4));
            }

            #[test]
            fn scene_belongs_to_chapter_via_incoming() {
                let db = $db_expr;
                let s = build_story(&db);

                let parents = db
                    .neighbors(s.scene1, Direction::Incoming, Some("contains"))
                    .unwrap();
                assert_eq!(parents.len(), 1);
                assert_eq!(parents[0].id, s.chapter1);
            }

            // -----------------------------------------------------------------
            // 3. Narrative ordering — follows edges
            // -----------------------------------------------------------------

            #[test]
            fn chapters_follow_in_order() {
                let db = $db_expr;
                let s = build_story(&db);

                let next_chapters = db
                    .neighbors(s.chapter1, Direction::Outgoing, Some("follows"))
                    .unwrap();
                assert_eq!(next_chapters.len(), 1);
                assert_eq!(next_chapters[0].id, s.chapter2);
            }

            #[test]
            fn scenes_follow_within_chapter() {
                let db = $db_expr;
                let s = build_story(&db);

                // Chapter 1: scene1 → scene2
                let next = db
                    .neighbors(s.scene1, Direction::Outgoing, Some("follows"))
                    .unwrap();
                assert_eq!(next.len(), 1);
                assert_eq!(next[0].id, s.scene2);

                // Chapter 2: scene3 → scene4
                let next = db
                    .neighbors(s.scene3, Direction::Outgoing, Some("follows"))
                    .unwrap();
                assert_eq!(next.len(), 1);
                assert_eq!(next[0].id, s.scene4);
            }

            #[test]
            fn no_follows_from_last_scene_in_chapter() {
                let db = $db_expr;
                let s = build_story(&db);

                // scene2 is last in chapter1 — no follows edge
                let next = db
                    .neighbors(s.scene2, Direction::Outgoing, Some("follows"))
                    .unwrap();
                assert!(next.is_empty());

                // scene4 is last in chapter2
                let next = db
                    .neighbors(s.scene4, Direction::Outgoing, Some("follows"))
                    .unwrap();
                assert!(next.is_empty());
            }

            // -----------------------------------------------------------------
            // 4. Scene details — characters, locations, plot points
            // -----------------------------------------------------------------

            #[test]
            fn scene_involves_characters() {
                let db = $db_expr;
                let s = build_story(&db);

                // Scene 1: only Elena
                let chars = db
                    .neighbors(s.scene1, Direction::Outgoing, Some("involves"))
                    .unwrap();
                assert_eq!(chars.len(), 1);
                assert_eq!(chars[0].id, s.elena);

                // Scene 2: Elena and Marcus
                let chars = db
                    .neighbors(s.scene2, Direction::Outgoing, Some("involves"))
                    .unwrap();
                assert_eq!(chars.len(), 2);
                let ids: Vec<u64> = chars.iter().map(|n| n.id).collect();
                assert!(ids.contains(&s.elena));
                assert!(ids.contains(&s.marcus));

                // Scene 4: Elena and Guardian
                let chars = db
                    .neighbors(s.scene4, Direction::Outgoing, Some("involves"))
                    .unwrap();
                assert_eq!(chars.len(), 2);
                let ids: Vec<u64> = chars.iter().map(|n| n.id).collect();
                assert!(ids.contains(&s.elena));
                assert!(ids.contains(&s.guardian));
            }

            #[test]
            fn scene_takes_place_in_location() {
                let db = $db_expr;
                let s = build_story(&db);

                let loc = db
                    .neighbors(s.scene1, Direction::Outgoing, Some("takes_place_in"))
                    .unwrap();
                assert_eq!(loc.len(), 1);
                assert_eq!(loc[0].id, s.forest_path);

                let loc = db
                    .neighbors(s.scene2, Direction::Outgoing, Some("takes_place_in"))
                    .unwrap();
                assert_eq!(loc.len(), 1);
                assert_eq!(loc[0].id, s.garden_wall);
            }

            #[test]
            fn scene_relates_to_plot_point() {
                let db = $db_expr;
                let s = build_story(&db);

                let plot = db
                    .neighbors(s.scene1, Direction::Outgoing, Some("relates_to"))
                    .unwrap();
                assert_eq!(plot.len(), 1);
                assert_eq!(plot[0].id, s.plot_find_map);

                let plot = db
                    .neighbors(s.scene4, Direction::Outgoing, Some("relates_to"))
                    .unwrap();
                assert_eq!(plot.len(), 1);
                assert_eq!(plot[0].id, s.plot_meet_guardian);
            }

            // -----------------------------------------------------------------
            // 5. Character graph — relationships between characters
            // -----------------------------------------------------------------

            #[test]
            fn character_relationship_exists() {
                let db = $db_expr;
                let s = build_story(&db);

                let related = db
                    .neighbors(s.elena, Direction::Outgoing, Some("relates_to"))
                    .unwrap();
                assert_eq!(related.len(), 1);
                assert_eq!(related[0].id, s.marcus);
            }

            #[test]
            fn find_all_scenes_for_character() {
                let db = $db_expr;
                let s = build_story(&db);

                // Elena appears in all 4 scenes (via incoming "involves" edges)
                let scenes = db
                    .neighbors(s.elena, Direction::Incoming, Some("involves"))
                    .unwrap();
                assert_eq!(scenes.len(), 4);
                let ids: Vec<u64> = scenes.iter().map(|n| n.id).collect();
                assert!(ids.contains(&s.scene1));
                assert!(ids.contains(&s.scene2));
                assert!(ids.contains(&s.scene3));
                assert!(ids.contains(&s.scene4));

                // Marcus appears in scene2 and scene3
                let scenes = db
                    .neighbors(s.marcus, Direction::Incoming, Some("involves"))
                    .unwrap();
                assert_eq!(scenes.len(), 2);
                let ids: Vec<u64> = scenes.iter().map(|n| n.id).collect();
                assert!(ids.contains(&s.scene2));
                assert!(ids.contains(&s.scene3));

                // Guardian appears only in scene4
                let scenes = db
                    .neighbors(s.guardian, Direction::Incoming, Some("involves"))
                    .unwrap();
                assert_eq!(scenes.len(), 1);
                assert_eq!(scenes[0].id, s.scene4);
            }

            #[test]
            fn find_all_scenes_at_location() {
                let db = $db_expr;
                let s = build_story(&db);

                // The lost garden hosts scene3 and scene4
                let scenes = db
                    .neighbors(s.lost_garden, Direction::Incoming, Some("takes_place_in"))
                    .unwrap();
                assert_eq!(scenes.len(), 2);
                let ids: Vec<u64> = scenes.iter().map(|n| n.id).collect();
                assert!(ids.contains(&s.scene3));
                assert!(ids.contains(&s.scene4));
            }

            // -----------------------------------------------------------------
            // 6. BFS — navigating the narrative tree
            // -----------------------------------------------------------------

            #[test]
            fn bfs_from_book_depth_1_finds_chapters() {
                let db = $db_expr;
                let s = build_story(&db);

                let reachable = db
                    .bfs(s.book, 1, Direction::Outgoing, Some("contains"))
                    .unwrap();
                assert_eq!(reachable.len(), 2);
                let ids: Vec<u64> = reachable.iter().map(|n| n.id).collect();
                assert!(ids.contains(&s.chapter1));
                assert!(ids.contains(&s.chapter2));
            }

            #[test]
            fn bfs_from_book_depth_2_contains_finds_scenes() {
                let db = $db_expr;
                let s = build_story(&db);

                let reachable = db
                    .bfs(s.book, 2, Direction::Outgoing, Some("contains"))
                    .unwrap();
                // 2 chapters + 4 scenes = 6 nodes
                assert_eq!(reachable.len(), 6);
                let ids: Vec<u64> = reachable.iter().map(|n| n.id).collect();
                assert!(ids.contains(&s.scene1));
                assert!(ids.contains(&s.scene4));
            }

            #[test]
            fn bfs_from_book_depth_3_all_edges_reaches_deep() {
                let db = $db_expr;
                let s = build_story(&db);

                // Following all edge kinds from the book, depth 3 should reach
                // characters, locations, and plot points via scenes
                let reachable = db
                    .bfs(s.book, 3, Direction::Outgoing, None)
                    .unwrap();
                let ids: Vec<u64> = reachable.iter().map(|n| n.id).collect();

                // Should reach characters through contains→contains→involves
                assert!(ids.contains(&s.elena), "should reach Elena through scene edges");
                assert!(ids.contains(&s.forest_path), "should reach location through scene edges");
                assert!(ids.contains(&s.plot_find_map), "should reach plot_point through scene edges");
            }

            // -----------------------------------------------------------------
            // 7. Subgraph — AI context extraction for co-authoring
            // -----------------------------------------------------------------

            #[test]
            fn subgraph_of_scene_depth_1_gives_ai_context() {
                let db = $db_expr;
                let s = build_story(&db);

                // Extracting context around scene2 for AI co-authoring
                let sg = db.subgraph(s.scene2, 1).unwrap();
                let node_ids: Vec<u64> = sg.nodes.iter().map(|n| n.id).collect();

                // Should include: scene2 itself, chapter1 (parent), scene1 (follows incoming),
                // elena, marcus (involves), garden_wall (takes_place_in), plot_open_gate (relates_to)
                assert!(node_ids.contains(&s.scene2), "scene2 itself");
                assert!(node_ids.contains(&s.chapter1), "parent chapter");
                assert!(node_ids.contains(&s.elena), "character Elena");
                assert!(node_ids.contains(&s.marcus), "character Marcus");
                assert!(node_ids.contains(&s.garden_wall), "location");
                assert!(node_ids.contains(&s.plot_open_gate), "plot point");
            }

            #[test]
            fn subgraph_of_chapter_depth_2_gives_full_chapter_context() {
                let db = $db_expr;
                let s = build_story(&db);

                let sg = db.subgraph(s.chapter1, 2).unwrap();
                let node_ids: Vec<u64> = sg.nodes.iter().map(|n| n.id).collect();

                // Chapter + scenes + their linked entities
                assert!(node_ids.contains(&s.chapter1));
                assert!(node_ids.contains(&s.scene1));
                assert!(node_ids.contains(&s.scene2));
                assert!(node_ids.contains(&s.elena));
                assert!(node_ids.contains(&s.marcus));
                assert!(node_ids.contains(&s.forest_path));
                assert!(node_ids.contains(&s.garden_wall));
                assert!(node_ids.contains(&s.plot_find_map));
                assert!(node_ids.contains(&s.plot_open_gate));
            }

            #[test]
            fn subgraph_of_book_depth_3_contains_all_story_elements() {
                let db = $db_expr;
                let s = build_story(&db);

                let sg = db.subgraph(s.book, 3).unwrap();
                // All 17 nodes should be reachable within depth 3
                assert_eq!(sg.nodes.len(), 17, "all 17 story nodes in subgraph");
            }

            // -----------------------------------------------------------------
            // 8. Kind index — board views
            // -----------------------------------------------------------------

            #[test]
            fn list_all_characters() {
                let db = $db_expr;
                let s = build_story(&db);

                let chars = db.list_nodes_by_kind("character", 100, 0).unwrap();
                assert_eq!(chars.len(), 3);
                let ids: Vec<u64> = chars.iter().map(|n| n.id).collect();
                assert!(ids.contains(&s.elena));
                assert!(ids.contains(&s.marcus));
                assert!(ids.contains(&s.guardian));
            }

            #[test]
            fn list_all_scenes() {
                let db = $db_expr;
                let s = build_story(&db);

                let scenes = db.list_nodes_by_kind("scene", 100, 0).unwrap();
                assert_eq!(scenes.len(), 4);
                let ids: Vec<u64> = scenes.iter().map(|n| n.id).collect();
                assert!(ids.contains(&s.scene1));
                assert!(ids.contains(&s.scene2));
                assert!(ids.contains(&s.scene3));
                assert!(ids.contains(&s.scene4));
            }

            #[test]
            fn list_all_locations() {
                let db = $db_expr;
                let s = build_story(&db);

                let locs = db.list_nodes_by_kind("location", 100, 0).unwrap();
                assert_eq!(locs.len(), 3);
                let ids: Vec<u64> = locs.iter().map(|n| n.id).collect();
                assert!(ids.contains(&s.forest_path));
                assert!(ids.contains(&s.garden_wall));
                assert!(ids.contains(&s.lost_garden));
            }

            #[test]
            fn list_all_plot_points() {
                let db = $db_expr;
                let _s = build_story(&db);

                let plots = db.list_nodes_by_kind("plot_point", 100, 0).unwrap();
                assert_eq!(plots.len(), 4);
            }

            #[test]
            fn list_chapters() {
                let db = $db_expr;
                let s = build_story(&db);

                let chapters = db.list_nodes_by_kind("chapter", 100, 0).unwrap();
                assert_eq!(chapters.len(), 2);
                let ids: Vec<u64> = chapters.iter().map(|n| n.id).collect();
                assert!(ids.contains(&s.chapter1));
                assert!(ids.contains(&s.chapter2));
            }

            // -----------------------------------------------------------------
            // 9. FTS — searching narrative content
            // -----------------------------------------------------------------

            #[test]
            fn fts_finds_character_by_name() {
                let db = $db_expr;
                let _s = build_story(&db);

                let results = db.search_fts("Elena", 10).unwrap();
                assert!(!results.is_empty(), "should find Elena");
                let has_elena = results.iter().any(|r| r.node.title == "Elena");
                assert!(has_elena);
            }

            #[test]
            fn fts_finds_scene_by_body_content() {
                let db = $db_expr;
                let _s = build_story(&db);

                let results = db.search_fts("parchment", 10).unwrap();
                assert!(!results.is_empty(), "should find scene by body text");
            }

            #[test]
            fn fts_finds_location_by_description() {
                let db = $db_expr;
                build_story(&db);

                let results = db.search_fts("moss covered stone wall", 10).unwrap();
                assert!(!results.is_empty(), "should find location");
            }

            #[test]
            fn fts_finds_plot_point() {
                let db = $db_expr;
                build_story(&db);

                let results = db.search_fts("translucent figure", 10).unwrap();
                assert!(!results.is_empty());
                let found_plot = results.iter().any(|r| r.node.kind == "plot_point" || r.node.kind == "scene");
                assert!(found_plot, "should find content mentioning translucent figure");
            }

            // -----------------------------------------------------------------
            // 10. Properties — character metadata
            // -----------------------------------------------------------------

            #[test]
            fn character_properties_stored() {
                let db = $db_expr;
                let s = build_story(&db);

                let elena = db.get_node(s.elena).unwrap().unwrap();
                assert_eq!(elena.properties.get("role").unwrap(), &serde_json::json!("protagonist"));
                assert_eq!(elena.properties.get("age").unwrap(), &serde_json::json!(28));

                let guardian = db.get_node(s.guardian).unwrap().unwrap();
                assert_eq!(guardian.properties.get("role").unwrap(), &serde_json::json!("antagonist"));
                assert_eq!(guardian.properties.get("species").unwrap(), &serde_json::json!("spirit"));
            }

            // -----------------------------------------------------------------
            // 11. Shortest path — narrative connections
            // -----------------------------------------------------------------

            #[test]
            fn shortest_path_from_book_to_character() {
                let db = $db_expr;
                let s = build_story(&db);

                // book → chapter → scene → character
                let path = db.shortest_path(s.book, s.elena).unwrap();
                assert!(path.is_some(), "should find path from book to Elena");
                let path = path.unwrap();
                assert_eq!(path.len(), 4); // book, chapter, scene, elena
                assert_eq!(path[0], s.book);
                assert_eq!(*path.last().unwrap(), s.elena);
            }

            #[test]
            fn shortest_path_between_characters_via_scene() {
                let db = $db_expr;
                let s = build_story(&db);

                // Elena relates_to Marcus directly
                let path = db.shortest_path(s.elena, s.marcus).unwrap();
                assert!(path.is_some());
                let path = path.unwrap();
                assert_eq!(path.len(), 2); // elena → marcus (direct relates_to)
            }

            #[test]
            fn no_path_from_location_to_book() {
                let db = $db_expr;
                let s = build_story(&db);

                // Locations have no outgoing edges toward the book
                let path = db.shortest_path(s.forest_path, s.book).unwrap();
                assert!(path.is_none());
            }

            // -----------------------------------------------------------------
            // 12. DFS — deep narrative exploration
            // -----------------------------------------------------------------

            #[test]
            fn dfs_from_book_explores_full_tree() {
                let db = $db_expr;
                let s = build_story(&db);

                let visited = db
                    .dfs(s.book, 5, Direction::Outgoing, None)
                    .unwrap();
                let ids: Vec<u64> = visited.iter().map(|n| n.id).collect();

                // Should reach chapters, scenes, and linked entities
                assert!(ids.contains(&s.chapter1));
                assert!(ids.contains(&s.scene1));
                assert!(ids.contains(&s.elena));
            }

            #[test]
            fn dfs_with_contains_filter_stays_in_tree() {
                let db = $db_expr;
                let s = build_story(&db);

                let visited = db
                    .dfs(s.book, 3, Direction::Outgoing, Some("contains"))
                    .unwrap();
                let ids: Vec<u64> = visited.iter().map(|n| n.id).collect();

                // Should reach chapters and scenes but NOT characters/locations
                assert!(ids.contains(&s.chapter1));
                assert!(ids.contains(&s.scene1));
                assert!(!ids.contains(&s.elena), "should not reach characters via contains only");
                assert!(!ids.contains(&s.forest_path), "should not reach locations via contains only");
            }

            // -----------------------------------------------------------------
            // 13. Update workflow — editing narrative elements
            // -----------------------------------------------------------------

            #[test]
            fn update_scene_body() {
                let db = $db_expr;
                let s = build_story(&db);

                db.update_node(
                    s.scene1,
                    NodePatch {
                        body: Some("Revised: Elena takes her morning walk and nearly trips over a root, revealing a hidden compartment beneath.".to_string()),
                        ..Default::default()
                    },
                )
                .unwrap();

                let updated = db.get_node(s.scene1).unwrap().unwrap();
                assert!(updated.body.contains("hidden compartment"));
            }

            #[test]
            fn update_character_properties() {
                let db = $db_expr;
                let s = build_story(&db);

                let mut new_props = HashMap::new();
                new_props.insert("role".to_string(), serde_json::json!("protagonist"));
                new_props.insert("age".to_string(), serde_json::json!(28));
                new_props.insert("arc".to_string(), serde_json::json!("from skeptic to believer"));

                db.update_node(
                    s.elena,
                    NodePatch {
                        properties: Some(Properties::from(new_props)),
                        ..Default::default()
                    },
                )
                .unwrap();

                let updated = db.get_node(s.elena).unwrap().unwrap();
                assert_eq!(
                    updated.properties.get("arc").unwrap(),
                    &serde_json::json!("from skeptic to believer")
                );
            }

            // -----------------------------------------------------------------
            // 14. Delete workflow — removing a scene
            // -----------------------------------------------------------------

            #[test]
            fn delete_scene_cascades_edges() {
                let db = $db_expr;
                let s = build_story(&db);

                // Delete scene1 — edges to chapter1, elena, forest_path, plot_find_map removed
                db.delete_node(s.scene1).unwrap();

                assert!(db.get_node(s.scene1).unwrap().is_none());

                // Chapter1 now contains only scene2
                let scenes = db
                    .neighbors(s.chapter1, Direction::Outgoing, Some("contains"))
                    .unwrap();
                assert_eq!(scenes.len(), 1);
                assert_eq!(scenes[0].id, s.scene2);

                // Elena still exists (no cascade to connected nodes)
                assert!(db.get_node(s.elena).unwrap().is_some());

                // Elena now appears in 3 scenes instead of 4
                let elena_scenes = db
                    .neighbors(s.elena, Direction::Incoming, Some("involves"))
                    .unwrap();
                assert_eq!(elena_scenes.len(), 3);
            }

            // -----------------------------------------------------------------
            // 15. Adding new chapter — extending the narrative
            // -----------------------------------------------------------------

            #[test]
            fn add_new_chapter_extends_story() {
                let db = $db_expr;
                let s = build_story(&db);

                let chapter3 = db
                    .create_node(make_node(
                        "chapter",
                        "The Bargain",
                        "The guardian offers a deal: tend the garden, or forget it exists forever.",
                    ))
                    .unwrap();
                db.create_edge(make_edge(s.book, chapter3.id, "contains"))
                    .unwrap();
                db.create_edge(make_weighted_edge(s.chapter2, chapter3.id, "follows", 1.0))
                    .unwrap();

                // Book now contains 3 chapters
                let chapters = db
                    .neighbors(s.book, Direction::Outgoing, Some("contains"))
                    .unwrap();
                assert_eq!(chapters.len(), 3);

                // Chapter2 → Chapter3 ordering
                let next = db
                    .neighbors(s.chapter2, Direction::Outgoing, Some("follows"))
                    .unwrap();
                assert_eq!(next.len(), 1);
                assert_eq!(next[0].id, chapter3.id);
            }

            // -----------------------------------------------------------------
            // 16. List recent — timeline of edits
            // -----------------------------------------------------------------

            #[test]
            fn list_recent_shows_newest_first() {
                let db = $db_expr;
                build_story(&db);

                std::thread::sleep(std::time::Duration::from_millis(10));
                let newest = db
                    .create_node(make_node(
                        "scene",
                        "Epilogue scene",
                        "Years later, Elena returns to the garden.",
                    ))
                    .unwrap();

                let recent = db.list_recent(3).unwrap();
                assert!(!recent.is_empty());
                assert_eq!(recent[0].id, newest.id, "newest entry should be first");
            }

            // -----------------------------------------------------------------
            // 17. Multi-location usage — shared locations across scenes
            // -----------------------------------------------------------------

            #[test]
            fn location_shared_across_scenes() {
                let db = $db_expr;
                let s = build_story(&db);

                // The lost garden is used in scene3 and scene4
                let scenes_at_garden = db
                    .neighbors(s.lost_garden, Direction::Incoming, Some("takes_place_in"))
                    .unwrap();
                assert_eq!(scenes_at_garden.len(), 2);

                // Forest path is used only in scene1
                let scenes_at_forest = db
                    .neighbors(s.forest_path, Direction::Incoming, Some("takes_place_in"))
                    .unwrap();
                assert_eq!(scenes_at_forest.len(), 1);
            }

            // -----------------------------------------------------------------
            // 18. Edge weights — scene ordering validation
            // -----------------------------------------------------------------

            #[test]
            fn follows_edges_have_correct_weights() {
                let db = $db_expr;
                let s = build_story(&db);

                let edges = db.edges_of(s.scene1, Direction::Outgoing).unwrap();
                let follows_edges: Vec<&Edge> =
                    edges.iter().filter(|e| e.kind == "follows").collect();
                assert_eq!(follows_edges.len(), 1);
                assert!((follows_edges[0].weight - 1.0).abs() < f32::EPSILON);
                assert_eq!(follows_edges[0].to_id, s.scene2);
            }
        }
    };
}

// Run all tests against MemoryBackend
story_tests!(memory_db(), memory);

// Run all tests against RedbBackend
#[cfg(feature = "redb-backend")]
mod redb {
    use super::*;

    macro_rules! story_redb_tests {
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
                fn build_complete_story() {
                    let (_dir, db) = setup();
                    let s = build_story(&db);
                    assert!(db.get_node(s.book).unwrap().is_some());
                    assert!(db.get_node(s.scene4).unwrap().is_some());
                    assert!(db.get_node(s.guardian).unwrap().is_some());
                }

                #[test]
                fn tree_structure_book_chapter_scene() {
                    let (_dir, db) = setup();
                    let s = build_story(&db);
                    let chapters = db
                        .neighbors(s.book, Direction::Outgoing, Some("contains"))
                        .unwrap();
                    assert_eq!(chapters.len(), 2);
                    let scenes = db
                        .neighbors(s.chapter1, Direction::Outgoing, Some("contains"))
                        .unwrap();
                    assert_eq!(scenes.len(), 2);
                }

                #[test]
                fn scene_involves_characters() {
                    let (_dir, db) = setup();
                    let s = build_story(&db);
                    let chars = db
                        .neighbors(s.scene2, Direction::Outgoing, Some("involves"))
                        .unwrap();
                    assert_eq!(chars.len(), 2);
                }

                #[test]
                fn bfs_from_book_reaches_scenes() {
                    let (_dir, db) = setup();
                    let s = build_story(&db);
                    let reachable = db
                        .bfs(s.book, 2, Direction::Outgoing, Some("contains"))
                        .unwrap();
                    assert_eq!(reachable.len(), 6);
                }

                #[test]
                fn shortest_path_works() {
                    let (_dir, db) = setup();
                    let s = build_story(&db);
                    let path = db.shortest_path(s.book, s.elena).unwrap();
                    assert!(path.is_some());
                    assert_eq!(path.unwrap().len(), 4);
                }

                #[test]
                fn subgraph_complete() {
                    let (_dir, db) = setup();
                    let s = build_story(&db);
                    let sg = db.subgraph(s.book, 3).unwrap();
                    assert_eq!(sg.nodes.len(), 17);
                }

                #[test]
                fn fts_works() {
                    let (_dir, db) = setup();
                    build_story(&db);
                    let results = db.search_fts("Elena", 10).unwrap();
                    assert!(!results.is_empty());
                }

                #[test]
                fn kind_index_works() {
                    let (_dir, db) = setup();
                    build_story(&db);
                    let chars = db.list_nodes_by_kind("character", 100, 0).unwrap();
                    assert_eq!(chars.len(), 3);
                    let scenes = db.list_nodes_by_kind("scene", 100, 0).unwrap();
                    assert_eq!(scenes.len(), 4);
                }

                #[test]
                fn properties_persist() {
                    let (_dir, db) = setup();
                    let s = build_story(&db);
                    let elena = db.get_node(s.elena).unwrap().unwrap();
                    assert_eq!(
                        elena.properties.get("role").unwrap(),
                        &serde_json::json!("protagonist")
                    );
                }

                #[test]
                fn delete_cascades_edges() {
                    let (_dir, db) = setup();
                    let s = build_story(&db);
                    db.delete_node(s.scene1).unwrap();
                    assert!(db.get_node(s.scene1).unwrap().is_none());
                    let scenes = db
                        .neighbors(s.chapter1, Direction::Outgoing, Some("contains"))
                        .unwrap();
                    assert_eq!(scenes.len(), 1);
                }
            }
        };
    }

    story_redb_tests!(tests);
}
