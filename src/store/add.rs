//! `add`: create one node under a parent.

use rusqlite::{Connection, OptionalExtension, params};

use super::{ToolResult, clean_agent, db_err, log_event, require, touch};
use crate::db::{now, position};
use crate::format::parse_date;
use crate::model::{DEFAULT_USER, Kind, NodeId, ROOT_ID};
use crate::tools::args::AddArgs;

const MAX_TITLE: usize = 300;
const MAX_ALIASES: usize = 10;
const MAX_ALIAS: usize = 100;
const MAX_NOTES: usize = 100_000;

pub fn add(conn: &mut Connection, args: AddArgs) -> ToolResult {
    let parent_id = match &args.parent {
        Some(p) => p.parse()?,
        None => ROOT_ID,
    };
    let title = clean_title(&args.title)?;
    let kind = args.kind.unwrap_or(Kind::Task);
    if let Some(p) = args.priority
        && !(1..=4).contains(&p)
    {
        return Err(format!("priority must be 1 (highest) to 4, not {p}."));
    }
    let due = args
        .due
        .as_deref()
        .map(|d| clean_date("due", d))
        .transpose()?;
    let defer = args
        .defer_until
        .as_deref()
        .map(|d| clean_date("defer_until", d))
        .transpose()?;
    let aliases = clean_aliases(args.aliases.as_deref().unwrap_or_default())?;
    let notes = args
        .notes
        .as_deref()
        .map(str::trim)
        .filter(|n| !n.is_empty())
        .map(str::to_string);
    if notes.as_ref().is_some_and(|n| n.len() > MAX_NOTES) {
        return Err(format!(
            "notes are longer than {MAX_NOTES} bytes; trim them."
        ));
    }
    let agent = clean_agent(args.agent.as_deref())?;

    let tx = conn.transaction().map_err(db_err)?;
    let parent = require(&tx, parent_id)?;
    if parent.status.is_closed() {
        return Err(format!(
            "{} {} is {}. Reopen it with set_status, or pick another parent.",
            parent.id,
            parent.title,
            parent.status.as_str()
        ));
    }
    if kind == Kind::Area && parent.kind == Kind::Task {
        return Err(format!(
            "{} {} is a task; areas can only go under areas. Add a task instead, or pick an area as parent.",
            parent.id, parent.title
        ));
    }

    let last: Option<String> = tx
        .query_row(
            "SELECT MAX(position) FROM nodes WHERE parent_id = ?1",
            params![parent_id.0],
            |r| r.get(0),
        )
        .optional()
        .map_err(db_err)?
        .flatten();
    let pos = match last {
        Some(l) => position::after(&l),
        None => position::first(),
    };

    let now = now();
    tx.execute(
        "INSERT INTO nodes (user_id, parent_id, position, kind, ordered, title, priority,
                            due_at, defer_until, aliases, created_at, updated_at, touched_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?11, ?11)",
        params![
            DEFAULT_USER,
            parent_id.0,
            pos,
            kind.as_str(),
            args.ordered.unwrap_or(false),
            title,
            args.priority,
            due,
            defer,
            aliases,
            now
        ],
    )
    .map_err(db_err)?;
    let id = NodeId(tx.last_insert_rowid());
    if let Some(body) = &notes {
        tx.execute(
            "INSERT INTO notes (node_id, body) VALUES (?1, ?2)",
            params![id.0, body],
        )
        .map_err(db_err)?;
    }
    log_event(&tx, id, &now, agent.as_deref(), "created", &title).map_err(db_err)?;
    touch(&tx, id, &now).map_err(db_err)?;
    tx.commit().map_err(db_err)?;

    Ok(format!(
        "Added {id} {}  {title}  (under {} {})",
        kind.as_str(),
        parent.id,
        parent.title
    ))
}

