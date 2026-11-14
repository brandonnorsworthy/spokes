//! `next`: the queue of what needs the user and what can be worked on.
//!
//! M0 ordering: questions and stalled claims first, then work items by level, due
//! date and age. Focus, due-soon and stale sections arrive in later milestones.

use std::collections::HashMap;

use chrono::{NaiveDate, Utc};
use rusqlite::{Connection, params};

use super::{NODE_COLS, ToolResult, db_err, node_from_row, require};
use crate::db::now;
use crate::format::{ago, parse_date, queue_line, today};
use crate::model::{DEFAULT_USER, Kind, Node, NodeId, ROOT_ID, Status};
use crate::tools::args::NextArgs;

const DEFAULT_LIMIT: u32 = 5;
const MAX_LIMIT: u32 = 50;

pub fn next(conn: &Connection, args: NextArgs) -> ToolResult {
    let scope_id = match &args.scope {
        Some(s) => s.parse()?,
        None => ROOT_ID,
    };
    let limit = args.limit.unwrap_or(DEFAULT_LIMIT).clamp(1, MAX_LIMIT) as usize;
    let scope = require(conn, scope_id)?;
    if scope.status.is_closed() {
        return Err(format!(
            "{} {} is {}; nothing under it is in the queue. Use set_status to reopen it.",
            scope.id,
            scope.title,
            scope.status.as_str()
        ));
    }

    let tree = Tree::load(conn)?;
    let mut queue = Queue {
        now: now(),
        today: today(),
        needs_you: Vec::new(),
        work: Vec::new(),
    };
    queue.walk(&tree, &scope, true);
    queue.work.sort_by(|a, b| {
        let key = |n: &Node| {
            (
                n.priority.unwrap_or(5),
                n.due_at.is_none(),
                n.due_at.clone(),
                n.created_at.clone(),
            )
        };
        key(a).cmp(&key(b))
    });

    let total = queue.needs_you.len() + queue.work.len();
    if total == 0 {
        return Ok(format!(
            "Nothing actionable under {} {}. Add a task with add.",
            scope.id, scope.title
        ));
    }

    let now_dt = Utc::now();
    let mut out = Vec::new();
    let mut shown = 0;
    if !queue.needs_you.is_empty() {
        out.push("NEEDS YOU".to_string());
        for n in queue.needs_you.iter().take(limit) {
            out.push(format!("  {}", needs_you_line(n, now_dt)));
            shown += 1;
        }
    }
    if shown < limit && !queue.work.is_empty() {
        out.push("UP NEXT".to_string());
        for n in queue.work.iter().take(limit - shown) {
            let mut line = format!("  {}", queue_line(n, queue.today));
            if let Some(step) = tree.next_step(n.id) {
                line.push_str(&format!("  next: {} {}", step.id, step.title));
            }
            out.push(line);
            shown += 1;
        }
    }
    if total > shown {
        out.push(format!(
            "(+{} more open. Pass scope or limit.)",
            total - shown
        ));
    }
    Ok(out.join("\n"))
}

fn needs_you_line(n: &Node, now_dt: chrono::DateTime<Utc>) -> String {
    if n.status == Status::Waiting {
        let mut line = format!("{}  waiting  {}", n.id, n.title);
        if let Some(q) = &n.waiting_on {
            line.push_str(&format!(": \"{q}\""));
        }
        let who = n
            .claimed_by
            .as_deref()
            .map(|a| format!("{a}, "))
            .unwrap_or_default();
        line.push_str(&format!(" ({who}{})", ago(&n.updated_at, now_dt)));
        line
    } else {
        let who = n.claimed_by.as_deref().unwrap_or("an agent");
        let since = n
            .claim_expires
            .as_deref()
            .map(|e| ago(e, now_dt))
            .unwrap_or_default();
        format!(
            "{}  stalled  {}  (claim by {who} expired {since})",
            n.id, n.title
        )
    }
}

/// The live (not done or dropped) nodes, indexed by parent, in sibling order.
struct Tree {
    children: HashMap<NodeId, Vec<Node>>,
}

impl Tree {
    fn load(conn: &Connection) -> Result<Self, String> {
        let mut stmt = conn
            .prepare_cached(&format!(
                "SELECT {NODE_COLS} FROM nodes n
                 WHERE n.user_id = ?1 AND n.status NOT IN ('done', 'dropped') AND n.parent_id IS NOT NULL
                 ORDER BY n.parent_id, n.position"
            ))
            .map_err(db_err)?;
        let rows: Vec<Node> = stmt
            .query_map(params![DEFAULT_USER], node_from_row)
            .and_then(|r| r.collect())
            .map_err(db_err)?;
        let mut children: HashMap<NodeId, Vec<Node>> = HashMap::new();
        for n in rows {
            if let Some(p) = n.parent_id {
                children.entry(p).or_default().push(n);
            }
        }
        Ok(Tree { children })
    }

