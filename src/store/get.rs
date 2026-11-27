//! `get`: one node, its breadcrumb and an outline of its children.

use std::collections::HashMap;

use chrono::NaiveDate;
use chrono_tz::Tz;
use rusqlite::{Connection, OptionalExtension, params};

use super::{
    NODE_COLS, ToolResult, ancestors, breadcrumb_with_parent_id, db_err, node_from_row, require,
};
use crate::format::{child_line, clock, earliest, header_line, today_in};
use crate::model::{DEFAULT_USER, Node, NodeId, ROOT_ID, Status};
use crate::tools::args::GetArgs;

const MAX_DEPTH: i64 = 10;
/// Cap on outline lines so a huge subtree stays cheap to read.
const MAX_LINES: usize = 60;
/// Events shown by `history: true`, newest first.
const HISTORY_LINES: i64 = 10;

pub fn get(conn: &Connection, args: GetArgs, tz: Tz) -> ToolResult {
    let today = today_in(tz);
    let id = match &args.id {
        Some(i) => i.parse()?,
        None => ROOT_ID,
    };
    let depth = match &args.depth {
        Some(d) => d.parse("depth")?.clamp(1, MAX_DEPTH),
        None => 1,
    };
    let node = require(conn, id)?;

    let above = ancestors(conn, id)?;
    let above_due = above
        .iter()
        .fold(None, |acc, a| earliest(acc, a.due_at.as_deref()));
    let mut out = vec![header_line(&node, today, above_due)];
    if node.parent_id.is_some() {
        let path = breadcrumb_with_parent_id(&above);
        out.push(if path.is_empty() {
            "(top level)".to_string()
        } else {
            path
        });
    }
    if let Some(line) = claim_line(&node, tz) {
        out.push(line);
    }
    if let Some(at) = &node.tracked_at {
        out.push(format!("Tracked since {}", local_date(at, tz)));
    }
    if let Some(rule) = node
        .recur
        .as_deref()
        .and_then(crate::recur::Rule::from_json)
    {
        out.push(format!("repeats {}", rule.describe()));
    }

    let (tree, deeper) = subtree(conn, id, depth)?;
    let mut outline = Outline {
        lines: Vec::new(),
        capped: 0,
        dropped: 0,
        today,
    };
    outline.walk(&tree, &node, 1, above_due);
    if outline.lines.is_empty() && outline.capped == 0 && outline.dropped == 0 {
        out.push("  (no children)".to_string());
    }
    out.append(&mut outline.lines);
    if outline.capped > 0 {
        out.push(format!(
            "(+{} more not shown. Use get on a child, or a smaller depth.)",
            outline.capped
        ));
    }
    if deeper > 0 {
        out.push(format!(
            "(+{deeper} more below depth {depth}. Pass a larger depth, or get a child.)"
        ));
    }
    if outline.dropped > 0 {
        out.push(format!("({} dropped not shown)", outline.dropped));
    }

    if args.notes.unwrap_or(false) {
        let source: Option<String> = conn
            .query_row(
                "SELECT source FROM nodes WHERE id = ?1",
                params![id.0],
                |r| r.get(0),
            )
            .map_err(db_err)?;
        if let Some(s) = source {
            out.push("SOURCE (the user's words)".to_string());
            out.push(s);
        }
        let body: Option<String> = conn
            .query_row(
                "SELECT body FROM notes WHERE node_id = ?1",
                params![id.0],
                |r| r.get(0),
            )
            .optional()
            .map_err(db_err)?;
        match body {
            Some(b) => {
                out.push("NOTES".to_string());
                out.push(b);
            }
            None => out.push("(no notes)".to_string()),
        }
    }
    if args.history.unwrap_or(false) {
        out.push("HISTORY (newest first)".to_string());
        out.extend(history(conn, id, tz)?);
    }
    let later_step = match above.last() {
        Some(parent) if parent.ordered => super::children(conn, parent.id)?
            .into_iter()
            .find(|c| !c.status.is_closed())
            .is_some_and(|first| first.id != node.id),
        _ => false,
    };
    let action = crate::hints::Action::Read {
        node: &node,
        later_step,
    };
    if let Some(line) = crate::hints::hint(&action) {
        out.push(line);
    }
    Ok(out.join("\n"))
}

/// The day of a stored UTC instant in the user's time zone, `2026-11-27`.
fn local_date(timestamp: &str, tz: Tz) -> String {
    match chrono::DateTime::parse_from_rfc3339(timestamp) {
        Ok(t) => t.with_timezone(&tz).date_naive().to_string(),
        Err(_) => timestamp.to_string(),
    }
}

