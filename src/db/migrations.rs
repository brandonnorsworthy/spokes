//! Embedded, forward-only schema migrations tracked with `PRAGMA user_version`.
//!
//! Each entry in [`STEPS`] moves the schema from version `i` to `i + 1`. Never edit
//! a step that has shipped; append a new one (e.g. `ALTER TABLE ... ADD COLUMN`).

use anyhow::Context;
use rusqlite::{Connection, Transaction};

/// One migration: plain SQL, or Rust code for steps that need it (e.g. backfills that
/// normalize text, which SQL cannot do).
enum Step {
    Sql(&'static str),
    Code(fn(&Transaction<'_>) -> rusqlite::Result<()>),
}

const STEPS: &[Step] = &[
    // 1: core tables from the spec's data model.
    Step::Sql(
        "CREATE TABLE nodes (
       id            INTEGER PRIMARY KEY AUTOINCREMENT,
       user_id       INTEGER NOT NULL DEFAULT 1,
       parent_id     INTEGER REFERENCES nodes(id),
       position      TEXT    NOT NULL,
       kind          TEXT    NOT NULL DEFAULT 'task' CHECK (kind IN ('area','task')),
       ordered       INTEGER NOT NULL DEFAULT 0,
       title         TEXT    NOT NULL,
       status        TEXT    NOT NULL DEFAULT 'open'
                     CHECK (status IN ('open','in_progress','waiting','done','dropped')),
       priority      INTEGER CHECK (priority BETWEEN 1 AND 4),
       focus_rank    TEXT,
       due_at        TEXT,
       defer_until   TEXT,
       recur         TEXT,
       aliases       TEXT,
       waiting_on    TEXT,
       claimed_by    TEXT,
       claim_expires TEXT,
       created_at    TEXT NOT NULL,
       updated_at    TEXT NOT NULL,
       touched_at    TEXT NOT NULL,
       completed_at  TEXT
     );
     CREATE INDEX nodes_children ON nodes(parent_id, position);
     CREATE INDEX nodes_queue    ON nodes(user_id, status, due_at);

     CREATE TABLE notes (
       node_id INTEGER PRIMARY KEY REFERENCES nodes(id),
       body    TEXT NOT NULL
     );

     CREATE TABLE events (
       id      INTEGER PRIMARY KEY,
       node_id INTEGER NOT NULL REFERENCES nodes(id),
       at      TEXT NOT NULL,
       agent   TEXT,
       type    TEXT NOT NULL,
       detail  TEXT
     );
     CREATE INDEX events_node ON events(node_id, at);",
    ),
    // 2: the search index (trigram FTS5 over normalized text), backfilled from existing rows.
    Step::Code(search_index),
    // 3: when a human answered an agent's question (answer). The task is open again and
    // shown under NEEDS YOU as answered until an agent claims it or it is closed.
    Step::Sql("ALTER TABLE nodes ADD COLUMN answered_at TEXT"),
    // 4: the user's raw words behind a node (SPOKE-181). Set at creation, only ever appended to.
    // A 0.6.x server refuses a database at this version, so a rollback restores a backup.
    Step::Sql("ALTER TABLE nodes ADD COLUMN source TEXT"),
];

fn search_index(tx: &Transaction<'_>) -> rusqlite::Result<()> {
    tx.execute_batch(
        "CREATE VIRTUAL TABLE node_fts USING fts5(title, aliases, notes, tokenize = 'trigram')",
    )?;
    let ids: Vec<i64> = tx
        .prepare("SELECT id FROM nodes")?
        .query_map([], |r| r.get(0))?
        .collect::<rusqlite::Result<_>>()?;
    for id in ids {
        crate::store::fts::reindex(tx, crate::model::NodeId(id))?;
    }
    Ok(())
}

/// The schema version a fully migrated database has.
pub fn latest() -> i64 {
    STEPS.len() as i64
}

pub fn apply(conn: &mut Connection) -> anyhow::Result<()> {
    apply_to(conn, latest())
}

/// Applies the steps up to schema version `target` (tests use it to build an older database).
fn apply_to(conn: &mut Connection, target: i64) -> anyhow::Result<()> {
    let current: i64 = conn.query_row("PRAGMA user_version", [], |r| r.get(0))?;
    if current > latest() {
        anyhow::bail!(
            "database schema version {current} is newer than this build supports ({}); upgrade spokes",
            latest()
        );
    }
    for (i, step) in STEPS
        .iter()
        .enumerate()
        .take(target as usize)
        .skip(current as usize)
    {
        let version = i as i64 + 1;
        let tx = conn.transaction()?;
        match step {
            Step::Sql(sql) => tx.execute_batch(sql),
            Step::Code(run) => run(&tx),
        }
        .with_context(|| format!("applying schema migration {version}"))?;
        // PRAGMA cannot take bound parameters; version is a trusted integer.
        tx.execute_batch(&format!("PRAGMA user_version = {version}"))?;
        tx.commit()?;
        tracing::info!(version, "applied schema migration");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A database written by 0.6.x (schema 3) gains a nullable `source` and keeps every row.
    #[test]
    fn step_four_adds_source_and_keeps_existing_rows() {
        let mut conn = Connection::open_in_memory().unwrap();
        apply_to(&mut conn, 3).unwrap();
        conn.execute_batch(
            "INSERT INTO nodes (id, parent_id, position, kind, title, created_at, updated_at, touched_at)
             VALUES (1, NULL, 'a', 'area', 'To do', 't', 't', 't'),
                    (2, 1, 'a', 'task', 'Old', 't', 't', 't');
             INSERT INTO notes (node_id, body) VALUES (2, 'keep me');",
        )
        .unwrap();
        apply(&mut conn).unwrap();
        let version: i64 = conn
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .unwrap();
        assert_eq!(version, latest());
        assert!(version >= 4);
        let (title, source): (String, Option<String>) = conn
            .query_row("SELECT title, source FROM nodes WHERE id = 2", [], |r| {
                Ok((r.get(0)?, r.get(1)?))
            })
            .unwrap();
        assert_eq!((title.as_str(), source), ("Old", None));
        let notes: String = conn
            .query_row("SELECT body FROM notes WHERE node_id = 2", [], |r| r.get(0))
            .unwrap();
        assert_eq!(notes, "keep me");
    }
}
