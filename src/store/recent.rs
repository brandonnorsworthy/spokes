//! `recent`: the most recently added nodes, newest first.

use chrono::Utc;
use rusqlite::{Connection, params};

use super::{NODE_COLS, ToolResult, ancestors, breadcrumb, db_err, node_from_row, require};
use crate::format::{ago, clock};
use crate::model::{DEFAULT_USER, ROOT_ID};
use crate::tools::args::RecentArgs;

const DEFAULT_LIMIT: i64 = 10;
const MAX_LIMIT: i64 = 50;

pub fn recent(conn: &Connection, args: RecentArgs, tz: chrono_tz::Tz) -> ToolResult {
    let limit = match &args.limit {
        Some(l) => l.parse("limit")?.clamp(1, MAX_LIMIT),
        None => DEFAULT_LIMIT,
    };
    let scope = match &args.scope {
        Some(s) => Some(require(conn, s.parse()?)?.id),
        None => None,
    };
    let include_done = args.include_done.unwrap_or(false);

    // Newest first; ids are never reused and only grow, so they break created_at ties.
    let sql = format!(
        "SELECT {NODE_COLS} FROM nodes n
         WHERE n.user_id = ?1 AND n.id != ?2 AND (?3 OR n.status NOT IN ('done','dropped'))
         ORDER BY n.created_at DESC, n.id DESC"
    );
    let mut stmt = conn.prepare_cached(&sql).map_err(db_err)?;
    let rows = stmt
        .query_map(
            params![DEFAULT_USER, ROOT_ID.0, include_done],
            node_from_row,
        )
        .map_err(db_err)?;

    let now = Utc::now();
    let mut lines = Vec::new();
    for row in rows {
        let node = row.map_err(db_err)?;
        let up = ancestors(conn, node.id)?;
        if scope.is_some_and(|s| !up.iter().any(|a| a.id == s)) {
            continue;
        }
        let path = breadcrumb(&up);
        lines.push(format!(
            "{} {}  {}  in {}  (added {}, {})",
            node.id,
            node.kind.as_str(),
            node.title,
            if path.is_empty() {
                "To do (top level)".to_string()
            } else {
                path
            },
            ago(&node.created_at, now),
            clock(&node.created_at, tz),
        ));
        if lines.len() as i64 >= limit {
            break;
        }
    }
    if lines.is_empty() {
        return Ok("No matching nodes.".to_string());
    }
    Ok(format!(
        "{} most recently added, newest first:\n  {}",
        lines.len(),
        lines.join("\n  ")
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::test_support::temp_db;

    fn insert(conn: &Connection, title: &str, parent: i64, at: &str, status: &str) {
        conn.execute(
            "INSERT INTO nodes (user_id, parent_id, position, kind, title, status, created_at, updated_at, touched_at)
             VALUES (?1, ?2, 'a', 'task', ?3, ?4, ?5, ?5, ?5)",
            params![DEFAULT_USER, parent, title, status, at],
        )
        .expect("insert");
    }

    #[test]
    fn newest_first_hides_closed_and_limits() {
        let (conn, dir) = temp_db("recent");
        insert(&conn, "Old", 1, "2026-11-01T10:00:00Z", "open");
        insert(&conn, "Newer", 1, "2026-11-10T10:00:00Z", "open");
        insert(&conn, "Newest done", 1, "2026-11-12T10:00:00Z", "done");
        let args = |limit: Option<&str>, done: bool| -> RecentArgs {
            serde_json::from_value(serde_json::json!({
                "limit": limit, "include_done": done
            }))
            .expect("args")
        };
        let out = recent(&conn, args(None, false), chrono_tz::UTC).expect("ok");
        let (newer, old) = (
            out.find("Newer").expect("newer"),
            out.find("Old").expect("old"),
        );
        assert!(newer < old && !out.contains("Newest done"));
        let out = recent(&conn, args(Some("1"), true), chrono_tz::UTC).expect("ok");
        assert!(out.contains("Newest done") && !out.contains("Newer"));
        let _ = std::fs::remove_dir_all(dir);
    }
}
