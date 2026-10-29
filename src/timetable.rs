//! Public-transport routing: the earliest arrival over a timetable stored in
//! the graph.
//!
//! Stops are nodes. A *hop* relationship runs from one stop to the next stop
//! of a trip pattern and carries one entry per trip in parallel list
//! properties: the departure from its start stop, the arrival at its end
//! stop, the trip id and the service id. Times are seconds after midnight of
//! the trip's service day and may pass 86400. Footpaths between stops are
//! relationships with a walking time.
//!
//! [`earliest_arrivals`](crate::timetable::earliest_arrivals()) answers "leave
//! at 08:10 from here, arrive there as early as possible": it returns, for
//! each number of transfers, the earliest journey, when that journey arrives
//! earlier than every journey with fewer transfers. Staying on the same trip
//! from hop to hop is one ride; changing vehicles at a stop takes the minimum
//! transfer time; a footpath between two rides takes its walking time. A trip
//! runs on a day when its service is among that day's services, and its
//! times are shifted by the day's offset, so yesterday's trips past midnight
//! are found with `offset: -86400`.
//!
//! The search runs in rounds, one more ride per round (RAPTOR): from every
//! stop reached earlier in the previous round it boards every trip leaving
//! later, rides each trip to its end, and then walks on from every stop a
//! ride reached earlier, over as many footpaths in a row as it takes.
//!
//! ```
//! use drevo::model::{Direction, Edge, Properties};
//! use drevo::timetable::{earliest_arrivals, Endpoint, ServiceDay, TimetableOptions};
//! use serde_json::json;
//!
//! // Trip 7 runs from stop 1 at 08:00 to stop 2 at 08:20.
//! let mut properties = Properties::new();
//! properties.insert("dep", json!([28_800]));
//! properties.insert("arr", json!([30_000]));
//! properties.insert("trip", json!([7]));
//! properties.insert("svc", json!([1]));
//! let hop = Edge {
//!     id: 10,
//!     uuid: [0; 16],
//!     from_id: 1,
//!     to_id: 2,
//!     kind: "HOP".into(),
//!     weight: 1.0,
//!     created_at: 0,
//!     properties,
//! };
//! let options = TimetableOptions {
//!     depart_at: 28_500,
//!     days: vec![ServiceDay { offset: 0, services: vec![1] }],
//!     ..TimetableOptions::default()
//! };
//! let journeys = earliest_arrivals(
//!     &[Endpoint { node: 1, cost: 120 }],
//!     &[Endpoint { node: 2, cost: 60 }],
//!     &options,
//!     &|stop, _direction, visit: &mut dyn FnMut(&Edge)| {
//!         if stop == hop.from_id {
//!             visit(&hop);
//!         }
//!         Ok(())
//!     },
//! )?;
//! assert_eq!(journeys.len(), 1);
//! assert_eq!(journeys[0].depart_at, 28_680); // walk 120 s to catch the 08:00 trip
//! assert_eq!(journeys[0].arrive_at, 30_060);
//! assert_eq!(journeys[0].transfers, 0);
//! # Ok::<(), drevo::error::DrevoError>(())
//! ```

use std::cmp::Reverse;
use std::collections::{BinaryHeap, HashMap, HashSet};
use std::rc::Rc;

use serde_json::Value;

use crate::error::{DrevoError, Result};
use crate::model::{Direction, Edge};

/// Where a journey may begin or end: a stop and the seconds it takes to get
/// there from the origin (or from there to the destination).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Endpoint {
    /// The stop node.
    pub node: u64,
    /// Seconds between the origin and the stop (or the stop and the destination).
    pub cost: i64,
}

/// A service day: the services that run on it, and where its midnight lies
/// relative to the midnight of the query date.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServiceDay {
    /// Seconds added to the times of its trips (`0` today, `-86400` yesterday).
    pub offset: i64,
    /// The service ids that run on this day.
    pub services: Vec<i64>,
}

/// The relationship type and property names of the timetable hops.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HopFields {
    /// Relationship type of a hop between two consecutive stops.
    pub kind: String,
    /// List property: departure from the start stop, per trip.
    pub dep: String,
    /// List property: arrival at the end stop, per trip.
    pub arr: String,
    /// List property: trip id, per trip.
    pub trip: String,
    /// List property: service id, per trip.
    pub service: String,
}

