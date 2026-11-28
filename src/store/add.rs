//! `add`: create one node, or a whole nested plan, under a parent in one transaction.

use rusqlite::{Connection, OptionalExtension, Transaction, params};

use super::{
    ToolResult, children, clean_agent, db_err, dupes, log_event, refuse_stage, renew_leases,
    require, touch,
};
use crate::db::{now, position};
use crate::format::parse_date;
use crate::hints;
use crate::model::{DEFAULT_USER, Kind, NodeId, NumArg, ROOT_ID, Refinement, Status, WorkType};
use crate::recur::{RecurArg, RecurChange};
use crate::tools::args::{AddArgs, NodeSpec};

const MAX_TITLE: usize = 300;
const MAX_ALIASES: usize = 10;
const MAX_ALIAS: usize = 100;
pub(crate) const MAX_NOTES: usize = 100_000;
/// Nodes one `add` call may create, and how deep `children` may nest.
const MAX_NODES: usize = 200;
const MAX_NEST: usize = 8;

/// A validated node ready to insert.
struct Clean {
    title: String,
    kind: Kind,
    work_type: Option<WorkType>,
    refinement: Option<Refinement>,
    tracked: bool,
    ordered: bool,
    priority: Option<i64>,
    due: Option<String>,
    defer: Option<String>,
    aliases: Option<String>,
    /// The repeat rule as stored JSON.
    recur: Option<String>,
    notes: Option<String>,
    source: Option<String>,
    children: Vec<Clean>,
}

pub fn add(conn: &mut Connection, args: AddArgs) -> ToolResult {
    let parent_arg = args.parent.as_ref().map(|p| p.parse()).transpose()?;
    let before = args.before.as_ref().map(|p| p.parse()).transpose()?;
    let after = args.after.as_ref().map(|p| p.parse()).transpose()?;
    if before.is_some() && after.is_some() {
        return Err(
            "pass before or after, not both, so nothing was added. Name one sibling to sit next to."
                .to_string(),
        );
    }
    let agent = clean_agent(args.agent.as_deref())?;
    let mut count = 0;
    // Without a title, the children go straight under the existing parent (SPOKE-228).
    let batch = is_batch(&args.node)?;
    let tops: Vec<Clean> = if batch {
        let kids = args.node.children.as_deref().unwrap_or_default();
        let mut tops = Vec::with_capacity(kids.len());
        for (i, kid) in kids.iter().enumerate() {
            tops.push(clean_spec(kid, &format!("children[{i}]"), 1, &mut count)?);
        }
        let titles: Vec<&str> = tops.iter().map(|c| c.title.as_str()).collect();
        if let Some((i, j)) = dupes::batch_clash(&titles) {
            return Err(format!(
                "children[{i}] (\"{}\") and children[{j}] (\"{}\") have the same title, so nothing was added. Give each its own title, or merge them into one.",
                titles[i], titles[j]
            ));
        }
        tops
    } else {
        vec![clean_spec(&args.node, "", 0, &mut count)?]
    };
    // How a top node is named in an error: by its path in a batch, plainly otherwise.
    let label = |i: usize, t: &Clean| {
        if batch {
            format!("children[{i}] (\"{}\"): ", t.title)
        } else {
            String::new()
        }
    };

    let tx = conn.transaction().map_err(db_err)?;
    // A sibling to sit next to (SPOKE-459): its parent is the parent when none is given.
    let anchor = match before.or(after) {
        Some(a) => Some(require(&tx, a)?),
        None => None,
    };
    let parent_id = match (parent_arg, &anchor) {
        (Some(p), _) => p,
        (None, Some(a)) => a.parent_id.ok_or_else(|| {
            format!(
                "{} is the root and has no siblings, so nothing was added. Name a sibling, or pass parent.",
                a.id
            )
        })?,
        (None, None) => ROOT_ID,
    };
    if let Some(a) = &anchor
        && a.parent_id != Some(parent_id)
    {
        let side = if before.is_some() { "before" } else { "after" };
        return Err(format!(
            "{} {} is not under {parent_id}, so the new node cannot go {side} it there and nothing was added. Name a child of {parent_id}, or omit parent.",
            a.id, a.title
        ));
    }
    let parent = require(&tx, parent_id)?;
    if parent.status.is_closed() {
        return Err(format!(
            "{} {} is {}. Reopen it with set_status, or pick another parent.",
            parent.id,
            parent.title,
            parent.status.as_str()
        ));
    }
    for (i, spec) in tops.iter().enumerate() {
        if spec.kind == Kind::Area && parent.kind == Kind::Task {
            return Err(format!(
                "{}{} {} is a task; areas can only go under areas. Add a task instead, or pick an area as parent.",
                label(i, spec),
                parent.id,
                parent.title
            ));
        }
        if spec.recur.is_some() && parent.ordered {
            return Err(format!(
                "{}{}",
                label(i, spec),
                recur_step_error(&parent.id.to_string(), &parent.title)
            ));
        }
        if let Some(existing) = dupes::sibling_clash(&tx, parent_id, &spec.title, None)? {
            return Err(format!(
                "{}{}",
                label(i, spec),
                dupes::refusal(&existing, &parent, &spec.title)
            ));
        }
    }

    let siblings = children(&tx, parent_id)?;
    let step_before = siblings
        .iter()
        .find(|c| !c.status.is_closed())
        .map(|c| c.id);
    // The gap the new nodes fill, as (position before, position after).
    let (mut prev, next): (Option<String>, Option<String>) = match &anchor {
        Some(a) => {
            let i = siblings.iter().position(|s| s.id == a.id).unwrap_or(0);
            if before.is_some() {
                let prev = i.checked_sub(1).map(|j| siblings[j].position.clone());
                (prev, Some(a.position.clone()))
            } else {
                let next = siblings.get(i + 1).map(|s| s.position.clone());
                (Some(a.position.clone()), next)
            }
        }
        None => (siblings.last().map(|s| s.position.clone()), None),
    };

    let now = now();
    let mut ids = Vec::new();
    let mut top_ids = Vec::with_capacity(tops.len());
    for spec in &tops {
        let pos = position::between(prev.as_deref(), next.as_deref());
        top_ids.push(NodeId(0));
        let first = ids.len();
        insert(
            &tx,
            parent_id,
            spec,
            Some(pos.clone()),
            &now,
            agent.as_deref(),
            &mut ids,
        )?;
        *top_ids.last_mut().expect("pushed above") = ids[first];
        prev = Some(pos);
    }
    let top = top_ids[0];
    // Where it landed, counted like the outline and move (dropped siblings are hidden).
    let placed = match &anchor {
        Some(a) => {
            let live: Vec<_> = children(&tx, parent_id)?
                .into_iter()
                .filter(|s| s.status != Status::Dropped || top_ids.contains(&s.id))
                .collect();
            let place = live.iter().position(|s| s.id == top).map_or(0, |i| i + 1);
            let side = if before.is_some() { "before" } else { "after" };
            let places = if top_ids.len() > 1 {
                format!("places {place} to {}", place + top_ids.len() - 1)
            } else {
                place.to_string()
            };
            Some(format!(
                ", {side} {} {}, {places} of {}",
                a.id,
                a.title,
                live.len()
            ))
        }
        None => None,
    };
    // An ordered parent whose next step a new node took over.
    let first_open = children(&tx, parent_id)?
        .into_iter()
        .find(|c| !c.status.is_closed())
        .map(|c| c.id);
    let now_next = parent.ordered
        && (step_before.is_some() || batch)
        && first_open.is_some_and(|c| top_ids.contains(&c));
    touch(&tx, ids[ids.len() - 1], &now).map_err(db_err)?;
    renew_leases(&tx, parent_id, agent.as_deref(), &now).map_err(db_err)?;
    let mut likely = Vec::new();
    for (spec, &id) in tops.iter().zip(&top_ids) {
        if let Some(line) = dupes::likely_line(
            &tx,
            id,
            parent_id,
            &spec.title,
            spec.aliases.as_deref(),
            &ids,
        )? {
            likely.push(line);
        }
    }
    let hint = add_hint(
        &tx,
        &tops,
        &ids,
        first_open.filter(|_| now_next),
        agent.is_none(),
        now_next.then_some(&parent),
        batch,
    )?;
    tx.commit().map_err(db_err)?;

    let mut line = if batch {
        let mut line = format!(
            "Added {} under {} {}{}, in the order given:",
            ids.len(),
            parent.id,
            parent.title,
            placed.unwrap_or_default()
        );
        let mut it = ids.iter().copied();
        for spec in &tops {
            outline(spec, &mut it, 1, &mut line);
        }
        line
    } else {
        let spec = &tops[0];
        let mut line = format!(
            "Added {top} {}  {}  (under {} {}{})",
            spec.kind.as_str(),
            spec.title,
            parent.id,
            parent.title,
            placed.unwrap_or_default()
        );
        // The created tree, one line per node with its id, indented by depth, so the
        // caller can tell which id is which without a follow-up get (SPOKE-324).
        if ids.len() > 1 {
            line.push_str(&format!(
                ", with {} beneath it in the order given:",
                ids.len() - 1
            ));
            let mut it = ids[1..].iter().copied();
            for child in &spec.children {
                outline(child, &mut it, 1, &mut line);
            }
        }
        line
    };
    for l in likely {
        line.push('\n');
        line.push_str(&l);
    }
    if let Some(hint) = hint {
        line.push('\n');
        line.push_str(&hint);
    }
    Ok(line)
}

