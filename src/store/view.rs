//! Read-only JSON views of the tree for the web API (`/api`): a slice of the tree for
//! lazy loading, one node in detail, and a subtree's whole history for replay. Nothing
//! here writes, renews a lease or logs.
//!
//! A slice is built from a handful of queries, not one per node: the skeleton of the
//! root's subtree (id, parent, status, defer date) in one recursive query, which gives
//! every count; the full rows of the nodes that go out, in one query; plus the root's
//! ancestors and the focus list.
//!
//! A history is the same shape of work for a whole subtree at once: one recursive query
//! for every node under the root (closed ones included), the full rows and the events of
//! the nodes that go out in one query each, capped at [`HISTORY_NODE_LIMIT`] nodes and
//! [`HISTORY_EVENT_LIMIT`] events.

use std::collections::{HashMap, VecDeque};

use chrono::NaiveDate;
use rusqlite::{Connection, OptionalExtension, params};
use serde::Serialize;

use super::focus::focus_numbers;
use super::{NODE_COLS, ancestors, db_err, load, node_from_row};
use crate::format::{is_unplanned, parse_date};
use crate::model::{DEFAULT_USER, Node, NodeId, Status};

/// Levels below the root a slice returns when not asked, and at most.
pub const DEFAULT_DEPTH: i64 = 3;
pub const MAX_DEPTH: i64 = 10;
/// Hard cap on nodes in one response.
pub const NODE_LIMIT: usize = 2000;
/// Caps on one history response: nodes (first in pre-order) and events (oldest first).
pub const HISTORY_NODE_LIMIT: usize = 5000;
pub const HISTORY_EVENT_LIMIT: usize = 20000;
/// Events in a node's detail, newest first (as `get history: true`).
const HISTORY: i64 = 10;

/// Why a view could not be built. Storage errors are logged where they happen.
#[derive(Debug)]
pub enum ViewError {
    NotFound(String),
    Internal,
}

impl From<rusqlite::Error> for ViewError {
    fn from(e: rusqlite::Error) -> Self {
        db_err(e);
        ViewError::Internal
    }
}

/// The shared store helpers report (already logged) storage errors as text.
impl From<String> for ViewError {
    fn from(_: String) -> Self {
        ViewError::Internal
    }
}

pub type ViewResult<T> = Result<T, ViewError>;

/// What a slice asks for.
#[derive(Debug, Clone, Copy)]
pub struct SliceQuery {
    pub root: NodeId,
    /// Levels below the root (0 = the root alone).
    pub depth: i64,
    /// Show done nodes and what lies under them (and dropped ones too, unless
    /// `include_dropped` says otherwise: the API makes the two agree when it is omitted).
    pub include_closed: bool,
    /// Show dropped nodes and what lies under them.
    pub include_dropped: bool,
}

/// One ancestor in a breadcrumb.
#[derive(Debug, Serialize)]
pub struct Crumb {
    pub id: i64,
    pub title: String,
}

/// A task's progress over its direct children that are not dropped.
#[derive(Debug, Serialize)]
pub struct Progress {
    pub done: i64,
    pub total: i64,
}

/// One node as the API shows it. Every field is always present (`null` when unset).
#[derive(Debug, Serialize)]
pub struct ViewNode {
    pub id: i64,
    pub parent: Option<i64>,
    pub kind: &'static str,
    pub ordered: bool,
    pub title: String,
    #[serde(rename = "type")]
    pub work_type: Option<&'static str>,
    pub stage: Option<&'static str>,
    pub status: &'static str,
    pub priority: Option<i64>,
    pub effective_priority: Option<i64>,
    pub focus: Option<usize>,
    pub due: Option<String>,
    pub defer_until: Option<String>,
    pub deferred: bool,
    pub claimed_by: Option<String>,
    pub claim_expires: Option<String>,
    pub stalled: bool,
    pub waiting_on: Option<String>,
    pub answered: bool,
    pub repeat: Option<String>,
    pub has_notes: bool,
    pub has_source: bool,
    pub unplanned: bool,
    pub is_step: bool,
    pub is_next: bool,
    pub progress: Option<Progress>,
    pub created_at: String,
    pub updated_at: String,
    pub touched_at: String,
    pub child_count: usize,
    pub descendant_count: usize,
    pub closed_child_count: usize,
    /// Direct children that are dropped (shown or not).
    pub dropped_child_count: usize,
    pub children_loaded: bool,
}