    fn kids(&self, id: NodeId) -> &[Node] {
        self.children.get(&id).map_or(&[], Vec::as_slice)
    }

    /// The first actionable leaf beneath a work item, for the `next:` hint.
    fn next_step(&self, id: NodeId) -> Option<&Node> {
        let mut current = self.kids(id).iter().find(|k| k.status == Status::Open)?;
        while let Some(k) = self
            .kids(current.id)
            .iter()
            .find(|k| k.status == Status::Open)
        {
            current = k;
        }
        Some(current)
    }
}

struct Queue {
    now: String,
    today: NaiveDate,
    needs_you: Vec<Node>,
    work: Vec<Node>,
}

impl Queue {
    /// Walks the live tree. `collect_work` is false once inside a task (the queue lists
    /// work items, not every leaf) or behind an earlier step of an ordered parent.
    fn walk(&mut self, tree: &Tree, parent: &Node, collect_work: bool) {
        let mut earlier_open_step = false;
        for kid in tree.kids(parent.id) {
            let blocked = parent.ordered && earlier_open_step;
            earlier_open_step = true;
            if kid
                .defer_until
                .as_deref()
                .and_then(parse_date)
                .is_some_and(|d| d > self.today)
            {
                continue;
            }
            match kid.status {
                Status::InProgress => {
                    let live = kid
                        .claim_expires
                        .as_deref()
                        .is_some_and(|e| e > self.now.as_str());
                    if !live {
                        self.needs_you.push(kid.clone());
                    }
                    // A claimed subtree is hidden from everyone else.
                    continue;
                }
                Status::Waiting => {
                    self.needs_you.push(kid.clone());
                    continue;
                }
                _ => {}
            }
            let is_work_item = kid.kind == Kind::Task && collect_work && !blocked;
            if is_work_item {
                self.work.push(kid.clone());
            }
            self.walk(
                tree,
                kid,
                collect_work && !blocked && kid.kind == Kind::Area,
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::IdArg;
    use crate::store::add;
    use crate::store::add::tests::args;
    use crate::store::test_support::temp_db;

    fn next_args(scope: Option<i64>, limit: Option<u32>) -> NextArgs {
        NextArgs {
            scope: scope.map(IdArg::Number),
            limit,
        }
    }

    #[test]
    fn empty_tree_says_what_to_do() {
        let (conn, dir) = temp_db("next-empty");
        assert_eq!(
            next(&conn, next_args(None, None)).unwrap(),
            "Nothing actionable under SPOKE-1 To do. Add a task with add."
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn lists_work_items_with_next_step_and_respects_order() {
        let (mut conn, dir) = temp_db("next");
        add(&mut conn, args(None, "Home", Kind::Area)).unwrap(); // 2
        let mut project = args(Some(2), "Fix garage", Kind::Task); // 3
        project.ordered = Some(true);
        project.priority = Some(2);
        add(&mut conn, project).unwrap();
        add(&mut conn, args(Some(3), "Buy parts", Kind::Task)).unwrap(); // 4
        add(&mut conn, args(Some(3), "Install", Kind::Task)).unwrap(); // 5
        let mut urgent = args(Some(2), "Pay bill", Kind::Task); // 6
        urgent.priority = Some(1);
        add(&mut conn, urgent).unwrap();
        add(&mut conn, args(Some(2), "Someday", Kind::Task)).unwrap(); // 7

        let out = next(&conn, next_args(None, None)).unwrap();
        assert_eq!(
            out,
            "UP NEXT\n  SPOKE-6  p1  Pay bill  unplanned\n  SPOKE-3  p2  Fix garage (0/2)  next: SPOKE-4 Buy parts\n  SPOKE-7  Someday  unplanned"
        );
        let out = next(&conn, next_args(None, Some(1))).unwrap();
        assert!(
            out.ends_with("(+2 more open. Pass scope or limit.)"),
            "{out}"
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn ordered_area_only_offers_first_step() {
        let (mut conn, dir) = temp_db("next-ordered");
        let mut area = args(None, "Pipeline", Kind::Area);
        area.ordered = Some(true);
        add(&mut conn, area).unwrap(); // 2
        add(&mut conn, args(Some(2), "First", Kind::Task)).unwrap();
        add(&mut conn, args(Some(2), "Second", Kind::Task)).unwrap();
        let out = next(&conn, next_args(Some(2), None)).unwrap();
        assert_eq!(out, "UP NEXT\n  SPOKE-3  First  unplanned");
        let _ = std::fs::remove_dir_all(dir);
    }
}