/// Whether the call adds children straight under an existing parent: no title, but
/// children. The fields that describe a new node are refused then, by name.
fn is_batch(node: &NodeSpec) -> Result<bool, String> {
    if !node.title.trim().is_empty() {
        return Ok(false);
    }
    if node.children.as_deref().is_none_or(<[_]>::is_empty) {
        return Err(
            "title is empty. Give the node a short title, or leave title out and pass children to add them under an existing parent."
                .to_string(),
        );
    }
    let given: Vec<&str> = [
        ("kind", node.kind.is_some()),
        ("ordered", node.ordered.is_some()),
        ("priority", node.priority.is_some()),
        ("due", node.due.is_some()),
        ("defer_until", node.defer_until.is_some()),
        ("aliases", node.aliases.is_some()),
        ("recur", node.recur.is_some()),
        ("type", node.work_type.is_some()),
        ("refinement", node.refinement.is_some()),
        ("tracked", node.tracked.is_some()),
        ("notes", node.notes.is_some()),
        ("source", node.source.is_some()),
    ]
    .into_iter()
    .filter_map(|(name, set)| set.then_some(name))
    .collect();
    if !given.is_empty() {
        return Err(format!(
            "{} describe a new node, but there is no title, so nothing was added. Give a title to create that node with the children under it, or leave {} out to add the children straight under parent (change the parent itself with update).",
            given.join(", "),
            if given.len() == 1 { "it" } else { "them" }
        ));
    }
    refuse_stage(node.stage.as_ref())?;
    refuse_unknown(&node.unknown, true)?;
    Ok(true)
}