impl Default for HopFields {
    fn default() -> Self {
        Self {
            kind: "HOP".into(),
            dep: "dep".into(),
            arr: "arr".into(),
            trip: "trip".into(),
            service: "svc".into(),
        }
    }
}

/// The footpaths between stops.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WalkFields {
    /// Relationship types of a footpath.
    pub kinds: Vec<String>,
    /// Numeric property: the walk in seconds.
    pub cost: String,
}

/// What [`earliest_arrivals`] searches over.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TimetableOptions {
    /// When the journey starts, in seconds after midnight of the query date.
    pub depart_at: i64,
    /// The service days whose trips may be used.
    pub days: Vec<ServiceDay>,
    /// Where the timetable is stored.
    pub hops: HopFields,
    /// Footpaths between rides; `None` allows no walking between stops.
    pub walk: Option<WalkFields>,
    /// Seconds added when changing vehicles at the same stop.
    pub min_transfer: i64,
    /// The most vehicle changes in a journey.
    pub max_transfers: usize,
    /// Journeys arriving later than `depart_at + max_duration` are not returned.
    pub max_duration: Option<i64>,
}

impl Default for TimetableOptions {
    fn default() -> Self {
        Self {
            depart_at: 0,
            days: Vec::new(),
            hops: HopFields::default(),
            walk: None,
            min_transfer: 0,
            max_transfers: 4,
            max_duration: None,
        }
    }
}

/// How a leg of a journey is travelled.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LegMode {
    /// From the origin to the first stop.
    Access,
    /// On one vehicle trip.
    Ride,
    /// On a footpath between two stops.
    Walk,
    /// From the last stop to the destination.
    Egress,
}

/// One leg of a journey.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Leg {
    /// How the leg is travelled.
    pub mode: LegMode,
    /// The stop the leg leaves from; `None` for the access leg.
    pub from: Option<u64>,
    /// The stop the leg arrives at; `None` for the egress leg.
    pub to: Option<u64>,
    /// When the leg leaves, in seconds after midnight of the query date.
    pub depart: i64,
    /// When the leg arrives.
    pub arrive: i64,
    /// The trip ridden, for a ride.
    pub trip: Option<i64>,
    /// The relationships travelled: the hops of a ride, the footpath of a walk.
    pub hops: Vec<u64>,
}

/// A journey from the origin to the destination.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Journey {
    /// When to leave the origin: the first ride's departure minus the time
    /// to reach its stop, or the query's `depart_at` without rides.
    pub depart_at: i64,
    /// When the destination is reached.
    pub arrive_at: i64,
    /// Vehicle changes: rides minus one, and zero for a journey without rides.
    pub transfers: usize,
    /// The legs in order.
    pub legs: Vec<Leg>,
}

/// The journeys from any of `starts` to any of `ends` that arrive earliest
/// for their number of transfers, ordered by arrival.
///
/// Each journey arrives strictly earlier than every journey with fewer
/// transfers, so at most `max_transfers + 1` come back. A journey without
/// rides — when a start is also an end, or a footpath joins them — counts as
/// no transfers. `edges_of(stop, Direction::Outgoing, visit)` must visit the
/// relationships leaving `stop`.
///
/// # Errors
///
/// [`DrevoError::InvalidArgument`] for a negative cost or transfer time, and
/// for a hop whose lists are missing, not integers or of different lengths,
/// or a footpath with a negative walking time.
pub fn earliest_arrivals<E>(
    starts: &[Endpoint],
    ends: &[Endpoint],
    options: &TimetableOptions,
    edges_of: &E,
) -> Result<Vec<Journey>>
where
    E: Fn(u64, Direction, &mut dyn FnMut(&Edge)) -> Result<()>,
{
    validate(starts, ends, options)?;
    Search::new(ends, options, edges_of).run(starts)
}

