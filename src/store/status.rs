//! `set_status`: mark nodes done, dropped or reopened.

use rusqlite::{Connection, params};

use super::{
    ToolResult, ancestors, children, clean_agent, db_err, load, log_event, renew_leases, require,
    touch,
};
use chrono::NaiveDate;

use crate::db::now;
use crate::format::{parse_date, short_date};
use crate::hints;
use crate::model::{Kind, Node, NodeId, Status};
use crate::recur::Rule;
use crate::tools::args::SetStatusArgs;

const MAX_IDS: usize = 100;

pub fn set_status(conn: &mut Connection, args: SetStatusArgs, today: NaiveDate) -> ToolResult {
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
        match change_one(&tx, raw.parse(), target, agent.as_deref(), &now, today)? {
            Outcome::Changed(node) => {
                let mut line = format!("{} {}  {}", node.id, target.as_str(), node.title);
                if target.is_closed() && node.focus_rank.is_some() {
                    line.push_str("  (taken off the focus list)");
                }
                if target == Status::Dropped && node.recur.is_some() {
                    line.push_str("  (it stops repeating)");
                }
                lines.push(line);
                changed.push(node);
            }
            Outcome::Unchanged(line) | Outcome::Recurred(line) => lines.push(line),
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
            let next_step = if parent.ordered {
                children(&tx, pid)?
                    .into_iter()
                    .find(|c| !c.status.is_closed())
            } else {
                None
            };
            let action = hints::Action::StepClosed {
                parent: &parent,
                next_step: next_step.as_ref(),
            };
            if let Some(line) = hints::hint(&action) {
                lines.push(line);
            }
        }
    }
    if target == Status::Dropped
        && let Some(line) = dropped_hint(&tx, &changed)?
    {
        lines.push(line);
    }
    tx.commit().map_err(db_err)?;

    let text = lines.join("\n");
    if failures == args.ids.len() {
        Err(text)
    } else {
        Ok(text)
    }
}

/// Unfinished work left under the nodes this call dropped (SPOKE-1079). Read after every
/// id has changed, so a child dropped in the same call is not named; a node dropped
/// beneath another one in the call is covered by that one's entry.
fn dropped_hint(conn: &Connection, dropped: &[Box<Node>]) -> Result<Option<String>, String> {
    let mut leftovers = Vec::new();
    for node in dropped {
        if ancestors(conn, node.id)?
            .iter()
            .any(|a| dropped.iter().any(|d| d.id == a.id))
        {
            continue;
        }
        let mut open = Vec::new();
        open_below(conn, node.id, &mut open)?;
        if !open.is_empty() {
            leftovers.push(((**node).clone(), open));
        }
    }
    let action = hints::Action::DroppedWithOpen {
        leftovers: &leftovers,
    };
    Ok(hints::hint(&action))
}

/// Every unfinished node beneath `id`, in tree order, looking through closed ones too.
fn open_below(conn: &Connection, id: NodeId, out: &mut Vec<Node>) -> Result<(), String> {
    for child in children(conn, id)? {
        let child_id = child.id;
        if !child.status.is_closed() {
            out.push(child);
        }
        open_below(conn, child_id, out)?;
    }
    Ok(())
}

enum Outcome {
    Changed(Box<Node>),
    /// A recurring task marked done: reset in place, with a reply line.
    Recurred(String),
    Unchanged(String),
    Refused(String),
}

fn change_one(
    conn: &Connection,
    id: Result<NodeId, String>,
    target: Status,
    agent: Option<&str>,
    now: &str,
    today: NaiveDate,
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

    if target == Status::Done
        && node.kind == Kind::Task
        && let Some(rule) = node.recur.as_deref().and_then(Rule::from_json)
    {
        return recur(conn, &node, rule, agent, now, today).map(Outcome::Recurred);
    }

    let completed_at = (target == Status::Done).then_some(now);
    // Any of these endings clears a claim and an open question; closing a node also
    // takes it off the focus list, which keeps that list short by itself.
    conn.execute(
        "UPDATE nodes SET status = ?1, completed_at = ?2, claimed_by = NULL, claim_expires = NULL,
                          waiting_on = NULL, answered_at = NULL, updated_at = ?3,
                          focus_rank = CASE WHEN ?1 IN ('done', 'dropped') THEN NULL ELSE focus_rank END
         WHERE id = ?4",
        params![target.as_str(), completed_at, now, id.0],
    )
    .map_err(db_err)?;
    let detail = format!("{} -> {}", node.status.as_str(), target.as_str());
    log_event(conn, id, now, agent, "status", &detail).map_err(db_err)?;
    touch(conn, id, now).map_err(db_err)?;
    renew_leases(conn, id, agent, now).map_err(db_err)?;
    Ok(Outcome::Changed(Box::new(node)))
}