/// A slice of the tree: the root's breadcrumb and the nodes in pre-order.
#[derive(Debug, Serialize)]
pub struct Slice {
    pub path: Vec<Crumb>,
    pub truncated: bool,
    pub nodes: Vec<ViewNode>,
}

/// One history entry, raw (`at` is RFC 3339 UTC; `detail` is not shortened).
#[derive(Debug, Serialize)]
pub struct Event {
    pub id: i64,
    pub at: String,
    pub agent: Option<String>,
    #[serde(rename = "type")]
    pub kind: String,
    pub detail: Option<String>,
}

/// A node with its notes body.
#[derive(Debug, Serialize)]
pub struct DetailNode {
    #[serde(flatten)]
    pub node: ViewNode,
    pub notes: Option<String>,
    /// The user's raw words, as stored (`null` when none).
    pub source: Option<String>,
}

/// One node in detail: the node and notes, its breadcrumb and its last events.
#[derive(Debug, Serialize)]
pub struct Detail {
    pub path: Vec<Crumb>,
    pub node: DetailNode,
    pub events: Vec<Event>,
}

/// The revision (largest event id; every write logs an event) and how many claims have
/// lapsed by `now`. Together with "today" they decide whether a cached view is stale.
pub fn freshness(conn: &Connection, now: &str) -> rusqlite::Result<(i64, i64)> {
    let rev: i64 = conn.query_row("SELECT COALESCE(MAX(id), 0) FROM events", [], |r| r.get(0))?;
    let lapsed: i64 = conn.query_row(
        "SELECT COUNT(*) FROM nodes
         WHERE user_id = ?1 AND status = 'in_progress' AND COALESCE(claim_expires, '') <= ?2",
        params![DEFAULT_USER, now],
        |r| r.get(0),
    )?;
    Ok((rev, lapsed))
}

/// A skeleton row: enough to count, order and find next steps.
struct Sk {
    id: i64,
    status: Status,
    defer_until: Option<String>,
}

/// The actionable step among a parent's children, as `get` marks `<- next`.
fn first_open_step(parent: &Node, siblings: &[Sk], today: NaiveDate) -> Option<i64> {
    if !parent.ordered || parent.status.is_closed() {
        return None;
    }
    siblings
        .iter()
        .find(|k| !k.status.is_closed())
        .filter(|k| k.status == Status::Open && !deferred(k.defer_until.as_deref(), today))
        .map(|k| k.id)
}

/// A parent's direct children as skeleton rows, in sibling order.
fn sibling_skeletons(conn: &Connection, parent: NodeId) -> rusqlite::Result<Vec<Sk>> {
    let mut stmt = conn.prepare_cached(
        "SELECT id, status, defer_until FROM nodes WHERE parent_id = ?1 ORDER BY position",
    )?;
    stmt.query_map(params![parent.0], |r| {
        let status: String = r.get(1)?;
        Ok(Sk {
            id: r.get(0)?,
            status: status.parse().unwrap_or(Status::Open),
            defer_until: r.get(2)?,
        })
    })?
    .collect()
}

