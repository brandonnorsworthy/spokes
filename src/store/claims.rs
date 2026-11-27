//! `claim`, `release`, `ask` and `answer`: how agents take work, give it back, and
//! hand a question to the human.
//!
//! A claim is `status = in_progress` plus `claimed_by` and a lease (`claim_expires`).
//! The check and the write are one `UPDATE ... WHERE`, so two agents cannot both win.
//! A claimed node and its subtree are hidden from `next`; a lapsed lease shows there as
//! stalled (evaluated when the queue is read, no background job). `ask` turns the task
//! `waiting` with the question in `waiting_on` (the asking agent stays in `claimed_by`
//! for display); `answer` appends the Q&A to the notes, reopens the task and sets
//! `answered_at`, which keeps it under NEEDS YOU as "answered" until an agent claims it.

use chrono_tz::Tz;
use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};

use super::add::MAX_NOTES;
use super::{
    DEFAULT_LEASE_MINUTES, ToolResult, ancestors, children, db_err, lease_end, log_event, require,
    touch,
};
use crate::db::now;
use crate::format::{clock, today_in};
use crate::model::{Kind, Node, NodeId, Status};
use crate::tools::args::{AnswerArgs, AskArgs, ClaimArgs, ReleaseArgs};

const MAX_LEASE_MINUTES: i64 = 24 * 60;
const MAX_QUESTION: usize = 1000;

/// The atomic claim. Wins only if the node is a claimable task (open, or already ours,
/// or a claim whose lease has run out) and no other agent holds a live claim on a
/// descendant. A claim inside another agent's claimed project is allowed: that is the
/// orchestrator pattern (the orchestrator claims the project, each worker claims its
/// own step). ?1 id, ?2 agent, ?3 now, ?4 new lease end.
const CLAIM_SQL: &str = "
UPDATE nodes
   SET status = 'in_progress', claimed_by = ?2, claim_expires = ?4,
       waiting_on = NULL, answered_at = NULL, updated_at = ?3
 WHERE id = ?1 AND kind = 'task' AND parent_id IS NOT NULL
   AND (status = 'open'
        OR (status = 'in_progress'
            AND (claimed_by = ?2 OR claim_expires IS NULL OR claim_expires <= ?3)))
   AND NOT EXISTS (
       WITH RECURSIVE down(id) AS (
         SELECT id FROM nodes WHERE parent_id = ?1
         UNION ALL
         SELECT c.id FROM nodes c JOIN down ON c.parent_id = down.id)
       SELECT 1 FROM nodes d JOIN down ON d.id = down.id
        WHERE d.status = 'in_progress' AND d.claimed_by != ?2 AND d.claim_expires > ?3)";

pub fn claim(conn: &mut Connection, args: ClaimArgs, tz: Tz) -> ToolResult {
    let id = args.id.parse()?;
    let agent = required_agent(&args.agent)?;
    let minutes = match &args.lease_minutes {
        Some(m) => match m.parse("lease_minutes")? {
            m @ 1..=MAX_LEASE_MINUTES => m,
            m => {
                return Err(format!(
                    "lease_minutes must be 1 to {MAX_LEASE_MINUTES}, not {m}. Omit it for the default {DEFAULT_LEASE_MINUTES}."
                ));
            }
        },
        None => DEFAULT_LEASE_MINUTES,
    };

    // IMMEDIATE takes the write lock up front, so the state read for the reply and the
    // log is the state the UPDATE sees, even with another process on the same file.
    let tx = conn
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(db_err)?;
    let before = require(&tx, id)?;
    let now = now();
    let expires = lease_end(&now, minutes);
    let won = tx
        .execute(CLAIM_SQL, params![id.0, agent, now, expires])
        .map_err(db_err)?;
    if won == 0 {
        return Err(refusal(&tx, &before, &agent, &now, tz)?);
    }

    let until = clock(&expires, tz);
    let (detail, mut reply) = match (before.status, before.claimed_by.as_deref()) {
        (Status::InProgress, Some(who)) if who == agent => (
            format!("renewed until {expires}"),
            format!("Renewed {id} {} for {agent} until {until}.", before.title),
        ),
        (Status::InProgress, prev) => {
            let prev = prev.unwrap_or("an agent");
            let lapsed = before
                .claim_expires
                .as_deref()
                .map(|e| format!(", whose lease expired {}", clock(e, tz)))
                .unwrap_or_default();
            (
                format!("took over from {prev}{lapsed}; until {expires}"),
                format!(
                    "Claimed {id} {} for {agent} until {until} (took over from {prev}{lapsed}).{}",
                    before.title,
                    if before.has_notes {
                        format!(" Read its notes (get notes: true) for where {prev} got to.")
                    } else {
                        String::new()
                    }
                ),
            )
        }
        _ => (
            format!("until {expires}"),
            format!(
                "Claimed {id} {} for {agent} until {until}. It is hidden from next for everyone else. Finish with set_status done, release, or ask.",
                before.title
            ),
        ),
    };
    if let Some(a) = ancestors(&tx, id)?
        .into_iter()
        .rev()
        .find(|a| live_other(a, &agent, &now))
    {
        reply.push_str(&format!(
            " It is a step of {} {}, which {} holds as the orchestrator.",
            a.id,
            a.title,
            a.claimed_by.as_deref().unwrap_or("another agent")
        ));
    }
    if before.answered_at.is_some() {
        reply.push_str(" The user answered a question on it: read the notes (get notes: true).");
    } else if before.has_notes && before.status != Status::InProgress {
        reply.push_str(" It has notes: read them with get notes: true.");
    }
    let renewed =
        before.status == Status::InProgress && before.claimed_by.as_deref() == Some(&agent);
    if !renewed
        && before.child_total > before.child_done
        && let Some(line) = parent_hint(&tx, &before, &agent, &now)?
    {
        reply.push('\n');
        reply.push_str(&line);
    }
    log_event(&tx, id, &now, Some(&agent), "claimed", &detail).map_err(db_err)?;
    touch(&tx, id, &now).map_err(db_err)?;
    tx.commit().map_err(db_err)?;
    Ok(reply)
}