fn clean_title(raw: &str) -> Result<String, String> {
    let title = raw.split_whitespace().collect::<Vec<_>>().join(" ");
    if title.is_empty() {
        return Err("title is empty. Give the node a short title.".to_string());
    }
    if title.chars().count() > MAX_TITLE {
        return Err(format!(
            "title is longer than {MAX_TITLE} characters. Keep it short and put detail in notes."
        ));
    }
    Ok(title)
}

/// Accepts YYYY-MM-DD (or a timestamp starting with one) and stores the date.
fn clean_date(field: &str, raw: &str) -> Result<String, String> {
    let raw = raw.trim();
    match parse_date(raw) {
        Some(d) if raw.len() == 10 || raw.as_bytes().get(10) == Some(&b'T') => {
            Ok(d.format("%Y-%m-%d").to_string())
        }
        _ => Err(format!(
            "{field} '{raw}' is not a date. Use YYYY-MM-DD, e.g. 2026-11-20."
        )),
    }
}

/// Aliases are stored newline-separated, as the schema describes.
fn clean_aliases(raw: &[String]) -> Result<Option<String>, String> {
    let list: Vec<String> = raw
        .iter()
        .map(|a| a.split_whitespace().collect::<Vec<_>>().join(" "))
        .filter(|a| !a.is_empty())
        .collect();
    if list.len() > MAX_ALIASES {
        return Err(format!(
            "give at most {MAX_ALIASES} aliases (2 to 5 is ideal)."
        ));
    }
    if list.iter().any(|a| a.chars().count() > MAX_ALIAS) {
        return Err(format!("each alias must be under {MAX_ALIAS} characters."));
    }
    Ok((!list.is_empty()).then(|| list.join("\n")))
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::model::IdArg;
    use crate::store::test_support::temp_db;

    pub(crate) fn args(parent: Option<i64>, title: &str, kind: Kind) -> AddArgs {
        AddArgs {
            parent: parent.map(IdArg::Number),
            title: title.into(),
            kind: Some(kind),
            ordered: None,
            priority: None,
            due: None,
            defer_until: None,
            aliases: None,
            notes: None,
            agent: Some("test".into()),
        }
    }

    #[test]
    fn adds_under_root_and_logs_event() {
        let (mut conn, dir) = temp_db("add");
        let out = add(&mut conn, args(None, "  Home  ", Kind::Area)).unwrap();
        assert_eq!(out, "Added SPOKE-2 area  Home  (under SPOKE-1 To do)");
        let (count, agent): (i64, String) = conn
            .query_row(
                "SELECT COUNT(*), MAX(agent) FROM events WHERE node_id = 2 AND type = 'created'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!((count, agent.as_str()), (1, "test"));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn rejects_bad_input_with_guidance() {
        let (mut conn, dir) = temp_db("add-bad");
        assert!(
            add(&mut conn, args(Some(99), "x", Kind::Task))
                .unwrap_err()
                .contains("SPOKE-99 does not exist")
        );
        assert!(
            add(&mut conn, args(None, "   ", Kind::Task))
                .unwrap_err()
                .contains("empty")
        );
        let mut a = args(None, "x", Kind::Task);
        a.due = Some("next friday".into());
        assert!(add(&mut conn, a).unwrap_err().contains("YYYY-MM-DD"));
        let mut a = args(None, "x", Kind::Task);
        a.priority = Some(7);
        assert!(add(&mut conn, a).is_err());
        add(&mut conn, args(None, "Task", Kind::Task)).unwrap();
        assert!(
            add(&mut conn, args(Some(2), "Area", Kind::Area))
                .unwrap_err()
                .contains("areas can only go under areas")
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn siblings_keep_insertion_order() {
        let (mut conn, dir) = temp_db("add-order");
        for t in ["a", "b", "c"] {
            add(&mut conn, args(None, t, Kind::Task)).unwrap();
        }
        let titles: Vec<String> = crate::store::children(&conn, ROOT_ID)
            .unwrap()
            .into_iter()
            .map(|n| n.title)
            .collect();
        assert_eq!(titles, ["a", "b", "c"]);
        let _ = std::fs::remove_dir_all(dir);
    }
}
