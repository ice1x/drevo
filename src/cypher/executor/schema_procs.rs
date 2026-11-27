//! Procedures that describe the graph and the server to a client
//! (issue #869): `db.schema.nodeTypeProperties`, `db.schema.relTypeProperties`,
//! `db.schema.visualization`, `dbms.components`, and the lists behind
//! `SHOW PROCEDURES` and `SHOW FUNCTIONS`.
//!
//! The schema procedures read every node and relationship once per call:
//! what they report (which properties a label has, whether every node has
//! them, which labels a relationship type joins) is exact, not sampled.

use std::collections::{BTreeMap, BTreeSet};

use super::{
    deadline, edge_to_value, node_labels_from_storage, node_to_value, procedure_columns,
    ExecResultT, Executor, Rows, Value, AGGREGATIONS, PROCEDURE_NAMES, READ_PROCEDURES,
    SCALAR_FUNCTIONS, STORE_FUNCTIONS,
};

/// What is known of one property of a label set or a relationship type.
#[derive(Default)]
struct Seen {
    /// How many of the elements have the property.
    present: usize,
    /// The types of its values, as Neo4j names them.
    types: BTreeSet<String>,
}

/// The properties of the elements of one kind, and how many elements.
#[derive(Default)]
struct Kind {
    elements: usize,
    properties: BTreeMap<String, Seen>,
}

impl Kind {
    fn add(&mut self, properties: &BTreeMap<String, Value>) {
        self.elements += 1;
        for (key, value) in properties {
            if matches!(value, Value::Null) {
                continue;
            }
            let seen = self.properties.entry(key.clone()).or_default();
            seen.present += 1;
            seen.types.insert(type_name(value));
        }
    }

    /// One row per property: its name, its types and whether every element
    /// has it; one row of nulls for a kind without properties.
    fn rows(&self, lead: Vec<Value>) -> Rows {
        if self.properties.is_empty() {
            let mut row = lead;
            row.extend([Value::Null, Value::Null, Value::Bool(false)]);
            return vec![row];
        }
        self.properties
            .iter()
            .map(|(name, seen)| {
                let mut row = lead.clone();
                row.push(Value::String(name.clone()));
                row.push(Value::List(
                    seen.types.iter().cloned().map(Value::String).collect(),
                ));
                row.push(Value::Bool(seen.present == self.elements));
                row
            })
            .collect()
    }
}

/// The type of a property value, as Neo4j's schema procedures name it.
fn type_name(value: &Value) -> String {
    let scalar = |value: &Value| match value {
        Value::String(_) => Some("String"),
        Value::Integer(_) => Some("Long"),
        Value::Float(_) => Some("Double"),
        Value::Bool(_) => Some("Boolean"),
        _ => None,
    };
    match value {
        Value::List(items) => {
            let kinds: BTreeSet<Option<&str>> = items.iter().map(scalar).collect();
            match kinds.into_iter().collect::<Vec<_>>().as_slice() {
                [Some(one)] => format!("{one}Array"),
                _ => "List".to_string(),
            }
        }
        Value::Map(_) => "Map".to_string(),
        other => scalar(other).unwrap_or("Any").to_string(),
    }
}

/// A label set or a relationship type as Neo4j writes it: `` :`A`:`B` ``.
fn backticked(names: &[String]) -> Value {
    Value::String(names.iter().map(|name| format!(":`{name}`")).collect())
}