fn validate(starts: &[Endpoint], ends: &[Endpoint], options: &TimetableOptions) -> Result<()> {
    if let Some(e) = starts.iter().chain(ends).find(|e| e.cost < 0) {
        return Err(DrevoError::InvalidArgument(format!(
            "the cost of a start or end stop must not be negative, got {}",
            e.cost
        )));
    }
    if options.min_transfer < 0 {
        return Err(DrevoError::InvalidArgument(format!(
            "minTransfer must not be negative, got {}",
            options.min_transfer
        )));
    }
    if let Some(d) = options.max_duration.filter(|d| *d < 0) {
        return Err(DrevoError::InvalidArgument(format!(
            "maxDuration must not be negative, got {d}"
        )));
    }
    Ok(())
}

/// One trip of a hop: its times on its service day, its trip and service.
#[derive(Debug, Clone, Copy)]
struct Entry {
    dep: i64,
    arr: i64,
    trip: i64,
    service: i64,
}

/// A hop leaving a stop, its entries sorted by departure.
#[derive(Debug)]
struct Hop {
    edge: u64,
    to: u64,
    entries: Vec<Entry>,
}

/// What leaves a stop: hops, footpaths, and where each trip leaves it.
#[derive(Debug, Default)]
struct StopData {
    hops: Vec<Hop>,
    /// `(to, seconds, relationship)`.
    walks: Vec<(u64, i64, u64)>,
    /// Trip id → `(hop, entry)` of each time the trip leaves this stop.
    trips: HashMap<i64, Vec<(usize, usize)>>,
}

/// How a stop was reached in a round.
#[derive(Debug, Clone)]
enum Parent {
    /// From the origin.
    Access,
    /// On a trip boarded at `board` in the previous round.
    Ride {
        board: u64,
        depart: i64,
        trip: i64,
        /// Index into the round's scanned hop lists, and how many hops.
        scan: usize,
        hops: usize,
    },
    /// On a footpath from `from`, reached earlier in this round.
    Walk { from: u64, edge: u64, depart: i64 },
}

#[derive(Debug, Clone)]
struct Label {
    time: i64,
    parent: Parent,
}

/// The ways a stop was reached in one round.
#[derive(Debug, Default, Clone)]
struct Reach {
    access: Option<Label>,
    ride: Option<Label>,
    walk: Option<Label>,
}

/// Which way of reaching a stop a leg continues from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Via {
    Access,
    Ride,
    Walk,
}

impl Reach {
    fn get(&self, via: Via) -> Option<&Label> {
        match via {
            Via::Access => self.access.as_ref(),
            Via::Ride => self.ride.as_ref(),
            Via::Walk => self.walk.as_ref(),
        }
    }

    /// The earliest arrival, and how; on a tie, not on foot, so a chain of
    /// footpaths always ends.
    fn arrival(&self) -> Option<(i64, Via)> {
        [Via::Access, Via::Ride, Via::Walk]
            .into_iter()
            .filter_map(|v| self.get(v).map(|l| (l.time, v)))
            .min_by_key(|(t, _)| *t)
    }

    /// The earliest time a vehicle can be boarded, and how the stop was
    /// reached for it: a ride needs the transfer time first.
    fn ready(&self, min_transfer: i64) -> Option<(i64, Via)> {
        [Via::Access, Via::Walk, Via::Ride]
            .into_iter()
            .filter_map(|v| {
                self.get(v)
                    .map(|l| (l.time + if v == Via::Ride { min_transfer } else { 0 }, v))
            })
            .min_by_key(|(t, _)| *t)
    }
}

struct Search<'a, E> {
    options: &'a TimetableOptions,
    edges_of: &'a E,
    /// The cheapest egress per end stop.
    egress: HashMap<u64, i64>,
    /// The services of each day.
    days: Vec<(i64, HashSet<i64>)>,
    limit: i64,
    stops: HashMap<u64, Rc<StopData>>,
    /// Per round, how each stop was reached.
    rounds: Vec<HashMap<u64, Reach>>,
    /// Per round, the hops scanned along each boarded trip.
    scans: Vec<Vec<Vec<u64>>>,
    best_arrival: HashMap<u64, i64>,
    best_ready: HashMap<u64, i64>,
}

