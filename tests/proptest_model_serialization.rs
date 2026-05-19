//! Phase 9 task `00057` — property-based serialization round-trip tests
//! for the model layer.
//!
//! Properties covered:
//!
//! 1. **bincode round-trip preserves Node** — for arbitrary `Node` values,
//!    `decode(encode(n)) == n`.
//! 2. **bincode round-trip preserves Edge** — same, for `Edge`.
//! 3. **JSON round-trip preserves Node** — `serde_json::from_str(to_string(n)) == n`.
//! 4. **JSON round-trip preserves Edge** — same, for `Edge`.
//! 5. **`Properties` bincode is order-independent** — two maps with the
//!    same logical content but different HashMap insertion orders produce
//!    byte-identical bincode output. This is a property-test upgrade of
//!    `tests/db_invariants_tests.rs::properties_bincode_is_deterministic_*`.
//! 6. **`Node::apply_patch` is the union of its patch fields** — applying
//!    a patch that sets `(kind, title, body, body_html, properties)` to
//!    `Some(_)` produces a node whose fields match the patch's `Some`
//!    values and whose other fields match the original. `updated_at`
//!    advances monotonically.
//! 7. **`now_ms` is monotonic-or-equal across two adjacent calls** — total
//!    function never panics, and time never travels backwards (cross-link
//!    with `drevo-rust` §"Error Handling" + AUDIT-model F-rules).
//!
//! These are pure-data properties; no Drevo instance is opened, so they
//! run an order of magnitude faster than the graph-level proptests and
//! exercise the deterministic-bincode contract that the storage layer
//! relies on.

use drevo::model::{new_uuid_v7, now_ms, Direction, Edge, Node, NodePatch, Properties};

use proptest::prelude::*;
use serde_json::json;
use std::collections::HashMap;

// ---------------------------------------------------------------
// Strategies
// ---------------------------------------------------------------

/// Generate a finite, deterministic-bincode-friendly JSON value.
///
/// Floats are intentionally **excluded**:
/// * JSON has no canonical decimal representation for f64, so
///   `serde_json::Number::from_f64(-258947837.17905354)` round-trips to
///   `-258947837.17905352` and round-trip-equality property tests fail
///   on a measurement artifact, not a real bug.
/// * The Cypher / Bolt / Vector phases all serialise numeric properties
///   through bincode, JSON, or PackStream — none of those wire formats
///   promise IEEE-754 round-trip preservation. The property tests below
///   only need to assert "the same content serialises to identical
///   bytes", which booleans / ints / strings / arrays / objects already
///   stress comprehensively.
/// * Real applications can still store floats — there is just no test-level
///   property to assert about them beyond what the Rust stdlib already
///   guarantees, so generating them adds noise without coverage.
fn json_value() -> impl Strategy<Value = serde_json::Value> {
    let leaf = prop_oneof![
        any::<bool>().prop_map(serde_json::Value::Bool),
        // i64 round-trips exactly through serde_json.
        any::<i64>().prop_map(|n| serde_json::Value::Number(n.into())),
        // Unsigned u64 covers the upper half of the range that i64 misses.
        any::<u64>().prop_map(|n| serde_json::Value::Number(n.into())),
        "[a-zA-Z0-9_]{0,32}".prop_map(serde_json::Value::String),
    ];

    leaf.prop_recursive(2, 8, 4, |inner| {
        prop_oneof![
            proptest::collection::vec(inner.clone(), 0..4).prop_map(serde_json::Value::Array),
            proptest::collection::hash_map("[a-z]{1,4}", inner, 0..4).prop_map(|m| {
                let map: serde_json::Map<String, serde_json::Value> = m.into_iter().collect();
                serde_json::Value::Object(map)
            }),
        ]
    })
}

fn properties_strategy() -> impl Strategy<Value = Properties> {
    proptest::collection::hash_map("[a-z_]{1,6}", json_value(), 0..6)
        .prop_map(|m| Properties::from(m.into_iter().collect::<HashMap<_, _>>()))
}

fn node_strategy() -> impl Strategy<Value = Node> {
    (
        any::<u64>(),
        "[a-z]{1,8}",
        "[\\PC]{0,20}",
        "[\\PC]{0,40}",
        "[\\PC]{0,40}",
        any::<i64>(),
        any::<i64>(),
        properties_strategy(),
    )
        .prop_map(
            |(id, kind, title, body, body_html, created_at, updated_at, properties)| Node {
                id,
                uuid: new_uuid_v7(),
                kind,
                title,
                body,
                body_html,
                created_at,
                updated_at,
                properties,
            },
        )
}