/// The claim-on-parent hint (SPOKE-692): name the open subtasks so a worker claims the one
/// it is working on.
fn parent_hint(
    conn: &Connection,
    parent: &Node,
    agent: &str,
    now: &str,
) -> Result<Option<String>, String> {
    let kids = children(conn, parent.id)?;
    let open: Vec<Node> = if parent.ordered {
        match kids.iter().position(|c| !c.status.is_closed()) {
            Some(i) if kids[i].status == Status::Open => kids[i..]
                .iter()
                .filter(|c| c.status == Status::Open)
                .cloned()
                .collect(),
            _ => Vec::new(),
        }
    } else {
        kids.into_iter()
            .filter(|c| c.status == Status::Open)
            .collect()
    };
    let held_below: bool = conn
        .query_row(
            "WITH RECURSIVE down(id) AS (
               SELECT id FROM nodes WHERE parent_id = ?1
               UNION ALL
               SELECT c.id FROM nodes c JOIN down ON c.parent_id = down.id)
             SELECT EXISTS (SELECT 1 FROM nodes d JOIN down ON d.id = down.id
              WHERE d.status = 'in_progress' AND d.claimed_by = ?2 AND d.claim_expires > ?3)",
            params![parent.id.0, agent, now],
            |r| r.get(0),
        )
        .map_err(db_err)?;
    let action = crate::hints::Action::ParentClaimed {
        parent,
        open: &open,
        held_below,
    };
    Ok(crate::hints::hint(&action))
}

/// Why a claim lost, and what to do instead.
fn refusal(
    conn: &Connection,
    node: &Node,
    agent: &str,
    now: &str,
    tz: Tz,
) -> Result<String, String> {
    let id = node.id;
    if node.parent_id.is_none() {
        return Ok(format!(
            "{id} is the root; claim a task under it. Use next to find one."
        ));
    }
    if node.kind == Kind::Area {
        return Ok(format!(
            "{id} {} is an area; areas are never claimed. Claim a task under it (next with scope {id}).",
            node.title
        ));
    }
    if node.status.is_closed() {
        return Ok(format!(
            "{id} {} is {}. Reopen it with set_status open first, or pick another task.",
            node.title,
            node.status.as_str()
        ));
    }
    if node.status == Status::Waiting {
        return Ok(format!(
            "{id} {} is waiting on the user: \"{}\". Ask them, record the reply with answer, then claim it.",
            node.title,
            node.waiting_on.as_deref().unwrap_or("")
        ));
    }
    if let Some(line) = held_by_other(node, agent, now, tz) {
        return Ok(line);
    }
    let below: Option<(i64, String, String, String)> = conn
        .query_row(
            "WITH RECURSIVE down(id) AS (
               SELECT id FROM nodes WHERE parent_id = ?1
               UNION ALL
               SELECT c.id FROM nodes c JOIN down ON c.parent_id = down.id)
             SELECT d.id, d.title, d.claimed_by, d.claim_expires FROM nodes d JOIN down ON d.id = down.id
              WHERE d.status = 'in_progress' AND d.claimed_by != ?2 AND d.claim_expires > ?3
              LIMIT 1",
            params![id.0, agent, now],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
        )
        .optional()
        .map_err(db_err)?;
    if let Some((did, title, who, until)) = below {
        return Ok(format!(
            "{} {title} under {id} is claimed by {who} until {}. Claim a different step, or ask the user.",
            NodeId(did),
            clock(&until, tz)
        ));
    }
    Ok(format!(
        "{id} could not be claimed just now. Run next and pick again."
    ))
}

