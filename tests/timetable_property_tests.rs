//! [`earliest_arrivals`] against a brute-force search on random small
//! timetables: for every number of transfers, the earliest arrival is the
//! same, and every journey returned is one a passenger can actually make.

use std::cmp::Reverse;
use std::collections::{BTreeSet, BinaryHeap, HashSet};

use drevo::error::Result;
use drevo::model::{Direction, Edge, Properties};
use drevo::timetable::{
    earliest_arrivals, Endpoint, HopFields, Journey, LegMode, ServiceDay, TimetableOptions,
    WalkFields,
};
use proptest::prelude::*;
use serde_json::json;

/// One trip: its id, service, and the stops it calls at with their times.
#[derive(Debug, Clone)]
struct Trip {
    id: i64,
    service: i64,
    /// `(stop, arrival, departure)` in order; times relative to the trip's day.
    calls: Vec<(u64, i64, i64)>,
}

#[derive(Debug, Clone)]
struct Network {
    stops: u64,
    trips: Vec<Trip>,
    /// `(from, to, seconds)`.
    walks: Vec<(u64, u64, i64)>,
    starts: Vec<Endpoint>,
    ends: Vec<Endpoint>,
    options: TimetableOptions,
}

impl Network {
    /// The hops as relationships: one per (stop sequence, consecutive pair),
    /// with one list entry per trip sharing that sequence, and the walks.
    fn edges(&self) -> Vec<Edge> {
        let mut edges = Vec::new();
        let mut patterns: Vec<(Vec<u64>, Vec<&Trip>)> = Vec::new();
        for trip in &self.trips {
            let stops: Vec<u64> = trip.calls.iter().map(|c| c.0).collect();
            match patterns.iter_mut().find(|(s, _)| *s == stops) {
                Some((_, trips)) => trips.push(trip),
                None => patterns.push((stops, vec![trip])),
            }
        }
        let mut id = 1000;
        for (stops, mut trips) in patterns {
            for hop in 0..stops.len() - 1 {
                trips.sort_by_key(|t| t.calls[hop].2);
                let mut props = Properties::new();
                props.insert(
                    "dep",
                    json!(trips.iter().map(|t| t.calls[hop].2).collect::<Vec<_>>()),
                );
                props.insert(
                    "arr",
                    json!(trips.iter().map(|t| t.calls[hop + 1].1).collect::<Vec<_>>()),
                );
                props.insert(
                    "trip",
                    json!(trips.iter().map(|t| t.id).collect::<Vec<_>>()),
                );
                props.insert(
                    "svc",
                    json!(trips.iter().map(|t| t.service).collect::<Vec<_>>()),
                );
                edges.push(edge(id, stops[hop], stops[hop + 1], "HOP", props));
                id += 1;
            }
        }
        for &(from, to, s) in &self.walks {
            let mut props = Properties::new();
            props.insert("s", json!(s));
            edges.push(edge(id, from, to, "WALK", props));
            id += 1;
        }
        edges
    }
}

fn edge(id: u64, from: u64, to: u64, kind: &str, properties: Properties) -> Edge {
    Edge {
        id,
        uuid: [0; 16],
        from_id: from,
        to_id: to,
        kind: kind.into(),
        weight: 1.0,
        created_at: 0,
        properties,
    }
}

fn plan(net: &Network) -> Result<Vec<Journey>> {
    let edges = net.edges();
    earliest_arrivals(
        &net.starts,
        &net.ends,
        &net.options,
        &|id, dir, visit: &mut dyn FnMut(&Edge)| {
            assert_eq!(dir, Direction::Outgoing);
            edges.iter().filter(|e| e.from_id == id).for_each(visit);
            Ok(())
        },
    )
}

/// A passenger at a stop, or on a trip, in the brute-force search.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
enum State {
    /// At `stop` after `rides` rides, able to board from `ready` (a change
    /// of vehicle after a ride takes the transfer time, over footpaths too),
    /// having walked `walked` seconds since the last ride or the start.
    At {
        stop: u64,
        rides: usize,
        ready: i64,
        walked: i64,
    },
    /// On trip `trip` (index) of day `day`, about to arrive at call `call`.
    On {
        trip: usize,
        day: usize,
        call: usize,
        rides: usize,
    },
}