impl<'a, E> Search<'a, E>
where
    E: Fn(u64, Direction, &mut dyn FnMut(&Edge)) -> Result<()>,
{
    fn new(ends: &[Endpoint], options: &'a TimetableOptions, edges_of: &'a E) -> Self {
        let mut egress = HashMap::new();
        for e in ends {
            let cost = egress.entry(e.node).or_insert(e.cost);
            *cost = (*cost).min(e.cost);
        }
        Self {
            options,
            edges_of,
            egress,
            days: options
                .days
                .iter()
                .map(|d| (d.offset, d.services.iter().copied().collect()))
                .collect(),
            limit: options
                .max_duration
                .map_or(i64::MAX, |d| options.depart_at.saturating_add(d)),
            stops: HashMap::new(),
            rounds: Vec::new(),
            scans: Vec::new(),
            best_arrival: HashMap::new(),
            best_ready: HashMap::new(),
        }
    }

    fn run(mut self, starts: &[Endpoint]) -> Result<Vec<Journey>> {
        let o = self.options;
        let mut round = HashMap::new();
        let mut marked = HashSet::new();
        for s in starts {
            let time = o.depart_at.saturating_add(s.cost);
            if time > self.limit {
                continue;
            }
            let reach: &mut Reach = round.entry(s.node).or_default();
            if reach.access.as_ref().is_none_or(|l| time < l.time) {
                reach.access = Some(Label {
                    time,
                    parent: Parent::Access,
                });
                self.improve(s.node, time, time, &mut marked);
            }
        }
        let accessed: Vec<(u64, i64)> = round
            .iter()
            .filter_map(|(stop, r)| r.access.as_ref().map(|l| (*stop, l.time)))
            .collect();
        self.walk_from(&accessed, &mut round, &mut marked, i64::MAX)?;
        self.rounds.push(round);
        self.scans.push(Vec::new());

        // The best arrival at the destination so far, and the journeys.
        let mut best = i64::MAX;
        let mut found: Vec<(usize, u64, i64)> = Vec::new();
        if let Some((end, arrive)) = self.destination(0) {
            best = arrive;
            found.push((0, end, arrive));
        }
        for k in 1..=o.max_transfers.saturating_add(1) {
            if marked.is_empty() {
                break;
            }
            let bound = best.min(self.limit.saturating_add(1));
            marked = self.ride_round(k, &marked, bound)?;
            if let Some((end, arrive)) = self.destination(k) {
                if arrive < best {
                    best = arrive;
                    found.push((k, end, arrive));
                }
            }
        }

        let mut journeys: Vec<Journey> = Vec::new();
        for (k, end, arrive) in found {
            let journey = self.journey(k, end, arrive);
            // Zero rides and one ride are both no transfers: keep the earlier.
            journeys.retain(|j| j.transfers != journey.transfers);
            journeys.push(journey);
        }
        journeys.sort_by_key(|j| j.arrive_at);
        Ok(journeys)
    }

    /// Records a better arrival or boarding time at `stop`; marks it when it
    /// can now board earlier.
    fn improve(&mut self, stop: u64, arrival: i64, ready: i64, marked: &mut HashSet<u64>) {
        let a = self.best_arrival.entry(stop).or_insert(i64::MAX);
        *a = (*a).min(arrival);
        let r = self.best_ready.entry(stop).or_insert(i64::MAX);
        if ready < *r {
            *r = ready;
            marked.insert(stop);
        }
    }

    /// Whether arriving at `stop` at `time` (ready to board at `ready`) is
    /// better than anything found so far.
    fn better(&self, stop: u64, time: i64, ready: i64) -> bool {
        time < self.best_arrival.get(&stop).copied().unwrap_or(i64::MAX)
            || ready < self.best_ready.get(&stop).copied().unwrap_or(i64::MAX)
    }

    /// The earliest arrival at the destination over the stops reached in
    /// round `k`.
    fn destination(&self, k: usize) -> Option<(u64, i64)> {
        let round = &self.rounds[k];
        self.egress
            .iter()
            .filter_map(|(stop, cost)| {
                let (time, _) = round.get(stop)?.arrival()?;
                let arrive = time.saturating_add(*cost);
                (arrive <= self.limit).then_some((*stop, arrive))
            })
            .min_by_key(|(stop, arrive)| (*arrive, *stop))
    }

    fn stop(&mut self, stop: u64) -> Result<Rc<StopData>> {
        if let Some(data) = self.stops.get(&stop) {
            return Ok(Rc::clone(data));
        }
        let data = Rc::new(load_stop(stop, self.options, self.edges_of)?);
        self.stops.insert(stop, Rc::clone(&data));
        Ok(data)
    }

    /// Walks on from each `(stop, time)` in the round being built, over
    /// footpaths in a row, earliest first (Dijkstra).
    fn walk_from(
        &mut self,
        from: &[(u64, i64)],
        round: &mut HashMap<u64, Reach>,
        marked: &mut HashSet<u64>,
        bound: i64,
    ) -> Result<()> {
        if self.options.walk.is_none() {
            return Ok(());
        }
        let mut heap: BinaryHeap<Reverse<(i64, u64)>> = from
            .iter()
            .map(|&(stop, time)| Reverse((time, stop)))
            .collect();
        let mut settled = HashSet::new();
        while let Some(Reverse((time, stop))) = heap.pop() {
            if !settled.insert(stop) {
                continue;
            }
            let data = self.stop(stop)?;
            for &(to, seconds, edge) in &data.walks {
                let t = time.saturating_add(seconds);
                if t > self.limit || t >= bound || !self.better(to, t, t) {
                    continue;
                }
                let reach = round.entry(to).or_default();
                if reach.walk.as_ref().is_some_and(|l| l.time <= t) {
                    continue;
                }
                reach.walk = Some(Label {
                    time: t,
                    parent: Parent::Walk {
                        from: stop,
                        edge,
                        depart: time,
                    },
                });
                self.improve(to, t, t, marked);
                heap.push(Reverse((t, to)));
            }
        }
        Ok(())
    }

    /// Round `k`: board every trip leaving a stop marked in round `k - 1`,
    /// ride it to its end, then walk. Returns the stops marked in round `k`.
    fn ride_round(&mut self, k: usize, marked: &HashSet<u64>, bound: i64) -> Result<HashSet<u64>> {
        let min_transfer = self.options.min_transfer;
        // Every boarding: (departure, stop, hop, entry, day).
        let mut boardings = Vec::new();
        let mut sorted: Vec<u64> = marked.iter().copied().collect();
        sorted.sort_unstable();
        for stop in sorted {
            let Some((ready, _)) = self.rounds[k - 1]
                .get(&stop)
                .and_then(|r| r.ready(min_transfer))
            else {
                continue;
            };
            let data = self.stop(stop)?;
            for (h, hop) in data.hops.iter().enumerate() {
                for (d, (offset, services)) in self.days.iter().enumerate() {
                    let first = hop
                        .entries
                        .partition_point(|e| e.dep.saturating_add(*offset) < ready);
                    for (i, e) in hop.entries.iter().enumerate().skip(first) {
                        let dep = e.dep.saturating_add(*offset);
                        if dep >= bound || dep > self.limit {
                            break;
                        }
                        if services.contains(&e.service) {
                            boardings.push((dep, stop, h, i, d));
                        }
                    }
                }
            }
        }
        boardings.sort_unstable();

        let mut round: HashMap<u64, Reach> = HashMap::new();
        let mut next = HashSet::new();
        let mut scans: Vec<Vec<u64>> = Vec::new();
        // (trip, day, hop relationship, entry) already ridden this round.
        let mut ridden: HashSet<(i64, usize, u64, usize)> = HashSet::new();
        let mut reached: Vec<u64> = Vec::new();
        for (depart, board, h, i, d) in boardings {
            let offset = self.days[d].0;
            let mut data = self.stop(board)?;
            let trip = data.hops[h].entries[i].trip;
            let (mut h, mut i) = (h, i);
            let scan = scans.len();
            let mut hops = Vec::new();
            loop {
                let hop = &data.hops[h];
                if !ridden.insert((trip, d, hop.edge, i)) {
                    break;
                }
                hops.push(hop.edge);
                let entry = hop.entries[i];
                let arrive = entry.arr.saturating_add(offset);
                if arrive >= bound || arrive > self.limit {
                    break;
                }
                let to = hop.to;
                if self.better(to, arrive, arrive.saturating_add(min_transfer)) {
                    let reach = round.entry(to).or_default();
                    if reach.ride.as_ref().is_none_or(|l| arrive < l.time) {
                        reach.ride = Some(Label {
                            time: arrive,
                            parent: Parent::Ride {
                                board,
                                depart,
                                trip,
                                scan,
                                hops: hops.len(),
                            },
                        });
                        reached.push(to);
                    }
                    self.improve(to, arrive, arrive.saturating_add(min_transfer), &mut next);
                }
                // The same trip, leaving the stop it just reached.
                data = self.stop(to)?;
                let Some(&(nh, ni)) = data.trips.get(&trip).and_then(|calls| {
                    calls
                        .iter()
                        .filter(|(nh, ni)| {
                            let e = data.hops[*nh].entries[*ni];
                            e.service == entry.service && e.dep >= entry.arr
                        })
                        .min_by_key(|(nh, ni)| data.hops[*nh].entries[*ni].dep)
                }) else {
                    break;
                };
                h = nh;
                i = ni;
            }
            scans.push(hops);
        }
        let walkers: Vec<(u64, i64)> = reached
            .into_iter()
            .collect::<HashSet<_>>()
            .into_iter()
            .filter_map(|stop| round.get(&stop)?.ride.as_ref().map(|l| (stop, l.time)))
            .collect();
        self.walk_from(&walkers, &mut round, &mut next, bound)?;
        self.rounds.push(round);
        self.scans.push(scans);
        Ok(next)
    }

    /// The journey of round `k` arriving at `end` at `arrive`.
    fn journey(&self, k: usize, end: u64, arrive: i64) -> Journey {
        let o = self.options;
        let mut legs = Vec::new();
        let mut round = k;
        let mut stop = end;
        let (time, mut via) = self.rounds[k][&end]
            .arrival()
            .unwrap_or((arrive, Via::Access));
        legs.push(Leg {
            mode: LegMode::Egress,
            from: Some(end),
            to: None,
            depart: time,
            arrive,
            trip: None,
            hops: Vec::new(),
        });
        // A journey has at most one leg per stop and round.
        let most = self.rounds.iter().map(HashMap::len).sum::<usize>() + 2;
        while legs.len() <= most {
            let Some(label) = self.rounds[round].get(&stop).and_then(|r| r.get(via)) else {
                break;
            };
            match &label.parent {
                Parent::Access => {
                    legs.push(Leg {
                        mode: LegMode::Access,
                        from: None,
                        to: Some(stop),
                        depart: o.depart_at,
                        arrive: label.time,
                        trip: None,
                        hops: Vec::new(),
                    });
                    break;
                }
                Parent::Walk { from, edge, depart } => {
                    legs.push(Leg {
                        mode: LegMode::Walk,
                        from: Some(*from),
                        to: Some(stop),
                        depart: *depart,
                        arrive: label.time,
                        trip: None,
                        hops: vec![*edge],
                    });
                    stop = *from;
                    via = self.rounds[round]
                        .get(from)
                        .and_then(Reach::arrival)
                        .map_or(Via::Access, |(_, v)| v);
                }
                Parent::Ride {
                    board,
                    depart,
                    trip,
                    scan,
                    hops,
                } => {
                    legs.push(Leg {
                        mode: LegMode::Ride,
                        from: Some(*board),
                        to: Some(stop),
                        depart: *depart,
                        arrive: label.time,
                        trip: Some(*trip),
                        hops: self.scans[round][*scan][..*hops].to_vec(),
                    });
                    stop = *board;
                    round -= 1;
                    let reach = &self.rounds[round][board];
                    via = [Via::Access, Via::Walk, Via::Ride]
                        .into_iter()
                        .filter_map(|v| {
                            let l = reach.get(v)?;
                            let ready = l.time + if v == Via::Ride { o.min_transfer } else { 0 };
                            (ready <= *depart).then_some((ready, v))
                        })
                        .min_by_key(|(ready, _)| *ready)
                        .map_or(Via::Access, |(_, v)| v);
                }
            }
        }
        legs.reverse();
        // Leave as late as the first ride allows: the legs before it move later.
        if let Some(first) = legs.iter().position(|l| l.mode == LegMode::Ride) {
            let slack = legs[first].depart - legs[first - 1].arrive;
            for leg in &mut legs[..first] {
                leg.depart += slack;
                leg.arrive += slack;
            }
        }
        let rides = legs.iter().filter(|l| l.mode == LegMode::Ride).count();
        Journey {
            depart_at: legs.first().map_or(o.depart_at, |l| l.depart),
            arrive_at: arrive,
            transfers: rides.saturating_sub(1),
            legs,
        }
    }
}