impl Executor<'_> {
    /// `db.schema.nodeTypeProperties()`: for every set of labels a node
    /// has, each property seen on such nodes.
    pub(super) fn proc_schema_node_type_properties(&self) -> ExecResultT<Rows> {
        let mut kinds: BTreeMap<Vec<String>, Kind> = BTreeMap::new();
        for node in self.engine().all_nodes()? {
            deadline::tick()?;
            let mut labels = node_labels_from_storage(&node);
            labels.sort();
            kinds
                .entry(labels)
                .or_default()
                .add(&node_to_value(&node).properties);
        }
        Ok(kinds
            .iter()
            .flat_map(|(labels, kind)| {
                let list = Value::List(labels.iter().cloned().map(Value::String).collect());
                kind.rows(vec![backticked(labels), list])
            })
            .collect())
    }

    /// `db.schema.relTypeProperties()`: for every relationship type, each
    /// property seen on such relationships.
    pub(super) fn proc_schema_rel_type_properties(&self) -> ExecResultT<Rows> {
        let mut kinds: BTreeMap<String, Kind> = BTreeMap::new();
        for edge in self.engine().all_edges()? {
            deadline::tick()?;
            kinds
                .entry(edge.kind.clone())
                .or_default()
                .add(&edge_to_value(&edge).properties);
        }
        Ok(kinds
            .iter()
            .flat_map(|(kind, seen)| seen.rows(vec![backticked(std::slice::from_ref(kind))]))
            .collect())
    }

    /// `db.schema.visualization()`: the labels and how relationship types
    /// join them, as one row of two lists of maps - `nodes` (`{name}`) and
    /// `relationships` (`{type, from, to}`).
    pub(super) fn proc_schema_visualization(&self) -> ExecResultT<Rows> {
        let mut labels_of: BTreeMap<u64, Vec<String>> = BTreeMap::new();
        let mut labels: BTreeSet<String> = BTreeSet::new();
        for node in self.engine().all_nodes()? {
            deadline::tick()?;
            let of_node = node_labels_from_storage(&node);
            labels.extend(of_node.iter().cloned());
            labels_of.insert(node.id, of_node);
        }
        let mut shapes: BTreeSet<(String, String, String)> = BTreeSet::new();
        for edge in self.engine().all_edges()? {
            deadline::tick()?;
            let (Some(from), Some(to)) = (labels_of.get(&edge.from_id), labels_of.get(&edge.to_id))
            else {
                continue;
            };
            for a in from {
                for b in to {
                    shapes.insert((edge.kind.clone(), a.clone(), b.clone()));
                }
            }
        }
        let map = |entries: &[(&str, &str)]| {
            Value::Map(
                entries
                    .iter()
                    .map(|(key, value)| ((*key).to_string(), Value::String((*value).to_string())))
                    .collect(),
            )
        };
        let nodes = labels.iter().map(|label| map(&[("name", label)])).collect();
        let relationships = shapes
            .iter()
            .map(|(kind, from, to)| map(&[("type", kind), ("from", from), ("to", to)]))
            .collect();
        Ok(vec![vec![Value::List(nodes), Value::List(relationships)]])
    }

    /// `dbms.components()`: the server's name, version and edition.
    pub(super) fn proc_components() -> Rows {
        vec![vec![
            Value::String("drevo".into()),
            Value::List(vec![Value::String(crate::VERSION.to_string())]),
            Value::String("community".into()),
        ]]
    }

    /// `SHOW PROCEDURES`: every procedure of this build with whether a READ
    /// session may call it and the columns it returns.
    pub(super) fn proc_show_procedures() -> Rows {
        let mut names: Vec<&str> = PROCEDURE_NAMES.to_vec();
        names.sort_unstable();
        names
            .into_iter()
            .filter_map(|name| Some((name, procedure_columns(name)?)))
            .map(|(name, columns)| {
                let mode = if READ_PROCEDURES.contains(&name) {
                    "READ"
                } else {
                    "WRITE"
                };
                vec![
                    Value::String(name.to_string()),
                    Value::String(mode.to_string()),
                    Value::List(
                        columns
                            .iter()
                            .map(|column| Value::String((*column).to_string()))
                            .collect(),
                    ),
                ]
            })
            .collect()
    }

    /// `SHOW FUNCTIONS`: every built-in function, scalar or aggregating.
    pub(super) fn proc_show_functions() -> Rows {
        let scalar = SCALAR_FUNCTIONS.iter().chain(STORE_FUNCTIONS);
        let mut rows: Vec<(&str, &str)> = scalar
            .map(|name| (*name, "scalar"))
            .chain(AGGREGATIONS.iter().map(|name| (*name, "aggregating")))
            .collect();
        rows.sort_unstable_by_key(|(name, _)| name.to_ascii_lowercase());
        rows.into_iter()
            .map(|(name, category)| {
                vec![
                    Value::String(name.to_string()),
                    Value::String(category.to_string()),
                ]
            })
            .collect()
    }
}