/// One line per created node, indented by depth, beside its new id (insert order).
fn outline(spec: &Clean, ids: &mut impl Iterator<Item = NodeId>, depth: usize, out: &mut String) {
    let Some(id) = ids.next() else { return };
    out.push('\n');
    out.push_str(&"  ".repeat(depth));
    out.push_str(&format!("{id} {}  {}", spec.kind.as_str(), spec.title));
    for child in &spec.children {
        outline(child, ids, depth + 1, out);
    }
}

/// The hint for what `add` just created (SPOKE-1079). `ids` are in the order `insert`
/// made them: each top node, then its children depth-first. In a batch (SPOKE-228)
/// the top nodes are the children given for an existing parent, and `took_next` is the
/// one that became that ordered parent's next step.
fn add_hint(
    conn: &Connection,
    tops: &[Clean],
    ids: &[NodeId],
    took_next: Option<NodeId>,
    no_agent: bool,
    next_step_of: Option<&crate::model::Node>,
    batch: bool,
) -> Result<Option<String>, String> {
    let mut tasks_without_aliases = Vec::new();
    let mut it = ids.iter().copied();
    for spec in tops {
        without_aliases(spec, &mut it, &mut tasks_without_aliases);
    }
    let top = require(conn, took_next.unwrap_or(ids[0]))?;
    let first_step = match ids.get(1) {
        Some(&id) if !batch && tops[0].ordered => Some(require(conn, id)?),
        _ => None,
    };
    let action = hints::Action::Added {
        top: &top,
        unaliased: &tasks_without_aliases,
        first_step: first_step.as_ref(),
        no_agent,
        next_step_of,
    };
    Ok(hints::hint(&action))
}

/// Walks `spec` in insert order beside its new ids.
fn without_aliases(spec: &Clean, ids: &mut impl Iterator<Item = NodeId>, out: &mut Vec<NodeId>) {
    let Some(id) = ids.next() else { return };
    if spec.kind == Kind::Task && spec.aliases.is_none() {
        out.push(id);
    }
    for child in &spec.children {
        without_aliases(child, ids, out);
    }
}

/// Inserts `spec` under `parent` at `pos` (`None`: last), then its children depth-first.
fn insert(
    tx: &Transaction<'_>,
    parent: NodeId,
    spec: &Clean,
    pos: Option<String>,
    now: &str,
    agent: Option<&str>,
    ids: &mut Vec<NodeId>,
) -> Result<(), String> {
    let pos = match pos {
        Some(p) => p,
        None => {
            let last: Option<String> = tx
                .query_row(
                    "SELECT MAX(position) FROM nodes WHERE parent_id = ?1",
                    params![parent.0],
                    |r| r.get(0),
                )
                .optional()
                .map_err(db_err)?
                .flatten();
            match last {
                Some(l) => position::after(&l),
                None => position::first(),
            }
        }
    };
    tx.execute(
        "INSERT INTO nodes (user_id, parent_id, position, kind, ordered, title, priority,
                            due_at, defer_until, aliases, recur, source, work_type, refinement,
                            tracked_at, created_at, updated_at, touched_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?12, ?13, ?14, ?15, ?16, ?11, ?11, ?11)",
        params![
            DEFAULT_USER,
            parent.0,
            pos,
            spec.kind.as_str(),
            spec.ordered,
            spec.title,
            spec.priority,
            spec.due,
            spec.defer,
            spec.aliases,
            now,
            spec.recur,
            spec.source,
            spec.work_type.map(WorkType::as_str),
            spec.refinement.map(Refinement::as_str),
            spec.tracked.then_some(now)
        ],
    )
    .map_err(db_err)?;
    let id = NodeId(tx.last_insert_rowid());
    ids.push(id);
    if let Some(body) = &spec.notes {
        tx.execute(
            "INSERT INTO notes (node_id, body) VALUES (?1, ?2)",
            params![id.0, body],
        )
        .map_err(db_err)?;
    }
    super::fts::reindex(tx, id).map_err(db_err)?;
    log_event(tx, id, now, agent, "created", &spec.title).map_err(db_err)?;
    for child in &spec.children {
        insert(tx, id, child, None, now, agent, ids)?;
    }
    Ok(())
}