/// The earliest arrival at the destination with at most `r` rides, for every
/// `r` up to `max_transfers + 1`, by Dijkstra over time on the passenger's
/// states.
fn brute_force(net: &Network) -> Vec<Option<i64>> {
    let o = &net.options;
    let max_rides = o.max_transfers + 1;
    let limit = o.max_duration.map_or(i64::MAX, |d| o.depart_at + d);
    // States carry the time walked and the readiness, so walking round a
    // cycle of footpaths makes new states for ever. Nothing a cycle adds can
    // beat a way without it, and every way without one is over by the time
    // the last trip has arrived and every footpath has been walked once.
    let latest_arrival = net
        .trips
        .iter()
        .flat_map(|t| {
            o.days
                .iter()
                .filter(|d| d.services.contains(&t.service))
                .map(|d| t.calls.last().map_or(0, |c| c.1) + d.offset)
        })
        .max()
        .unwrap_or(o.depart_at);
    let horizon = latest_arrival
        .max(o.depart_at + net.starts.iter().map(|s| s.cost).max().unwrap_or(0))
        + net.walks.iter().map(|w| w.2).sum::<i64>();
    let mut heap = BinaryHeap::new();
    for s in &net.starts {
        heap.push(Reverse((
            o.depart_at + s.cost,
            State::At {
                stop: s.node,
                rides: 0,
                ready: o.depart_at + s.cost,
                walked: 0,
            },
        )));
    }
    let mut seen = HashSet::new();
    let mut best = vec![None::<i64>; max_rides + 1];
    while let Some(Reverse((time, state))) = heap.pop() {
        if time > limit || time > horizon || !seen.insert(state) {
            continue;
        }
        match state {
            State::At {
                stop,
                rides,
                ready,
                walked,
            } => {
                for e in net.ends.iter().filter(|e| e.node == stop) {
                    let arrive = time + e.cost;
                    if arrive <= limit {
                        for slot in &mut best[rides..] {
                            *slot = Some(slot.map_or(arrive, |b: i64| b.min(arrive)));
                        }
                    }
                }
                for &(from, to, s) in &net.walks {
                    if from == stop && o.max_walk.is_none_or(|m| walked + s <= m) {
                        heap.push(Reverse((
                            time + s,
                            State::At {
                                stop: to,
                                rides,
                                ready: ready.max(time + s),
                                walked: walked + s,
                            },
                        )));
                    }
                }
                if rides == max_rides {
                    continue;
                }
                for (t, trip) in net.trips.iter().enumerate() {
                    for (d, day) in o.days.iter().enumerate() {
                        if !day.services.contains(&trip.service) {
                            continue;
                        }
                        for (c, call) in trip.calls.iter().enumerate().take(trip.calls.len() - 1) {
                            if call.0 == stop && call.2 + day.offset >= ready {
                                heap.push(Reverse((
                                    call.2 + day.offset,
                                    State::On {
                                        trip: t,
                                        day: d,
                                        call: c + 1,
                                        rides: rides + 1,
                                    },
                                )));
                            }
                        }
                    }
                }
            }
            State::On {
                trip,
                day,
                call,
                rides,
            } => {
                let calls = &net.trips[trip].calls;
                let offset = o.days[day].offset;
                let arrive = calls[call].1 + offset;
                heap.push(Reverse((
                    arrive,
                    State::At {
                        stop: calls[call].0,
                        rides,
                        ready: arrive + o.min_transfer,
                        walked: 0,
                    },
                )));
                if call + 1 < calls.len() {
                    heap.push(Reverse((
                        calls[call].2 + offset,
                        State::On {
                            trip,
                            day,
                            call: call + 1,
                            rides,
                        },
                    )));
                }
            }
        }
    }
    best
}

