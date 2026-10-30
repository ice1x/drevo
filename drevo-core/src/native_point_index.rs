//! Opt-in **point indexes**: the position of nodes with a label, read from two
//! numeric properties in degrees (latitude and longitude), kept current from a
//! [`NativeGraph`]'s change feed, serving nearest-neighbour lookups by
//! great-circle distance.
//!
//! Positions are bucketed into a grid of [`CELL_DEGREES`]-wide cells. A lookup
//! visits the cells around the query point ring by ring and stops once no
//! unvisited cell can hold anything closer than what it has found, so it reads
//! a few cells near the point instead of every node.
//!
//! A node is indexed when it carries the spec's label (as its kind or as a
//! secondary label) and both properties hold finite numbers, the latitude in
//! −90..=90 and the longitude in −180..=180. Anything else — a missing
//! property, a string, a number out of range — leaves the node out.
//!
//! Distances are haversine distances on a sphere of [`EARTH_RADIUS_M`]
//! metres. Results come nearest first; equal distances are ordered by node id.
//!
//! ```
//! use drevo_core::engine::GraphEngine;
//! use drevo_core::model::{NewNode, Properties};
//! use drevo_core::native::NativeGraph;
//! use drevo_core::native_point_index::{NativePointIndex, Nearest, PointIndexSpec};
//! use serde_json::json;
//!
//! # fn main() -> drevo_core::error::Result<()> {
//! let graph = NativeGraph::new();
//! let stop = |name: &str, lat: f64, lon: f64| -> drevo_core::error::Result<u64> {
//!     let mut properties = Properties::new();
//!     properties.insert("lat", json!(lat));
//!     properties.insert("lon", json!(lon));
//!     Ok(graph
//!         .create_node(NewNode {
//!             kind: "Stop".into(),
//!             title: name.into(),
//!             body: String::new(),
//!             body_html: String::new(),
//!             properties,
//!         })?
//!         .id)
//! };
//! let opera = stop("Opera", 42.6935, 23.3290)?;
//! let _airport = stop("Airport", 42.6952, 23.4062)?;
//!
//! let spec = PointIndexSpec::new("Stop", "lat", "lon").expect("valid spec");
//! let mut idx = NativePointIndex::new(vec![spec]);
//! idx.sync(&graph);
//!
//! let query = Nearest { k: 1, max_meters: None };
//! let hits = idx
//!     .nearest("Stop", "lat", "lon", (42.6977, 23.3219), &query, &mut |_| true)
//!     .expect("a spec covers Stop(lat, lon)");
//! assert_eq!(hits.len(), 1);
//! assert_eq!(hits[0].0, opera);
//! assert!(hits[0].1 > 700.0 && hits[0].1 < 800.0); // metres
//! # Ok(())
//! # }
//! ```

use std::cmp::{Ordering, Reverse};
use std::collections::{BinaryHeap, HashMap};
use std::fmt;

use crate::engine::GraphEngine;
use crate::labels::secondary_labels;
use crate::model::Node;
use crate::native::{NativeGraph, WalOp};

/// Mean Earth radius in metres (IUGG), the radius of the haversine distance.
pub const EARTH_RADIUS_M: f64 = 6_371_008.8;

/// The width of a grid cell, in degrees of latitude and of longitude.
pub const CELL_DEGREES: f64 = 0.002;

/// Great-circle distance in metres between two `(lat, lon)` points in degrees.
#[must_use]
pub fn haversine_m(a: (f64, f64), b: (f64, f64)) -> f64 {
    let (lat1, lon1) = (a.0.to_radians(), a.1.to_radians());
    let (lat2, lon2) = (b.0.to_radians(), b.1.to_radians());
    let dlat = lat2 - lat1;
    let dlon = lon2 - lon1;
    let s = (dlat / 2.0).sin().powi(2) + lat1.cos() * lat2.cos() * (dlon / 2.0).sin().powi(2);
    2.0 * EARTH_RADIUS_M * s.sqrt().min(1.0).asin()
}

/// Why a [`PointIndexSpec`] was refused.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PointSpecError {
    /// The label is empty.
    EmptyLabel,
    /// A property name is empty.
    EmptyProperty,
    /// The latitude and longitude name the same property.
    SameProperty,
}

impl fmt::Display for PointSpecError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::EmptyLabel => "a point index needs a label",
            Self::EmptyProperty => "a point index property name is empty",
            Self::SameProperty => "the latitude and longitude must be different properties",
        })
    }
}

impl std::error::Error for PointSpecError {}

