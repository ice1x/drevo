//! Triggers: Cypher statements a database runs when a transaction commits,
//! installed and called the way APOC's `apoc.trigger` is.
//!
//! ```cypher
//! CALL apoc.trigger.install('neo4j', 'stamp_updated_at',
//!   'UNWIND keys($assignedNodeProperties) AS k
//!    UNWIND $assignedNodeProperties[k] AS change
//!    WITH DISTINCT change.node AS n WHERE n:Task AND k <> "updated_at"
//!    SET n.updated_at = timestamp()',
//!   {phase: 'before'})
//! ```
//!
//! A trigger's statement sees what the transaction changed as parameters
//! ([`change_params`](crate::trigger::change_params)): `$createdNodes`,
//! `$deletedNodes`, `$createdRelationships`, `$deletedRelationships`,
//! `$assignedLabels`, `$removedLabels`, `$assignedNodeProperties`,
//! `$removedNodeProperties`, `$assignedRelationshipProperties` and
//! `$removedRelationshipProperties`.
//!
//! The [`TriggerRegistry`](crate::trigger::TriggerRegistry) of a database is
//! persisted in its `semantic.json` sidecar, next to the index and
//! constraint definitions.

use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;

use serde::{Deserialize, Serialize};

use crate::cypher::executor::{edge_to_value, json_to_value, node_to_value, Value};
use drevo_core::native::TxChanges;

/// When a trigger runs relative to the commit of the transaction that fired
/// it.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum TriggerPhase {
    /// Inside the transaction, before it commits; a failing trigger rolls the
    /// transaction back.
    #[serde(rename = "before")]
    Before,
    /// In a transaction of its own, right after the commit; a failure is
    /// logged and the committed transaction stands.
    #[default]
    #[serde(rename = "after")]
    After,
    /// Like `after`, off the committing request's path.
    #[serde(rename = "afterAsync")]
    AfterAsync,
}

impl TriggerPhase {
    /// The phase named `name` in a trigger selector (`before`, `after`,
    /// `afterAsync`), if any.
    #[must_use]
    pub fn parse(name: &str) -> Option<Self> {
        match name {
            "before" => Some(TriggerPhase::Before),
            "after" => Some(TriggerPhase::After),
            "afterAsync" => Some(TriggerPhase::AfterAsync),
            _ => None,
        }
    }

    /// The selector name of the phase.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            TriggerPhase::Before => "before",
            TriggerPhase::After => "after",
            TriggerPhase::AfterAsync => "afterAsync",
        }
    }
}

/// One installed trigger.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Trigger {
    /// The trigger name, unique within its database.
    pub name: String,
    /// The Cypher statement it runs.
    pub statement: String,
    /// When it runs.
    #[serde(default)]
    pub phase: TriggerPhase,
    /// Extra parameters the statement sees next to the change parameters.
    #[serde(default)]
    pub params: BTreeMap<String, serde_json::Value>,
    /// Whether it is stopped (`apoc.trigger.stop`): kept, but not run.
    #[serde(default)]
    pub paused: bool,
}

/// The triggers of one database, in installation order.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct TriggerRegistry {
    triggers: Vec<Trigger>,
}

impl TriggerRegistry {
    /// The trigger named `name`, if installed.
    #[must_use]
    pub fn get(&self, name: &str) -> Option<&Trigger> {
        self.triggers.iter().find(|t| t.name == name)
    }

    /// Every installed trigger, in installation order.
    #[must_use]
    pub fn list(&self) -> &[Trigger] {
        &self.triggers
    }

    /// Install `trigger`, replacing one of the same name in place. Returns the
    /// replaced trigger.
    pub fn install(&mut self, trigger: Trigger) -> Option<Trigger> {
        match self.triggers.iter_mut().find(|t| t.name == trigger.name) {
            Some(slot) => Some(std::mem::replace(slot, trigger)),
            None => {
                self.triggers.push(trigger);
                None
            }
        }
    }

    /// Remove the trigger named `name`, returning it.
    pub fn remove(&mut self, name: &str) -> Option<Trigger> {
        let at = self.triggers.iter().position(|t| t.name == name)?;
        Some(self.triggers.remove(at))
    }

    /// Remove every trigger, returning them in installation order.
    pub fn clear(&mut self) -> Vec<Trigger> {
        std::mem::take(&mut self.triggers)
    }

    /// Stop (`paused = true`) or start the trigger named `name`. Returns the
    /// updated trigger, or `None` if there is no such trigger.
    pub fn set_paused(&mut self, name: &str, paused: bool) -> Option<&Trigger> {
        let t = self.triggers.iter_mut().find(|t| t.name == name)?;
        t.paused = paused;
        Some(t)
    }

