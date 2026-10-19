//! The property map of a node or relationship.
//!
//! [`Properties`] maps names to JSON values. It is a vector sorted by name,
//! and the names are shared across every record: a graph whose nodes all
//! have `lat` and `lon` stores those two strings once, not once per node.
//!
//! ```
//! use drevo_core::model::Properties;
//!
//! let mut p = Properties::new();
//! p.insert("status", serde_json::json!("open"));
//! p.insert("estimate", serde_json::json!(3));
//! assert_eq!(p.get("status"), Some(&serde_json::json!("open")));
//! assert_eq!(p.keys().collect::<Vec<_>>(), vec!["estimate", "status"]);
//! assert_eq!(p.remove("estimate"), Some(serde_json::json!(3)));
//! assert_eq!(p.len(), 1);
//! ```
//!
//! It serializes as a JSON object (keys in order), and as JSON text inside
//! binary formats such as bincode, which cannot hold `serde_json::Value`.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::{Arc, Mutex, OnceLock};

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// The shared property names.
struct Names {
    set: HashSet<Arc<str>>,
    /// The size after the last sweep of unused names.
    swept_at: usize,
}

fn names() -> &'static Mutex<Names> {
    static NAMES: OnceLock<Mutex<Names>> = OnceLock::new();
    NAMES.get_or_init(|| {
        Mutex::new(Names {
            set: HashSet::new(),
            swept_at: 1024,
        })
    })
}

/// The shared copy of `name`. Names no record uses any more are dropped
/// when the pool has doubled since the last sweep.
fn intern(name: &str) -> Arc<str> {
    let mut names = names().lock().unwrap_or_else(|e| e.into_inner());
    if let Some(shared) = names.set.get(name) {
        return Arc::clone(shared);
    }
    if names.set.len() >= 2 * names.swept_at {
        names.set.retain(|n| Arc::strong_count(n) > 1);
        names.swept_at = names.set.len().max(1024);
    }
    let shared: Arc<str> = Arc::from(name);
    names.set.insert(Arc::clone(&shared));
    shared
}

/// Names to JSON values, sorted by name, with names shared across records.
#[derive(Clone, Default, PartialEq)]
pub struct Properties {
    entries: Vec<(Arc<str>, Value)>,
}

impl Properties {
    /// An empty map.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// An empty map with room for `capacity` entries.
    #[must_use]
    pub fn with_capacity(capacity: usize) -> Self {
        Self {
            entries: Vec::with_capacity(capacity),
        }
    }

    fn find(&self, key: &str) -> Result<usize, usize> {
        self.entries.binary_search_by(|(k, _)| (**k).cmp(key))
    }

    /// The number of entries.
    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether there are no entries.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// The value of `key`.
    #[must_use]
    pub fn get(&self, key: &str) -> Option<&Value> {
        self.find(key).ok().map(|i| &self.entries[i].1)
    }

    /// The value of `key`, mutably.
    pub fn get_mut(&mut self, key: &str) -> Option<&mut Value> {
        self.find(key).ok().map(|i| &mut self.entries[i].1)
    }

    /// Whether `key` has a value.
    #[must_use]
    pub fn contains_key(&self, key: &str) -> bool {
        self.find(key).is_ok()
    }

    /// Set `key` to `value`, returning the previous value.
    pub fn insert(&mut self, key: impl AsRef<str>, value: Value) -> Option<Value> {
        let key = key.as_ref();
        match self.find(key) {
            Ok(i) => Some(std::mem::replace(&mut self.entries[i].1, value)),
            Err(i) => {
                self.entries.insert(i, (intern(key), value));
                None
            }
        }
    }

    /// Remove `key`, returning its value.
    pub fn remove(&mut self, key: &str) -> Option<Value> {
        self.find(key).ok().map(|i| self.entries.remove(i).1)
    }

    /// Keep only the entries `keep` accepts.
    pub fn retain(&mut self, mut keep: impl FnMut(&str, &mut Value) -> bool) {
        self.entries.retain_mut(|(k, v)| keep(k, v));
    }

    /// Remove every entry.
    pub fn clear(&mut self) {
        self.entries.clear();
    }

    /// The entries, by name.
    pub fn iter(&self) -> impl Iterator<Item = (&str, &Value)> {
        self.entries.iter().map(|(k, v)| (&**k, v))
    }

    /// The entries, by name, with mutable values.
    pub fn iter_mut(&mut self) -> impl Iterator<Item = (&str, &mut Value)> {
        self.entries.iter_mut().map(|(k, v)| (&**k, v))
    }

    /// The names, in order.
    pub fn keys(&self) -> impl Iterator<Item = &str> {
        self.entries.iter().map(|(k, _)| &**k)
    }

    /// The values, by name.
    pub fn values(&self) -> impl Iterator<Item = &Value> {
        self.entries.iter().map(|(_, v)| v)
    }

    /// The values, by name, mutably.
    pub fn values_mut(&mut self) -> impl Iterator<Item = &mut Value> {
        self.entries.iter_mut().map(|(_, v)| v)
    }

    /// The entries as an ordered map of owned names.
    #[must_use]
    pub fn to_btree_map(&self) -> BTreeMap<String, Value> {
        self.iter()
            .map(|(k, v)| (k.to_string(), v.clone()))
            .collect()
    }
}

impl std::fmt::Debug for Properties {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_map().entries(self.iter()).finish()
    }
}

impl<K: AsRef<str>> Extend<(K, Value)> for Properties {
    fn extend<I: IntoIterator<Item = (K, Value)>>(&mut self, iter: I) {
        for (k, v) in iter {
            self.insert(k, v);
        }
    }
}