fn live_other(n: &Node, agent: &str, now: &str) -> bool {
    n.status == Status::InProgress
        && n.claimed_by.as_deref().is_some_and(|w| w != agent)
        && n.claim_expires.as_deref().is_some_and(|e| e > now)
}

/// `SPOKE-142 is claimed by opus-red until 14:20. Pick another or ask the user.`
fn held_by_other(node: &Node, agent: &str, now: &str, tz: Tz) -> Option<String> {
    live_other(node, agent, now).then(|| {
        format!(
            "{} is claimed by {} until {}. Pick another task, or ask the user.",
            node.id,
            node.claimed_by.as_deref().unwrap_or("another agent"),
            clock(node.claim_expires.as_deref().unwrap_or(""), tz)
        )
    })
}

pub fn release(conn: &mut Connection, args: ReleaseArgs, tz: Tz) -> ToolResult {
    let id = args.id.parse()?;
    let agent = required_agent(&args.agent)?;
    let note = args
        .note
        .as_deref()
        .map(str::trim)
        .filter(|n| !n.is_empty());
    let tx = conn
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(db_err)?;
    let node = require(&tx, id)?;
    let now = now();
    match node.status {
        Status::InProgress | Status::Waiting => {}
        other => {
            return Err(format!(
                "{id} {} is not claimed ({}); there is nothing to release.",
                node.title,
                other.as_str()
            ));
        }
    }
    if let Some(line) = held_by_other(&node, &agent, &now, tz) {
        return Err(format!(
            "{line} Only {} can release it.",
            node.claimed_by.as_deref().unwrap_or("its holder")
        ));
    }
    if node.status == Status::Waiting && node.claimed_by.as_deref().is_some_and(|w| w != agent) {
        return Err(format!(
            "{id} is waiting on a question from {}. Record the user's reply with answer instead.",
            node.claimed_by.as_deref().unwrap_or("another agent")
        ));
    }
    tx.execute(
        "UPDATE nodes SET status = 'open', claimed_by = NULL, claim_expires = NULL,
                          waiting_on = NULL, updated_at = ?1
         WHERE id = ?2",
        params![now, id.0],
    )
    .map_err(db_err)?;
    if let Some(note) = note {
        append_notes(
            &tx,
            id,
            &format!("Released by {agent} ({}): {note}", today_in(tz)),
        )?;
    }
    log_event(&tx, id, &now, Some(&agent), "released", note.unwrap_or("")).map_err(db_err)?;
    touch(&tx, id, &now).map_err(db_err)?;
    tx.commit().map_err(db_err)?;
    let mut reply = format!(
        "Released {id} {}; it is open in the queue again.",
        node.title
    );
    if note.is_some() {
        reply.push_str(" Your note was added to its notes.");
    }
    Ok(reply)
}

pub fn ask(conn: &mut Connection, args: AskArgs, tz: Tz) -> ToolResult {
    let id = args.id.parse()?;
    let agent = required_agent(&args.agent)?;
    let question = args
        .question
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    if question.is_empty() {
        return Err("question is empty. Write the exact question for the user.".to_string());
    }
    if question.chars().count() > MAX_QUESTION {
        return Err(format!(
            "question is longer than {MAX_QUESTION} characters. Keep it short; put background in notes with update notes_append."
        ));
    }
    let tx = conn
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(db_err)?;
    let node = require(&tx, id)?;
    let now = now();
    if node.kind == Kind::Area || node.parent_id.is_none() {
        return Err(format!(
            "{id} {} is an area; ask on the task the question is about.",
            node.title
        ));
    }
    if node.status.is_closed() {
        return Err(format!(
            "{id} {} is {}. Reopen it with set_status open first.",
            node.title,
            node.status.as_str()
        ));
    }
    if let Some(line) = held_by_other(&node, &agent, &now, tz) {
        return Err(line);
    }
    if node.status == Status::Waiting
        && let Some(who) = node.claimed_by.as_deref().filter(|w| *w != agent)
    {
        return Err(format!(
            "{id} is already waiting on a question from {who}: \"{}\". Get that answered first.",
            node.waiting_on.as_deref().unwrap_or("")
        ));
    }
    tx.execute(
        "UPDATE nodes SET status = 'waiting', waiting_on = ?1, claimed_by = ?2,
                          claim_expires = NULL, answered_at = NULL, updated_at = ?3
         WHERE id = ?4",
        params![question, agent, now, id.0],
    )
    .map_err(db_err)?;
    log_event(&tx, id, &now, Some(&agent), "asked", &question).map_err(db_err)?;
    touch(&tx, id, &now).map_err(db_err)?;
    tx.commit().map_err(db_err)?;
    Ok(format!(
        "{id} {} is waiting on the user: \"{question}\". It is at the top of next under NEEDS YOU. Stop work on it until it is answered.",
        node.title
    ))
}

