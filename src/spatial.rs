//! Nearest nodes to a latitude/longitude: `CALL drevo.spatial.nearest` and
//! [`NativeService::nearest`](crate::native_service::NativeService::nearest).
//!
//! A lookup reads a declared point index
//! ([`NativePointIndex`](crate::native_point_index::NativePointIndex)) when
//! one covers the label and the two position properties, and otherwise scans
//! the nodes with the label. Both give the same answer: the nodes nearest to
//! the point by great-circle distance, nearest first, equal distances by node
//! id.

use std::collections::HashMap;
use std::sync::Arc;

use serde_json::Value as JsonValue;

use crate::cypher::executor::node_labels_from_storage;
use crate::error::DrevoError;
use crate::model::Node;
use crate::native_point_index::{haversine_m, position, NativePointIndex, Nearest};

/// What [`NativeService::nearest`](crate::native_service::NativeService::nearest)
/// looks for.
#[derive(Debug, Clone, PartialEq)]
pub struct NearestOptions {
    /// The latitude property, in degrees (default `lat`).
    pub lat: String,
    /// The longitude property, in degrees (default `lon`).
    pub lon: String,
    /// At most this many nodes (default 1).
    pub k: usize,
    /// Only nodes at most this many metres away.
    pub max_meters: Option<f64>,
    /// Only nodes whose properties equal these values.
    pub filter: Vec<(String, JsonValue)>,
}

impl Default for NearestOptions {
    fn default() -> Self {
        Self {
            lat: "lat".to_string(),
            lon: "lon".to_string(),
            k: 1,
            max_meters: None,
            filter: Vec::new(),
        }
    }
}

/// One node a lookup found.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Near {
    /// The node id.
    pub node: u64,
    /// The great-circle distance from the query point, in metres.
    pub meters: f64,
}

/// Refuse a point off the globe or options that cannot be answered.
///
/// # Errors
/// [`DrevoError::InvalidArgument`] saying what is wrong.
pub(crate) fn validate(from: (f64, f64), options: &NearestOptions) -> Result<(), DrevoError> {
    let refuse = |message: String| Err(DrevoError::InvalidArgument(message));
    if !(-90.0..=90.0).contains(&from.0) {
        return refuse(format!(
            "the latitude must be between -90 and 90 degrees, got {}",
            from.0
        ));
    }
    if !(-180.0..=180.0).contains(&from.1) {
        return refuse(format!(
            "the longitude must be between -180 and 180 degrees, got {}",
            from.1
        ));
    }
    if options.lat.is_empty() || options.lon.is_empty() {
        return refuse("lat and lon must be property names".to_string());
    }
    if options.max_meters.is_some_and(|m| m.is_nan() || m < 0.0) {
        return refuse("maxMeters must be a number of metres, not negative".to_string());
    }
    Ok(())
}

/// Whether two stored values are equal the way Cypher compares them: numbers
/// by value (`1` equals `1.0`), `null` equal to nothing.
fn same_value(have: &JsonValue, want: &JsonValue) -> bool {
    match (have, want) {
        (JsonValue::Null, _) | (_, JsonValue::Null) => false,
        (JsonValue::Number(a), JsonValue::Number(b)) => match (a.as_i64(), b.as_i64()) {
            (Some(a), Some(b)) => a == b,
            _ => a.as_f64() == b.as_f64(),
        },
        (JsonValue::Array(a), JsonValue::Array(b)) => {
            a.len() == b.len() && a.iter().zip(b).all(|(a, b)| same_value(a, b))
        }
        _ => have == want,
    }
}

/// Whether `node` passes the `filter` of `options`.
fn passes(node: &Node, options: &NearestOptions) -> bool {
    options.filter.iter().all(|(key, want)| {
        node.properties
            .get(key)
            .is_some_and(|have| same_value(have, want))
    })
}

/// The nodes with `label` nearest to `from`, nearest first, ties by id, as
/// `(node, metres)`. With `index` (a point index fresh for the nodes `node`
/// and `scan` read), a covering spec answers; otherwise every node `scan`
/// returns is checked. The caller has [`validate`]d `from` and `options`.
pub(crate) fn nearest(
    index: Option<&NativePointIndex>,
    label: &str,
    from: (f64, f64),
    options: &NearestOptions,
    node: &dyn Fn(u64) -> Result<Option<Arc<Node>>, DrevoError>,
    scan: &dyn Fn() -> Result<Vec<Arc<Node>>, DrevoError>,
) -> Result<Vec<(Arc<Node>, f64)>, DrevoError> {
    if options.k == 0 {
        return Ok(Vec::new());
    }
    let query = Nearest {
        k: options.k,
        max_meters: options.max_meters,
    };
    if let Some(index) = index {
        let mut failed = None;
        let mut nodes = HashMap::new();
        let found = index.nearest(label, &options.lat, &options.lon, from, &query, &mut |id| {
            if failed.is_some() {
                return false;
            }
            match node(id) {
                Ok(Some(n)) if passes(&n, options) => {
                    nodes.insert(id, n);
                    true
                }
                Ok(_) => false,
                Err(e) => {
                    failed = Some(e);
                    false
                }
            }
        });
        if let Some(e) = failed {
            return Err(e);
        }
        if let Some(found) = found {
            return Ok(found
                .into_iter()
                .filter_map(|(id, meters)| nodes.remove(&id).map(|n| (n, meters)))
                .collect());
        }
    }
    let mut hits = Vec::new();
    for n in scan()? {
        if !node_labels_from_storage(&n).iter().any(|l| l == label) || !passes(&n, options) {
            continue;
        }
        let Some(at) = position(&n, &options.lat, &options.lon) else {
            continue;
        };
        let meters = haversine_m(from, at);
        if options.max_meters.is_none_or(|max| meters <= max) {
            hits.push((n, meters));
        }
    }
    hits.sort_by(|a, b| a.1.total_cmp(&b.1).then(a.0.id.cmp(&b.0.id)));
    hits.truncate(options.k);
    Ok(hits)
}