/// Validates a spec and its children; errors name the child that failed.
fn clean_spec(
    spec: &NodeSpec,
    path: &str,
    nest: usize,
    count: &mut usize,
) -> Result<Clean, String> {
    *count += 1;
    if *count > MAX_NODES {
        return Err(format!(
            "one add can create at most {MAX_NODES} nodes. Split the plan into several calls."
        ));
    }
    let at = |e: String| {
        if path.is_empty() {
            e
        } else {
            format!("{path} (\"{}\"): {e}", spec.title.trim())
        }
    };
    let title = clean_title(&spec.title).map_err(at)?;
    let kind = spec.kind.unwrap_or(Kind::Task);
    let priority = clean_priority(spec.priority.as_ref(), false).map_err(at)?;
    let due = spec
        .due
        .as_deref()
        .map(|d| clean_date("due", d))
        .transpose()
        .map_err(at)?;
    let defer = spec
        .defer_until
        .as_deref()
        .map(|d| clean_date("defer_until", d))
        .transpose()
        .map_err(at)?;
    let aliases = clean_aliases(spec.aliases.as_deref().unwrap_or_default()).map_err(at)?;
    let notes = clean_notes(spec.notes.as_deref()).map_err(at)?;
    let source = clean_source(spec.source.as_deref()).map_err(at)?;
    let recur = clean_recur(spec.recur.as_ref(), kind).map_err(at)?;
    refuse_stage(spec.stage.as_ref()).map_err(at)?;
    refuse_unknown(&spec.unknown, nest == 0).map_err(at)?;
    let work_type =
        clean_added("type", spec.work_type.as_deref(), WorkType::parse_loose).map_err(at)?;
    let refinement = clean_added(
        "refinement",
        spec.refinement.as_deref(),
        Refinement::parse_loose,
    )
    .map_err(at)?;
    let tracked = spec.tracked.unwrap_or(false);
    let ordered = spec.ordered.unwrap_or(false);

    let kids = spec.children.as_deref().unwrap_or_default();
    if !kids.is_empty() && nest >= MAX_NEST {
        return Err(at(format!(
            "children nest deeper than {MAX_NEST} levels. Add the deeper part in a second call."
        )));
    }
    let mut children = Vec::with_capacity(kids.len());
    for (i, kid) in kids.iter().enumerate() {
        let kid_path = if path.is_empty() {
            format!("children[{i}]")
        } else {
            format!("{path}.children[{i}]")
        };
        let clean = clean_spec(kid, &kid_path, nest + 1, count)?;
        if clean.kind == Kind::Area && kind == Kind::Task {
            return Err(format!(
                "{kid_path} (\"{}\") is an area under the task \"{title}\"; areas can only go under areas. Make it a task.",
                clean.title
            ));
        }
        if clean.recur.is_some() && ordered {
            return Err(format!(
                "{kid_path} (\"{}\"): {}",
                clean.title,
                recur_step_error("its parent", &title)
            ));
        }
        children.push(clean);
    }
    let titles: Vec<&str> = children.iter().map(|c| c.title.as_str()).collect();
    if let Some((i, j)) = dupes::batch_clash(&titles) {
        let child = |k: usize| {
            if path.is_empty() {
                format!("children[{k}]")
            } else {
                format!("{path}.children[{k}]")
            }
        };
        return Err(format!(
            "{} (\"{}\") and {} (\"{}\") have the same title under \"{title}\", so nothing was added. Give each step its own title, or merge them into one.",
            child(i),
            titles[i],
            child(j),
            titles[j]
        ));
    }
    Ok(Clean {
        title,
        kind,
        work_type,
        refinement,
        tracked,
        ordered,
        priority,
        due,
        defer,
        aliases,
        recur,
        notes,
        source,
        children,
    })
}

/// The fields one node of `add` takes, for the refusal below.
const NODE_FIELDS: &str = "title, kind, ordered, priority, due, defer_until, aliases, recur, type, refinement, tracked, notes, source, children";

/// Refuses fields `add` does not take, so a caller never believes it set one (SPOKE-222).
/// `top` is the node named by the call itself, where `parent` and `agent` are also taken.
fn refuse_unknown(
    unknown: &std::collections::BTreeMap<String, serde_json::Value>,
    top: bool,
) -> Result<(), String> {
    if unknown.is_empty() {
        return Ok(());
    }
    let list = unknown
        .keys()
        .map(String::as_str)
        .collect::<Vec<_>>()
        .join(", ");
    let fix = unknown
        .keys()
        .find_map(|name| unknown_fix(name, top))
        .unwrap_or_else(|| {
            let extra = if top { ", plus parent and agent" } else { "" };
            format!("Each node takes {NODE_FIELDS}{extra}.")
        });
    Err(format!(
        "add does not take {list}, so nothing was added. {fix}"
    ))
}

/// What to do instead, for the fields callers are known to reach for.
fn unknown_fix(name: &str, top: bool) -> Option<String> {
    let fix = match name {
        "status" | "state" | "done" => {
            "To record work that is already finished, add it, then set_status done with the new ids."
        }
        "after" | "before" => {
            "A child is placed by its order in children; before or after (a sibling id) is taken only at the top of the call."
        }
        "position" | "rank" => {
            "Place the new node with before or after (a sibling id) at the top of the call; without either it goes last."
        }
        "parent" | "parent_id" if !top => {
            "A child's parent is the node it is nested in; pass parent only at the top of the call."
        }
        "agent" if !top => "Pass agent once, at the top of the call.",
        "id" => "add gives out the ids; it cannot take one.",
        "claimed_by" | "claim" => "Claim the task with claim once it exists.",
        "focus" => "Put it on the focus list with focus once it exists.",
        "notes_append" | "source_append" => "Those are update fields; on add give notes or source.",
        _ => return None,
    };
    Some(fix.to_string())
}

/// A type or refinement given on add: parsed loosely; an empty string is an error (omit the field).
fn clean_added<T>(
    field: &str,
    raw: Option<&str>,
    parse: fn(&str) -> Result<T, String>,
) -> Result<Option<T>, String> {
    match raw {
        None => Ok(None),
        Some(r) if r.trim().is_empty() => {
            Err(format!("{field} is empty. Omit {field} to leave it unset."))
        }
        Some(r) => parse(r).map(Some),
    }
}

/// A type or refinement given on update: parsed loosely; an empty string clears it.
pub(crate) fn clean_updated<T>(
    raw: Option<&str>,
    parse: fn(&str) -> Result<T, String>,
) -> Result<Option<Option<T>>, String> {
    match raw {
        None => Ok(None),
        Some(r) if r.trim().is_empty() => Ok(Some(None)),
        Some(r) => parse(r).map(|v| Some(Some(v))),
    }
}

/// 1 to 4; with `allow_clear`, 0 means "unset, inherit again".
pub(crate) fn clean_priority(
    raw: Option<&NumArg>,
    allow_clear: bool,
) -> Result<Option<i64>, String> {
    match raw.map(|p| p.parse("priority")).transpose()? {
        None => Ok(None),
        Some(0) if allow_clear => Ok(Some(0)),
        Some(p) if (1..=4).contains(&p) => Ok(Some(p)),
        Some(p) if allow_clear => Err(format!(
            "priority must be 1 (highest) to 4, or 0 to clear it, not {p}."
        )),
        Some(p) => Err(format!("priority must be 1 (highest) to 4, not {p}.")),
    }
}