/// A slice of the tree below `q.root`, `q.depth` levels deep, capped at [`NODE_LIMIT`].
pub fn slice(conn: &Connection, q: SliceQuery, today: NaiveDate, now: &str) -> ViewResult<Slice> {
    let Some(root) = load(conn, q.root)? else {
        return Err(ViewError::NotFound(format!("{} does not exist", q.root)));
    };
    let shown = |s: Status| match s {
        Status::Done => q.include_closed,
        Status::Dropped => q.include_dropped,
        _ => true,
    };

    // Every node beneath the root, by parent in sibling order. The walk does not go
    // below a node that is not shown (a done one without include_closed, a dropped one
    // without include_dropped), which still appears (as a leaf) so its parent can count it.
    let mut kids: HashMap<i64, Vec<Sk>> = HashMap::new();
    {
        let mut stmt = conn.prepare_cached(
            "WITH RECURSIVE sub(id) AS (
               SELECT id FROM nodes WHERE parent_id = ?1 AND user_id = ?2
               UNION ALL
               SELECT c.id FROM sub
               JOIN nodes p ON p.id = sub.id
                 AND (?3 OR p.status != 'done') AND (?4 OR p.status != 'dropped')
               JOIN nodes c ON c.parent_id = sub.id
             )
             SELECT n.id, n.parent_id, n.status, n.defer_until
             FROM sub JOIN nodes n ON n.id = sub.id
             ORDER BY n.parent_id, n.position",
        )?;
        let mut rows = stmt.query(params![
            q.root.0,
            DEFAULT_USER,
            q.include_closed,
            q.include_dropped
        ])?;
        while let Some(r) = rows.next()? {
            let status: String = r.get(2)?;
            let status = status.parse().map_err(|_| {
                rusqlite::Error::FromSqlConversionFailure(
                    2,
                    rusqlite::types::Type::Text,
                    format!("bad status '{status}'").into(),
                )
            })?;
            kids.entry(r.get(1)?).or_default().push(Sk {
                id: r.get(0)?,
                status,
                defer_until: r.get(3)?,
            });
        }
    }
    let visible = |id: i64| -> Vec<&Sk> {
        kids.get(&id)
            .map(|v| v.iter().filter(|k| shown(k.status)).collect())
            .unwrap_or_default()
    };

    // Visible descendants of every visible node: list them root-first, then add up
    // from the bottom.
    let mut order = vec![root.id.0];
    let mut i = 0;
    while i < order.len() {
        let id = order[i];
        order.extend(visible(id).iter().map(|k| k.id));
        i += 1;
    }
    let mut descendants: HashMap<i64, usize> = HashMap::with_capacity(order.len());
    for &id in order.iter().rev() {
        let below: usize = visible(id)
            .iter()
            .map(|k| 1 + descendants.get(&k.id).copied().unwrap_or(0))
            .sum();
        descendants.insert(id, below);
    }

    // Expand breadth-first so shallow levels fill first. A node's children go out all
    // together or not at all (the root's may be cut short when there are more than fit);
    // at the first group that does not fit, expansion stops.
    let mut take: HashMap<i64, usize> = HashMap::new();
    let mut budget = NODE_LIMIT - 1;
    let mut truncated = false;
    let mut queue = VecDeque::from([(root.id.0, 0_i64)]);
    while let Some((id, level)) = queue.pop_front() {
        if level >= q.depth {
            continue;
        }
        let vis = visible(id);
        if vis.is_empty() {
            continue;
        }
        if vis.len() <= budget {
            budget -= vis.len();
            take.insert(id, vis.len());
            queue.extend(vis.iter().map(|k| (k.id, level + 1)));
        } else {
            truncated = true;
            if id == root.id.0 {
                take.insert(id, budget);
            }
            break;
        }
    }

    // Pre-order: each node, then its children in position order.
    let mut ids: Vec<i64> = Vec::new();
    let mut stack = vec![root.id.0];
    while let Some(id) = stack.pop() {
        ids.push(id);
        if let Some(&n) = take.get(&id) {
            stack.extend(visible(id).iter().take(n).rev().map(|k| k.id));
        }
    }

    // Full rows for what goes out, in one query.
    let mut rows: HashMap<i64, Node> = HashMap::with_capacity(ids.len());
    {
        let list = serde_json::to_string(&ids).unwrap_or_else(|_| "[]".to_string());
        let mut stmt = conn.prepare_cached(&format!(
            "SELECT {NODE_COLS} FROM nodes n WHERE n.id IN (SELECT value FROM json_each(?1))"
        ))?;
        let got = stmt.query_map(params![list], node_from_row)?;
        for n in got {
            let n = n?;
            rows.insert(n.id.0, n);
        }
    }

    let above = ancestors(conn, root.id)?;
    let path: Vec<Crumb> = above
        .iter()
        .map(|a| Crumb {
            id: a.id.0,
            title: a.title.clone(),
        })
        .collect();
    let focus = focus_numbers(conn)?;

    // The actionable step of each emitted parent, as `get` marks `<- next`.
    let mut next_of: HashMap<i64, Option<i64>> = HashMap::new();
    if let Some(parent) = above.last() {
        let siblings = sibling_skeletons(conn, parent.id)?;
        next_of.insert(parent.id.0, first_open_step(parent, &siblings, today));
    }
    for id in &ids {
        if let Some(n) = rows.get(id) {
            let siblings = kids.get(id).map_or(&[][..], Vec::as_slice);
            next_of.insert(*id, first_open_step(n, siblings, today));
        }
    }

    // Effective level: own, or the nearest ancestor's; pre-order sees parents first.
    let inherited = above.iter().rev().find_map(|a| a.priority);
    let mut level: HashMap<i64, Option<i64>> = HashMap::with_capacity(ids.len());
    let mut nodes = Vec::with_capacity(ids.len());
    for id in &ids {
        let Some(n) = rows.remove(id) else { continue };
        let from_above = match n.parent_id {
            Some(p) if *id != root.id.0 => level.get(&p.0).copied().flatten(),
            _ => inherited,
        };
        let effective = n.priority.or(from_above);
        level.insert(*id, effective);
        let all = kids.get(id).map_or(&[][..], Vec::as_slice);
        let child_count = all.iter().filter(|k| shown(k.status)).count();
        let is_next = n
            .parent_id
            .is_some_and(|p| next_of.get(&p.0).copied().flatten() == Some(*id));
        nodes.push(view_node(
            n,
            ViewCounts {
                effective,
                focus: focus.get(&NodeId(*id)).copied(),
                is_next,
                child_count,
                descendant_count: descendants.get(id).copied().unwrap_or(0),
                closed_child_count: all.iter().filter(|k| k.status.is_closed()).count(),
                dropped_child_count: all.iter().filter(|k| k.status == Status::Dropped).count(),
                children_loaded: take.get(id).copied().unwrap_or(0) == child_count,
            },
            today,
            now,
        ));
    }
    Ok(Slice {
        path,
        truncated,
        nodes,
    })
}