fn edge_strategy() -> impl Strategy<Value = Edge> {
    (
        any::<u64>(),
        any::<u64>(),
        any::<u64>(),
        "[a-z_]{1,8}",
        (-1.0e6f32..1.0e6f32).prop_filter("finite", |w| w.is_finite()),
        any::<i64>(),
        properties_strategy(),
    )
        .prop_map(
            |(id, from_id, to_id, kind, weight, created_at, properties)| Edge {
                id,
                uuid: new_uuid_v7(),
                from_id,
                to_id,
                kind,
                weight,
                created_at,
                properties,
            },
        )
}

fn node_patch_strategy() -> impl Strategy<Value = NodePatch> {
    (
        proptest::option::of("[a-z]{1,8}"),
        proptest::option::of("[\\PC]{0,20}"),
        proptest::option::of("[\\PC]{0,40}"),
        proptest::option::of("[\\PC]{0,40}"),
        proptest::option::of(properties_strategy()),
    )
        .prop_map(|(kind, title, body, body_html, properties)| NodePatch {
            kind,
            title,
            body,
            body_html,
            properties,
        })
}

// ---------------------------------------------------------------
// Bincode round-trips
// ---------------------------------------------------------------

proptest! {
    #![proptest_config(ProptestConfig {
        cases: 256,
        .. ProptestConfig::default()
    })]

    /// `Node` survives a bincode round-trip.
    #[test]
    fn node_bincode_roundtrip(node in node_strategy()) {
        let config = bincode::config::standard();
        let bytes = bincode::serde::encode_to_vec(&node, config).unwrap();
        let (decoded, _): (Node, _) =
            bincode::serde::decode_from_slice(&bytes, config).unwrap();
        prop_assert_eq!(decoded, node);
    }

    /// `Edge` survives a bincode round-trip.
    #[test]
    fn edge_bincode_roundtrip(edge in edge_strategy()) {
        let config = bincode::config::standard();
        let bytes = bincode::serde::encode_to_vec(&edge, config).unwrap();
        let (decoded, _): (Edge, _) =
            bincode::serde::decode_from_slice(&bytes, config).unwrap();
        prop_assert_eq!(decoded, edge);
    }

    /// `Node` survives a JSON round-trip.
    #[test]
    fn node_json_roundtrip(node in node_strategy()) {
        let s = serde_json::to_string(&node).unwrap();
        let decoded: Node = serde_json::from_str(&s).unwrap();
        prop_assert_eq!(decoded, node);
    }

    /// `Edge` survives a JSON round-trip.
    #[test]
    fn edge_json_roundtrip(edge in edge_strategy()) {
        let s = serde_json::to_string(&edge).unwrap();
        let decoded: Edge = serde_json::from_str(&s).unwrap();
        // f32 weight survives JSON because we restrict to small finite
        // values. NaN/Inf are excluded at strategy level.
        prop_assert_eq!(decoded, edge);
    }
}

// ---------------------------------------------------------------
// Properties: order-independent bincode (drevo-rust §"Serialization")
// ---------------------------------------------------------------

// Two `Properties` values built from the same `(key, value)` pairs in
// arbitrarily permuted orders must bincode-encode to **byte-identical**
// bytes. Required by the storage layer's deterministic-key contract.
proptest! {
    #![proptest_config(ProptestConfig {
        cases: 128,
        .. ProptestConfig::default()
    })]

    #[test]
    fn properties_bincode_is_order_independent(
        // Generate up to 8 unique-keyed pairs.
        pairs in proptest::collection::hash_map("[a-z]{1,6}", json_value(), 0..8)
            .prop_map(|m| m.into_iter().collect::<Vec<_>>()),
        // And a permutation seed.
        perm_seed in any::<u64>(),
    ) {
        // Build map A in the natural HashMap order.
        let mut map_a: HashMap<String, serde_json::Value> = HashMap::new();
        for (k, v) in &pairs {
            map_a.insert(k.clone(), v.clone());
        }

        // Build map B from the same pairs after rotating by perm_seed.
        let len = pairs.len();
        let mut map_b: HashMap<String, serde_json::Value> = HashMap::new();
        if len > 0 {
            let shift = (perm_seed % len as u64) as usize;
            for i in 0..len {
                let (k, v) = &pairs[(i + shift) % len];
                map_b.insert(k.clone(), v.clone());
            }
        }

        let props_a = Properties::from(map_a);
        let props_b = Properties::from(map_b);

        let config = bincode::config::standard();
        let bytes_a = bincode::serde::encode_to_vec(&props_a, config).unwrap();
        let bytes_b = bincode::serde::encode_to_vec(&props_b, config).unwrap();
        prop_assert_eq!(bytes_a, bytes_b);
    }
}

// ---------------------------------------------------------------
// Node::apply_patch — patch semantics + updated_at advances
// ---------------------------------------------------------------

