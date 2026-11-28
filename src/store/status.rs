//! `set_status`: mark nodes done, dropped or reopened.
//!
//! One rule keeps work findable (SPOKE-345): unfinished work never sits under a done or
//! dropped node. A node closes only when nothing beneath it is unfinished, and reopens
//! only when nothing above it is closed.

use rusqlite::{Connection, params};

use super::{
    ToolResult, ancestors, children, clean_agent, db_err, load, log_event, renew_leases, require,
    touch,
};
use chrono::NaiveDate;

use crate::db::now;
use crate::format::{parse_date, short_date};
use crate::hints;
use crate::model::{Kind, Node, NodeId, Status, id_ranges};
use crate::recur::Rule;
use crate::tools::args::SetStatusArgs;

const MAX_IDS: usize = 100;
/// How many unfinished nodes a refusal names by title; the rest are given as ids only.
const LISTED_OPEN: usize = 10;
/// Titles in that list are cut to this many characters, to keep a refusal short.
const LISTED_TITLE: usize = 60;

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
    // Deepest first when closing and shallowest first when reopening, so the rule is
    // judged against what the whole call leaves behind, whatever order the ids came in.
    let ids: Vec<Result<NodeId, String>> = args.ids.iter().map(|raw| raw.parse()).collect();
    let mut order = Vec::with_capacity(ids.len());
    for (i, id) in ids.iter().enumerate() {
        let depth = match id {
            Ok(id) => ancestors(&tx, *id)?.len(),
            Err(_) => 0,
        };
        order.push((depth, i));
    }
    if target.is_closed() {
        order.sort_by_key(|&(depth, i)| (std::cmp::Reverse(depth), i));
    } else {
        order.sort();
    }
    let mut outcomes = Vec::with_capacity(ids.len());
    for (_, i) in order {
        let id = ids[i].clone();
        outcomes.push((
            i,
            change_one(&tx, id, target, agent.as_deref(), &now, today)?,
        ));
    }
    // The reply lists them as they were asked for.
    outcomes.sort_by_key(|(i, _)| *i);
    for (_, outcome) in outcomes {
        match outcome {
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
    tx.commit().map_err(db_err)?;

    let text = lines.join("\n");
    if failures == args.ids.len() {
        Err(text)
    } else {
        Ok(text)
    }
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

    if target.is_closed() {
        let mut open = Vec::new();
        open_below(conn, id, &mut open)?;
        if !open.is_empty() {
            return Ok(Outcome::Refused(open_below_refusal(&node, target, &open)));
        }
    } else if node.status.is_closed() {
        let closed: Vec<Node> = ancestors(conn, id)?
            .into_iter()
            .filter(|a| a.status.is_closed())
            .collect();
        if !closed.is_empty() {
            return Ok(Outcome::Refused(closed_above_refusal(&node, &closed)));
        }
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

/// Why a node cannot close yet. Every blocking id is given, so the caller can pass them
/// all in one call.
fn open_below_refusal(node: &Node, target: Status, open: &[Node]) -> String {
    let word = if target == Status::Done {
        "marked done"
    } else {
        "dropped"
    };
    let mut named: Vec<String> = open
        .iter()
        .take(LISTED_OPEN)
        .map(|n| format!("{} {}", n.id, short_title(&n.title)))
        .collect();
    // The node itself takes one of the call's ids.
    let fits = open.len() < MAX_IDS;
    if open.len() > LISTED_OPEN {
        let mut rest: Vec<NodeId> = open[LISTED_OPEN..].iter().map(|n| n.id).collect();
        rest.sort_by_key(|id| id.0);
        named.push(if fits {
            format!("+{} more ({})", rest.len(), id_ranges(&rest))
        } else {
            format!(
                "+{} more (search with scope {} lists them)",
                rest.len(),
                node.id
            )
        });
    }
    let how = if fits {
        "Finish, drop or move them first, or pass their ids in this same call.".to_string()
    } else {
        format!(
            "Finish, drop or move them first, deepest ones first: one call takes {MAX_IDS} ids."
        )
    };
    let mut line = format!(
        "{} {} was not {word}: {} unfinished under it: {}. {how}",
        node.id,
        node.title,
        open.len(),
        named.join(", ")
    );
    // Done only reschedules a repeating task, so passing its id would not clear the way.
    if target == Status::Done {
        for n in open.iter().filter(|n| n.recur.is_some()).take(LISTED_OPEN) {
            line.push_str(&format!(
                " {} repeats, so done only schedules it again: drop it or move it out.",
                n.id
            ));
        }
    }
    line
}

fn short_title(title: &str) -> String {
    match title.char_indices().nth(LISTED_TITLE) {
        Some((cut, _)) => format!("{}…", title[..cut].trim_end()),
        None => title.to_string(),
    }
}

/// Why a node cannot reopen yet: it would be open work that no listing shows.
fn closed_above_refusal(node: &Node, closed: &[Node]) -> String {
    let named: Vec<String> = closed
        .iter()
        .map(|a| format!("{} {} {}", a.id, a.title, a.status.as_str()))
        .collect();
    let ids: Vec<String> = closed.iter().map(|a| a.id.to_string()).collect();
    format!(
        "{} {} was not reopened: it is under closed work ({}). Pass {} in this same call to reopen them too, or move {} under unfinished work first.",
        node.id,
        node.title,
        named.join(", "),
        ids.join(", "),
        node.id
    )
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
    // Its finished steps come back with it (a checklist that repeats). A dropped step
    // stays dropped, so nothing beneath it comes back either.
    let reopened = conn
        .execute(
            "WITH RECURSIVE down(id) AS (
               SELECT id FROM nodes WHERE parent_id = ?1 AND status != 'dropped'
               UNION ALL
               SELECT c.id FROM nodes c JOIN down ON c.parent_id = down.id
                WHERE c.status != 'dropped')
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

    /// Runs one call and returns its reply, or the refusal text.
    fn run(conn: &mut Connection, ids: &[&str], status: SettableStatus) -> Result<String, String> {
        set_status(
            conn,
            status_args(ids, status),
            crate::format::today_in(chrono_tz::UTC),
        )
    }

    fn status_of(conn: &Connection, id: i64) -> Status {
        require(conn, NodeId(id)).unwrap().status
    }

    /// Plan (2) > Build (3) > Wire (4), and Plan > Ship (5).
    fn plan(conn: &mut Connection) {
        add(conn, args(None, "Plan", Kind::Task)).unwrap(); // 2
        add(conn, args(Some(2), "Build", Kind::Task)).unwrap(); // 3
        add(conn, args(Some(3), "Wire", Kind::Task)).unwrap(); // 4
        add(conn, args(Some(2), "Ship", Kind::Task)).unwrap(); // 5
    }

    #[test]
    fn refuses_to_close_over_unfinished_work_at_any_depth() {
        let (mut conn, dir) = temp_db("status-open-below");
        plan(&mut conn);
        run(&mut conn, &["5"], SettableStatus::Done).unwrap();
        for (target, word) in [
            (SettableStatus::Done, "not marked done"),
            (SettableStatus::Dropped, "not dropped"),
        ] {
            let err = run(&mut conn, &["2"], target).unwrap_err();
            assert_eq!(
                err,
                format!(
                    "SPOKE-2 Plan was {word}: 2 unfinished under it: SPOKE-3 Build, SPOKE-4 Wire. Finish, drop or move them first, or pass their ids in this same call."
                )
            );
            assert_eq!(status_of(&conn, 2), Status::Open);
        }
        // A closed node in between does not hide the grandchild from the check.
        conn.execute("UPDATE nodes SET status = 'done' WHERE id = 3", [])
            .unwrap();
        let err = run(&mut conn, &["2"], SettableStatus::Done).unwrap_err();
        assert!(
            err.contains("1 unfinished under it: SPOKE-4 Wire."),
            "{err}"
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn closes_a_whole_branch_in_one_call_in_any_order() {
        let (mut conn, dir) = temp_db("status-branch");
        plan(&mut conn);
        let out = run(&mut conn, &["2", "3", "5", "4"], SettableStatus::Done).unwrap();
        // The reply keeps the order the ids were passed in.
        assert_eq!(
            out,
            "SPOKE-2 done  Plan\nSPOKE-3 done  Build\nSPOKE-5 done  Ship\nSPOKE-4 done  Wire"
        );
        let out = run(&mut conn, &["4", "3", "2"], SettableStatus::Open).unwrap();
        assert_eq!(
            out,
            "SPOKE-4 open  Wire\nSPOKE-3 open  Build\nSPOKE-2 open  Plan"
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn a_refused_child_keeps_its_parents_open_and_lets_the_rest_through() {
        let (mut conn, dir) = temp_db("status-partial");
        plan(&mut conn);
        let out = run(&mut conn, &["2", "3", "5"], SettableStatus::Dropped).unwrap();
        let lines: Vec<&str> = out.lines().collect();
        assert!(
            lines[0].starts_with("SPOKE-2 Plan was not dropped: 2 unfinished"),
            "{out}"
        );
        assert!(
            lines[1].starts_with("SPOKE-3 Build was not dropped: 1 unfinished"),
            "{out}"
        );
        assert_eq!(lines[2], "SPOKE-5 dropped  Ship");
        assert_eq!(status_of(&conn, 3), Status::Open);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn claimed_waiting_deferred_and_areas_count_as_unfinished() {
        let (mut conn, dir) = temp_db("status-kinds");
        add(&mut conn, args(None, "Home", Kind::Area)).unwrap(); // 2
        add(&mut conn, args(Some(2), "Garage", Kind::Area)).unwrap(); // 3
        for title in ["Held", "Asked", "Later"] {
            add(&mut conn, args(Some(3), title, Kind::Task)).unwrap(); // 4, 5, 6
        }
        conn.execute_batch(
            "UPDATE nodes SET status = 'in_progress', claimed_by = 'red' WHERE id = 4;
             UPDATE nodes SET status = 'waiting', waiting_on = 'which?' WHERE id = 5;
             UPDATE nodes SET defer_until = '2999-01-01' WHERE id = 6;",
        )
        .unwrap();
        let err = run(&mut conn, &["2"], SettableStatus::Dropped).unwrap_err();
        assert!(
            err.contains(
                "4 unfinished under it: SPOKE-3 Garage, SPOKE-4 Held, SPOKE-5 Asked, SPOKE-6 Later."
            ),
            "{err}"
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn a_long_refusal_still_gives_every_id() {
        let (mut conn, dir) = temp_db("status-long");
        add(&mut conn, args(None, "Big", Kind::Task)).unwrap(); // 2
        for i in 0..13 {
            add(&mut conn, args(Some(2), &format!("S{i}"), Kind::Task)).unwrap(); // 3..=15
        }
        let err = run(&mut conn, &["2"], SettableStatus::Done).unwrap_err();
        assert!(
            err.contains("13 unfinished under it: SPOKE-3 S0, "),
            "{err}"
        );
        assert!(
            err.contains("SPOKE-12 S9, +3 more (SPOKE-13..SPOKE-15)."),
            "{err}"
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn a_refusal_too_big_for_one_call_says_so_and_cuts_long_titles() {
        let (mut conn, dir) = temp_db("status-huge");
        add(&mut conn, args(None, "Huge", Kind::Task)).unwrap(); // 2
        let long = "é".repeat(80);
        add(&mut conn, args(Some(2), &long, Kind::Task)).unwrap(); // 3
        for i in 0..99 {
            add(&mut conn, args(Some(2), &format!("S{i}"), Kind::Task)).unwrap();
        }
        let err = run(&mut conn, &["2"], SettableStatus::Done).unwrap_err();
        assert!(
            err.contains(&format!("SPOKE-3 {}…, SPOKE-4 S0", "é".repeat(60))),
            "{err}"
        );
        assert!(
            err.ends_with("+90 more (search with scope SPOKE-2 lists them). Finish, drop or move them first, deepest ones first: one call takes 100 ids."),
            "{err}"
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn refuses_to_reopen_under_closed_work() {
        let (mut conn, dir) = temp_db("status-reopen");
        plan(&mut conn);
        run(&mut conn, &["2", "3", "4", "5"], SettableStatus::Done).unwrap();
        let err = run(&mut conn, &["4"], SettableStatus::Open).unwrap_err();
        assert_eq!(
            err,
            "SPOKE-4 Wire was not reopened: it is under closed work (SPOKE-2 Plan done, SPOKE-3 Build done). Pass SPOKE-2, SPOKE-3 in this same call to reopen them too, or move SPOKE-4 under unfinished work first."
        );
        assert_eq!(status_of(&conn, 4), Status::Done);
        // Releasing a claim on a node already under closed work is not a reopen.
        conn.execute(
            "UPDATE nodes SET status = 'in_progress', claimed_by = 'red' WHERE id = 4",
            [],
        )
        .unwrap();
        run(&mut conn, &["4"], SettableStatus::Open).unwrap();
        assert_eq!(status_of(&conn, 4), Status::Open);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn a_repeating_task_waits_for_its_steps_and_leaves_dropped_branches_closed() {
        let (mut conn, dir) = temp_db("status-recur");
        let mut chore = args(None, "Weekly", Kind::Task); // 2
        chore.node.recur = Some(crate::recur::RecurArg::Text("weekly".into()));
        add(&mut conn, chore).unwrap();
        add(&mut conn, args(Some(2), "Sweep", Kind::Task)).unwrap(); // 3
        add(&mut conn, args(Some(2), "Skip", Kind::Task)).unwrap(); // 4
        add(&mut conn, args(Some(4), "Inside", Kind::Task)).unwrap(); // 5
        let err = run(&mut conn, &["2"], SettableStatus::Done).unwrap_err();
        assert!(
            err.starts_with("SPOKE-2 Weekly was not marked done: 3 unfinished"),
            "{err}"
        );
        run(&mut conn, &["5"], SettableStatus::Done).unwrap();
        run(&mut conn, &["4"], SettableStatus::Dropped).unwrap();
        run(&mut conn, &["3"], SettableStatus::Done).unwrap();
        run(&mut conn, &["2"], SettableStatus::Done).unwrap();
        assert_eq!(status_of(&conn, 2), Status::Open);
        assert_eq!(status_of(&conn, 3), Status::Open);
        assert_eq!(status_of(&conn, 4), Status::Dropped);
        assert_eq!(status_of(&conn, 5), Status::Done);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn a_repeating_child_is_named_when_it_blocks_done() {
        let (mut conn, dir) = temp_db("status-recur-child");
        add(&mut conn, args(None, "Project", Kind::Task)).unwrap(); // 2
        let mut chore = args(Some(2), "Water", Kind::Task); // 3
        chore.node.recur = Some(crate::recur::RecurArg::Text("weekly".into()));
        add(&mut conn, chore).unwrap();
        // The child is only rescheduled, so the parent stays open and the reply says why.
        let err = run(&mut conn, &["2", "3"], SettableStatus::Done).unwrap();
        assert!(
            err.contains(
                "SPOKE-3 repeats, so done only schedules it again: drop it or move it out."
            ),
            "{err}"
        );
        run(&mut conn, &["2", "3"], SettableStatus::Dropped).unwrap();
        let _ = std::fs::remove_dir_all(dir);
    }
}