pub fn answer(conn: &mut Connection, args: AnswerArgs, tz: Tz) -> ToolResult {
    let id = args.id.parse()?;
    let text = args.text.trim();
    if text.is_empty() {
        return Err("text is empty. Pass the user's reply.".to_string());
    }
    let tx = conn
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(db_err)?;
    let node = require(&tx, id)?;
    if node.status != Status::Waiting {
        return Err(format!(
            "{id} {} is not waiting on a question ({}). To add information, use update notes_append.",
            node.title,
            node.status.as_str()
        ));
    }
    let now = now();
    let day = today_in(tz);
    let who = node.claimed_by.as_deref().unwrap_or("an agent");
    let question = node
        .waiting_on
        .as_deref()
        .unwrap_or("(question not recorded)");
    append_notes(
        &tx,
        id,
        &format!("Question from {who} ({day}): {question}\nAnswer from the user ({day}): {text}"),
    )?;
    tx.execute(
        "UPDATE nodes SET status = 'open', waiting_on = NULL, claimed_by = NULL,
                          claim_expires = NULL, answered_at = ?1, updated_at = ?1
         WHERE id = ?2",
        params![now, id.0],
    )
    .map_err(db_err)?;
    log_event(&tx, id, &now, None, "answered", text).map_err(db_err)?;
    touch(&tx, id, &now).map_err(db_err)?;
    tx.commit().map_err(db_err)?;
    Ok(format!(
        "Answered {id} {}; it is open again, with the question and answer in its notes. next lists it under NEEDS YOU as answered until an agent claims it.",
        node.title
    ))
}

/// Adds a paragraph to the notes body and keeps the search index in step.
fn append_notes(conn: &Connection, id: NodeId, text: &str) -> Result<(), String> {
    let old: Option<String> = conn
        .query_row(
            "SELECT body FROM notes WHERE node_id = ?1",
            params![id.0],
            |r| r.get(0),
        )
        .optional()
        .map_err(db_err)?;
    let body = match old {
        Some(old) => format!("{old}\n\n{text}"),
        None => text.to_string(),
    };
    if body.len() > MAX_NOTES {
        return Err(format!(
            "notes would grow past {MAX_NOTES} bytes. Shorten them with update notes first."
        ));
    }
    conn.execute(
        "INSERT INTO notes (node_id, body) VALUES (?1, ?2)
         ON CONFLICT(node_id) DO UPDATE SET body = excluded.body",
        params![id.0, body],
    )
    .map_err(db_err)?;
    super::fts::reindex(conn, id).map_err(db_err)
}

/// The agent label is required here: it is who holds the task.
fn required_agent(raw: &str) -> Result<String, String> {
    super::clean_agent(Some(raw))?.ok_or_else(|| {
        "agent is empty. Pass a short label for yourself, the same for the whole session (e.g. \"opus-red\")."
            .to_string()
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::add;
    use crate::store::add::tests::args;
    use crate::store::test_support::temp_db;

    fn claim_args(id: i64, agent: &str) -> ClaimArgs {
        ClaimArgs {
            id: crate::model::IdArg::Number(id),
            agent: agent.to_string(),
            lease_minutes: None,
        }
    }

    /// Separate connections (as separate processes would have) racing on one file:
    /// the single-statement check-and-write lets exactly one win.
    #[test]
    fn racing_connections_have_one_winner() {
        let (mut conn, dir) = temp_db("claim-race");
        add(&mut conn, args(None, "Contested", Kind::Task)).unwrap(); // 2
        let path = dir.join("t.db");
        let start = std::sync::Arc::new(std::sync::Barrier::new(8));
        let handles: Vec<_> = (0..8)
            .map(|i| {
                let path = path.clone();
                let start = start.clone();
                std::thread::spawn(move || {
                    let mut c = crate::db::open(&path).unwrap();
                    start.wait();
                    claim(&mut c, claim_args(2, &format!("agent-{i}")), chrono_tz::UTC)
                })
            })
            .collect();
        let results: Vec<ToolResult> = handles.into_iter().map(|h| h.join().unwrap()).collect();
        let wins = results.iter().filter(|r| r.is_ok()).count();
        assert_eq!(wins, 1, "{results:?}");
        for r in results.iter().filter_map(|r| r.as_ref().err()) {
            assert!(r.starts_with("SPOKE-2 is claimed by agent-"), "{r}");
        }
        let _ = std::fs::remove_dir_all(dir);
    }
}