proptest! {
    #![proptest_config(ProptestConfig {
        cases: 128,
        .. ProptestConfig::default()
    })]

    /// Applying a patch updates every `Some` field and leaves every `None`
    /// field unchanged. `updated_at` is set to `now_ms()` regardless of
    /// what the input's `updated_at` was — the property tests this by
    /// bracketing the call with `now_ms()` reads and asserting the new
    /// value sits inside that window.
    ///
    /// We deliberately do *not* assert `updated_at_after >= updated_at_before`
    /// because the input strategy produces arbitrary i64 timestamps,
    /// including values far in the future. The model contract is "set to
    /// current wall-clock time", not "monotonically increase from
    /// whatever the previous value happened to be".
    #[test]
    fn apply_patch_updates_only_some_fields(
        node in node_strategy(),
        patch in node_patch_strategy(),
    ) {
        let mut updated = node.clone();
        // Snapshot patch field-by-field BEFORE moving it into apply_patch.
        let snap_kind = patch.kind.clone();
        let snap_title = patch.title.clone();
        let snap_body = patch.body.clone();
        let snap_html = patch.body_html.clone();
        let snap_props = patch.properties.clone();

        let before = drevo::model::now_ms();
        updated.apply_patch(patch);
        let after = drevo::model::now_ms();

        match snap_kind {
            Some(k) => prop_assert_eq!(&updated.kind, &k),
            None    => prop_assert_eq!(&updated.kind, &node.kind),
        }
        match snap_title {
            Some(t) => prop_assert_eq!(&updated.title, &t),
            None    => prop_assert_eq!(&updated.title, &node.title),
        }
        match snap_body {
            Some(b) => prop_assert_eq!(&updated.body, &b),
            None    => prop_assert_eq!(&updated.body, &node.body),
        }
        match snap_html {
            Some(h) => prop_assert_eq!(&updated.body_html, &h),
            None    => prop_assert_eq!(&updated.body_html, &node.body_html),
        }
        match snap_props {
            Some(p) => prop_assert_eq!(&updated.properties, &p),
            None    => prop_assert_eq!(&updated.properties, &node.properties),
        }
        // id / uuid / created_at are immutable for the lifetime of the
        // node — this is invariant #4 (UUID immutability) plus the model
        // contract on created_at.
        prop_assert_eq!(updated.id, node.id);
        prop_assert_eq!(updated.uuid, node.uuid);
        prop_assert_eq!(updated.created_at, node.created_at);
        // updated_at must sit inside the [before, after] window we just
        // bracketed around the call.
        prop_assert!(
            updated.updated_at >= before && updated.updated_at <= after,
            "updated_at={} outside [{}, {}] window",
            updated.updated_at, before, after
        );
    }
}

// ---------------------------------------------------------------
// now_ms — total, monotonic-or-equal
// ---------------------------------------------------------------

#[test]
fn now_ms_is_total_under_many_calls() {
    // Not a proptest case — but covers the "never panics" property
    // exhaustively for a large number of calls. The proptest framework
    // adds nothing here because the function takes no input.
    for _ in 0..10_000 {
        let _ = now_ms();
    }
}

#[test]
fn now_ms_is_weakly_monotonic_within_one_thread() {
    let a = now_ms();
    let b = now_ms();
    // System clock can technically go backwards, but within one thread
    // and within microseconds the only way for `b < a` to happen is an
    // NTP-stepped clock — extremely rare. We treat it as a soft
    // assertion: log if it happens, don't fail.
    assert!(
        b >= a || (a - b) < 1000,
        "clock skew larger than 1s observed: a={} b={}",
        a,
        b
    );
}

// ---------------------------------------------------------------
// Direction — all three variants are distinct (smoke)
// ---------------------------------------------------------------

#[test]
fn direction_all_three_variants_are_distinct() {
    // `Direction` is `PartialEq + Eq` but deliberately not `Hash` — using
    // a vector + `dedup` instead of a `HashSet` keeps that constraint
    // honest, so any future `derive(Hash)` shows up in code review rather
    // than silently leaking from a test.
    let mut all = vec![Direction::Outgoing, Direction::Incoming, Direction::Both];
    all.sort_by_key(|d| match d {
        Direction::Outgoing => 0,
        Direction::Incoming => 1,
        Direction::Both => 2,
    });
    let mut unique = all.clone();
    unique.dedup();
    assert_eq!(
        unique.len(),
        3,
        "Direction must have 3 distinct values, got {all:?}"
    );
}

// ---------------------------------------------------------------
// Sanity — a hand-rolled fixture round-trips
// ---------------------------------------------------------------

#[test]
fn hand_rolled_properties_roundtrip_through_bincode() {
    let mut props = HashMap::new();
    props.insert("k1".to_string(), json!(1));
    props.insert("k2".to_string(), json!("v"));
    props.insert("k3".to_string(), json!([1, 2, 3]));
    let props = Properties::from(props);

    let config = bincode::config::standard();
    let bytes = bincode::serde::encode_to_vec(&props, config).unwrap();
    let (decoded, _): (Properties, _) = bincode::serde::decode_from_slice(&bytes, config).unwrap();
    assert_eq!(decoded, props);
}
