//! `focus`: place a node on, move it within, or take it off the global ranked list.
//!
//! The list is ordered by `nodes.focus_rank`, a fractional key, so any placement
//! writes one row. Closed nodes leave the list (`set_status` clears their rank).

use std::collections::HashMap;

use rusqlite::{Connection, params};

use super::{ToolResult, clean_agent, db_err, log_event, renew_leases, require, touch};
use crate::db::{now, position};
use crate::model::{DEFAULT_USER, NodeId};
use crate::tools::args::FocusArgs;

/// The focus list is meant to stay short; `focus` and `next` warn past this.
pub const FOCUS_SOFT_MAX: usize = 10;

/// The live focus list in rank order: (id, rank key, title).
pub(crate) fn focus_list(conn: &Connection) -> Result<Vec<(NodeId, String, String)>, String> {
    let mut stmt = conn
        .prepare_cached(
            "SELECT id, focus_rank, title FROM nodes
             WHERE user_id = ?1 AND focus_rank IS NOT NULL AND status NOT IN ('done', 'dropped')
             ORDER BY focus_rank",
        )
        .map_err(db_err)?;
    stmt.query_map(params![DEFAULT_USER], |r| {
        Ok((NodeId(r.get(0)?), r.get(1)?, r.get(2)?))
    })
    .and_then(|rows| rows.collect())
    .map_err(db_err)
}

/// Node id -> its 1-based place on the focus list.
pub(crate) fn focus_numbers(conn: &Connection) -> Result<HashMap<NodeId, usize>, String> {
    Ok(focus_list(conn)?
        .into_iter()
        .enumerate()
        .map(|(i, (id, _, _))| (id, i + 1))
        .collect())
}

pub(crate) fn too_long_note(len: usize) -> Option<String> {
    (len > FOCUS_SOFT_MAX).then(|| {
        format!(
            "The focus list has {len} entries; keep it to about {FOCUS_SOFT_MAX}. Take some off with focus remove: true."
        )
    })
}

pub fn focus(conn: &mut Connection, args: FocusArgs) -> ToolResult {
    let id = args.id.parse()?;
    let agent = clean_agent(args.agent.as_deref())?;
    let rank = args.rank.as_ref().map(|r| r.parse("rank")).transpose()?;
    let before = args.before.as_ref().map(|b| b.parse()).transpose()?;
    let after = args.after.as_ref().map(|a| a.parse()).transpose()?;
    let remove = args.remove.unwrap_or(false);
    let placements =
        usize::from(rank.is_some()) + usize::from(before.is_some()) + usize::from(after.is_some());
    if remove && placements > 0 {
        return Err("pass remove: true on its own, without rank, before or after.".to_string());
    }
    if placements > 1 {
        return Err("pass only one of rank, before or after.".to_string());
    }
    if let Some(r) = rank
        && r < 1
    {
        return Err(format!(
            "rank must be 1 or more (1 = top), not {r}. To take a node off the list, pass remove: true."
        ));
    }

    let tx = conn.transaction().map_err(db_err)?;
    let node = require(&tx, id)?;
    if node.parent_id.is_none() {
        return Err(format!(
            "{id} is the root; put the areas or tasks under it on the focus list instead."
        ));
    }
    let list = focus_list(&tx)?;
    let now = now();

    if remove {
        if node.focus_rank.is_none() {
            return Ok(format!("{id} {} is not on the focus list.", node.title));
        }
        tx.execute(
            "UPDATE nodes SET focus_rank = NULL, updated_at = ?1 WHERE id = ?2",
            params![now, id.0],
        )
        .map_err(db_err)?;
        log_event(&tx, id, &now, agent.as_deref(), "focus", "removed").map_err(db_err)?;
        tx.commit().map_err(db_err)?;
        return Ok(format!(
            "Took {id} {} off the focus list ({} left).",
            node.title,
            list.len().saturating_sub(1)
        ));
    }

    if node.status.is_closed() {
        return Err(format!(
            "{id} {} is {}; only open work goes on the focus list. Reopen it with set_status first.",
            node.title,
            node.status.as_str()
        ));
    }
    let others: Vec<&(NodeId, String, String)> = list.iter().filter(|(n, _, _)| *n != id).collect();
    let anchor_index = |anchor: NodeId| -> Result<usize, String> {
        if anchor == id {
            return Err(format!("{id} cannot be placed before or after itself."));
        }
        others.iter().position(|(n, _, _)| *n == anchor).ok_or_else(|| {
            format!(
                "{anchor} is not on the focus list, so {id} cannot sit next to it. Use rank (1 = top) instead, or put {anchor} on the list first."
            )
        })
    };
    // The new place, as an index into the list without this node.
    let slot = if let Some(r) = rank {
        (r as usize - 1).min(others.len())
    } else if let Some(b) = before {
        anchor_index(b)?
    } else if let Some(a) = after {
        anchor_index(a)? + 1
    } else if let Some(at) = list.iter().position(|(n, _, _)| *n == id) {
        return Ok(format!(
            "{id} {} is already #{} of {} on the focus list. Pass rank, before or after to move it.",
            node.title,
            at + 1,
            list.len()
        ));
    } else {
        others.len()
    };
    let lo = slot.checked_sub(1).map(|i| others[i].1.as_str());
    let hi = others.get(slot).map(|o| o.1.as_str());
    let key = position::between(lo, hi);
    tx.execute(
        "UPDATE nodes SET focus_rank = ?1, updated_at = ?2 WHERE id = ?3",
        params![key, now, id.0],
    )
    .map_err(db_err)?;
    let total = others.len() + 1;
    log_event(
        &tx,
        id,
        &now,
        agent.as_deref(),
        "focus",
        &format!("#{} of {total}", slot + 1),
    )
    .map_err(db_err)?;
    touch(&tx, id, &now).map_err(db_err)?;
    renew_leases(&tx, id, agent.as_deref(), &now).map_err(db_err)?;
    tx.commit().map_err(db_err)?;

    let mut line = format!(
        "{id} {} is now #{} of {total} on the focus list.",
        node.title,
        slot + 1
    );
    if let Some(note) = too_long_note(total) {
        line.push(' ');
        line.push_str(&note);
    }
    Ok(line)
}