/// One node with its notes, breadcrumb and last events.
pub fn detail(
    conn: &Connection,
    id: NodeId,
    include_closed: bool,
    include_dropped: bool,
    today: NaiveDate,
    now: &str,
) -> ViewResult<Detail> {
    let q = SliceQuery {
        root: id,
        depth: 0,
        include_closed,
        include_dropped,
    };
    let Slice { path, nodes, .. } = slice(conn, q, today, now)?;
    let Some(node) = nodes.into_iter().next() else {
        return Err(ViewError::NotFound(format!("{id} does not exist")));
    };
    let notes: Option<String> = conn
        .query_row(
            "SELECT body FROM notes WHERE node_id = ?1",
            params![id.0],
            |r| r.get(0),
        )
        .optional()?;
    let source: Option<String> = conn.query_row(
        "SELECT source FROM nodes WHERE id = ?1",
        params![id.0],
        |r| r.get(0),
    )?;
    let mut stmt = conn.prepare_cached(
        "SELECT id, at, agent, type, detail FROM events WHERE node_id = ?1
         ORDER BY id DESC LIMIT ?2",
    )?;
    let events = stmt
        .query_map(params![id.0, HISTORY], |r| {
            Ok(Event {
                id: r.get(0)?,
                at: r.get(1)?,
                agent: r.get(2)?,
                kind: r.get(3)?,
                detail: r.get(4)?,
            })
        })?
        .collect::<rusqlite::Result<_>>()?;
    Ok(Detail {
        path,
        node: DetailNode {
            node,
            notes,
            source,
        },
        events,
    })
}

/// One event of a replayed subtree: the raw [`Event`] fields plus the node it belongs to
/// and what could be read out of `detail` (all `null` when it does not parse).
#[derive(Debug, Serialize, PartialEq)]
pub struct HistoryEvent {
    pub id: i64,
    pub node: i64,
    pub at: String,
    pub agent: Option<String>,
    #[serde(rename = "type")]
    pub kind: String,
    pub detail: Option<String>,
    pub from_parent: Option<i64>,
    pub to_parent: Option<i64>,
    pub from_status: Option<&'static str>,
    pub to_status: Option<&'static str>,
}

/// A subtree with everything needed to replay its history.
#[derive(Debug, Serialize)]
pub struct History {
    pub path: Vec<Crumb>,
    pub node_limit: usize,
    pub event_limit: usize,
    pub truncated: bool,
    pub nodes: Vec<ViewNode>,
    pub events: Vec<HistoryEvent>,
}

/// What `detail` says about a move or a status change.
#[derive(Debug, Default, PartialEq)]
pub struct Parsed {
    pub from_parent: Option<i64>,
    pub to_parent: Option<i64>,
    pub from_status: Option<&'static str>,
    pub to_status: Option<&'static str>,
}

fn status_str(s: &str) -> Option<&'static str> {
    s.trim().parse::<Status>().ok().map(Status::as_str)
}