/// One declared point index: nodes with `label`, positioned by the numbers in
/// the `lat` and `lon` properties.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct PointIndexSpec {
    label: String,
    lat: String,
    lon: String,
}

impl PointIndexSpec {
    /// A spec for nodes with `label`, positioned by the properties `lat` and
    /// `lon`.
    ///
    /// # Errors
    /// [`PointSpecError`] for an empty label or property name, or the same
    /// property twice.
    pub fn new(
        label: impl Into<String>,
        lat: impl Into<String>,
        lon: impl Into<String>,
    ) -> Result<Self, PointSpecError> {
        let (label, lat, lon) = (label.into(), lat.into(), lon.into());
        if label.is_empty() {
            return Err(PointSpecError::EmptyLabel);
        }
        if lat.is_empty() || lon.is_empty() {
            return Err(PointSpecError::EmptyProperty);
        }
        if lat == lon {
            return Err(PointSpecError::SameProperty);
        }
        Ok(Self { label, lat, lon })
    }

    /// Whether this spec was declared as `label` / `lat` / `lon`.
    fn is(&self, label: &str, lat: &str, lon: &str) -> bool {
        self.label == label && self.lat == lat && self.lon == lon
    }

    /// The label indexed nodes carry.
    #[must_use]
    pub fn label(&self) -> &str {
        &self.label
    }

    /// The latitude property.
    #[must_use]
    pub fn lat(&self) -> &str {
        &self.lat
    }

    /// The longitude property.
    #[must_use]
    pub fn lon(&self) -> &str {
        &self.lon
    }
}

/// What a nearest-neighbour lookup asks for.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Nearest {
    /// At most this many nodes.
    pub k: usize,
    /// Only nodes at most this many metres away.
    pub max_meters: Option<f64>,
}

/// The position of `node` from its `lat` and `lon` properties: finite numbers,
/// the latitude in −90..=90 and the longitude in −180..=180. `None` otherwise.
#[must_use]
pub fn position(node: &Node, lat: &str, lon: &str) -> Option<(f64, f64)> {
    let number = |key: &str| node.properties.get(key).and_then(serde_json::Value::as_f64);
    let (lat, lon) = (number(lat)?, number(lon)?);
    ((-90.0..=90.0).contains(&lat) && (-180.0..=180.0).contains(&lon)).then_some((lat, lon))
}

/// Cells from the South Pole to the North Pole.
const LAT_CELLS: i64 = (180.0 / CELL_DEGREES) as i64;
/// Cells around the globe; longitude 180 shares a cell column with −180.
const LON_CELLS: i64 = (360.0 / CELL_DEGREES) as i64;

/// The grid cell holding `(lat, lon)`.
fn cell_of((lat, lon): (f64, f64)) -> (i64, i64) {
    let row = (((lat + 90.0) / CELL_DEGREES).floor() as i64).clamp(0, LAT_CELLS - 1);
    let col = (((lon + 180.0) / CELL_DEGREES).floor() as i64).rem_euclid(LON_CELLS);
    (row, col)
}

/// `sin²(x / 2)` of an angle in radians.
fn hav(x: f64) -> f64 {
    (x / 2.0).sin().powi(2)
}

/// A candidate on the lookup's frontier, ordered by distance, then id.
#[derive(Debug, Clone, Copy)]
struct Candidate {
    meters: f64,
    id: u64,
}

impl PartialEq for Candidate {
    fn eq(&self, other: &Self) -> bool {
        self.cmp(other) == Ordering::Equal
    }
}

impl Eq for Candidate {}

impl PartialOrd for Candidate {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for Candidate {
    fn cmp(&self, other: &Self) -> Ordering {
        self.meters
            .total_cmp(&other.meters)
            .then(self.id.cmp(&other.id))
    }
}

/// A grid cell: its row (latitude) and column (longitude).
type Cell = (i64, i64);

/// A node in a cell, with its position.
type Entry = (u64, (f64, f64));

/// The grid of one spec.
#[derive(Default)]
struct Grid {
    /// cell → the nodes in it, with their positions.
    cells: HashMap<Cell, Vec<Entry>>,
    /// node id → its cell, so the node can be moved or removed.
    at: HashMap<u64, Cell>,
}

impl Grid {
    fn remove(&mut self, id: u64) {
        let Some(cell) = self.at.remove(&id) else {
            return;
        };
        if let Some(nodes) = self.cells.get_mut(&cell) {
            nodes.retain(|(have, _)| *have != id);
            if nodes.is_empty() {
                self.cells.remove(&cell);
            }
        }
    }

