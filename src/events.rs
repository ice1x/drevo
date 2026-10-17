//! `GET /events`: committed changes as a Server-Sent Events stream.
//!
//! Every node or relationship a committed write creates, changes or deletes
//! is one event, in commit order. Rolled-back work never appears: the stream
//! reads the change feed, which holds committed changes only.
//!
//! ```text
//! GET /events?since=41&labels=Task,DEPENDS_ON
//!
//! id: 42
//! event: node
//! data: {"op":"upsert","id":7,"uuid":"0192…","labels":["Task"],"cursor":42}
//!
//! id: 43
//! event: relationship
//! data: {"op":"upsert","id":3,"uuid":"0192…","type":"DEPENDS_ON","from":7,"to":8,"cursor":43}
//!
//! id: 44
//! event: node
//! data: {"op":"delete","id":8,"cursor":44}
//! ```
//!
//! An event names what changed, not its values: read them with a query.
//!
//! Events an application posts with `CALL drevo.events.post(name, payload)`
//! arrive as `posted` events once the posting write commits, after the
//! changes it committed:
//!
//! ```text
//! id: 45:1
//! event: posted
//! data: {"name":"invoice_approved","payload":{"invoice":"INV-7"},"cursor":45}
//! ```
//!
//! - **Resuming.** The SSE `id` is the stream position: the change cursor,
//!   followed by `:<n>` once named events have been posted, `n` counting
//!   them. A client resumes after a reconnect from `?since=<id>` or the
//!   `Last-Event-ID` header, which `EventSource` sends by itself; `since`
//!   also takes a bare cursor from an event's data. Without either, the
//!   stream starts at the latest change.
//! - **`lagged`.** The server keeps the latest changes and named events
//!   (10 000 of each by default) for resuming; named events are kept in
//!   memory only. A position older than that, or one the server never
//!   issued (after a restart), gets a `lagged` event with the current
//!   cursor: reload, then follow from there.
//! - **Filters.** `labels` keeps the nodes with any of the listed labels and
//!   the relationships of any of the listed types; `names` keeps the named
//!   events with any of the listed names. Naming one kind leaves the other
//!   out unless it is named too. A deletion carries only the id, since what
//!   was deleted is gone, so it passes `labels`. Deleting a node removes its
//!   relationships with it, without an event for each.
//! - **Privileges.** With logins required, a node or relationship the
//!   subscriber may not traverse is left out.
//! - **Database.** `?db=<name>` follows another database than the default.

use std::collections::{BTreeSet, VecDeque};
use std::convert::Infallible;
use std::sync::Arc;
use std::time::Duration;

