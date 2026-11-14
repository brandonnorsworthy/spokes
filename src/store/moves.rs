//! `move`: reparent a node and/or reorder it among its siblings.

use rusqlite::{Connection, params};

use super::{
    ToolResult, ancestors, children, clean_agent, db_err, log_event, renew_leases, require, touch,
};
use crate::db::{now, position};
use crate::model::{Kind, Node, Status};
use crate::tools::args::MoveArgs;

pub fn move_node(conn: &mut Connection, args: MoveArgs) -> ToolResult {
    let id = args.id.parse()?;
    let agent = clean_agent(args.agent.as_deref())?;
    let parent_arg = args.parent.as_ref().map(|p| p.parse()).transpose()?;
    let before = args.before.as_ref().map(|p| p.parse()).transpose()?;
    let after = args.after.as_ref().map(|p| p.parse()).transpose()?;
    if before.is_some() && after.is_some() {
        return Err("pass before or after, not both.".to_string());
    }
    if parent_arg.is_none() && before.is_none() && after.is_none() {
        return Err(
            "nothing to do. Pass parent (new parent), and/or before or after (a sibling to sit next to)."
                .to_string(),
        );
    }

    let tx = conn.transaction().map_err(db_err)?;
    let node = require(&tx, id)?;
    let Some(old_parent) = node.parent_id else {
        return Err(format!("{id} is the root; it cannot move."));
    };
    let anchor = match before.or(after) {
        Some(a) if a == id => {
            return Err(format!(
                "{id} cannot be placed before or after itself. Name a sibling."
            ));
        }
        Some(a) => Some(require(&tx, a)?),
        None => None,
    };
    let target_id = match (parent_arg, &anchor) {
        (Some(p), _) => p,
        (None, Some(a)) => a
            .parent_id
            .ok_or_else(|| format!("{} is the root and has no siblings. Name a sibling.", a.id))?,
        (None, None) => old_parent,
    };
    if let Some(a) = &anchor
        && a.parent_id != Some(target_id)
    {
        return Err(format!(
            "{} {} is not under {target_id}, so {id} cannot sit next to it there. Name a child of {target_id}, or omit parent.",
            a.id, a.title
        ));
    }

    let target = require(&tx, target_id)?;
    if target.id == id {
        return Err(format!(
            "{id} cannot be its own parent. Pick a different parent."
        ));
    }
    if ancestors(&tx, target.id)?.iter().any(|a| a.id == id) {
        return Err(format!(
            "{} {} is inside {id} {}; a node cannot move under itself. Pick a parent outside it.",
            target.id, target.title, node.title
        ));
    }
    if target.status.is_closed() {
        return Err(format!(
            "{} {} is {}. Reopen it with set_status, or pick another parent.",
            target.id,
            target.title,
            target.status.as_str()
        ));
    }
    if node.kind == Kind::Area && target.kind == Kind::Task {
        return Err(format!(
            "{} {} is a task; areas can only go under areas. Pick an area, or make {id} a task with update first.",
            target.id, target.title
        ));
    }

    let siblings: Vec<Node> = children(&tx, target.id)?
        .into_iter()
        .filter(|s| s.id != id)
        .collect();
    let pos = match (&anchor, before.is_some()) {
        (Some(a), true) => {
            let i = siblings.iter().position(|s| s.id == a.id).unwrap_or(0);
            let prev = i.checked_sub(1).map(|j| siblings[j].position.as_str());
            position::between(prev, Some(&a.position))
        }
        (Some(a), false) => {
            let i = siblings.iter().position(|s| s.id == a.id).unwrap_or(0);
            let next = siblings.get(i + 1).map(|s| s.position.as_str());
            position::between(Some(&a.position), next)
        }
        (None, _) => match siblings.last() {
            Some(last) => position::after(&last.position),
            None => position::first(),
        },
    };

    let now = now();
    tx.execute(
        "UPDATE nodes SET parent_id = ?1, position = ?2, updated_at = ?3 WHERE id = ?4",
        params![target.id.0, pos, now, id.0],
    )
    .map_err(db_err)?;
    let detail = if old_parent == target.id {
        format!("reordered under {}", target.id)
    } else {
        format!("{old_parent} -> {}", target.id)
    };
    log_event(&tx, id, &now, agent.as_deref(), "moved", &detail).map_err(db_err)?;
    touch(&tx, old_parent, &now).map_err(db_err)?;
    touch(&tx, id, &now).map_err(db_err)?;
    renew_leases(&tx, id, agent.as_deref(), &now).map_err(db_err)?;

    // Where it landed, counted like the outline (dropped siblings are hidden there).
    let live: Vec<Node> = children(&tx, target.id)?
        .into_iter()
        .filter(|s| s.status != Status::Dropped || s.id == id)
        .collect();
    let place = live.iter().position(|s| s.id == id).map_or(0, |i| i + 1);
    tx.commit().map_err(db_err)?;
    Ok(format!(
        "Moved {id}  {}  (under {} {}, {place} of {})",
        node.title,
        target.id,
        target.title,
        live.len()
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{IdArg, NodeId, SettableStatus};
    use crate::store::add::tests::args;
    use crate::store::test_support::temp_db;
    use crate::store::{add, set_status};
    use crate::tools::args::SetStatusArgs;
    use serde_json::json;

    fn mv(conn: &mut Connection, v: serde_json::Value) -> ToolResult {
        move_node(conn, serde_json::from_value(v).unwrap())
    }

    fn order(conn: &Connection, parent: i64) -> Vec<i64> {
        children(conn, NodeId(parent))
            .unwrap()
            .iter()
            .map(|n| n.id.0)
            .collect()
    }

    fn close(conn: &mut Connection, id: &str, status: SettableStatus) {
        set_status(
            conn,
            SetStatusArgs {
                ids: vec![IdArg::Text(id.to_string())],
                status,
                agent: None,
            },
        )
        .unwrap();
    }

    /// Task 2 "Project" with step tasks 3, 4, 5, 6 (A, B, C, D) in that order.
    fn four_siblings(name: &str) -> (Connection, std::path::PathBuf) {
        let (mut conn, dir) = temp_db(name);
        add(&mut conn, args(None, "Project", Kind::Task)).unwrap();
        for t in ["A", "B", "C", "D"] {
            add(&mut conn, args(Some(2), t, Kind::Task)).unwrap();
        }
        (conn, dir)
    }

    #[test]
    fn reorders_before_and_after_siblings() {
        let (mut conn, dir) = four_siblings("mv-reorder");
        assert_eq!(order(&conn, 2), [3, 4, 5, 6]);
        // Parent omitted: inferred from the anchor.
        let out = mv(&mut conn, json!({"id": "SPOKE-5", "before": "SPOKE-4"})).unwrap();
        assert_eq!(out, "Moved SPOKE-5  C  (under SPOKE-2 Project, 2 of 4)");
        assert_eq!(order(&conn, 2), [3, 5, 4, 6]);
        // Before the first.
        let out = mv(&mut conn, json!({"id": 6, "before": 3})).unwrap();
        assert_eq!(out, "Moved SPOKE-6  D  (under SPOKE-2 Project, 1 of 4)");
        assert_eq!(order(&conn, 2), [6, 3, 5, 4]);
        // After the last.
        let out = mv(&mut conn, json!({"id": 6, "after": 4})).unwrap();
        assert_eq!(out, "Moved SPOKE-6  D  (under SPOKE-2 Project, 4 of 4)");
        assert_eq!(order(&conn, 2), [3, 5, 4, 6]);
        // After a middle sibling.
        mv(&mut conn, json!({"id": 3, "after": 5})).unwrap();
        assert_eq!(order(&conn, 2), [5, 3, 4, 6]);
        // Same parent given explicitly, no anchor: goes last.
        mv(&mut conn, json!({"id": 5, "parent": 2})).unwrap();
        assert_eq!(order(&conn, 2), [3, 4, 6, 5]);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn reparents_keeping_id_and_children() {
        let (mut conn, dir) = temp_db("mv-reparent");
        add(&mut conn, args(None, "Home", Kind::Area)).unwrap(); // 2
        add(&mut conn, args(None, "Work", Kind::Area)).unwrap(); // 3
        add(&mut conn, args(Some(3), "W1", Kind::Task)).unwrap(); // 4
        add(&mut conn, args(Some(3), "W2", Kind::Task)).unwrap(); // 5
        add(&mut conn, args(Some(2), "Chores", Kind::Task)).unwrap(); // 6
        add(&mut conn, args(Some(6), "Sweep", Kind::Task)).unwrap(); // 7

        let out = mv(&mut conn, json!({"id": 6, "parent": 3})).unwrap();
        assert_eq!(out, "Moved SPOKE-6  Chores  (under SPOKE-3 Work, 3 of 3)");
        assert_eq!(order(&conn, 3), [4, 5, 6]);
        assert_eq!(order(&conn, 2), Vec::<i64>::new());
        assert_eq!(order(&conn, 6), [7]);

        let out = mv(&mut conn, json!({"id": 6, "parent": "SPOKE-2"})).unwrap();
        assert_eq!(out, "Moved SPOKE-6  Chores  (under SPOKE-2 Home, 1 of 1)");

        let out = mv(&mut conn, json!({"id": 6, "parent": 3, "after": 4})).unwrap();
        assert_eq!(out, "Moved SPOKE-6  Chores  (under SPOKE-3 Work, 2 of 3)");
        assert_eq!(order(&conn, 3), [4, 6, 5]);
        assert_eq!(order(&conn, 6), [7]);

        // An area can move under another area, and under the root.
        mv(&mut conn, json!({"id": 3, "parent": 2})).unwrap();
        assert_eq!(order(&conn, 2), [3]);
        mv(&mut conn, json!({"id": 3, "parent": 1, "before": 2})).unwrap();
        assert_eq!(order(&conn, 1), [3, 2]);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn repeated_moves_keep_strict_order() {
        let (mut conn, dir) = temp_db("mv-fractional");
        add(&mut conn, args(None, "P", Kind::Task)).unwrap(); // 2
        for t in ["A", "B", "C"] {
            add(&mut conn, args(Some(2), t, Kind::Task)).unwrap(); // 3,4,5
        }
        // Keep squeezing C between A and B, then A between those, alternating.
        for i in 0..60 {
            if i % 2 == 0 {
                mv(&mut conn, json!({"id": 5, "after": 3})).unwrap();
                assert_eq!(order(&conn, 2), [3, 5, 4]);
                mv(&mut conn, json!({"id": 4, "before": 5})).unwrap();
                assert_eq!(order(&conn, 2), [3, 4, 5]);
            } else {
                mv(&mut conn, json!({"id": 5, "before": 4})).unwrap();
                assert_eq!(order(&conn, 2), [3, 5, 4]);
                mv(&mut conn, json!({"id": 5, "after": 3})).unwrap();
                mv(&mut conn, json!({"id": 4, "after": 5})).unwrap();
                assert_eq!(order(&conn, 2), [3, 5, 4]);
                mv(&mut conn, json!({"id": 4, "before": 5})).unwrap();
            }
            let positions: Vec<String> = children(&conn, NodeId(2))
                .unwrap()
                .into_iter()
                .map(|n| n.position)
                .collect();
            assert!(positions.windows(2).all(|w| w[0] < w[1]), "{positions:?}");
        }
        // Repeatedly wedge a node between the same two neighbours.
        for _ in 0..40 {
            mv(&mut conn, json!({"id": 3, "after": 4})).unwrap();
            mv(&mut conn, json!({"id": 3, "before": 4})).unwrap();
        }
        let kids = children(&conn, NodeId(2)).unwrap();
        assert!(kids.windows(2).all(|w| w[0].position < w[1].position));
        assert_eq!(kids[0].id.0, 3);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn logs_moved_event() {
        let (mut conn, dir) = four_siblings("mv-event");
        mv(&mut conn, json!({"id": 6, "before": 3, "agent": "tester"})).unwrap();
        let (count, agent, detail): (i64, String, String) = conn
            .query_row(
                "SELECT COUNT(*), MAX(agent), MAX(detail) FROM events WHERE node_id = 6 AND type = 'moved'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .unwrap();
        assert_eq!(
            (count, agent.as_str(), detail.as_str()),
            (1, "tester", "reordered under SPOKE-2")
        );
        add(&mut conn, args(None, "Other", Kind::Task)).unwrap(); // 7
        mv(&mut conn, json!({"id": 6, "parent": 7})).unwrap();
        let detail: String = conn
            .query_row(
                "SELECT detail FROM events WHERE node_id = 6 AND type = 'moved' ORDER BY id DESC LIMIT 1",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(detail, "SPOKE-2 -> SPOKE-7");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn dropped_siblings_are_not_counted_in_place() {
        let (mut conn, dir) = four_siblings("mv-dropped");
        close(&mut conn, "SPOKE-4", SettableStatus::Dropped);
        let out = mv(&mut conn, json!({"id": 3, "after": 6})).unwrap();
        assert_eq!(out, "Moved SPOKE-3  A  (under SPOKE-2 Project, 3 of 3)");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn rejects_invalid_moves_with_guidance() {
        let (mut conn, dir) = four_siblings("mv-errors");
        add(&mut conn, args(None, "Home", Kind::Area)).unwrap(); // 7
        add(&mut conn, args(Some(5), "Sub", Kind::Task)).unwrap(); // 8 under C
        add(&mut conn, args(Some(8), "Deep", Kind::Task)).unwrap(); // 9
        add(&mut conn, args(None, "Finished", Kind::Task)).unwrap(); // 10
        add(&mut conn, args(None, "Gone", Kind::Task)).unwrap(); // 11
        close(&mut conn, "SPOKE-10", SettableStatus::Done);
        close(&mut conn, "SPOKE-11", SettableStatus::Dropped);
        let before = order(&conn, 2);

        let err = |conn: &mut Connection, v: serde_json::Value| mv(conn, v).unwrap_err();

        assert!(err(&mut conn, json!({"id": 1, "parent": 7})).contains("is the root"));
        assert!(err(&mut conn, json!({"id": 1, "before": 7})).contains("is the root"));
        assert!(err(&mut conn, json!({"id": 5, "parent": 5})).contains("its own parent"));
        assert!(err(&mut conn, json!({"id": 5, "parent": 9})).contains("cannot move under itself"));
        assert!(
            err(&mut conn, json!({"id": 7, "parent": 2})).contains("areas can only go under areas")
        );
        assert!(err(&mut conn, json!({"id": 4, "parent": 10})).contains("is done"));
        assert!(err(&mut conn, json!({"id": 4, "parent": 11})).contains("is dropped"));
        assert!(err(&mut conn, json!({"id": 4, "before": 3, "after": 5})).contains("not both"));
        assert!(err(&mut conn, json!({"id": 4})).contains("nothing to do"));
        assert!(
            err(&mut conn, json!({"id": 4, "parent": 7, "before": 3}))
                .contains("is not under SPOKE-7")
        );
        assert!(
            err(&mut conn, json!({"id": 4, "after": 4}))
                .contains("cannot be placed before or after itself")
        );
        assert!(err(&mut conn, json!({"id": 99, "parent": 2})).contains("SPOKE-99 does not exist"));
        assert!(err(&mut conn, json!({"id": 4, "parent": 99})).contains("SPOKE-99 does not exist"));
        assert!(err(&mut conn, json!({"id": 4, "after": 99})).contains("SPOKE-99 does not exist"));
        assert!(err(&mut conn, json!({"id": "bogus", "parent": 2})).contains("bogus"));
        // Nothing changed.
        assert_eq!(order(&conn, 2), before);
        let _ = std::fs::remove_dir_all(dir);
    }
}