/// Trims notes; empty notes are no notes.
pub(crate) fn clean_notes(raw: Option<&str>) -> Result<Option<String>, String> {
    let notes = raw.map(str::trim).filter(|n| !n.is_empty());
    if notes.is_some_and(|n| n.len() > MAX_NOTES) {
        return Err(format!(
            "notes are longer than {MAX_NOTES} bytes; trim them."
        ));
    }
    Ok(notes.map(str::to_string))
}

/// The user's words are kept as typed: only surrounding blank space is trimmed.
pub(crate) fn clean_source(raw: Option<&str>) -> Result<Option<String>, String> {
    let source = raw.map(str::trim).filter(|s| !s.is_empty());
    if source.is_some_and(|s| s.len() > MAX_NOTES) {
        return Err(format!(
            "source is longer than {MAX_NOTES} bytes. Keep the user's words, but split a very long message."
        ));
    }
    Ok(source.map(str::to_string))
}

pub(crate) fn clean_title(raw: &str) -> Result<String, String> {
    let title = raw.split_whitespace().collect::<Vec<_>>().join(" ");
    if title.is_empty() {
        return Err("title is empty. Give the node a short title.".to_string());
    }
    if title.chars().count() > MAX_TITLE {
        return Err(format!(
            "title is longer than {MAX_TITLE} characters. Keep it short and put detail in notes."
        ));
    }
    Ok(title)
}

/// Accepts YYYY-MM-DD (or a timestamp starting with one) and stores the date.
pub(crate) fn clean_date(field: &str, raw: &str) -> Result<String, String> {
    let raw = raw.trim();
    match parse_date(raw) {
        Some(d) if raw.len() == 10 || raw.as_bytes().get(10) == Some(&b'T') => {
            Ok(d.format("%Y-%m-%d").to_string())
        }
        _ => Err(format!(
            "{field} '{raw}' is not a date. Use YYYY-MM-DD, e.g. 2026-11-20."
        )),
    }
}

/// A repeat rule as stored JSON; areas never finish, so they cannot repeat.
/// `Some(None)` means "clear it" (only meaningful for update).
pub(crate) fn clean_recur_change(raw: Option<&RecurArg>) -> Result<Option<Option<String>>, String> {
    match raw.map(RecurArg::parse).transpose()? {
        None => Ok(None),
        Some(RecurChange::Clear) => Ok(Some(None)),
        Some(RecurChange::Set(rule)) => Ok(Some(Some(rule.to_json()))),
    }
}

fn clean_recur(raw: Option<&RecurArg>, kind: Kind) -> Result<Option<String>, String> {
    let recur = clean_recur_change(raw)?.flatten();
    if recur.is_some() && kind == Kind::Area {
        return Err(AREA_RECUR.to_string());
    }
    Ok(recur)
}

pub(crate) const AREA_RECUR: &str =
    "areas never finish, so they cannot repeat. Put recur on a task under it.";

/// Steps of an ordered parent do not repeat on their own: a step that reset itself
/// would hold every later step back forever. The parent repeats instead, and its
/// finished steps reopen with it.
pub(crate) fn recur_step_error(parent: &str, title: &str) -> String {
    format!(
        "{parent} {title} has ordered steps, and a step cannot repeat on its own (it would hold the later steps back). Put recur on {title} instead: when it repeats, its steps reopen."
    )
}