    fn insert(&mut self, id: u64, at: (f64, f64)) {
        let cell = cell_of(at);
        self.cells.entry(cell).or_default().push((id, at));
        self.at.insert(id, cell);
    }

    /// The nearest nodes to `from`; see [`NativePointIndex::nearest`].
    fn nearest(
        &self,
        from: (f64, f64),
        query: &Nearest,
        accept: &mut dyn FnMut(u64) -> bool,
    ) -> Vec<(u64, f64)> {
        let mut found = Vec::new();
        if query.k == 0 || self.cells.is_empty() {
            return found;
        }
        let (row, col) = cell_of(from);
        let mut frontier = BinaryHeap::new();
        let visit = |frontier: &mut BinaryHeap<Reverse<Candidate>>, cell: &(i64, i64)| {
            for &(id, at) in self.cells.get(cell).into_iter().flatten() {
                frontier.push(Reverse(Candidate {
                    meters: haversine_m(from, at),
                    id,
                }));
            }
        };
        let mut ring = 0_i64;
        loop {
            // Visit the cells `ring` steps away, or every cell not visited
            // yet once the rings would cover more cells than are occupied:
            // near the poles the bound below grows slowly, and a scan of the
            // occupied cells is then cheaper than more rings.
            let side = 2 * ring + 1;
            let everything = side * side > self.cells.len() as i64 || side >= LON_CELLS;
            if everything {
                for cell in self.cells.keys() {
                    let lon_steps = (cell.1 - col).rem_euclid(LON_CELLS);
                    let lon_steps = lon_steps.min(LON_CELLS - lon_steps);
                    if (cell.0 - row).abs().max(lon_steps) >= ring {
                        visit(&mut frontier, cell);
                    }
                }
            } else {
                for dr in -ring..=ring {
                    let r = row + dr;
                    if !(0..LAT_CELLS).contains(&r) {
                        continue;
                    }
                    let step = if dr.abs() == ring { 1 } else { 2 * ring.max(1) };
                    let mut dc = -ring;
                    while dc <= ring {
                        visit(&mut frontier, &(r, (col + dc).rem_euclid(LON_CELLS)));
                        dc += step;
                    }
                }
            }
            // Nothing outside the visited cells is closer than `bound`.
            let bound = if everything {
                f64::INFINITY
            } else {
                Self::unvisited_bound(from, (row, col), ring)
            };
            while let Some(Reverse(next)) = frontier.peek().copied() {
                if next.meters >= bound {
                    break;
                }
                if query.max_meters.is_some_and(|max| next.meters > max) {
                    return found;
                }
                frontier.pop();
                if accept(next.id) {
                    found.push((next.id, next.meters));
                    if found.len() == query.k {
                        return found;
                    }
                }
            }
            if everything || query.max_meters.is_some_and(|max| bound > max) {
                return found;
            }
            ring += 1;
        }
    }

    /// A lower bound on the distance from `from` to any point outside the
    /// cells at most `ring` steps from its cell `(row, col)`, slightly
    /// lowered so rounding never puts a closer point beyond it.
    fn unvisited_bound(from: (f64, f64), (row, col): (i64, i64), ring: i64) -> f64 {
        let south = (row - ring) as f64 * CELL_DEGREES - 90.0;
        let north = (row + ring + 1) as f64 * CELL_DEGREES - 90.0;
        let west = (col - ring) as f64 * CELL_DEGREES - 180.0;
        let east = (col + ring + 1) as f64 * CELL_DEGREES - 180.0;
        // Beyond the visited rows, the latitude alone is that far away.
        let below = if south <= -90.0 {
            f64::INFINITY
        } else {
            from.0 - south
        };
        let above = if north >= 90.0 {
            f64::INFINITY
        } else {
            north - from.0
        };
        let by_lat = EARTH_RADIUS_M * below.min(above).to_radians();
        // Within the visited rows but beyond the visited columns: at least
        // `gap` degrees of longitude, at latitudes no further from the
        // equator than the rows reach.
        let gap = (from.1 - west).min(east - from.1);
        let by_lon = if gap >= 180.0 {
            f64::INFINITY
        } else {
            let widest = south.max(-90.0).abs().max(north.min(90.0).abs());
            let s = from.0.to_radians().cos() * widest.to_radians().cos() * hav(gap.to_radians());
            2.0 * EARTH_RADIUS_M * s.max(0.0).sqrt().min(1.0).asin()
        };
        by_lat.min(by_lon) * (1.0 - 1e-9) - 1e-6
    }
}

/// Point indexes over declared [`PointIndexSpec`]s. See the
/// [module docs](self).
#[derive(Default)]
pub struct NativePointIndex {
    specs: Vec<PointIndexSpec>,
    grids: Vec<Grid>,
    /// The change-feed cursor this index has consumed up to.
    cursor: u64,
}

impl fmt::Debug for NativePointIndex {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("NativePointIndex")
            .field("specs", &self.specs)
            .field("cursor", &self.cursor)
            .finish_non_exhaustive()
    }
}

