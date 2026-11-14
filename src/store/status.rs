//! `set_status`: mark nodes done, dropped or reopened.

use rusqlite::{Connection, params};

use super::{ToolResult, children, clean_agent, db_err, load, log_event, require, touch};
use crate::db::now;
use crate::model::{Kind, Node, NodeId, Status};
use crate::tools::args::SetStatusArgs;

const MAX_IDS: usize = 100;

pub fn set_status(conn: &mut Connection, args: SetStatusArgs) -> ToolResult {
    if args.ids.is_empty() {
        return Err("ids is empty. Pass one or more ids, e.g. [\"SPOKE-12\"].".to_string());
    }
    if args.ids.len() > MAX_IDS {
        return Err(format!("pass at most {MAX_IDS} ids per call."));
    }
    let target: Status = args.status.into();
    let agent = clean_agent(args.agent.as_deref())?;
    let now = now();

    let tx = conn.transaction().map_err(db_err)?;
    let mut lines = Vec::new();
    let mut changed = Vec::new();
    let mut failures = 0;
    for raw in &args.ids {
        match change_one(&tx, raw.parse(), target, agent.as_deref(), &now)? {
            Outcome::Changed(node) => {
                let mut line = format!("{} {}  {}", node.id, target.as_str(), node.title);
                if target.is_closed() && node.focus_rank.is_some() {
                    line.push_str("  (taken off the focus list)");
                }
                lines.push(line);
                changed.push(node);
            }
            Outcome::Unchanged(line) => lines.push(line),
            Outcome::Refused(line) => {
                failures += 1;
                lines.push(line);
            }
        }
    }

    // Hints about parents: the next step of an ordered parent, or that all are finished.
    if target.is_closed() {
        let mut hinted: Vec<NodeId> = Vec::new();
        for node in &changed {
            let Some(pid) = node.parent_id else { continue };
            if hinted.contains(&pid) {
                continue;
            }
            hinted.push(pid);
            let Some(parent) = load(&tx, pid)? else {
                continue;
            };
            if parent.kind != Kind::Task || parent.status.is_closed() {
                continue;
            }
            if parent.child_total > 0 && parent.child_done == parent.child_total {
                lines.push(format!(
                    "All steps of {} {} are done. If it is finished too, mark it done with set_status.",
                    parent.id, parent.title
                ));
            } else if parent.ordered
                && let Some(step) = children(&tx, pid)?
                    .into_iter()
                    .find(|c| !c.status.is_closed())
            {
                lines.push(format!(
                    "Next step of {} {}: {} {}",
                    parent.id, parent.title, step.id, step.title
                ));
            }
        }
    }
    tx.commit().map_err(db_err)?;

    let text = lines.join("\n");
    if failures == args.ids.len() {
        Err(text)
    } else {
        Ok(text)
    }
}

enum Outcome {
    Changed(Box<Node>),
    Unchanged(String),
    Refused(String),
}