/// Aliases are stored newline-separated, as the schema describes.
pub(crate) fn clean_aliases(raw: &[String]) -> Result<Option<String>, String> {
    let list: Vec<String> = raw
        .iter()
        .map(|a| a.split_whitespace().collect::<Vec<_>>().join(" "))
        .filter(|a| !a.is_empty())
        .collect();
    if list.len() > MAX_ALIASES {
        return Err(format!(
            "give at most {MAX_ALIASES} aliases (2 to 5 is ideal)."
        ));
    }
    if list.iter().any(|a| a.chars().count() > MAX_ALIAS) {
        return Err(format!("each alias must be under {MAX_ALIAS} characters."));
    }
    Ok((!list.is_empty()).then(|| list.join("\n")))
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::model::{IdArg, id_ranges};
    use crate::store::test_support::temp_db;

    fn step(title: &str) -> NodeSpec {
        NodeSpec {
            title: title.into(),
            ..NodeSpec::default()
        }
    }

    pub(crate) fn args(parent: Option<i64>, title: &str, kind: Kind) -> AddArgs {
        AddArgs {
            parent: parent.map(IdArg::Number),
            node: NodeSpec {
                title: title.into(),
                kind: Some(kind),
                ..NodeSpec::default()
            },
            before: None,
            after: None,
            agent: Some("test".into()),
        }
    }

    #[test]
    fn adds_under_root_and_logs_event() {
        let (mut conn, dir) = temp_db("add");
        let out = add(&mut conn, args(None, "  Home  ", Kind::Area)).unwrap();
        assert_eq!(out, "Added SPOKE-2 area  Home  (under SPOKE-1 To do)");
        let (count, agent): (i64, String) = conn
            .query_row(
                "SELECT COUNT(*), MAX(agent) FROM events WHERE node_id = 2 AND type = 'created'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!((count, agent.as_str()), (1, "test"));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn rejects_bad_input_with_guidance() {
        let (mut conn, dir) = temp_db("add-bad");
        assert!(
            add(&mut conn, args(Some(99), "x", Kind::Task))
                .unwrap_err()
                .contains("SPOKE-99 does not exist")
        );
        assert!(
            add(&mut conn, args(None, "   ", Kind::Task))
                .unwrap_err()
                .contains("empty")
        );
        let mut a = args(None, "x", Kind::Task);
        a.node.due = Some("next friday".into());
        assert!(add(&mut conn, a).unwrap_err().contains("YYYY-MM-DD"));
        let mut a = args(None, "x", Kind::Task);
        a.node.priority = Some(NumArg::Number(7));
        assert!(add(&mut conn, a).is_err());
        add(&mut conn, args(None, "Task", Kind::Task)).unwrap();
        assert!(
            add(&mut conn, args(Some(2), "Area", Kind::Area))
                .unwrap_err()
                .contains("areas can only go under areas")
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn tracked_marks_top_level_and_nested_nodes() {
        let (mut conn, dir) = temp_db("add-tracked");
        let mut a = args(None, "Epic", Kind::Task);
        a.node.tracked = Some(true);
        let mut kid = args(None, "Kid", Kind::Task).node;
        kid.tracked = Some(true);
        let plain = args(None, "Plain", Kind::Task).node;
        a.node.children = Some(vec![kid, plain]);
        add(&mut conn, a).unwrap();
        let tracked: Vec<(String, bool)> = conn
            .prepare("SELECT title, tracked_at IS NOT NULL FROM nodes WHERE id > 1 ORDER BY id")
            .unwrap()
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        assert_eq!(
            tracked,
            vec![
                ("Epic".to_string(), true),
                ("Kid".to_string(), true),
                ("Plain".to_string(), false)
            ]
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn stores_type_and_refinement_loosely_parsed() {
        let (mut conn, dir) = temp_db("add-type");
        let mut a = args(None, "Fix", Kind::Task);
        a.node.work_type = Some("Bug".into());
        a.node.refinement = Some("Planned".into());
        add(&mut conn, a).unwrap();
        let row: (Option<String>, Option<String>) = conn
            .query_row(
                "SELECT work_type, refinement FROM nodes WHERE id = 2",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(row, (Some("bug".into()), Some("planned".into())));
        let mut a = args(None, "Empty", Kind::Task);
        a.node.refinement = Some(" ".into());
        assert!(add(&mut conn, a).unwrap_err().contains("Omit refinement"));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn bad_nested_type_names_child_and_writes_nothing() {
        let (mut conn, dir) = temp_db("add-type-bad");
        let mut a = args(None, "Plan", Kind::Task);
        let mut bad = step("Push");
        bad.work_type = Some("bugg".into());
        a.node.children = Some(vec![step("ok"), bad]);
        let err = add(&mut conn, a).unwrap_err();
        assert!(
            err.starts_with("children[1] (\"Push\"): unknown type"),
            "{err}"
        );
        assert!(err.contains("bug, feature"), "{err}");
        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM nodes", [], |r| r.get(0))
            .unwrap();
        assert_eq!(count, 1);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn siblings_keep_insertion_order() {
        let (mut conn, dir) = temp_db("add-order");
        for t in ["a", "b", "c"] {
            add(&mut conn, args(None, t, Kind::Task)).unwrap();
        }
        let titles: Vec<String> = crate::store::children(&conn, ROOT_ID)
            .unwrap()
            .into_iter()
            .map(|n| n.title)
            .collect();
        assert_eq!(titles, ["a", "b", "c"]);
        let _ = std::fs::remove_dir_all(dir);
    }

    fn order(conn: &Connection, parent: i64) -> Vec<i64> {
        crate::store::children(conn, NodeId(parent))
            .unwrap()
            .iter()
            .map(|n| n.id.0)
            .collect()
    }

    /// Ordered task 2 "Ladder" with steps 3, 4, 5 (M0, M1, M2).
    fn ladder(name: &str) -> (Connection, std::path::PathBuf) {
        let (mut conn, dir) = temp_db(name);
        let mut a = args(None, "Ladder", Kind::Task);
        a.node.ordered = Some(true);
        a.node.children = Some(vec![step("M0"), step("M1"), step("M2")]);
        add(&mut conn, a).unwrap();
        (conn, dir)
    }

    fn placed(
        parent: Option<i64>,
        title: &str,
        before: Option<i64>,
        after: Option<i64>,
    ) -> AddArgs {
        let mut a = args(parent, title, Kind::Task);
        a.before = before.map(IdArg::Number);
        a.after = after.map(IdArg::Number);
        a
    }

    /// SPOKE-459: after/before on add place the node, and the parent comes from the sibling.
    #[test]
    fn after_and_before_place_the_new_node() {
        let (mut conn, dir) = ladder("add-after");
        let out = add(&mut conn, placed(None, "M0.5", None, Some(3))).unwrap();
        assert_eq!(
            out.lines().next().unwrap(),
            "Added SPOKE-6 task  M0.5  (under SPOKE-2 Ladder, after SPOKE-3 M0, 2 of 4)"
        );
        assert_eq!(order(&conn, 2), [3, 6, 4, 5]);
        let out = add(&mut conn, placed(Some(2), "M1.5", Some(5), None)).unwrap();
        assert!(
            out.starts_with(
                "Added SPOKE-7 task  M1.5  (under SPOKE-2 Ladder, before SPOKE-5 M2, 4 of 5)"
            ),
            "{out}"
        );
        assert_eq!(order(&conn, 2), [3, 6, 4, 7, 5]);
        // After the last one, and before the first one.
        add(&mut conn, placed(None, "M3", None, Some(5))).unwrap();
        add(&mut conn, placed(None, "Prep", Some(3), None)).unwrap();
        assert_eq!(order(&conn, 2), [9, 3, 6, 4, 7, 5, 8]);
        // Children of a placed node keep their own order.
        let mut a = placed(None, "M2.5", None, Some(5));
        a.node.children = Some(vec![step("a"), step("b")]);
        add(&mut conn, a).unwrap();
        assert_eq!(order(&conn, 2), [9, 3, 6, 4, 7, 5, 10, 8]);
        assert_eq!(order(&conn, 10), [11, 12]);
        let positions: Vec<String> = crate::store::children(&conn, NodeId(2))
            .unwrap()
            .into_iter()
            .map(|n| n.position)
            .collect();
        assert!(positions.windows(2).all(|w| w[0] < w[1]), "{positions:?}");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn placed_ahead_of_the_next_step_says_so() {
        let (mut conn, dir) = ladder("add-before-next");
        let out = add(&mut conn, placed(None, "Prep", Some(3), None)).unwrap();
        assert!(
            out.contains("Next: SPOKE-6 is now the next step of SPOKE-2 Ladder."),
            "{out}"
        );
        // Behind the next step: no such line.
        let out = add(&mut conn, placed(None, "Later", None, Some(4))).unwrap();
        assert!(!out.contains("now the next step"), "{out}");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn bad_placement_is_refused_and_writes_nothing() {
        let (mut conn, dir) = ladder("add-after-bad");
        add(&mut conn, args(None, "Other", Kind::Task)).unwrap(); // 6
        let cases = [
            (placed(None, "x", Some(3), Some(4)), "not both"),
            (placed(Some(6), "x", None, Some(3)), "is not under SPOKE-6"),
            (placed(None, "x", None, Some(99)), "SPOKE-99 does not exist"),
            (
                placed(None, "x", None, Some(1)),
                "is the root and has no siblings",
            ),
            (placed(None, "M1", None, Some(3)), "SPOKE-4"),
        ];
        for (a, needle) in cases {
            let err = add(&mut conn, a).unwrap_err();
            assert!(err.contains(needle), "{needle}: {err}");
        }
        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM nodes", [], |r| r.get(0))
            .unwrap();
        assert_eq!(count, 6);
        assert_eq!(order(&conn, 2), [3, 4, 5]);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn nested_plan_is_one_call_and_one_line() {
        let (mut conn, dir) = temp_db("add-nested");
        let mut a = args(None, "Deploy", Kind::Task);
        a.node.ordered = Some(true);
        let mut s2 = step("Write");
        s2.children = Some(vec![step("Draft"), step("Review")]);
        a.node.children = Some(vec![step("Map"), s2, step("Ship")]);
        let out = add(&mut conn, a).unwrap();
        // The created tree, one line per node (SPOKE-324); the add hint (SPOKE-1079)
        // follows it.
        let lines: Vec<&str> = out.lines().collect();
        assert_eq!(
            lines[..6],
            [
                "Added SPOKE-2 task  Deploy  (under SPOKE-1 To do), with 5 beneath it in the order given:",
                "  SPOKE-3 task  Map",
                "  SPOKE-4 task  Write",
                "    SPOKE-5 task  Draft",
                "    SPOKE-6 task  Review",
                "  SPOKE-7 task  Ship",
            ],
            "{out}"
        );
        assert!(lines[6..].iter().all(|l| l.starts_with("Next: ")), "{out}");
        let titles: Vec<String> = crate::store::children(&conn, NodeId(2))
            .unwrap()
            .into_iter()
            .map(|n| n.title)
            .collect();
        assert_eq!(titles, ["Map", "Write", "Ship"]);
        let draft_parent: i64 = conn
            .query_row(
                "SELECT parent_id FROM nodes WHERE title = 'Draft'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(draft_parent, 4);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn nested_errors_name_the_child_and_write_nothing() {
        let (mut conn, dir) = temp_db("add-nested-bad");
        let mut a = args(None, "Plan", Kind::Task);
        let mut bad = step("Area inside");
        bad.kind = Some(Kind::Area);
        a.node.children = Some(vec![step("ok"), bad]);
        let err = add(&mut conn, a).unwrap_err();
        assert!(err.starts_with("children[1] (\"Area inside\")"), "{err}");
        let mut a = args(None, "Plan", Kind::Task);
        a.node.children = Some(vec![step("ok"), step("  ")]);
        let err = add(&mut conn, a).unwrap_err();
        assert!(
            err.contains("children[1]") && err.contains("empty"),
            "{err}"
        );
        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM nodes", [], |r| r.get(0))
            .unwrap();
        assert_eq!(count, 1);
        assert_eq!(
            id_ranges(&[NodeId(3), NodeId(4), NodeId(5), NodeId(9), NodeId(10)]),
            "SPOKE-3..SPOKE-5, SPOKE-9, SPOKE-10"
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    /// `add` with parent and children but no title (SPOKE-228).
    fn batch(parent: i64, kids: Vec<NodeSpec>) -> AddArgs {
        AddArgs {
            parent: Some(IdArg::Number(parent)),
            node: NodeSpec {
                children: Some(kids),
                ..NodeSpec::default()
            },
            before: None,
            after: None,
            agent: Some("test".into()),
        }
    }

    fn titles_under(conn: &Connection, parent: i64) -> Vec<String> {
        crate::store::children(conn, NodeId(parent))
            .unwrap()
            .into_iter()
            .map(|c| c.title)
            .collect()
    }

    fn with_aliases(title: &str) -> NodeSpec {
        NodeSpec {
            aliases: Some(vec!["a".into(), "b".into()]),
            ..step(title)
        }
    }

    #[test]
    fn batch_adds_children_under_an_existing_parent_in_order() {
        let (mut conn, dir) = temp_db("add-batch");
        add(&mut conn, args(None, "Project", Kind::Task)).unwrap(); // SPOKE-2
        add(&mut conn, batch(2, vec![with_aliases("Old step")])).unwrap(); // SPOKE-3
        let mut nested = with_aliases("Two");
        nested.children = Some(vec![with_aliases("Two a"), with_aliases("Two b")]);
        let out = add(
            &mut conn,
            batch(2, vec![with_aliases("One"), nested, with_aliases("Three")]),
        )
        .unwrap();
        assert_eq!(
            out,
            "Added 5 under SPOKE-2 Project, in the order given:\n  SPOKE-4 task  One\n  SPOKE-5 task  Two\n    SPOKE-6 task  Two a\n    SPOKE-7 task  Two b\n  SPOKE-8 task  Three"
        );
        assert_eq!(titles_under(&conn, 2), ["Old step", "One", "Two", "Three"]);
        assert_eq!(titles_under(&conn, 5), ["Two a", "Two b"]);
        let created: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM events WHERE type = 'created' AND node_id BETWEEN 4 AND 8",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(created, 5);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn batch_after_a_sibling_keeps_the_children_together() {
        let (mut conn, dir) = temp_db("add-batch-after");
        add(&mut conn, args(None, "Plan", Kind::Task)).unwrap(); // SPOKE-2
        add(
            &mut conn,
            batch(2, vec![with_aliases("A"), with_aliases("D")]),
        )
        .unwrap(); // SPOKE-3, SPOKE-4
        let mut a = batch(2, vec![with_aliases("B"), with_aliases("C")]);
        a.parent = None;
        a.after = Some(IdArg::Number(3));
        let out = add(&mut conn, a).unwrap();
        assert!(
            out.starts_with("Added 2 under SPOKE-2 Plan, after SPOKE-3 A, places 2 to 3 of 4, in the order given:"),
            "{out}"
        );
        assert_eq!(titles_under(&conn, 2), ["A", "B", "C", "D"]);
        let mut b = batch(2, vec![with_aliases("Z1"), with_aliases("Z2")]);
        b.before = Some(IdArg::Number(3));
        add(&mut conn, b).unwrap();
        assert_eq!(titles_under(&conn, 2), ["Z1", "Z2", "A", "B", "C", "D"]);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn batch_into_an_ordered_parent_names_the_next_step() {
        let (mut conn, dir) = temp_db("add-batch-ordered");
        let mut p = args(None, "Ladder", Kind::Task);
        p.node.ordered = Some(true);
        add(&mut conn, p).unwrap(); // SPOKE-2
        let out = add(
            &mut conn,
            batch(2, vec![with_aliases("Rung 1"), with_aliases("Rung 2")]),
        )
        .unwrap();
        assert!(
            out.ends_with("Next: SPOKE-3 is now the next step of SPOKE-2 Ladder."),
            "{out}"
        );
        // A later batch behind an open step does not take it over.
        let out = add(&mut conn, batch(2, vec![with_aliases("Rung 3")])).unwrap();
        assert!(!out.contains("next step"), "{out}");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn batch_hint_lists_unaliased_tasks() {
        let (mut conn, dir) = temp_db("add-batch-hint");
        add(&mut conn, args(None, "Project", Kind::Task)).unwrap(); // SPOKE-2
        let out = add(&mut conn, batch(2, vec![step("x"), step("y")])).unwrap();
        assert!(out.contains("No aliases on SPOKE-3, SPOKE-4"), "{out}");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn batch_refusals_name_the_child_and_write_nothing() {
        let (mut conn, dir) = temp_db("add-batch-bad");
        add(&mut conn, args(None, "Task", Kind::Task)).unwrap(); // SPOKE-2
        add(&mut conn, batch(2, vec![with_aliases("Taken")])).unwrap(); // SPOKE-3
        let before: i64 = conn
            .query_row("SELECT COUNT(*) FROM nodes", [], |r| r.get(0))
            .unwrap();

        let err = add(&mut conn, batch(2, vec![step("New"), step("taken!")])).unwrap_err();
        assert!(err.starts_with("children[1] (\"taken!\"): "), "{err}");
        let err = add(&mut conn, batch(2, vec![step("Same"), step("same")])).unwrap_err();
        assert!(
            err.contains("children[0] (\"Same\") and children[1] (\"same\") have the same title"),
            "{err}"
        );
        let area = NodeSpec {
            kind: Some(Kind::Area),
            ..step("Area")
        };
        let err = add(&mut conn, batch(2, vec![step("Fine"), area])).unwrap_err();
        assert!(err.contains("children[1] (\"Area\"): "), "{err}");
        assert!(err.contains("areas can only go under areas"), "{err}");
        let err = add(&mut conn, batch(2, vec![step("  ")])).unwrap_err();
        assert!(err.starts_with("children[0]"), "{err}");

        // Fields that describe a new node, with no title for it.
        let mut a = batch(2, vec![step("Kid")]);
        a.node.notes = Some("hi".into());
        a.node.ordered = Some(true);
        let err = add(&mut conn, a).unwrap_err();
        assert!(
            err.starts_with("ordered, notes describe a new node, but there is no title"),
            "{err}"
        );
        // No title and no children.
        let mut a = batch(2, vec![]);
        a.node.children = None;
        let err = add(&mut conn, a).unwrap_err();
        assert!(err.contains("leave title out and pass children"), "{err}");

        let after: i64 = conn
            .query_row("SELECT COUNT(*) FROM nodes", [], |r| r.get(0))
            .unwrap();
        assert_eq!(before, after);
        let _ = std::fs::remove_dir_all(dir);
    }
}