/// Reads the structured part of an event's `detail` by type, with plain string handling:
/// `moved`: `SPOKE-a -> SPOKE-b`; `status`: `x -> y`; `recurred`: `x -> done -> open; ...`.
/// Anything else, or anything that does not parse, gives all `None`.
pub fn parse_detail(kind: &str, detail: &str) -> Parsed {
    let parts: Vec<&str> = detail.split(" -> ").collect();
    match (kind, parts.as_slice()) {
        ("moved", [a, b]) => {
            let id = |s: &str| s.trim().parse::<NodeId>().ok().map(|n| n.0);
            match (id(a), id(b)) {
                (Some(a), Some(b)) => Parsed {
                    from_parent: Some(a),
                    to_parent: Some(b),
                    ..Parsed::default()
                },
                _ => Parsed::default(),
            }
        }
        ("status", [a, b]) => match (status_str(a), status_str(b)) {
            (Some(a), Some(b)) => Parsed {
                from_status: Some(a),
                to_status: Some(b),
                ..Parsed::default()
            },
            _ => Parsed::default(),
        },
        ("recurred", [a, done, rest]) => {
            let reopened = rest.split(';').next().is_some_and(|s| s.trim() == "open");
            match status_str(a) {
                Some(a) if done.trim() == "done" && reopened => Parsed {
                    from_status: Some(a),
                    to_status: Some("open"),
                    ..Parsed::default()
                },
                _ => Parsed::default(),
            }
        }
        _ => Parsed::default(),
    }
}