    /// The triggers that run in `phase`, in installation order.
    pub fn active(&self, phase: TriggerPhase) -> impl Iterator<Item = &Trigger> {
        self.triggers
            .iter()
            .filter(move |t| !t.paused && t.phase == phase)
    }

    /// Whether any trigger would run on a commit.
    #[must_use]
    pub fn has_active(&self) -> bool {
        self.triggers.iter().any(|t| !t.paused)
    }
}

thread_local! {
    /// Set while a trigger's statement runs on this thread.
    static IN_TRIGGER: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Whether the current thread is running a trigger's statement. Its writes
/// do not fire triggers again.
#[must_use]
pub fn in_trigger() -> bool {
    IN_TRIGGER.with(std::cell::Cell::get)
}

/// A trigger whose statement failed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TriggerFailure {
    /// The trigger's name.
    pub name: String,
    /// Why it failed.
    pub message: String,
}

impl std::fmt::Display for TriggerFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "trigger `{}` failed: {}", self.name, self.message)
    }
}

/// Run `trigger`'s statement through `exec` with the change parameters and
/// the trigger's own parameters (a change parameter wins a name clash),
/// marking the thread as running a trigger meanwhile.
///
/// # Errors
///
/// A [`TriggerFailure`] when the statement does not parse or fails.
pub fn run<F>(
    trigger: &Trigger,
    changes: &HashMap<String, Value>,
    exec: F,
) -> Result<(), TriggerFailure>
where
    F: FnOnce(
        &crate::cypher::ast::Query,
        HashMap<String, Value>,
    )
        -> Result<crate::cypher::executor::ExecResult, crate::cypher::executor::ExecError>,
{
    let failure = |message: String| TriggerFailure {
        name: trigger.name.clone(),
        message,
    };
    let query =
        crate::cypher::parser::parse(&trigger.statement).map_err(|e| failure(e.to_string()))?;
    let mut params: HashMap<String, Value> = trigger
        .params
        .iter()
        .map(|(k, v)| (k.clone(), json_to_value(v)))
        .collect();
    params.extend(changes.iter().map(|(k, v)| (k.clone(), v.clone())));
    let previous = IN_TRIGGER.with(|flag| flag.replace(true));
    let result = exec(&query, params);
    IN_TRIGGER.with(|flag| flag.set(previous));
    result.map(|_| ()).map_err(|e| failure(e.to_string()))
}

/// Collects `{key: [entry, …]}` maps, keeping keys sorted.
#[derive(Default)]
struct Grouped(BTreeMap<String, Vec<Value>>);

impl Grouped {
    fn push(&mut self, key: &str, entry: Value) {
        self.0.entry(key.to_string()).or_default().push(entry);
    }

    fn into_value(self) -> Value {
        Value::Map(
            self.0
                .into_iter()
                .map(|(k, v)| (k, Value::List(v)))
                .collect(),
        )
    }
}

fn entry(pairs: Vec<(&str, Value)>) -> Value {
    Value::Map(pairs.into_iter().map(|(k, v)| (k.to_string(), v)).collect())
}

/// Property assignments and removals between two property maps, recorded
/// against `owner` under the field name `owner_key` (`node` or
/// `relationship`).
fn diff_properties(
    old: &BTreeMap<String, Value>,
    new: &BTreeMap<String, Value>,
    owner_key: &str,
    owner: &Value,
    assigned: &mut Grouped,
    removed: &mut Grouped,
) {
    for (key, value) in new {
        let before = old.get(key);
        if before == Some(value) {
            continue;
        }
        assigned.push(
            key,
            entry(vec![
                ("key", Value::String(key.clone())),
                ("old", before.cloned().unwrap_or(Value::Null)),
                ("new", value.clone()),
                (owner_key, owner.clone()),
            ]),
        );
    }
    for (key, value) in old {
        if new.contains_key(key) {
            continue;
        }
        removed.push(
            key,
            entry(vec![
                ("key", Value::String(key.clone())),
                ("old", value.clone()),
                (owner_key, owner.clone()),
            ]),
        );
    }
}