/// Reads the hops and footpaths leaving `stop`.
fn load_stop<E>(stop: u64, options: &TimetableOptions, edges_of: &E) -> Result<StopData>
where
    E: Fn(u64, Direction, &mut dyn FnMut(&Edge)) -> Result<()>,
{
    let fields = &options.hops;
    let mut data = StopData::default();
    let mut error = None;
    edges_of(stop, Direction::Outgoing, &mut |edge: &Edge| {
        if error.is_some() || edge.from_id != stop {
            return;
        }
        if edge.kind == fields.kind {
            match read_hop(edge, fields) {
                Ok(hop) => data.hops.push(hop),
                Err(e) => error = Some(e),
            }
        } else if let Some(walk) = options
            .walk
            .as_ref()
            .filter(|w| w.kinds.contains(&edge.kind))
        {
            match edge.properties.get(&walk.cost) {
                None | Some(Value::Null) => {}
                Some(v) => match whole(v) {
                    Some(s) if s >= 0 => data.walks.push((edge.to_id, s, edge.id)),
                    _ => {
                        error = Some(DrevoError::InvalidArgument(format!(
                            "relationship {} of type {}: `{}` must be a non-negative number of seconds, got {v}",
                            edge.id, edge.kind, walk.cost
                        )));
                    }
                },
            }
        }
    })?;
    if let Some(e) = error {
        return Err(e);
    }
    data.hops.sort_by_key(|h| h.edge);
    data.walks.sort_unstable();
    for (h, hop) in data.hops.iter().enumerate() {
        for (i, e) in hop.entries.iter().enumerate() {
            data.trips.entry(e.trip).or_default().push((h, i));
        }
    }
    Ok(data)
}