/// Checks that a journey is one a passenger can make on `net`.
fn check_journey(net: &Network, j: &Journey) {
    let o = &net.options;
    let legs = &j.legs;
    assert!(legs.len() >= 2, "{j:?}");
    assert_eq!(legs[0].mode, LegMode::Access, "{j:?}");
    assert_eq!(legs.last().unwrap().mode, LegMode::Egress, "{j:?}");
    assert_eq!(legs[0].depart, j.depart_at, "{j:?}");
    assert!(j.depart_at >= o.depart_at, "{j:?}");
    assert_eq!(legs.last().unwrap().arrive, j.arrive_at, "{j:?}");
    let start = legs[0].to.unwrap();
    let access = net
        .starts
        .iter()
        .filter(|s| s.node == start)
        .map(|s| s.cost)
        .min();
    assert_eq!(Some(legs[0].arrive - legs[0].depart), access, "{j:?}");
    let last = legs.last().unwrap();
    let egress = net
        .ends
        .iter()
        .filter(|e| Some(e.node) == last.from)
        .map(|e| e.cost)
        .min();
    assert_eq!(Some(last.arrive - last.depart), egress, "{j:?}");
    let rides = legs.iter().filter(|l| l.mode == LegMode::Ride).count();
    assert_eq!(j.transfers, rides.saturating_sub(1), "{j:?}");
    let edges = net.edges();
    for pair in legs.windows(2) {
        assert_eq!(pair[0].to, pair[1].from, "legs connect: {j:?}");
        assert!(pair[1].depart >= pair[0].arrive, "no time travel: {j:?}");
    }
    let mut last_ride_arrive = None;
    let mut walked = 0;
    for leg in legs {
        match leg.mode {
            LegMode::Ride => {
                if let Some(arrive) = last_ride_arrive {
                    assert!(
                        leg.depart >= arrive + o.min_transfer,
                        "a change of vehicle, over footpaths or not, takes the transfer time: {j:?}"
                    );
                }
                last_ride_arrive = Some(leg.arrive);
                walked = 0;
            }
            LegMode::Walk => {
                walked += leg.arrive - leg.depart;
                if let Some(max) = o.max_walk {
                    assert!(
                        walked <= max,
                        "footpaths in a row stay within maxWalk: {j:?}"
                    );
                }
            }
            LegMode::Access | LegMode::Egress => {}
        }
    }
    for leg in legs {
        match leg.mode {
            LegMode::Ride => {
                let trip = net
                    .trips
                    .iter()
                    .find(|t| Some(t.id) == leg.trip)
                    .expect("a real trip");
                let day =
                    o.days
                        .iter()
                        .find(|d| {
                            d.services.contains(&trip.service)
                                && trip.calls.iter().any(|c| {
                                    c.0 == leg.from.unwrap() && c.2 + d.offset == leg.depart
                                })
                        })
                        .expect("the trip runs then");
                let board = trip
                    .calls
                    .iter()
                    .position(|c| c.0 == leg.from.unwrap() && c.2 + day.offset == leg.depart)
                    .unwrap();
                let alight = board + leg.hops.len();
                assert_eq!(trip.calls[alight].0, leg.to.unwrap(), "{j:?}");
                assert_eq!(trip.calls[alight].1 + day.offset, leg.arrive, "{j:?}");
                for (i, hop) in leg.hops.iter().enumerate() {
                    let e = edges.iter().find(|e| e.id == *hop).expect("a hop");
                    assert_eq!(
                        (e.from_id, e.to_id),
                        (trip.calls[board + i].0, trip.calls[board + i + 1].0)
                    );
                }
            }
            LegMode::Walk => {
                assert_eq!(leg.hops.len(), 1, "{j:?}");
                let e = edges
                    .iter()
                    .find(|e| e.id == leg.hops[0])
                    .expect("a real footpath");
                assert_eq!(
                    (e.kind.as_str(), Some(e.from_id), Some(e.to_id)),
                    ("WALK", leg.from, leg.to),
                    "{j:?}"
                );
                assert_eq!(
                    Some(leg.arrive - leg.depart),
                    e.properties.get("s").and_then(|v| v.as_i64()),
                    "{j:?}"
                );
            }
            LegMode::Access | LegMode::Egress => assert!(leg.hops.is_empty()),
        }
    }
}

