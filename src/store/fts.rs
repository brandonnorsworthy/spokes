//! Keeps `node_fts` (rowid = node id) in step with a node's title, aliases and notes.
//!
//! Text is normalized in Rust before it is indexed, so the index is written from the
//! code paths that change text (`add`, `update`) rather than by SQL triggers. Status
//! and position are not indexed: search joins `nodes` for those, so `move` and
//! `set_status` never leave the index stale.

use rusqlite::{Connection, OptionalExtension, params};

use crate::model::NodeId;
use crate::text::index_form;

/// Title, aliases, notes and source of one node.
type IndexRow = (String, Option<String>, Option<String>, Option<String>);

/// Rewrites the index row for one node from its current title, aliases and notes.
pub fn reindex(conn: &Connection, id: NodeId) -> rusqlite::Result<()> {
    let row: Option<IndexRow> = conn
        .query_row(
            "SELECT n.title, n.aliases, t.body, n.source FROM nodes n
             LEFT JOIN notes t ON t.node_id = n.id WHERE n.id = ?1",
            params![id.0],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
        )
        .optional()?;
    conn.execute("DELETE FROM node_fts WHERE rowid = ?1", params![id.0])?;
    let Some((title, aliases, notes, source)) = row else {
        return Ok(());
    };
    let aliases = aliases
        .unwrap_or_default()
        .lines()
        .map(|a| index_form(a, true))
        .filter(|a| !a.is_empty())
        .collect::<Vec<_>>()
        .join("\n");
    // The source is searched like notes: one index column, no schema change.
    let notes = [notes, source]
        .into_iter()
        .flatten()
        .map(|t| index_form(&t, false))
        .collect::<Vec<_>>()
        .join(" ");
    conn.execute(
        "INSERT INTO node_fts (rowid, title, aliases, notes) VALUES (?1, ?2, ?3, ?4)",
        params![id.0, index_form(&title, true), aliases, notes],
    )?;
    Ok(())
}