/// Who holds the node, what it waits on, or that a question was answered.
fn claim_line(node: &Node, tz: Tz) -> Option<String> {
    let who = node.claimed_by.as_deref().unwrap_or("an agent");
    match node.status {
        Status::InProgress => {
            let until = node.claim_expires.as_deref().unwrap_or("");
            let lapsed = until <= crate::db::now().as_str();
            Some(if lapsed {
                format!("stalled: claim by {who} expired {}", clock(until, tz))
            } else {
                format!("claimed by {who} until {}", clock(until, tz))
            })
        }
        Status::Waiting => Some(format!(
            "waiting on the user: \"{}\" (asked by {who})",
            node.waiting_on.as_deref().unwrap_or("")
        )),
        Status::Open => node.answered_at.as_deref().map(|at| {
            format!(
                "answered {}, ready to claim (the answer is in the notes)",
                clock(at, tz)
            )
        }),
        _ => None,
    }
}

/// `  Nov 14 14:02  opus-red  claimed  until ...`, newest first.
fn history(conn: &Connection, id: NodeId, tz: Tz) -> Result<Vec<String>, String> {
    let mut stmt = conn
        .prepare_cached(
            "SELECT at, agent, type, detail FROM events WHERE node_id = ?1
             ORDER BY id DESC LIMIT ?2",
        )
        .map_err(db_err)?;
    let rows: Vec<(String, Option<String>, String, Option<String>)> = stmt
        .query_map(params![id.0, HISTORY_LINES], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?))
        })
        .and_then(|r| r.collect())
        .map_err(db_err)?;
    Ok(rows
        .into_iter()
        .map(|(at, agent, kind, detail)| {
            let mut line = format!("  {}  {kind}", clock(&at, tz));
            if let Some(a) = agent {
                line.push_str(&format!(" by {a}"));
            }
            if let Some(d) = detail.filter(|d| !d.is_empty()) {
                let d = local_times(&d, tz);
                let short: String = d.chars().take(120).collect();
                let more = if d.chars().count() > 120 { "..." } else { "" };
                line.push_str(&format!(": {short}{more}"));
            }
            line
        })
        .collect())
}

/// Hidden by `defer_until` until a later day.
fn deferred(node: &Node, today: NaiveDate) -> bool {
    node.defer_until
        .as_deref()
        .and_then(crate::format::parse_date)
        .is_some_and(|d| d > today)
}

