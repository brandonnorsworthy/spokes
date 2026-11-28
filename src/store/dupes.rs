//! Duplicate protection (SPOKE-260): two unfinished nodes under one parent may not share
//! a title, and `add` lists likely duplicates elsewhere in the tree.
//!
//! "The same title" is compared the way search compares text: case, accents, punctuation
//! and spacing are ignored (`agent-feedback`, `Agent Feedback`, `agentfeedback`). Only
//! unfinished siblings (open, in progress, waiting) count; a done or dropped sibling with
//! that title never blocks, because re-adding finished work is normal and reopening must
//! always work. The check runs inside the write's transaction, and every MCP write goes
//! through one connection behind a lock, so two agents adding at once cannot both win.

use std::collections::HashSet;

use rusqlite::{Connection, params};

use super::{children, db_err};
use crate::model::{DEFAULT_USER, Node, NodeId, ROOT_ID, Status};
use crate::text::{keywords, normalize, squash};

/// How many likely duplicates `add` lists.
const LISTED: usize = 3;
/// Two titles look alike when they share at least [`MIN_SHARED`] keywords, making up
/// [`OVERLAP`] of the shorter title's and [`OVERLAP_LONG`] of the longer one's. On the
/// production tree (2026-11-27) this flags about 4% of open titles, all close pairs.
const MIN_SHARED: usize = 3;
const OVERLAP: f64 = 0.8;
const OVERLAP_LONG: f64 = 0.3;

/// The form titles are compared in. A title with no letters or digits ("???") keeps its
/// lowercased text, so it still only matches itself.
pub(crate) fn title_key(title: &str) -> String {
    let key = squash(&normalize(title));
    if key.is_empty() {
        title.to_lowercase()
    } else {
        key
    }
}

/// The unfinished child of `parent` (other than `except`) whose title is the same as
/// `title`, if any.
pub(crate) fn sibling_clash(
    conn: &Connection,
    parent: NodeId,
    title: &str,
    except: Option<NodeId>,
) -> Result<Option<Node>, String> {
    let key = title_key(title);
    Ok(children(conn, parent)?
        .into_iter()
        .find(|c| Some(c.id) != except && !c.status.is_closed() && title_key(&c.title) == key))
}

/// The refusal for a write that would make `existing`'s duplicate under `parent`.
pub(crate) fn refusal(existing: &Node, parent: &Node, title: &str) -> String {
    let same = if existing.title == title {
        "that title".to_string()
    } else {
        format!("\"{}\", the same title", existing.title)
    };
    format!(
        "{} {} already has {} {} ({}) with {same}, so nothing was changed. Use {} instead (add under it, or update it), give this one a different title, or close {} first if it is finished or unwanted.",
        parent.id,
        parent.title,
        existing.id,
        existing.title,
        existing.status.as_str(),
        existing.id,
        existing.id
    )
}

/// Refuses two children of one `add` call with the same title under the same parent.
/// `titles` are the cleaned titles of one parent's new children, in order.
pub(crate) fn batch_clash(titles: &[&str]) -> Option<(usize, usize)> {
    let keys: Vec<String> = titles.iter().map(|t| title_key(t)).collect();
    for (j, k) in keys.iter().enumerate() {
        if let Some(i) = keys[..j].iter().position(|e| e == k) {
            return Some((i, j));
        }
    }
    None
}

struct Candidate {
    id: NodeId,
    parent: Option<NodeId>,
    title: String,
    status: Status,
    aliases: Option<String>,
}