/// A whole number from JSON.
fn whole(v: &Value) -> Option<i64> {
    v.as_i64().or_else(|| {
        v.as_f64()
            .filter(|f| f.is_finite() && f.fract() == 0.0)
            .map(|f| f as i64)
    })
}

fn read_hop(edge: &Edge, fields: &HopFields) -> Result<Hop> {
    let list = |name: &str| -> Result<Vec<i64>> {
        let refuse = || {
            DrevoError::InvalidArgument(format!(
                "relationship {} of type {}: `{name}` must be a list of whole numbers",
                edge.id, edge.kind
            ))
        };
        match edge.properties.get(name) {
            Some(Value::Array(items)) => {
                items.iter().map(|v| whole(v).ok_or_else(refuse)).collect()
            }
            _ => Err(refuse()),
        }
    };
    let (dep, arr, trip, service) = (
        list(&fields.dep)?,
        list(&fields.arr)?,
        list(&fields.trip)?,
        list(&fields.service)?,
    );
    if arr.len() != dep.len() || trip.len() != dep.len() || service.len() != dep.len() {
        return Err(DrevoError::InvalidArgument(format!(
            "relationship {} of type {}: `{}`, `{}`, `{}` and `{}` must have the same length",
            edge.id, edge.kind, fields.dep, fields.arr, fields.trip, fields.service
        )));
    }
    let mut entries: Vec<Entry> = (0..dep.len())
        .map(|i| Entry {
            dep: dep[i],
            arr: arr[i],
            trip: trip[i],
            service: service[i],
        })
        .collect();
    if let Some(e) = entries.iter().find(|e| e.arr < e.dep) {
        return Err(DrevoError::InvalidArgument(format!(
            "relationship {} of type {}: trip {} arrives ({}) before it departs ({})",
            edge.id, edge.kind, e.trip, e.arr, e.dep
        )));
    }
    entries.sort_by_key(|e| (e.dep, e.trip));
    Ok(Hop {
        edge: edge.id,
        to: edge.to_id,
        entries,
    })
}
