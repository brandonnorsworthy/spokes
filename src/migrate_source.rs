//! `spokes migrate-source` (SPOKE-269): copies the words under the "## Source (verbatim ...)"
//! headings that agents wrote in notes into the new `source` field (the heading line itself
//! is left out, so a migrated source looks like one written natively).
//!
//! It only adds. Notes are never edited, shortened or cleared (`docs/invariants.md`: nothing
//! is ever deleted), so the words appear in both places afterwards. A node whose `source` is
//! already set is skipped, which makes a second run a no-op.

use std::path::Path;

use anyhow::Context;
use rusqlite::{Connection, TransactionBehavior, params};

use crate::model::NodeId;

const MARKER: &str = "## Source (verbatim";
const AGENT: &str = "migrate-source";

/// One node the migration would change.
#[derive(Debug, PartialEq, Eq)]
pub struct Item {
    pub id: i64,
    pub title: String,
    /// The text that becomes the node's `source`.
    pub source: String,
}

/// The text under each "## Source (verbatim" heading of a note, joined by a blank line. A block
/// starts at a line beginning with the marker and runs to the next `## ` heading or the end.
/// The heading line is dropped; everything under it is taken as is, trimmed at the ends only.
pub fn extract(notes: &str) -> Option<String> {
    let mut blocks: Vec<String> = Vec::new();
    let mut current: Option<Vec<&str>> = None;
    for line in notes.lines() {
        if line.starts_with(MARKER) {
            if let Some(b) = current.take() {
                blocks.push(b.join("\n"));
            }
            current = Some(Vec::new());
        } else if line.starts_with("## ") {
            if let Some(b) = current.take() {
                blocks.push(b.join("\n"));
            }
        } else if let Some(b) = current.as_mut() {
            b.push(line);
        }
    }
    if let Some(b) = current.take() {
        blocks.push(b.join("\n"));
    }
    let text = blocks
        .iter()
        .map(|b| b.trim())
        .filter(|b| !b.is_empty())
        .collect::<Vec<_>>()
        .join("\n\n");
    (!text.is_empty()).then_some(text)
}

/// Every node that has such a block in its notes and no `source` yet.
pub fn plan(conn: &Connection) -> anyhow::Result<Vec<Item>> {
    let mut stmt = conn.prepare(
        "SELECT n.id, n.title, t.body FROM nodes n JOIN notes t ON t.node_id = n.id
         WHERE n.source IS NULL AND t.body LIKE '%' || ?1 || '%' ORDER BY n.id",
    )?;
    let rows = stmt.query_map(params![MARKER], |r| {
        Ok((
            r.get::<_, i64>(0)?,
            r.get::<_, String>(1)?,
            r.get::<_, String>(2)?,
        ))
    })?;
    let mut items = Vec::new();
    for row in rows {
        let (id, title, body) = row?;
        if let Some(source) = extract(&body) {
            items.push(Item { id, title, source });
        }
    }
    Ok(items)
}

/// Writes every planned `source` in one transaction, logging an `updated` event per node.
/// Nothing else about a node changes (not its notes, not `updated_at`, not `touched_at`).
pub fn apply(conn: &mut Connection, items: &[Item]) -> anyhow::Result<()> {
    let now = crate::db::now();
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    for item in items {
        // The WHERE clause keeps this idempotent and safe against a concurrent writer.
        let changed = tx.execute(
            "UPDATE nodes SET source = ?1 WHERE id = ?2 AND source IS NULL",
            params![item.source, item.id],
        )?;
        if changed == 1 {
            crate::store::fts::reindex(&tx, NodeId(item.id))?;
            crate::store::log_event(
                &tx,
                NodeId(item.id),
                &now,
                Some(AGENT),
                "updated",
                "source copied from the notes block (SPOKE-269); notes unchanged",
            )?;
        }
    }
    tx.commit()?;
    Ok(())
}