/// One line naming nodes that look like the one just added (`new_id`, under `parent`,
/// with `aliases`): the same title elsewhere in the tree, a title or alias matching the
/// other's, or most of the same words. Unfinished nodes count, plus a closed sibling
/// with the same title (it may only need reopening). `skip` are the nodes this add made.
pub(crate) fn likely_line(
    conn: &Connection,
    new_id: NodeId,
    parent: NodeId,
    title: &str,
    aliases: Option<&str>,
    skip: &[NodeId],
) -> Result<Option<String>, String> {
    let key = title_key(title);
    let new_aliases: HashSet<String> = alias_keys(aliases);
    let words: HashSet<String> = keywords(title).into_iter().collect();
    let skip: HashSet<NodeId> = skip.iter().copied().chain([new_id, ROOT_ID]).collect();

    let mut stmt = conn
        .prepare_cached(
            "SELECT id, parent_id, title, status, aliases FROM nodes
             WHERE user_id = ?1 AND (status NOT IN ('done','dropped') OR parent_id = ?2)",
        )
        .map_err(db_err)?;
    let rows = stmt
        .query_map(params![DEFAULT_USER, parent.0], |r| {
            let status: String = r.get(3)?;
            Ok(Candidate {
                id: NodeId(r.get(0)?),
                parent: r.get::<_, Option<i64>>(1)?.map(NodeId),
                title: r.get(2)?,
                status: status.parse().unwrap_or(Status::Open),
                aliases: r.get(4)?,
            })
        })
        .and_then(|rows| rows.collect::<rusqlite::Result<Vec<_>>>())
        .map_err(db_err)?;

    // (rank, score, text): lower rank first, then higher score, then lower id.
    let mut found: Vec<(u8, f64, NodeId, String)> = Vec::new();
    for c in rows.into_iter().filter(|c| !skip.contains(&c.id)) {
        let same_title = title_key(&c.title) == key;
        if c.status.is_closed() {
            // Only closed siblings get here (see the query).
            if same_title {
                found.push((
                    0,
                    1.0,
                    c.id,
                    format!(
                        "{} {} ({}, same title, same parent: reopen it with set_status if it is the same work)",
                        c.id,
                        c.title,
                        c.status.as_str()
                    ),
                ));
            }
            continue;
        }
        let place = c.parent.map_or(String::new(), |p| format!(", under {p}"));
        if same_title {
            found.push((
                1,
                1.0,
                c.id,
                format!("{} {} (same title{place})", c.id, c.title),
            ));
            continue;
        }
        let theirs = alias_keys(c.aliases.as_deref());
        if theirs.contains(&key) || new_aliases.contains(&title_key(&c.title)) {
            found.push((
                2,
                1.0,
                c.id,
                format!("{} {} (alias match{place})", c.id, c.title),
            ));
            continue;
        }
        let other: HashSet<String> = keywords(&c.title).into_iter().collect();
        let shared = words.intersection(&other).count();
        let (small, large) = (words.len().min(other.len()), words.len().max(other.len()));
        if shared >= MIN_SHARED && small > 0 {
            // Most of the shorter title's words, and a fair share of the longer one's: a
            // short title inside a long one counts ("Say which instance a call hit").
            let score = shared as f64 / small as f64;
            if score >= OVERLAP && shared as f64 / large as f64 >= OVERLAP_LONG {
                found.push((
                    3,
                    score,
                    c.id,
                    format!("{} {} (similar title{place})", c.id, c.title),
                ));
            }
        }
    }
    if found.is_empty() {
        return Ok(None);
    }
    found.sort_by(|a, b| {
        a.0.cmp(&b.0)
            .then(b.1.total_cmp(&a.1))
            .then(a.2.0.cmp(&b.2.0))
    });
    let more = found.len().saturating_sub(LISTED);
    let mut names: Vec<String> = found.into_iter().take(LISTED).map(|f| f.3).collect();
    if more > 0 {
        names.push(format!("+{more} more"));
    }
    Ok(Some(format!(
        "Possible duplicates of {new_id}: {}. If one is the same work, use it and drop {new_id} with set_status.",
        names.join("; ")
    )))
}

fn alias_keys(aliases: Option<&str>) -> HashSet<String> {
    aliases
        .unwrap_or_default()
        .lines()
        .map(title_key)
        .filter(|k| !k.is_empty())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn key_ignores_case_punctuation_spacing_and_accents() {
        for t in [
            "agent-feedback",
            "Agent Feedback",
            "agentfeedback",
            " AGENT_feedback! ",
        ] {
            assert_eq!(title_key(t), "agentfeedback", "{t}");
        }
        assert_eq!(title_key("Café"), title_key("cafe"));
        assert_ne!(title_key("agent feedback 2"), title_key("agent feedback"));
        assert_eq!(title_key("???"), "???");
        assert_ne!(title_key("???"), title_key("!!!"));
    }

    #[test]
    fn batch_clash_finds_the_first_repeat() {
        assert_eq!(batch_clash(&["a", "b", "c"]), None);
        assert_eq!(
            batch_clash(&["Write tests", "Ship", "write-tests"]),
            Some((0, 2))
        );
    }
}