/// Every node under `root` (root included, closed nodes and what lies under them too) in
/// pre-order, and every event of those nodes, oldest first. Capped at
/// [`HISTORY_NODE_LIMIT`] nodes and [`HISTORY_EVENT_LIMIT`] events; `truncated` says a cap
/// cut something. Counts and `children_loaded` are as in a slice with `include_closed`.
pub fn history(
    conn: &Connection,
    root: NodeId,
    today: NaiveDate,
    now: &str,
) -> ViewResult<History> {
    let Some(root) = load(conn, root)? else {
        return Err(ViewError::NotFound(format!("{root} does not exist")));
    };

    // The whole subtree's skeleton, by parent in sibling order.
    let mut kids: HashMap<i64, Vec<Sk>> = HashMap::new();
    {
        let mut stmt = conn.prepare_cached(
            "WITH RECURSIVE sub(id) AS (
               SELECT id FROM nodes WHERE parent_id = ?1 AND user_id = ?2
               UNION ALL
               SELECT c.id FROM sub JOIN nodes c ON c.parent_id = sub.id
             )
             SELECT n.id, n.parent_id, n.status, n.defer_until
             FROM sub JOIN nodes n ON n.id = sub.id
             ORDER BY n.parent_id, n.position",
        )?;
        let mut rows = stmt.query(params![root.id.0, DEFAULT_USER])?;
        while let Some(r) = rows.next()? {
            let status: String = r.get(2)?;
            let status = status.parse().map_err(|_| {
                rusqlite::Error::FromSqlConversionFailure(
                    2,
                    rusqlite::types::Type::Text,
                    format!("bad status '{status}'").into(),
                )
            })?;
            kids.entry(r.get(1)?).or_default().push(Sk {
                id: r.get(0)?,
                status,
                defer_until: r.get(3)?,
            });
        }
    }
    let children = |id: i64| kids.get(&id).map_or(&[][..], Vec::as_slice);

    // Pre-order over everything (parents before children), for counts and the cut.
    let mut all: Vec<i64> = Vec::new();
    let mut stack = vec![root.id.0];
    while let Some(id) = stack.pop() {
        all.push(id);
        stack.extend(children(id).iter().rev().map(|k| k.id));
    }
    let mut descendants: HashMap<i64, usize> = HashMap::with_capacity(all.len());
    for &id in all.iter().rev() {
        let below: usize = children(id)
            .iter()
            .map(|k| 1 + descendants.get(&k.id).copied().unwrap_or(0))
            .sum();
        descendants.insert(id, below);
    }
    let mut truncated = all.len() > HISTORY_NODE_LIMIT;
    let ids: Vec<i64> = all.into_iter().take(HISTORY_NODE_LIMIT).collect();
    let included: std::collections::HashSet<i64> = ids.iter().copied().collect();

    let list = serde_json::to_string(&ids).unwrap_or_else(|_| "[]".to_string());
    let mut rows: HashMap<i64, Node> = HashMap::with_capacity(ids.len());
    {
        let mut stmt = conn.prepare_cached(&format!(
            "SELECT {NODE_COLS} FROM nodes n WHERE n.id IN (SELECT value FROM json_each(?1))"
        ))?;
        for n in stmt.query_map(params![list], node_from_row)? {
            let n = n?;
            rows.insert(n.id.0, n);
        }
    }

    let above = ancestors(conn, root.id)?;
    let path: Vec<Crumb> = above
        .iter()
        .map(|a| Crumb {
            id: a.id.0,
            title: a.title.clone(),
        })
        .collect();
    let focus = focus_numbers(conn)?;

    let mut next_of: HashMap<i64, Option<i64>> = HashMap::new();
    if let Some(parent) = above.last() {
        let siblings = sibling_skeletons(conn, parent.id)?;
        next_of.insert(parent.id.0, first_open_step(parent, &siblings, today));
    }
    for id in &ids {
        if let Some(n) = rows.get(id) {
            next_of.insert(*id, first_open_step(n, children(*id), today));
        }
    }

    let inherited = above.iter().rev().find_map(|a| a.priority);
    let mut level: HashMap<i64, Option<i64>> = HashMap::with_capacity(ids.len());
    let mut nodes = Vec::with_capacity(ids.len());
    for id in &ids {
        let Some(n) = rows.remove(id) else { continue };
        let from_above = match n.parent_id {
            Some(p) if *id != root.id.0 => level.get(&p.0).copied().flatten(),
            _ => inherited,
        };
        let effective = n.priority.or(from_above);
        level.insert(*id, effective);
        let all = children(*id);
        let is_next = n
            .parent_id
            .is_some_and(|p| next_of.get(&p.0).copied().flatten() == Some(*id));
        nodes.push(view_node(
            n,
            ViewCounts {
                effective,
                focus: focus.get(&NodeId(*id)).copied(),
                is_next,
                child_count: all.len(),
                descendant_count: descendants.get(id).copied().unwrap_or(0),
                closed_child_count: all.iter().filter(|k| k.status.is_closed()).count(),
                dropped_child_count: all.iter().filter(|k| k.status == Status::Dropped).count(),
                children_loaded: all.iter().all(|k| included.contains(&k.id)),
            },
            today,
            now,
        ));
    }

    // Events of those nodes, oldest first; one more than the cap tells whether it cut.
    let mut events: Vec<HistoryEvent> = {
        let mut stmt = conn.prepare_cached(
            "SELECT id, node_id, at, agent, type, detail FROM events
             WHERE node_id IN (SELECT value FROM json_each(?1))
             ORDER BY id LIMIT ?2",
        )?;
        stmt.query_map(params![list, HISTORY_EVENT_LIMIT as i64 + 1], |r| {
            let kind: String = r.get(4)?;
            let detail: Option<String> = r.get(5)?;
            let p = detail
                .as_deref()
                .map(|d| parse_detail(&kind, d))
                .unwrap_or_default();
            Ok(HistoryEvent {
                id: r.get(0)?,
                node: r.get(1)?,
                at: r.get(2)?,
                agent: r.get(3)?,
                kind,
                detail,
                from_parent: p.from_parent,
                to_parent: p.to_parent,
                from_status: p.from_status,
                to_status: p.to_status,
            })
        })?
        .collect::<rusqlite::Result<_>>()?
    };
    if events.len() > HISTORY_EVENT_LIMIT {
        events.truncate(HISTORY_EVENT_LIMIT);
        truncated = true;
    }

    Ok(History {
        path,
        node_limit: HISTORY_NODE_LIMIT,
        event_limit: HISTORY_EVENT_LIMIT,
        truncated,
        nodes,
        events,
    })
}

fn deferred(defer_until: Option<&str>, today: NaiveDate) -> bool {
    defer_until.and_then(parse_date).is_some_and(|d| d > today)
}

/// What a node's JSON needs beyond its own row.
struct ViewCounts {
    effective: Option<i64>,
    focus: Option<usize>,
    is_next: bool,
    child_count: usize,
    descendant_count: usize,
    closed_child_count: usize,
    dropped_child_count: usize,
    children_loaded: bool,
}