impl NativePointIndex {
    /// An index over `specs`, positioned before any change; call
    /// [`sync`](Self::sync) to populate it.
    #[must_use]
    pub fn new(specs: Vec<PointIndexSpec>) -> Self {
        let grids = specs.iter().map(|_| Grid::default()).collect();
        Self {
            specs,
            grids,
            cursor: 0,
        }
    }

    /// The declared specs.
    #[must_use]
    pub fn specs(&self) -> &[PointIndexSpec] {
        &self.specs
    }

    /// The change-feed cursor this index has consumed up to.
    #[must_use]
    pub fn cursor(&self) -> u64 {
        self.cursor
    }

    /// How many nodes the spec declared as `label` / `lat` / `lon` holds, or
    /// `None` when no such spec is declared.
    #[must_use]
    pub fn len(&self, label: &str, lat: &str, lon: &str) -> Option<usize> {
        self.grid(label, lat, lon).map(|grid| grid.at.len())
    }

    fn grid(&self, label: &str, lat: &str, lon: &str) -> Option<&Grid> {
        self.specs
            .iter()
            .zip(&self.grids)
            .find(|(spec, _)| spec.is(label, lat, lon))
            .map(|(_, grid)| grid)
    }

    /// Replace the declared specs and rebuild from `graph`.
    pub fn set_specs(&mut self, specs: Vec<PointIndexSpec>, graph: &NativeGraph) {
        self.grids = specs.iter().map(|_| Grid::default()).collect();
        self.specs = specs;
        self.rebuild_from(graph);
        self.cursor = graph.change_head();
    }

    /// Apply every change `graph` committed since the last sync.
    pub fn sync(&mut self, graph: &NativeGraph) {
        if self.specs.is_empty() {
            self.cursor = graph.change_head();
            return;
        }
        let batch = graph.changes_since(self.cursor);
        if batch.lagged {
            self.rebuild_from(graph);
            self.cursor = graph.change_head().max(batch.cursor);
            return;
        }
        for op in batch.ops {
            match op {
                WalOp::UpsertNode(node) => self.index_node(&node),
                WalOp::DeleteNode(id) => {
                    for grid in &mut self.grids {
                        grid.remove(id);
                    }
                }
                WalOp::UpsertEdge(_)
                | WalOp::DeleteEdge(_)
                | WalOp::SetEmbedding(..)
                | WalOp::DeleteEmbedding(_)
                | WalOp::Prepare { .. }
                | WalOp::CommitPrepared { .. }
                | WalOp::RollbackPrepared { .. } => {}
            }
        }
        self.cursor = batch.cursor;
    }

    fn rebuild_from(&mut self, graph: &NativeGraph) {
        for grid in &mut self.grids {
            *grid = Grid::default();
        }
        if self.specs.is_empty() {
            return;
        }
        if let Ok(nodes) = graph.all_nodes() {
            for node in &nodes {
                self.index_node(node);
            }
        }
    }

    /// Replace `node`'s entry in every grid.
    fn index_node(&mut self, node: &Node) {
        let mut labels = secondary_labels(node);
        labels.push(node.kind.clone());
        for (spec, grid) in self.specs.iter().zip(&mut self.grids) {
            grid.remove(node.id);
            if !labels.contains(&spec.label) {
                continue;
            }
            if let Some(at) = position(node, &spec.lat, &spec.lon) {
                grid.insert(node.id, at);
            }
        }
    }

    /// The nodes nearest to `from` (`(lat, lon)` in degrees) under the spec
    /// declared as `label` / `lat` / `lon`, as `(id, metres)` nearest first,
    /// ties by id. Only nodes `accept` admits count towards `query.k`; it is
    /// called nearest first, at most once per node. `None` when no such spec
    /// is declared: the caller scans instead.
    pub fn nearest(
        &self,
        label: &str,
        lat: &str,
        lon: &str,
        from: (f64, f64),
        query: &Nearest,
        accept: &mut dyn FnMut(u64) -> bool,
    ) -> Option<Vec<(u64, f64)>> {
        self.grid(label, lat, lon)
            .map(|grid| grid.nearest(from, query, accept))
    }
}
