//! `get`: one node, its breadcrumb and an outline of its children.

use std::collections::HashMap;

use chrono::NaiveDate;
use rusqlite::{Connection, OptionalExtension, params};

use super::{NODE_COLS, ToolResult, ancestors, breadcrumb, db_err, node_from_row, require};
use crate::format::{child_line, earliest, header_line, today};
use crate::model::{DEFAULT_USER, Node, NodeId, ROOT_ID, Status};
use crate::tools::args::GetArgs;

const MAX_DEPTH: i64 = 10;
/// Cap on outline lines so a huge subtree stays cheap to read.
const MAX_LINES: usize = 60;

pub fn get(conn: &Connection, args: GetArgs) -> ToolResult {
    let id = match &args.id {
        Some(i) => i.parse()?,
        None => ROOT_ID,
    };
    let depth = match &args.depth {
        Some(d) => d.parse("depth")?.clamp(1, MAX_DEPTH),
        None => 1,
    };
    let node = require(conn, id)?;
    let today = today();

    let above = ancestors(conn, id)?;
    let above_due = above
        .iter()
        .fold(None, |acc, a| earliest(acc, a.due_at.as_deref()));
    let mut out = vec![header_line(&node, today, above_due)];
    if node.parent_id.is_some() {
        let path = breadcrumb(&above);
        out.push(if path.is_empty() {
            "(top level)".to_string()
        } else {
            path
        });
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
    Ok(out.join("\n"))
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
            .filter(|k| k.status == Status::Open)
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
        }
    }

    #[test]
    fn shows_root_and_children() {
        let (mut conn, dir) = temp_db("get");
        for t in ["Home", "Work", "Hobbies"] {
            add(&mut conn, args(None, t, Kind::Area)).unwrap();
        }
        let out = get(&conn, get_args(None, None)).unwrap();
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
            },
        )
        .unwrap();
        assert_eq!(
            out,
            "SPOKE-3 task ordered (0/2) +notes  Deploy\nHobbies\n  1. SPOKE-4 open  Step one  <- next\n  2. SPOKE-5 open  Step two\nNOTES\nplan"
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn missing_node_is_an_actionable_error() {
        let (conn, dir) = temp_db("get-missing");
        let err = get(&conn, get_args(Some(42), None)).unwrap_err();
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
        )
        .unwrap();
        let out = get(&conn, get_args(Some(2), None)).unwrap();
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