/// Resets a recurring task in place: logs `recurred`, reopens it with the next due
/// date and hides it until shortly before then; clears any claim, question or answer
/// mark; reopens its finished steps. The id and focus place stay.
fn recur(
    conn: &Connection,
    node: &Node,
    rule: Rule,
    agent: Option<&str>,
    now: &str,
    today: NaiveDate,
) -> Result<String, String> {
    let id = node.id;
    let occ = rule.next(node.due_at.as_deref().and_then(parse_date), today);
    let due = occ.due.format("%Y-%m-%d").to_string();
    let defer = occ.defer_until.map(|d| d.format("%Y-%m-%d").to_string());
    conn.execute(
        "UPDATE nodes SET status = 'open', completed_at = ?1, claimed_by = NULL, claim_expires = NULL,
                          waiting_on = NULL, answered_at = NULL, updated_at = ?1,
                          due_at = ?2, defer_until = ?3, recur = ?4
         WHERE id = ?5",
        params![now, due, defer, occ.rule.to_json(), id.0],
    )
    .map_err(db_err)?;
    // Its finished steps come back with it (a checklist that repeats).
    let reopened = conn
        .execute(
            "WITH RECURSIVE down(id) AS (
               SELECT id FROM nodes WHERE parent_id = ?1
               UNION ALL
               SELECT c.id FROM nodes c JOIN down ON c.parent_id = down.id)
             UPDATE nodes SET status = 'open', completed_at = NULL, updated_at = ?2
             WHERE id IN (SELECT id FROM down) AND status = 'done'",
            params![id.0, now],
        )
        .map_err(db_err)?;
    let mut detail = format!("{} -> done -> open; next due {due}", node.status.as_str());
    if let Some(d) = &defer {
        detail.push_str(&format!(", hidden until {d}"));
    }
    if reopened > 0 {
        detail.push_str(&format!(", {reopened} steps reopened"));
    }
    log_event(conn, id, now, agent, "recurred", &detail).map_err(db_err)?;
    touch(conn, id, now).map_err(db_err)?;
    renew_leases(conn, id, agent, now).map_err(db_err)?;

    let when = match occ.defer_until {
        Some(d) => format!("back in the queue {}", short_date(&d.to_string(), today)),
        None => "open again now".to_string(),
    };
    let mut line = format!(
        "{id} done  {}  (repeats {}: {when}, due {})",
        node.title,
        rule.describe(),
        short_date(&due, today)
    );
    if reopened > 0 {
        line.push_str(&format!(". Its {reopened} finished steps were reopened"));
    }
    Ok(line)
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
        let out = set_status(
            &mut conn,
            status_args(&["SPOKE-3"], SettableStatus::Done),
            crate::format::today_in(chrono_tz::UTC),
        )
        .unwrap();
        assert_eq!(
            out,
            "SPOKE-3 done  Step\nNext: All steps of SPOKE-2 Project are done. If it is finished too, mark it done with set_status."
        );
        let out = set_status(
            &mut conn,
            status_args(&["3"], SettableStatus::Done),
            crate::format::today_in(chrono_tz::UTC),
        )
        .unwrap();
        assert_eq!(out, "SPOKE-3 is already done  Step");
        set_status(
            &mut conn,
            status_args(&["SPOKE-3"], SettableStatus::Open),
            crate::format::today_in(chrono_tz::UTC),
        )
        .unwrap();
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
            crate::format::today_in(chrono_tz::UTC),
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
            crate::format::today_in(chrono_tz::UTC),
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
        let out = set_status(
            &mut conn,
            status_args(&["SPOKE-3"], SettableStatus::Done),
            crate::format::today_in(chrono_tz::UTC),
        )
        .unwrap();
        assert_eq!(
            out,
            "SPOKE-3 done  One\nNext: SPOKE-4 Two, the next step of SPOKE-2 Plan. Claim it before starting."
        );
        let out = set_status(
            &mut conn,
            status_args(&["4", "5"], SettableStatus::Done),
            crate::format::today_in(chrono_tz::UTC),
        )
        .unwrap();
        assert!(out.ends_with("Next: All steps of SPOKE-2 Plan are done. If it is finished too, mark it done with set_status."), "{out}");
        let _ = std::fs::remove_dir_all(dir);
    }
}