impl<K: AsRef<str>> FromIterator<(K, Value)> for Properties {
    fn from_iter<I: IntoIterator<Item = (K, Value)>>(iter: I) -> Self {
        let mut p = Properties::new();
        p.extend(iter);
        p
    }
}

impl From<HashMap<String, Value>> for Properties {
    fn from(map: HashMap<String, Value>) -> Self {
        map.into_iter().collect()
    }
}

impl From<BTreeMap<String, Value>> for Properties {
    fn from(map: BTreeMap<String, Value>) -> Self {
        map.into_iter().collect()
    }
}

impl<const N: usize> From<[(&str, Value); N]> for Properties {
    fn from(entries: [(&str, Value); N]) -> Self {
        entries.into_iter().collect()
    }
}

impl IntoIterator for Properties {
    type Item = (String, Value);
    type IntoIter = std::iter::Map<
        std::vec::IntoIter<(Arc<str>, Value)>,
        fn((Arc<str>, Value)) -> (String, Value),
    >;

    fn into_iter(self) -> Self::IntoIter {
        fn owned((k, v): (Arc<str>, Value)) -> (String, Value) {
            (k.to_string(), v)
        }
        self.entries.into_iter().map(owned as fn(_) -> _)
    }
}

impl<'a> IntoIterator for &'a Properties {
    type Item = (&'a str, &'a Value);
    type IntoIter = std::iter::Map<
        std::slice::Iter<'a, (Arc<str>, Value)>,
        fn(&'a (Arc<str>, Value)) -> (&'a str, &'a Value),
    >;

    fn into_iter(self) -> Self::IntoIter {
        fn borrowed(entry: &(Arc<str>, Value)) -> (&str, &Value) {
            (&*entry.0, &entry.1)
        }
        self.entries.iter().map(borrowed as fn(_) -> _)
    }
}

impl std::ops::Index<&str> for Properties {
    type Output = Value;

    fn index(&self, key: &str) -> &Value {
        static NULL: Value = Value::Null;
        self.get(key).unwrap_or(&NULL)
    }
}

impl Serialize for Properties {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeMap;
        if serializer.is_human_readable() {
            let mut map = serializer.serialize_map(Some(self.len()))?;
            for (k, v) in self.iter() {
                map.serialize_entry(k, v)?;
            }
            map.end()
        } else {
            // Bincode cannot hold `serde_json::Value`: JSON text inside.
            let sorted: BTreeMap<&str, &Value> = self.iter().collect();
            let json = serde_json::to_vec(&sorted).map_err(serde::ser::Error::custom)?;
            json.serialize(serializer)
        }
    }
}

impl<'de> Deserialize<'de> for Properties {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        if deserializer.is_human_readable() {
            let map = BTreeMap::<String, Value>::deserialize(deserializer)?;
            Ok(map.into())
        } else {
            let json = Vec::<u8>::deserialize(deserializer)?;
            let map: BTreeMap<String, Value> =
                serde_json::from_slice(&json).map_err(serde::de::Error::custom)?;
            Ok(map.into())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn behaves_like_a_map() {
        let mut state: u64 = 0x1234_5678_9abc_def1;
        let mut next = || {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state
        };
        let mut ours = Properties::new();
        let mut reference: BTreeMap<String, Value> = BTreeMap::new();
        for step in 0..5_000 {
            let key = format!("k{}", next() % 12);
            match next() % 3 {
                0 => assert_eq!(ours.remove(&key), reference.remove(&key)),
                _ => assert_eq!(
                    ours.insert(&key, json!(step)),
                    reference.insert(key.clone(), json!(step))
                ),
            }
            assert_eq!(ours.to_btree_map(), reference, "step {step}");
            assert_eq!(ours.get(&key), reference.get(&key));
            assert_eq!(ours.len(), reference.len());
        }
    }

    #[test]
    fn names_are_shared_between_maps() {
        let a: Properties = [("shared_name_for_test", json!(1))].into();
        let b: Properties = [("shared_name_for_test", json!(2))].into();
        assert!(Arc::ptr_eq(&a.entries[0].0, &b.entries[0].0));
    }

    #[test]
    fn serializes_like_a_json_object_and_through_bincode() {
        let p: Properties = [("b", json!([1, 2])), ("a", json!({"x": true}))].into();
        assert_eq!(
            serde_json::to_string(&p).unwrap(),
            r#"{"a":{"x":true},"b":[1,2]}"#
        );
        let back: Properties = serde_json::from_str(r#"{"b":[1,2],"a":{"x":true}}"#).unwrap();
        assert_eq!(back, p);
        let bytes = bincode::serde::encode_to_vec(&p, bincode::config::standard()).unwrap();
        let (decoded, _): (Properties, usize) =
            bincode::serde::decode_from_slice(&bytes, bincode::config::standard()).unwrap();
        assert_eq!(decoded, p);
    }

    #[test]
    fn indexing_a_missing_key_is_null_and_iteration_is_ordered() {
        let p: Properties = [("z", json!(1)), ("m", json!(2)), ("a", json!(3))].into();
        assert_eq!(p["m"], json!(2));
        assert_eq!(p["missing"], Value::Null);
        let owned: Vec<(String, Value)> = p.clone().into_iter().collect();
        assert_eq!(owned[0].0, "a");
        let keys: Vec<&str> = (&p).into_iter().map(|(k, _)| k).collect();
        assert_eq!(keys, vec!["a", "m", "z"]);
        assert_eq!(
            format!("{p:?}"),
            r#"{"a": Number(3), "m": Number(2), "z": Number(1)}"#
        );
    }
}
