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
//! - **Resuming.** The SSE `id` is the change's position (its cursor). A
//!   client resumes after a reconnect from `?since=<cursor>` or the
//!   `Last-Event-ID` header, which `EventSource` sends by itself. Without
//!   either, the stream starts at the latest change.
//! - **`lagged`.** The server keeps the latest changes (10 000 by default)
//!   for resuming. An older cursor, or one the server never issued (after a
//!   restart), gets a `lagged` event with the current cursor: reload, then
//!   follow from there.
//! - **Filters.** `labels` keeps the nodes with any of the listed labels and
//!   the relationships of any of the listed types. A deletion carries only
//!   the id, since what was deleted is gone, so it always passes. Deleting a
//!   node removes its relationships with it, without an event for each.
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
    /// The database to follow; the default one when absent.
    db: Option<String>,
}

/// What a subscriber may see.
struct Filter {
    labels: Option<BTreeSet<String>>,
    context: Option<drevo_auth::privileges::SecurityContext>,
    database: String,
}

impl Filter {
    fn node(&self, labels: &[String]) -> bool {
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

/// The SSE event for change `op` at `cursor`, or `None` when it is not
/// shown: filtered out, or not a node or relationship change.
fn event_for(op: &WalOp, cursor: u64, filter: &Filter) -> Option<Event> {
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
        WalOp::DeleteNode(id) => (
            "node",
            serde_json::json!({ "op": "delete", "id": id, "cursor": cursor }),
        ),
        WalOp::DeleteEdge(id) => (
            "relationship",
            serde_json::json!({ "op": "delete", "id": id, "cursor": cursor }),
        ),
        _ => return None,
    };
    Some(
        Event::default()
            .id(cursor.to_string())
            .event(name)
            .data(data.to_string()),
    )
}

/// A `lagged` event: changes before `cursor` are gone; reload.
fn lagged(cursor: u64) -> Event {
    Event::default()
        .id(cursor.to_string())
        .event("lagged")
        .data(serde_json::json!({ "cursor": cursor }).to_string())
}

/// One subscriber's place in the change feed.
struct Follower {
    service: Arc<NativeService>,
    cursor: u64,
    filter: Filter,
    pending: VecDeque<Event>,
}

impl Follower {
    /// Queue the events after the cursor and move it on; `false` when there
    /// were no new changes.
    fn catch_up(&mut self) -> bool {
        let head = self.service.change_head();
        if self.cursor > head {
            // A cursor this server never issued: it restarted, or the
            // cursor is someone else's.
            self.pending.push_back(lagged(head));
            self.cursor = head;
            return true;
        }
        if self.cursor == head {
            return false;
        }
        let batch = self.service.changes_since(self.cursor);
        if batch.lagged {
            self.pending.push_back(lagged(batch.cursor));
        } else {
            for (i, op) in batch.ops.iter().enumerate() {
                let at = self.cursor + i as u64 + 1;
                if let Some(event) = event_for(op, at, &self.filter) {
                    self.pending.push_back(event);
                }
            }
        }
        self.cursor = batch.cursor;
        true
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
    let cursor = match since.as_deref().map(str::trim) {
        None | Some("") => service.change_head(),
        Some(text) => match text.parse::<u64>() {
            Ok(cursor) => cursor,
            Err(_) => return bad(format!("`{text}` is not a cursor")),
        },
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
    let labels = query.labels.map(|l| {
        l.split(',')
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .collect::<BTreeSet<_>>()
    });
    let follower = Follower {
        service,
        cursor,
        filter: Filter {
            labels: labels.filter(|l| !l.is_empty()),
            context,
            database,
        },
        pending: VecDeque::new(),
    };
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