fn change_one(
    conn: &Connection,
    id: Result<NodeId, String>,
    target: Status,
    agent: Option<&str>,
    now: &str,
) -> Result<Outcome, String> {
    let id = match id {
        Ok(id) => id,
        Err(e) => return Ok(Outcome::Refused(e)),
    };
    let node = match require(conn, id) {
        Ok(n) => n,
        Err(e) => return Ok(Outcome::Refused(e)),
    };
    if node.parent_id.is_none() {
        return Ok(Outcome::Refused(format!(
            "{id} is the root and always stays open."
        )));
    }
    if node.kind == Kind::Area && target == Status::Done {
        return Ok(Outcome::Refused(format!(
            "{id} {} is an area; areas never finish. Use dropped to retire it.",
            node.title
        )));
    }
    if node.status == target {
        return Ok(Outcome::Unchanged(format!(
            "{id} is already {}  {}",
            target.as_str(),
            node.title
        )));
    }

    let completed_at = (target == Status::Done).then_some(now);
    // Any of these endings clears a claim and an open question; closing a node also
    // takes it off the focus list, which keeps that list short by itself.
    conn.execute(
        "UPDATE nodes SET status = ?1, completed_at = ?2, claimed_by = NULL, claim_expires = NULL,
                          waiting_on = NULL, updated_at = ?3,
                          focus_rank = CASE WHEN ?1 IN ('done', 'dropped') THEN NULL ELSE focus_rank END
         WHERE id = ?4",
        params![target.as_str(), completed_at, now, id.0],
    )
    .map_err(db_err)?;
    let detail = format!("{} -> {}", node.status.as_str(), target.as_str());
    log_event(conn, id, now, agent, "status", &detail).map_err(db_err)?;
    touch(conn, id, now).map_err(db_err)?;
    Ok(Outcome::Changed(Box::new(node)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{IdArg, SettableStatus};
    use crate::store::add;
    use crate::store::add::tests::args;
    use crate::store::test_support::temp_db;

    fn status_args(ids: &[&str], status: SettableStatus) -> SetStatusArgs {
        SetStatusArgs {
            ids: ids.iter().map(|s| IdArg::Text((*s).to_string())).collect(),
            status,
            agent: Some("test".into()),
        }
    }

    #[test]
    fn done_then_reopen_logs_events_and_hints_parent() {
        let (mut conn, dir) = temp_db("status");
        add(&mut conn, args(None, "Project", Kind::Task)).unwrap(); // 2
        add(&mut conn, args(Some(2), "Step", Kind::Task)).unwrap(); // 3
        let out = set_status(&mut conn, status_args(&["SPOKE-3"], SettableStatus::Done)).unwrap();
        assert_eq!(
            out,
            "SPOKE-3 done  Step\nAll steps of SPOKE-2 Project are done. If it is finished too, mark it done with set_status."
        );
        let out = set_status(&mut conn, status_args(&["3"], SettableStatus::Done)).unwrap();
        assert_eq!(out, "SPOKE-3 is already done  Step");
        set_status(&mut conn, status_args(&["SPOKE-3"], SettableStatus::Open)).unwrap();
        let events: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM events WHERE node_id = 3 AND type = 'status'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(events, 2);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn refuses_areas_root_and_missing_ids() {
        let (mut conn, dir) = temp_db("status-bad");
        add(&mut conn, args(None, "Home", Kind::Area)).unwrap(); // 2
        add(&mut conn, args(None, "Task", Kind::Task)).unwrap(); // 3
        let err = set_status(
            &mut conn,
            status_args(
                &["SPOKE-2", "SPOKE-1", "SPOKE-99", "nope"],
                SettableStatus::Done,
            ),
        )
        .unwrap_err();
        assert!(err.contains("areas never finish"), "{err}");
        assert!(err.contains("root"), "{err}");
        assert!(err.contains("SPOKE-99 does not exist"), "{err}");
        assert!(err.contains("not a node id"), "{err}");
        // Partial success is not an error.
        let out = set_status(
            &mut conn,
            status_args(&["SPOKE-3", "SPOKE-99"], SettableStatus::Dropped),
        )
        .unwrap();
        assert!(out.starts_with("SPOKE-3 dropped  Task"), "{out}");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn ordered_parent_names_the_next_step() {
        let (mut conn, dir) = temp_db("status-next");
        let mut p = args(None, "Plan", Kind::Task); // 2
        p.node.ordered = Some(true);
        add(&mut conn, p).unwrap();
        for t in ["One", "Two", "Three"] {
            add(&mut conn, args(Some(2), t, Kind::Task)).unwrap(); // 3, 4, 5
        }
        let out = set_status(&mut conn, status_args(&["SPOKE-3"], SettableStatus::Done)).unwrap();
        assert_eq!(
            out,
            "SPOKE-3 done  One\nNext step of SPOKE-2 Plan: SPOKE-4 Two"
        );
        let out = set_status(&mut conn, status_args(&["4", "5"], SettableStatus::Done)).unwrap();
        assert!(out.ends_with("All steps of SPOKE-2 Plan are done. If it is finished too, mark it done with set_status."), "{out}");
        let _ = std::fs::remove_dir_all(dir);
    }
}
