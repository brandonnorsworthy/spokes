//! `get`: one node, its breadcrumb and an outline of its children.

use chrono::NaiveDate;
use rusqlite::{Connection, OptionalExtension, params};

use super::{ToolResult, ancestors, breadcrumb, children, db_err, require};
use crate::format::{child_line, header_line, today};
use crate::model::{Node, NodeId, ROOT_ID, Status};
use crate::tools::args::GetArgs;

const MAX_DEPTH: u32 = 5;
/// Cap on outline lines so a huge subtree stays cheap to read.
const MAX_LINES: usize = 60;

pub fn get(conn: &Connection, args: GetArgs) -> ToolResult {
    let id = match &args.id {
        Some(i) => i.parse()?,
        None => ROOT_ID,
    };
    let depth = args.depth.unwrap_or(1).clamp(1, MAX_DEPTH);
    let node = require(conn, id)?;
    let today = today();

    let mut out = vec![header_line(&node, today)];
    if node.parent_id.is_some() {
        let path = breadcrumb(&ancestors(conn, id)?);
        out.push(if path.is_empty() {
            "(top level)".to_string()
        } else {
            path
        });
    }

    let mut outline = Outline {
        lines: Vec::new(),
        omitted: 0,
        dropped: 0,
        today,
    };
    outline.walk(conn, &node, 1, depth)?;
    if outline.lines.is_empty() && outline.dropped == 0 {
        out.push("  (no children)".to_string());
    }
    out.append(&mut outline.lines);
    if outline.omitted > 0 {
        out.push(format!(
            "(+{} more not shown. Use get on a child, or a smaller depth.)",
            outline.omitted
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

struct Outline {
    lines: Vec<String>,
    omitted: usize,
    dropped: usize,
    today: NaiveDate,
}

impl Outline {
    fn walk(
        &mut self,
        conn: &Connection,
        parent: &Node,
        level: u32,
        max: u32,
    ) -> Result<(), String> {
        let kids = children(conn, parent.id)?;
        // In an ordered parent only the first unfinished step is actionable.
        let next_step: Option<NodeId> = parent
            .ordered
            .then(|| kids.iter().find(|k| !k.status.is_closed()).map(|k| k.id))
            .flatten();
        let indent = "  ".repeat(level as usize);
        let mut number = 0;
        for kid in &kids {
            if kid.status == Status::Dropped {
                self.dropped += 1;
                continue;
            }
            if self.lines.len() >= MAX_LINES {
                self.omitted += 1;
                continue;
            }
            number += 1;
            let bullet = if parent.ordered {
                format!("{number}. ")
            } else {
                "- ".to_string()
            };
            let mut line = format!("{indent}{bullet}{}", child_line(kid, self.today));
            if next_step == Some(kid.id) {
                line.push_str("  <- next");
            }
            self.lines.push(line);
            if level < max && kid.child_total > 0 {
                self.walk(conn, kid, level + 1, max)?;
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{IdArg, Kind};
    use crate::store::add;
    use crate::store::add::tests::args;
    use crate::store::test_support::temp_db;

    fn get_args(id: Option<i64>, depth: Option<u32>) -> GetArgs {
        GetArgs {
            id: id.map(IdArg::Number),
            depth,
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
        p.ordered = Some(true);
        p.notes = Some("plan".into());
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
            "SPOKE-3 task ordered (0/2) +notes  Deploy\nHobbies\n  1. SPOKE-4 open unplanned  Step one  <- next\n  2. SPOKE-5 open unplanned  Step two\nNOTES\nplan"
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
}