fn view_node(n: Node, c: ViewCounts, today: NaiveDate, now: &str) -> ViewNode {
    let stalled = n.status == Status::InProgress && n.claim_expires.as_deref().unwrap_or("") <= now;
    let unplanned = is_unplanned(&n);
    let progress = (n.kind == crate::model::Kind::Task && n.child_total > 0).then_some(Progress {
        done: n.child_done,
        total: n.child_total,
    });
    ViewNode {
        id: n.id.0,
        parent: n.parent_id.map(|p| p.0),
        kind: n.kind.as_str(),
        ordered: n.ordered,
        work_type: n.work_type.map(|t| t.as_str()),
        stage: n.stage.map(|s| s.as_str()),
        status: n.status.as_str(),
        priority: n.priority,
        effective_priority: c.effective,
        focus: c.focus,
        deferred: deferred(n.defer_until.as_deref(), today),
        due: n.due_at,
        defer_until: n.defer_until,
        claimed_by: n.claimed_by,
        claim_expires: n.claim_expires,
        stalled,
        waiting_on: n.waiting_on,
        answered: n.answered_at.is_some(),
        repeat: n
            .recur
            .as_deref()
            .and_then(crate::recur::Rule::from_json)
            .map(|r| r.describe()),
        has_notes: n.has_notes,
        has_source: n.has_source,
        unplanned,
        is_step: n.is_step,
        is_next: c.is_next,
        progress,
        created_at: n.created_at,
        updated_at: n.updated_at,
        touched_at: n.touched_at,
        title: n.title,
        child_count: c.child_count,
        descendant_count: c.descendant_count,
        closed_child_count: c.closed_child_count,
        dropped_child_count: c.dropped_child_count,
        children_loaded: c.children_loaded,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_move_status_and_recurred_details() {
        let p = parse_detail("moved", "SPOKE-3 -> SPOKE-12");
        assert_eq!((p.from_parent, p.to_parent), (Some(3), Some(12)));
        assert_eq!(
            parse_detail("moved", "reordered under SPOKE-4"),
            Parsed::default()
        );
        assert_eq!(
            parse_detail("moved", "SPOKE-x -> SPOKE-2"),
            Parsed::default()
        );

        let p = parse_detail("status", "open -> done");
        assert_eq!((p.from_status, p.to_status), (Some("open"), Some("done")));
        assert_eq!(parse_detail("status", "open -> bogus"), Parsed::default());
        assert_eq!(parse_detail("status", "open"), Parsed::default());

        let p = parse_detail(
            "recurred",
            "in_progress -> done -> open; next due 2026-12-01, hidden until 2026-11-25, 2 steps reopened",
        );
        assert_eq!(
            (p.from_status, p.to_status),
            (Some("in_progress"), Some("open"))
        );
        let p = parse_detail("recurred", "open -> done -> open");
        assert_eq!((p.from_status, p.to_status), (Some("open"), Some("open")));
        assert_eq!(parse_detail("recurred", "open -> done"), Parsed::default());
        assert_eq!(
            parse_detail("recurred", "open -> done -> waiting"),
            Parsed::default()
        );

        assert_eq!(parse_detail("created", "A -> B"), Parsed::default());
        assert_eq!(parse_detail("moved", ""), Parsed::default());
    }

    #[test]
    fn history_caps_events_oldest_first() {
        let (mut conn, dir) = crate::store::test_support::temp_db("history-events");
        let tx = conn.transaction().unwrap();
        {
            let mut ins = tx
                .prepare(
                    "INSERT INTO events (node_id, at, type, detail)
                     VALUES (1, '2026-01-01T00:00:00Z', 'updated', ?1)",
                )
                .unwrap();
            for i in 0..HISTORY_EVENT_LIMIT + 5 {
                ins.execute([i.to_string()]).unwrap();
            }
        }
        tx.commit().unwrap();
        let today = NaiveDate::from_ymd_opt(2026, 11, 23).unwrap();
        let h = history(&conn, NodeId(1), today, "2026-11-23T00:00:00Z").unwrap();
        assert!(h.truncated);
        assert_eq!(h.nodes.len(), 1);
        assert_eq!(h.events.len(), HISTORY_EVENT_LIMIT);
        assert!(h.events.windows(2).all(|w| w[0].id < w[1].id));
        let oldest: i64 = conn
            .query_row("SELECT MIN(id) FROM events", [], |r| r.get(0))
            .unwrap();
        assert_eq!(h.events[0].id, oldest);
        drop(conn);
        let _ = std::fs::remove_dir_all(dir);
    }
}