/// Dry run (`apply == false`) or the real thing: a backup first, then the copy. Returns the
/// text to print.
pub fn run(db_path: &Path, backup_dir: &Path, apply_it: bool) -> anyhow::Result<String> {
    let mut conn = crate::db::open(db_path)?;
    let items = plan(&conn)?;
    let mut out = format!(
        "{} node(s) have a \"{MARKER}\" block in their notes and no source yet:\n",
        items.len()
    );
    for i in &items {
        out.push_str(&format!(
            "  SPOKE-{}  {} bytes  {}\n",
            i.id,
            i.source.len(),
            i.title
        ));
    }
    if !apply_it {
        out.push_str("Dry run: nothing written. Run again with --apply to copy them.\n");
        return Ok(out);
    }
    if items.is_empty() {
        out.push_str("Nothing to do.\n");
        return Ok(out);
    }
    std::fs::create_dir_all(backup_dir)
        .with_context(|| format!("creating {}", backup_dir.display()))?;
    let name = crate::backup::backup_file_name().replacen("spokes-", "pre-migrate-source-", 1);
    let dest = backup_dir.join(name);
    let bytes = crate::backup::write_backup(db_path, &dest)?;
    out.push_str(&format!("Backup: {} ({bytes} bytes)\n", dest.display()));
    apply(&mut conn, &items)?;
    out.push_str(&format!(
        "Copied {} source(s). Notes were not changed.\n",
        items.len()
    ));
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::test_support::temp_db;

    #[test]
    fn extracts_blocks_up_to_the_next_heading() {
        let notes = "intro line\n\n## Source (verbatim, 2026-11-23)\n\"first\"\n(Context: more)\n\n## Idea\nmine\n\n## Source (verbatim, 2026-11-24)\n\"second\"";
        assert_eq!(
            extract(notes).as_deref(),
            Some("\"first\"\n(Context: more)\n\n\"second\"")
        );
        assert_eq!(extract("no block here\n## Idea\nx"), None);
        // A heading with nothing under it has no words to copy.
        assert_eq!(
            extract("## Source (verbatim, 2026-11-23)\n\n## Idea\nx"),
            None
        );
        // A marker in the middle of a line is quoted text, not a heading.
        assert_eq!(extract("\"see ## Source (verbatim, x) here\""), None);
    }

    #[test]
    fn copies_without_touching_notes_and_is_idempotent() {
        let (mut conn, dir) = temp_db("migrate-source");
        let note_a = "## Source (verbatim, 2026-11-23)\n\"raw words\"\n\n## Idea\nstuff";
        let note_b = "plain notes";
        for (id, title, body) in [(2, "A", note_a), (3, "B", note_b)] {
            conn.execute(
                "INSERT INTO nodes (id, parent_id, position, kind, title, created_at, updated_at, touched_at)
                 VALUES (?1, 1, ?1, 'task', ?2, 't', 't', 't')",
                params![id, title],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO notes (node_id, body) VALUES (?1, ?2)",
                params![id, body],
            )
            .unwrap();
        }
        conn.execute("UPDATE nodes SET source = 'already' WHERE id = 3", [])
            .unwrap();

        let items = plan(&conn).unwrap();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].id, 2);
        assert_eq!(items[0].source, "\"raw words\"");

        apply(&mut conn, &items).unwrap();
        let (source, updated): (String, String) = conn
            .query_row(
                "SELECT source, updated_at FROM nodes WHERE id = 2",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(source, items[0].source);
        assert_eq!(updated, "t");
        for (id, body) in [(2, note_a), (3, note_b)] {
            let notes: String = conn
                .query_row("SELECT body FROM notes WHERE node_id = ?1", [id], |r| {
                    r.get(0)
                })
                .unwrap();
            assert_eq!(notes, body, "notes of {id} must be byte-identical");
        }
        let src3: String = conn
            .query_row("SELECT source FROM nodes WHERE id = 3", [], |r| r.get(0))
            .unwrap();
        assert_eq!(src3, "already");

        // Second run: nothing planned, nothing logged.
        let events_before: i64 = conn
            .query_row("SELECT COUNT(*) FROM events", [], |r| r.get(0))
            .unwrap();
        assert!(plan(&conn).unwrap().is_empty());
        apply(&mut conn, &items).unwrap();
        let events_after: i64 = conn
            .query_row("SELECT COUNT(*) FROM events", [], |r| r.get(0))
            .unwrap();
        assert_eq!(events_before, events_after);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