prop_compose! {
    fn trip_strategy(stops: u64)(
        route in prop::collection::vec(0..stops, 2..5),
        start in 0i64..400,
        gaps in prop::collection::vec((10i64..80, 0i64..20), 4),
        service in 1i64..=3,
        late in prop::bool::weighted(0.2),
    ) -> (Vec<u64>, i64, Vec<(i64, i64)>, i64, bool) {
        (route, start, gaps, service, late)
    }
}

prop_compose! {
    fn network()(stops in 3u64..7)(
        stops in Just(stops),
        trips in prop::collection::vec(trip_strategy(stops), 1..9),
        walks in prop::collection::vec((0..stops, 0..stops, 5i64..120), 0..5),
        starts in prop::collection::vec((0..stops, 0i64..60), 1..3),
        ends in prop::collection::vec((0..stops, 0i64..60), 1..3),
        depart_at in 0i64..200,
        min_transfer in 0i64..40,
        max_transfers in 0usize..4,
        max_duration in prop::option::of(100i64..600),
        max_walk in prop::option::of(5i64..250),
        repeat in 1usize..3,
    ) -> Network {
        let mut out = Vec::new();
        let mut id = 1;
        for (route, start, gaps, service, late) in trips {
            let mut route = route;
            route.dedup();
            if route.len() < 2 {
                continue;
            }
            // `repeat` trips on the same stop sequence share the hop relationships.
            for k in 0..repeat as i64 {
                let mut t = start + k * 37 + if late { 86_400 - 300 } else { 0 };
                let mut calls = Vec::new();
                for (i, stop) in route.iter().enumerate() {
                    let (run, dwell) = gaps[i % gaps.len()];
                    let arrive = t;
                    let depart = arrive + dwell;
                    calls.push((*stop, arrive, depart));
                    t = depart + run;
                }
                out.push(Trip { id, service, calls });
                id += 1;
            }
        }
        Network {
            stops,
            trips: out,
            walks: walks.into_iter().filter(|w| w.0 != w.1).collect(),
            starts: starts.into_iter().map(|(node, cost)| Endpoint { node, cost }).collect(),
            ends: ends.into_iter().map(|(node, cost)| Endpoint { node, cost }).collect(),
            options: TimetableOptions {
                depart_at,
                days: vec![
                    ServiceDay { offset: 0, services: vec![1, 2] },
                    ServiceDay { offset: -86_400, services: vec![3] },
                ],
                hops: HopFields::default(),
                walk: Some(WalkFields { kinds: vec!["WALK".into()], cost: "s".into() }),
                min_transfer,
                max_transfers,
                max_duration,
                max_walk,
            },
        }
    }
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 400, ..ProptestConfig::default() })]

    #[test]
    fn earliest_arrival_per_transfer_count_matches_brute_force(net in network()) {
        let journeys = plan(&net).expect("plan");
        let expected = brute_force(&net);
        for t in 0..=net.options.max_transfers {
            let got = journeys.iter().filter(|j| j.transfers <= t).map(|j| j.arrive_at).min();
            prop_assert_eq!(got, expected[t + 1], "at most {} transfers on {:?}: {:?}", t, net, journeys);
        }
        for j in &journeys {
            check_journey(&net, j);
        }
        // Pareto: strictly earlier with every extra transfer, ordered by arrival.
        let arrivals: Vec<i64> = journeys.iter().map(|j| j.arrive_at).collect();
        let transfers: BTreeSet<usize> = journeys.iter().map(|j| j.transfers).collect();
        prop_assert_eq!(transfers.len(), journeys.len(), "one journey per transfer count");
        for pair in journeys.windows(2) {
            prop_assert!(pair[0].arrive_at < pair[1].arrive_at, "{:?}", arrivals);
            prop_assert!(pair[0].transfers > pair[1].transfers, "{:?}", journeys);
        }
        let _ = net.stops;
    }
}