/// Event details store UTC timestamps (`until 2026-11-14T08:49:35Z`); show them as
/// wall-clock time in `SPOKES_TZ` like the rest of the output.
fn local_times(detail: &str, tz: Tz) -> String {
    detail
        .split(' ')
        .map(|word| {
            let end = word
                .find(|c: char| !(c.is_ascii_alphanumeric() || c == '-' || c == ':' || c == '+'))
                .unwrap_or(word.len());
            let (stamp, tail) = word.split_at(end);
            if stamp.len() >= 20 && chrono::DateTime::parse_from_rfc3339(stamp).is_ok() {
                format!("{}{tail}", clock(stamp, tz))
            } else {
                word.to_string()
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// Descendants of `id` that are not beneath a dropped node.
const SUBTREE: &str = "WITH RECURSIVE sub(id, level) AS (
       SELECT id, 1 FROM nodes WHERE parent_id = ?1 AND user_id = ?2
       UNION ALL
       SELECT c.id, sub.level + 1 FROM sub
       JOIN nodes p ON p.id = sub.id AND p.status != 'dropped'
       JOIN nodes c ON c.parent_id = sub.id
     )";

/// The outline down to `depth` levels, by parent in sibling order, plus how many
/// live (not dropped) nodes lie deeper.
fn subtree(
    conn: &Connection,
    id: NodeId,
    depth: i64,
) -> Result<(HashMap<NodeId, Vec<Node>>, i64), String> {
    let mut stmt = conn
        .prepare_cached(&format!(
            "{SUBTREE} SELECT {NODE_COLS} FROM sub JOIN nodes n ON n.id = sub.id
             WHERE sub.level <= ?3 ORDER BY n.parent_id, n.position"
        ))
        .map_err(db_err)?;
    let rows: Vec<Node> = stmt
        .query_map(params![id.0, DEFAULT_USER, depth], node_from_row)
        .and_then(|r| r.collect())
        .map_err(db_err)?;
    let mut tree: HashMap<NodeId, Vec<Node>> = HashMap::new();
    for n in rows {
        if let Some(p) = n.parent_id {
            tree.entry(p).or_default().push(n);
        }
    }
    let deeper: i64 = conn
        .prepare_cached(&format!(
            "{SUBTREE} SELECT COUNT(*) FROM sub JOIN nodes n ON n.id = sub.id
             WHERE sub.level > ?3 AND n.status != 'dropped'"
        ))
        .and_then(|mut s| s.query_row(params![id.0, DEFAULT_USER, depth], |r| r.get(0)))
        .map_err(db_err)?;
    Ok((tree, deeper))
}

struct Outline {
    lines: Vec<String>,
    /// Nodes within the depth that did not fit under the line cap.
    capped: usize,
    dropped: usize,
    today: NaiveDate,
}

impl Outline {
    /// `parent_due` is the earliest due date above `parent`.
    fn walk(
        &mut self,
        tree: &HashMap<NodeId, Vec<Node>>,
        parent: &Node,
        level: usize,
        parent_due: Option<&str>,
    ) {
        let Some(kids) = tree.get(&parent.id) else {
            return;
        };
        let bound = earliest(parent_due, parent.due_at.as_deref());
        // In an ordered parent only the first unfinished step is actionable.
        let next_step: Option<NodeId> = (parent.ordered && !parent.status.is_closed())
            .then(|| kids.iter().find(|k| !k.status.is_closed()))
            .flatten()
            .filter(|k| k.status == Status::Open && !deferred(k, self.today))
            .map(|k| k.id);
        let indent = "  ".repeat(level);
        let mut number = 0;
        for kid in kids {
            if kid.status == Status::Dropped {
                self.dropped += 1;
                continue;
            }
            number += 1;
            if self.lines.len() >= MAX_LINES {
                self.capped += 1;
            } else {
                let bullet = if parent.ordered {
                    format!("{number}. ")
                } else {
                    "- ".to_string()
                };
                let mut line = format!("{indent}{bullet}{}", child_line(kid, self.today, bound));
                if next_step == Some(kid.id) {
                    line.push_str("  <- next");
                }
                self.lines.push(line);
            }
            self.walk(tree, kid, level + 1, bound);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{IdArg, Kind, NumArg};
    use crate::store::add;
    use crate::store::add::tests::args;
    use crate::store::test_support::temp_db;

    fn get_args(id: Option<i64>, depth: Option<i64>) -> GetArgs {
        GetArgs {
            id: id.map(IdArg::Number),
            depth: depth.map(NumArg::Number),
            notes: None,
            history: None,
        }
    }

    #[test]
    fn shows_root_and_children() {
        let (mut conn, dir) = temp_db("get");
        for t in ["Home", "Work", "Hobbies"] {
            add(&mut conn, args(None, t, Kind::Area)).unwrap();
        }
        let out = get(&conn, get_args(None, None), chrono_tz::UTC).unwrap();
        assert_eq!(
            out,
            "SPOKE-1 area  To do\n  - SPOKE-2 area  Home\n  - SPOKE-3 area  Work\n  - SPOKE-4 area  Hobbies"
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn ordered_parent_marks_next_step_and_breadcrumb() {
        let (mut conn, dir) = temp_db("get-ordered");
        add(&mut conn, args(None, "Hobbies", Kind::Area)).unwrap();
        let mut p = args(Some(2), "Deploy", Kind::Task);
        p.node.ordered = Some(true);
        p.node.notes = Some("plan".into());
        add(&mut conn, p).unwrap();
        add(&mut conn, args(Some(3), "Step one", Kind::Task)).unwrap();
        add(&mut conn, args(Some(3), "Step two", Kind::Task)).unwrap();
        let out = get(
            &conn,
            GetArgs {
                id: Some(IdArg::Text("SPOKE-3".into())),
                depth: None,
                notes: Some(true),
                history: None,
            },
            chrono_tz::UTC,
        )
        .unwrap();
        assert_eq!(
            out,
            "SPOKE-3 task ordered (0/2) +notes  Deploy\nHobbies (SPOKE-2)\n  1. SPOKE-4 open  Step one  <- next\n  2. SPOKE-5 open  Step two\nNOTES\nplan"
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn missing_node_is_an_actionable_error() {
        let (conn, dir) = temp_db("get-missing");
        let err = get(&conn, get_args(Some(42), None), chrono_tz::UTC).unwrap_err();
        assert!(err.starts_with("SPOKE-42 does not exist"), "{err}");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn outline_is_capped_and_counts_what_it_left_out() {
        let (mut conn, dir) = temp_db("get-cap");
        add(&mut conn, args(None, "Big", Kind::Area)).unwrap(); // 2
        for i in 0..70 {
            add(&mut conn, args(Some(2), &format!("Item {i}"), Kind::Task)).unwrap();
        }
        // Children of the first item sit below depth 1; one dropped item is hidden.
        add(&mut conn, args(Some(3), "Deep", Kind::Task)).unwrap();
        crate::store::set_status(
            &mut conn,
            serde_json::from_value(serde_json::json!({"ids": ["SPOKE-4"], "status": "dropped"}))
                .unwrap(),
            crate::format::today_in(chrono_tz::UTC),
        )
        .unwrap();
        let out = get(&conn, get_args(Some(2), None), chrono_tz::UTC).unwrap();
        let lines: Vec<&str> = out.lines().collect();
        assert_eq!(lines.len(), 2 + 60 + 3, "{out}");
        assert_eq!(
            &lines[62..],
            [
                "(+9 more not shown. Use get on a child, or a smaller depth.)",
                "(+1 more below depth 1. Pass a larger depth, or get a child.)",
                "(1 dropped not shown)"
            ]
        );
        let _ = std::fs::remove_dir_all(dir);
    }
}