use axum::extract::{Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use serde::Deserialize;

use crate::event_bus::Posted;
use crate::native::WalOp;
use crate::native_api::NativeApiState;
use crate::native_service::NativeService;

/// How often an idle stream looks for new changes.
const POLL: Duration = Duration::from_millis(200);

/// The query string of `GET /events`.
#[derive(Debug, Default, Deserialize)]
pub struct EventsQuery {
    /// Send the changes after this cursor.
    since: Option<String>,
    /// Comma-separated labels and relationship types to keep.
    labels: Option<String>,
    /// Comma-separated names of the posted events to keep.
    names: Option<String>,
    /// The database to follow; the default one when absent.
    db: Option<String>,
}

/// What a subscriber may see.
struct Filter {
    labels: Option<BTreeSet<String>>,
    names: Option<BTreeSet<String>>,
    context: Option<drevo_auth::privileges::SecurityContext>,
    database: String,
}

impl Filter {
    /// Whether graph changes are followed at all: unless only named events
    /// were asked for.
    fn changes(&self) -> bool {
        self.labels.is_some() || self.names.is_none()
    }

    fn posted(&self, name: &str) -> bool {
        match &self.names {
            Some(want) => want.contains(name),
            None => self.labels.is_none(),
        }
    }

    fn deletion(&self) -> bool {
        self.changes()
    }

    fn node(&self, labels: &[String]) -> bool {
        if !self.changes() {
            return false;
        }
        let named = self
            .labels
            .as_ref()
            .is_none_or(|want| labels.iter().any(|l| want.contains(l)));
        let allowed = self.context.as_ref().is_none_or(|c| {
            let refs: Vec<&str> = labels.iter().map(String::as_str).collect();
            c.can_traverse_node(&self.database, &refs)
        });
        named && allowed
    }

    fn relationship(&self, rel_type: &str) -> bool {
        if !self.changes() {
            return false;
        }
        let named = self
            .labels
            .as_ref()
            .is_none_or(|want| want.contains(rel_type));
        let allowed = self
            .context
            .as_ref()
            .is_none_or(|c| c.can_traverse_relationship(&self.database, rel_type));
        named && allowed
    }
}

/// A stream position as an SSE id: the change cursor, and the number of the
/// latest named event once there is one.
fn position(cursor: u64, seq: u64) -> String {
    if seq == 0 {
        cursor.to_string()
    } else {
        format!("{cursor}:{seq}")
    }
}

/// Where a client asked to start.
#[derive(Debug, Clone, Copy, PartialEq)]
enum Since {
    /// At the latest change.
    Latest,
    /// After the change at this cursor.
    Cursor(u64),
    /// At a position this stream sent.
    Position(u64, u64),
}

/// Read `since` or `Last-Event-ID`: empty, `<cursor>` or `<cursor>:<n>`.
fn parse_since(text: Option<&str>) -> Option<Since> {
    let text = match text.map(str::trim) {
        None | Some("") => return Some(Since::Latest),
        Some(text) => text,
    };
    match text.split_once(':') {
        None => text.parse().ok().map(Since::Cursor),
        Some((cursor, seq)) => Some(Since::Position(cursor.parse().ok()?, seq.parse().ok()?)),
    }
}

/// The SSE event for change `op` at `cursor`, sent with SSE id `id`, or
/// `None` when it is not shown: filtered out, or not a node or relationship
/// change.
fn event_for(op: &WalOp, cursor: u64, id: String, filter: &Filter) -> Option<Event> {
    let uuid = |bytes: &[u8; 16]| uuid::Uuid::from_bytes(*bytes).hyphenated().to_string();
    let (name, data) = match op {
        WalOp::UpsertNode(node) => {
            let labels = crate::cypher::executor::node_labels_from_storage(node);
            if !filter.node(&labels) {
                return None;
            }
            (
                "node",
                serde_json::json!({
                    "op": "upsert",
                    "id": node.id,
                    "uuid": uuid(&node.uuid),
                    "labels": labels,
                    "cursor": cursor,
                }),
            )
        }
        WalOp::UpsertEdge(edge) => {
            if !filter.relationship(&edge.kind) {
                return None;
            }
            (
                "relationship",
                serde_json::json!({
                    "op": "upsert",
                    "id": edge.id,
                    "uuid": uuid(&edge.uuid),
                    "type": edge.kind,
                    "from": edge.from_id,
                    "to": edge.to_id,
                    "cursor": cursor,
                }),
            )
        }
        WalOp::DeleteNode(gone) if filter.deletion() => (
            "node",
            serde_json::json!({ "op": "delete", "id": gone, "cursor": cursor }),
        ),
        WalOp::DeleteEdge(gone) if filter.deletion() => (
            "relationship",
            serde_json::json!({ "op": "delete", "id": gone, "cursor": cursor }),
        ),
        _ => return None,
    };
    Some(Event::default().id(id).event(name).data(data.to_string()))
}

/// The SSE event for named event `posted`, sent with SSE id `id`.
fn posted_event(posted: &Posted, id: String) -> Event {
    Event::default().id(id).event("posted").data(
        serde_json::json!({
            "name": posted.name,
            "payload": posted.payload,
            "cursor": posted.cursor,
        })
        .to_string(),
    )
}

/// A `lagged` event: what came before `cursor` is gone; reload.
fn lagged(cursor: u64, seq: u64) -> Event {
    Event::default()
        .id(position(cursor, seq))
        .event("lagged")
        .data(serde_json::json!({ "cursor": cursor }).to_string())
}

/// One subscriber's place in the change feed and among the named events.
struct Follower {
    service: Arc<NativeService>,
    cursor: u64,
    seq: u64,
    filter: Filter,
    pending: VecDeque<Event>,
}

impl Follower {
    /// Tell the subscriber to reload, and follow from the latest change and
    /// named event.
    fn lag(&mut self, cursor: u64, seq: u64) {
        self.cursor = cursor;
        self.seq = seq;
        self.pending.push_back(lagged(cursor, seq));
    }

    /// Queue the events after the position and move it on; `false` when
    /// there was nothing new.
    fn catch_up(&mut self) -> bool {
        // Named events first: each is stamped with a cursor its changes
        // reached, so the changes read next include them.
        let posted = self.service.event_bus().after(self.seq);
        let head = self.service.change_head();
        if self.cursor > head || posted.lagged {
            // A position this server never issued (it restarted, or the
            // position is someone else's), or named events no longer kept.
            self.lag(head, posted.head);
            return true;
        }
        let ops = if self.cursor < head {
            let batch = self.service.changes_since(self.cursor);
            if batch.lagged {
                self.lag(batch.cursor, posted.head);
                return true;
            }
            batch.ops
        } else {
            Vec::new()
        };
        let end = self.cursor + ops.len() as u64;
        let mut named = posted.events.into_iter().peekable();
        let mut moved = !ops.is_empty();
        let mut send_named = |f: &mut Self, upto: u64| {
            while let Some(e) = named.next_if(|e| e.cursor <= upto) {
                f.seq = e.seq;
                moved = true;
                if f.filter.posted(&e.name) {
                    let id = position(f.cursor, f.seq);
                    f.pending.push_back(posted_event(&e, id));
                }
            }
        };
        for op in &ops {
            let at = self.cursor + 1;
            send_named(self, at - 1);
            self.cursor = at;
            if let Some(event) = event_for(op, at, position(at, self.seq), &self.filter) {
                self.pending.push_back(event);
            }
        }
        // Events stamped past the changes read wait for the next round.
        send_named(self, end);
        moved
    }
}

/// `GET /events` — see the [module docs](self).
pub async fn stream(
    State(state): State<NativeApiState>,
    signed_in: Option<axum::Extension<crate::security::SignedIn>>,
    headers: HeaderMap,
    Query(query): Query<EventsQuery>,
) -> Response {
    let bad = |message: String| {
        (
            StatusCode::BAD_REQUEST,
            axum::Json(serde_json::json!({ "error": message })),
        )
            .into_response()
    };
    let since = query.since.or_else(|| {
        headers
            .get("last-event-id")
            .and_then(|v| v.to_str().ok())
            .map(str::to_string)
    });
    let registry = state.registry();
    let database = query
        .db
        .filter(|d| !d.is_empty())
        .unwrap_or_else(|| registry.default_name().to_string());
    let Some(service) = registry.get(&database) else {
        return (
            StatusCode::NOT_FOUND,
            axum::Json(serde_json::json!({ "error": format!("no database named `{database}`") })),
        )
            .into_response();
    };
    let Some(since) = parse_since(since.as_deref()) else {
        let text = since.unwrap_or_default();
        return bad(format!("`{text}` is not a stream position"));
    };
    let signed_in = signed_in.map(|axum::Extension(user)| user);
    let context = state.security().context_for(signed_in.as_ref(), &database);
    if let Some(c) = &context {
        if !c.can_access(&database) {
            return (
                StatusCode::FORBIDDEN,
                axum::Json(serde_json::json!({
                    "error": format!("access to database `{database}` is not allowed"),
                    "code": "Neo.ClientError.Security.Forbidden",
                })),
            )
                .into_response();
        }
    }
    let list = |l: Option<String>| {
        l.map(|l| {
            l.split(',')
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_string)
                .collect::<BTreeSet<_>>()
        })
        .filter(|l| !l.is_empty())
    };
    let mut follower = Follower {
        cursor: 0,
        seq: 0,
        filter: Filter {
            labels: list(query.labels),
            names: list(query.names),
            context,
            database,
        },
        pending: VecDeque::new(),
        service,
    };
    match since {
        Since::Latest => {
            follower.seq = follower.service.event_bus().head();
            follower.cursor = follower.service.change_head();
        }
        Since::Position(cursor, seq) => {
            follower.cursor = cursor;
            follower.seq = seq;
        }
        Since::Cursor(cursor) => match follower.service.event_bus().seq_before_cursor(cursor) {
            Some(seq) => {
                follower.cursor = cursor;
                follower.seq = seq;
            }
            None => {
                let seq = follower.service.event_bus().head();
                let head = follower.service.change_head();
                follower.lag(head, seq);
            }
        },
    }
    let events = futures::stream::unfold(follower, |mut f| async move {
        loop {
            if let Some(event) = f.pending.pop_front() {
                return Some((Ok::<Event, Infallible>(event), f));
            }
            if !f.catch_up() {
                tokio::time::sleep(POLL).await;
            }
        }
    });
    Sse::new(events)
        .keep_alive(KeepAlive::default())
        .into_response()
}