/// The parameters a trigger statement sees for `changes`, named and shaped
/// as in APOC:
///
/// * `createdNodes`, `deletedNodes`, `createdRelationships`,
///   `deletedRelationships` — lists of nodes / relationships (a deleted one
///   as it was before the transaction);
/// * `assignedLabels`, `removedLabels` — `{label: [node, …]}`;
/// * `assignedNodeProperties` — `{key: [{key, old, new, node}, …]}`, and
///   `removedNodeProperties` — `{key: [{key, old, node}, …]}`; likewise
///   `assignedRelationshipProperties` / `removedRelationshipProperties` with
///   `relationship`.
///
/// As in Neo4j, the labels and properties of a created node count as
/// assigned, and those of a deleted node as removed.
#[must_use]
pub fn change_params(changes: &TxChanges) -> HashMap<String, Value> {
    let mut created_nodes = Vec::new();
    let mut deleted_nodes = Vec::new();
    let mut assigned_labels = Grouped::default();
    let mut removed_labels = Grouped::default();
    let mut assigned_node_props = Grouped::default();
    let mut removed_node_props = Grouped::default();
    let empty = BTreeMap::new();

    for change in &changes.nodes {
        let before = change.before.as_deref().map(node_to_value);
        let after = change.after.as_deref().map(node_to_value);
        // The node the entries point at: its new version, or its last one.
        let Some(current) = after.as_ref().or(before.as_ref()).map(Arc::clone) else {
            continue;
        };
        let node = Value::Node(Arc::clone(&current));
        match (&before, &after) {
            (None, Some(_)) => created_nodes.push(node.clone()),
            (Some(_), None) => deleted_nodes.push(node.clone()),
            _ => {}
        }
        let old_labels = before.as_ref().map_or(&[][..], |n| n.labels.as_slice());
        let new_labels = after.as_ref().map_or(&[][..], |n| n.labels.as_slice());
        for label in new_labels.iter().filter(|l| !old_labels.contains(l)) {
            assigned_labels.push(label, node.clone());
        }
        for label in old_labels.iter().filter(|l| !new_labels.contains(l)) {
            removed_labels.push(label, node.clone());
        }
        diff_properties(
            before.as_ref().map_or(&empty, |n| &n.properties),
            after.as_ref().map_or(&empty, |n| &n.properties),
            "node",
            &node,
            &mut assigned_node_props,
            &mut removed_node_props,
        );
    }

    let mut created_rels = Vec::new();
    let mut deleted_rels = Vec::new();
    let mut assigned_rel_props = Grouped::default();
    let mut removed_rel_props = Grouped::default();
    for change in &changes.edges {
        let before = change.before.as_deref().map(edge_to_value);
        let after = change.after.as_deref().map(edge_to_value);
        let Some(current) = after.as_ref().or(before.as_ref()).map(Arc::clone) else {
            continue;
        };
        let rel = Value::Relationship(current);
        match (&before, &after) {
            (None, Some(_)) => created_rels.push(rel.clone()),
            (Some(_), None) => deleted_rels.push(rel.clone()),
            _ => {}
        }
        diff_properties(
            before.as_ref().map_or(&empty, |r| &r.properties),
            after.as_ref().map_or(&empty, |r| &r.properties),
            "relationship",
            &rel,
            &mut assigned_rel_props,
            &mut removed_rel_props,
        );
    }

    HashMap::from([
        ("createdNodes".to_string(), Value::List(created_nodes)),
        ("deletedNodes".to_string(), Value::List(deleted_nodes)),
        (
            "createdRelationships".to_string(),
            Value::List(created_rels),
        ),
        (
            "deletedRelationships".to_string(),
            Value::List(deleted_rels),
        ),
        ("assignedLabels".to_string(), assigned_labels.into_value()),
        ("removedLabels".to_string(), removed_labels.into_value()),
        (
            "assignedNodeProperties".to_string(),
            assigned_node_props.into_value(),
        ),
        (
            "removedNodeProperties".to_string(),
            removed_node_props.into_value(),
        ),
        (
            "assignedRelationshipProperties".to_string(),
            assigned_rel_props.into_value(),
        ),
        (
            "removedRelationshipProperties".to_string(),
            removed_rel_props.into_value(),
        ),
    ])
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Edge, Node, Properties};
    use drevo_core::native::Change;

    fn node(id: u64, kind: &str, props: &[(&str, serde_json::Value)]) -> Arc<Node> {
        let mut properties = Properties::default();
        for (k, v) in props {
            properties.insert(*k, v.clone());
        }
        Arc::new(Node {
            id,
            uuid: [0; 16],
            kind: kind.to_string(),
            title: String::new(),
            body: String::new(),
            body_html: String::new(),
            created_at: 0,
            updated_at: 0,
            properties,
        })
    }

    fn edge(id: u64, props: &[(&str, serde_json::Value)]) -> Arc<Edge> {
        let mut properties = Properties::default();
        for (k, v) in props {
            properties.insert(*k, v.clone());
        }
        Arc::new(Edge {
            id,
            uuid: [0; 16],
            from_id: 1,
            to_id: 2,
            kind: "ASSIGNED_TO".to_string(),
            weight: 1.0,
            created_at: 0,
            properties,
        })
    }

    fn list_ids(v: &Value) -> Vec<u64> {
        match v {
            Value::List(items) => items
                .iter()
                .map(|i| match i {
                    Value::Node(n) => n.id,
                    Value::Relationship(r) => r.id,
                    other => panic!("not an entity: {other:?}"),
                })
                .collect(),
            other => panic!("not a list: {other:?}"),
        }
    }

    fn group<'a>(v: &'a Value, key: &str) -> &'a [Value] {
        match v {
            Value::Map(m) => match m.get(key) {
                Some(Value::List(items)) => items,
                other => panic!("no list under {key}: {other:?}"),
            },
            other => panic!("not a map: {other:?}"),
        }
    }

    fn field<'a>(entry: &'a Value, name: &str) -> &'a Value {
        match entry {
            Value::Map(m) => m.get(name).unwrap_or(&Value::Null),
            other => panic!("not a map: {other:?}"),
        }
    }

    fn keys(v: &Value) -> Vec<&str> {
        match v {
            Value::Map(m) => m.keys().map(String::as_str).collect(),
            other => panic!("not a map: {other:?}"),
        }
    }

    #[test]
    fn no_changes_give_empty_parameters() {
        let p = change_params(&TxChanges::default());
        for name in [
            "createdNodes",
            "deletedNodes",
            "createdRelationships",
            "deletedRelationships",
        ] {
            assert_eq!(p[name], Value::List(vec![]), "{name}");
        }
        for name in [
            "assignedLabels",
            "removedLabels",
            "assignedNodeProperties",
            "removedNodeProperties",
            "assignedRelationshipProperties",
            "removedRelationshipProperties",
        ] {
            assert_eq!(p[name], Value::Map(BTreeMap::new()), "{name}");
        }
    }

    #[test]
    fn a_created_node_assigns_its_labels_and_properties() {
        let task = node(7, "Task", &[("status", serde_json::json!("open"))]);
        let changes = TxChanges {
            nodes: vec![Change {
                before: None,
                after: Some(task),
            }],
            edges: vec![],
        };
        let p = change_params(&changes);
        assert_eq!(list_ids(&p["createdNodes"]), vec![7]);
        assert_eq!(list_ids(&p["deletedNodes"]), Vec::<u64>::new());
        assert_eq!(keys(&p["assignedLabels"]), vec!["Task"]);
        let status = group(&p["assignedNodeProperties"], "status");
        assert_eq!(status.len(), 1);
        assert_eq!(field(&status[0], "key"), &Value::String("status".into()));
        assert_eq!(field(&status[0], "old"), &Value::Null);
        assert_eq!(field(&status[0], "new"), &Value::String("open".into()));
        assert!(matches!(field(&status[0], "node"), Value::Node(n) if n.id == 7));
    }

    #[test]
    fn an_updated_node_reports_only_what_changed() {
        let before = node(
            3,
            "Task",
            &[
                ("status", serde_json::json!("open")),
                ("owner", serde_json::json!("ann")),
                ("estimate", serde_json::json!(3)),
            ],
        );
        let after = node(
            3,
            "Task",
            &[
                ("status", serde_json::json!("done")),
                ("owner", serde_json::json!("ann")),
                ("_labels", serde_json::json!(["Urgent"])),
            ],
        );
        let p = change_params(&TxChanges {
            nodes: vec![Change {
                before: Some(before),
                after: Some(after),
            }],
            edges: vec![],
        });
        assert_eq!(list_ids(&p["createdNodes"]), Vec::<u64>::new());
        assert_eq!(keys(&p["assignedNodeProperties"]), vec!["status"]);
        let status = group(&p["assignedNodeProperties"], "status");
        assert_eq!(field(&status[0], "old"), &Value::String("open".into()));
        assert_eq!(field(&status[0], "new"), &Value::String("done".into()));
        assert_eq!(keys(&p["removedNodeProperties"]), vec!["estimate"]);
        let estimate = group(&p["removedNodeProperties"], "estimate");
        assert_eq!(field(&estimate[0], "old"), &Value::Integer(3));
        assert_eq!(field(&estimate[0], "new"), &Value::Null);
        // A secondary label is a label, not a property.
        assert_eq!(keys(&p["assignedLabels"]), vec!["Urgent"]);
        assert_eq!(keys(&p["removedLabels"]), Vec::<&str>::new());
    }

    #[test]
    fn a_deleted_node_removes_its_labels_and_properties() {
        let gone = node(9, "Task", &[("status", serde_json::json!("done"))]);
        let p = change_params(&TxChanges {
            nodes: vec![Change {
                before: Some(gone),
                after: None,
            }],
            edges: vec![],
        });
        assert_eq!(list_ids(&p["deletedNodes"]), vec![9]);
        assert_eq!(keys(&p["removedLabels"]), vec!["Task"]);
        let status = group(&p["removedNodeProperties"], "status");
        assert!(matches!(field(&status[0], "node"), Value::Node(n) if n.id == 9));
        assert_eq!(keys(&p["assignedNodeProperties"]), Vec::<&str>::new());
    }

    #[test]
    fn relationship_changes_use_the_relationship_key() {
        let created = edge(1, &[("since", serde_json::json!(2026))]);
        let old = edge(2, &[("role", serde_json::json!("dev"))]);
        let new = edge(2, &[("role", serde_json::json!("lead"))]);
        let gone = edge(3, &[]);
        let p = change_params(&TxChanges {
            nodes: vec![],
            edges: vec![
                Change {
                    before: None,
                    after: Some(created),
                },
                Change {
                    before: Some(old),
                    after: Some(new),
                },
                Change {
                    before: Some(gone),
                    after: None,
                },
            ],
        });
        assert_eq!(list_ids(&p["createdRelationships"]), vec![1]);
        assert_eq!(list_ids(&p["deletedRelationships"]), vec![3]);
        assert_eq!(
            keys(&p["assignedRelationshipProperties"]),
            vec!["role", "since"]
        );
        let role = group(&p["assignedRelationshipProperties"], "role");
        assert_eq!(field(&role[0], "old"), &Value::String("dev".into()));
        assert!(matches!(field(&role[0], "relationship"), Value::Relationship(r) if r.id == 2));
    }

    fn trigger(name: &str, phase: TriggerPhase) -> Trigger {
        Trigger {
            name: name.to_string(),
            statement: "RETURN 1".to_string(),
            phase,
            params: BTreeMap::new(),
            paused: false,
        }
    }

    #[test]
    fn the_registry_installs_replaces_stops_and_removes() {
        let mut r = TriggerRegistry::default();
        assert!(!r.has_active());
        assert!(r.install(trigger("a", TriggerPhase::Before)).is_none());
        assert!(r.install(trigger("b", TriggerPhase::After)).is_none());
        let replaced = r.install(Trigger {
            statement: "RETURN 2".to_string(),
            ..trigger("a", TriggerPhase::Before)
        });
        assert_eq!(replaced.map(|t| t.statement), Some("RETURN 1".to_string()));
        let names: Vec<&str> = r.list().iter().map(|t| t.name.as_str()).collect();
        assert_eq!(names, vec!["a", "b"], "a replaced trigger keeps its place");

        assert!(r.set_paused("a", true).is_some_and(|t| t.paused));
        assert_eq!(r.active(TriggerPhase::Before).count(), 0);
        assert_eq!(r.active(TriggerPhase::After).count(), 1);
        assert!(r.has_active());
        assert!(r.set_paused("missing", true).is_none());

        assert_eq!(r.remove("b").map(|t| t.name), Some("b".to_string()));
        assert!(!r.has_active(), "only the stopped trigger is left");
        assert_eq!(r.clear().len(), 1);
        assert!(r.list().is_empty());
    }

    #[test]
    fn phases_round_trip_by_name_and_in_the_sidecar() {
        for phase in [
            TriggerPhase::Before,
            TriggerPhase::After,
            TriggerPhase::AfterAsync,
        ] {
            assert_eq!(TriggerPhase::parse(phase.name()), Some(phase));
        }
        assert_eq!(TriggerPhase::parse("rollback"), None);

        let mut r = TriggerRegistry::default();
        r.install(Trigger {
            params: BTreeMap::from([("limit".to_string(), serde_json::json!(5))]),
            ..trigger("audit", TriggerPhase::AfterAsync)
        });
        let json = serde_json::to_string(&r).unwrap();
        assert!(json.contains("\"afterAsync\""), "{json}");
        let back: TriggerRegistry = serde_json::from_str(&json).unwrap();
        assert_eq!(back, r);
        // A definition with only a name and a statement loads as `after`.
        let minimal: Trigger =
            serde_json::from_str(r#"{"name":"t","statement":"RETURN 1"}"#).unwrap();
        assert_eq!(minimal.phase, TriggerPhase::After);
        assert!(!minimal.paused);
    }
}
