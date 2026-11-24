//! `search`: find nodes by several keywords, matched as alternatives.
//!
//! Candidates come from the trigram FTS5 index (`node_fts`), which holds normalized
//! title, aliases and notes. Each candidate is then scored in Rust per keyword:
//! whole word > word prefix > substring > typo (edit distance or shared trigrams), and
//! title (x3) > aliases (x2) > notes (x1, exact matches only). Keywords under three
//! letters, which trigrams cannot see, match whole words in titles and aliases only.

use std::collections::HashMap;

use chrono::NaiveDate;
use rusqlite::{Connection, params};
use serde::Serialize;

use super::focus::focus_numbers;
use super::view::Crumb;
use super::{ToolResult, db_err, require};
use crate::format::{due_flag, is_unplanned, parse_date, progress, short_date};
use crate::model::{DEFAULT_USER, Node, NodeId, ROOT_ID, Status};
use crate::text::{keywords, match_strength, normalize, trigrams};
use crate::tools::args::SearchArgs;

const DEFAULT_LIMIT: i64 = 10;
const MAX_LIMIT: i64 = 50;
const MAX_KEYWORDS: usize = 12;
/// FTS candidates scored per call; ranked by bm25 first, so the best survive the cut.
const MAX_CANDIDATES: i64 = 500;

const W_TITLE: f64 = 3.0;
const W_ALIAS: f64 = 2.0;
const W_NOTES: f64 = 1.0;

/// Normalized index text of one node.
struct Doc {
    title: String,
    aliases: String,
    notes: String,
}

/// The parts of every node needed for scope, closed ancestors, paths and levels.
struct Meta {
    parent: Option<NodeId>,
    title: String,
    status: Status,
    priority: Option<i64>,
}

#[derive(Clone, Copy, PartialEq)]
enum Field {
    Title,
    Alias,
    Notes,
}

struct Hit {
    id: NodeId,
    score: f64,
    /// Set when nothing matched the title: why this node is in the results.
    via: Option<String>,
}

/// Keywords of a query as `search` uses them (empty when it has no words).
pub(crate) fn query_keywords(query: &str) -> Vec<String> {
    let mut kws = keywords(query);
    kws.truncate(MAX_KEYWORDS);
    kws
}

/// Ranked matches before they are formatted: shared by the MCP tool and `/api/search`.
struct Found {
    /// Best first.
    hits: Vec<Hit>,
    hidden_closed: usize,
    meta: HashMap<NodeId, Meta>,
}

/// Scores every candidate for `kws` (non-empty) and ranks them, keeping only nodes under
/// `scope` and, unless `include_done`, nodes with nothing closed at or above them.
fn find(
    conn: &Connection,
    query: &str,
    kws: &[String],
    scope: NodeId,
    include_done: bool,
) -> Result<Found, String> {
    let phrase = format!(" {} ", normalize(query));
    let docs = candidates(conn, kws)?;
    let mut hits: Vec<Hit> = docs
        .iter()
        .filter_map(|(id, doc)| score(*id, doc, kws, &phrase))
        .collect();
    // Typo fallback: a keyword nothing resembles in a title or alias gets a full scan
    // of titles and aliases (short columns), since a typo may share no trigram at all.
    let unmatched = kws.iter().filter(|k| k.chars().count() >= 4).any(|k| {
        !docs
            .values()
            .any(|d| best(k, &d.title, true) >= 0.6 || best(k, &d.aliases, true) >= 0.6)
    });
    if unmatched {
        for (id, doc) in scan_titles(conn)? {
            if docs.contains_key(&id) {
                continue;
            }
            if let Some(hit) = score(id, &doc, kws, &phrase) {
                hits.push(hit);
            }
        }
    }

    let meta = load_meta(conn)?;
    let mut hidden_closed = 0;
    hits.retain(|h| {
        if h.id == ROOT_ID || !within(&meta, h.id, scope) {
            return false;
        }
        if !include_done && closed_here_or_above(&meta, h.id) {
            hidden_closed += 1;
            return false;
        }
        true
    });
    hits.sort_by(|a, b| {
        b.score
            .total_cmp(&a.score)
            .then_with(|| {
                let level = |id| effective_level(&meta, id).unwrap_or(5);
                level(a.id).cmp(&level(b.id))
            })
            .then(a.id.cmp(&b.id))
    });
    Ok(Found {
        hits,
        hidden_closed,
        meta,
    })
}

pub fn search(conn: &Connection, args: SearchArgs, today: NaiveDate) -> ToolResult {
    let kws = query_keywords(&args.query);
    if kws.is_empty() {
        return Err(
            "query has no words. Pass a few keywords as alternatives, e.g. \"server homelab nas\"."
                .to_string(),
        );
    }
    let limit = match &args.limit {
        Some(l) => l.parse("limit")?.clamp(1, MAX_LIMIT),
        None => DEFAULT_LIMIT,
    } as usize;
    let scope = match &args.scope {
        Some(s) => require(conn, s.parse()?)?.id,
        None => ROOT_ID,
    };
    let include_done = args.include_done.unwrap_or(false);
    let Found {
        hits,
        hidden_closed,
        meta,
    } = find(conn, &args.query, &kws, scope, include_done)?;

    let hidden_note = (hidden_closed > 0).then(|| {
        format!(
            "({hidden_closed} done or dropped matches hidden; pass include_done: true to see them.)"
        )
    });
    if hits.is_empty() {
        let mut out = format!(
            "No matches for {}. Try synonyms or other spellings, or browse with get {ROOT_ID}.",
            kws.join(", ")
        );
        if let Some(note) = hidden_note {
            out.push('\n');
            out.push_str(&note);
        }
        return Ok(out);
    }

    let focus = focus_numbers(conn)?;
    let mut out = vec![format!(
        "{} {} for {}:",
        hits.len(),
        if hits.len() == 1 { "match" } else { "matches" },
        kws.join(", ")
    )];
    for hit in hits.iter().take(limit) {
        let node = require(conn, hit.id)?;
        out.push(format!(
            "  {}",
            result_line(&node, &meta, &focus, hit, today)
        ));
    }
    if hits.len() > limit {
        out.push(format!(
            "(+{} more matches. Add or change keywords, or pass scope.)",
            hits.len() - limit
        ));
    }
    if let Some(note) = hidden_note {
        out.push(note);
    }
    Ok(out.join("\n"))
}

/// Default and largest `limit` of `/api/search`.
pub const API_DEFAULT_LIMIT: usize = DEFAULT_LIMIT as usize;
pub const API_MAX_LIMIT: usize = MAX_LIMIT as usize;

/// One match as `/api/search` shows it.
#[derive(Debug, Serialize)]
pub struct ApiHit {
    pub id: i64,
    pub title: String,
    pub kind: &'static str,
    pub status: &'static str,
    pub effective_priority: Option<i64>,
    pub focus: Option<usize>,
    /// Ancestors from the root down (the root included, the node itself not).
    pub path: Vec<Crumb>,
    /// Why it matched when the title did not: `alias "nas"` or `in notes`.
    pub via: Option<String>,
}

/// The `/api/search` body, apart from the common fields.
#[derive(Debug, Serialize)]
pub struct ApiSearch {
    pub keywords: Vec<String>,
    /// Matches found (before `limit`).
    pub total: usize,
    pub hidden_closed: usize,
    pub results: Vec<ApiHit>,
}

/// The same ranking as the `search` tool, as data. `kws` comes from [`query_keywords`]
/// and is non-empty; `scope` must exist (checked by the caller).
pub fn api_search(
    conn: &Connection,
    query: &str,
    kws: Vec<String>,
    scope: NodeId,
    include_closed: bool,
    limit: usize,
) -> Result<ApiSearch, String> {
    let Found {
        hits,
        hidden_closed,
        meta,
    } = find(conn, query, &kws, scope, include_closed)?;
    let focus = focus_numbers(conn)?;
    let mut results = Vec::new();
    for hit in hits.iter().take(limit) {
        let node = require(conn, hit.id)?;
        let mut path: Vec<Crumb> = lineage(&meta, hit.id)
            .skip(1)
            .map(|(id, m)| Crumb {
                id: id.0,
                title: m.title.clone(),
            })
            .collect();
        path.reverse();
        results.push(ApiHit {
            id: node.id.0,
            title: node.title,
            kind: node.kind.as_str(),
            status: node.status.as_str(),
            effective_priority: effective_level(&meta, hit.id),
            focus: focus.get(&hit.id).copied(),
            path,
            via: hit.via.clone(),
        });
    }
    Ok(ApiSearch {
        keywords: kws,
        total: hits.len(),
        hidden_closed,
        results,
    })
}

/// `SPOKE-30 task p1 (0/4)  Homelab server migration  in Hobbies / Homelab  (alias "nas")`.
fn result_line(
    node: &Node,
    meta: &HashMap<NodeId, Meta>,
    focus: &HashMap<NodeId, usize>,
    hit: &Hit,
    today: NaiveDate,
) -> String {
    let mut flags = vec![node.id.to_string(), node.kind.as_str().to_string()];
    if node.status != Status::Open {
        flags.push(node.status.as_str().to_string());
    }
    if let Some(level) = effective_level(meta, node.id) {
        flags.push(format!("p{level}"));
    }
    if let Some(n) = focus.get(&node.id) {
        flags.push(format!("focus #{n}"));
    }
    if let Some(due) = &node.due_at {
        flags.push(due_flag(due, today));
    }
    if let Some(defer) = node.defer_until.as_deref()
        && parse_date(defer).is_some_and(|d| d > today)
    {
        flags.push(format!("deferred until {}", short_date(defer, today)));
    }
    if node.recur.is_some() {
        flags.push("repeats".to_string());
    }
    if let Some(p) = progress(node) {
        flags.push(p);
    }
    if node.has_notes {
        flags.push("+notes".to_string());
    }
    if is_unplanned(node) {
        flags.push("unplanned".to_string());
    }
    let path = path_of(meta, node.id);
    let mut line = format!(
        "{}  {}  in {}",
        flags.join(" "),
        node.title,
        if path.is_empty() {
            "To do (top level)".to_string()
        } else {
            path
        }
    );
    if let Some(via) = &hit.via {
        line.push_str(&format!("  ({via})"));
    }
    line
}

/// FTS candidates for keywords of three letters or more (any shared trigram, best bm25
/// first), plus whole-word title/alias candidates for shorter keywords.
fn candidates(conn: &Connection, kws: &[String]) -> Result<HashMap<NodeId, Doc>, String> {
    let mut docs = HashMap::new();
    let mut grams: Vec<String> = Vec::new();
    for kw in kws {
        for g in trigrams(kw) {
            if !grams.contains(&g) {
                grams.push(g);
            }
        }
    }
    let read = |r: &rusqlite::Row<'_>| -> rusqlite::Result<(NodeId, Doc)> {
        Ok((
            NodeId(r.get(0)?),
            Doc {
                title: r.get(1)?,
                aliases: r.get(2)?,
                notes: r.get(3)?,
            },
        ))
    };
    if !grams.is_empty() {
        // Trigrams are alphanumeric (normalized), so quoting them is safe.
        let expr = grams
            .iter()
            .map(|g| format!("\"{g}\""))
            .collect::<Vec<_>>()
            .join(" OR ");
        let mut stmt = conn
            .prepare_cached(
                "SELECT rowid, title, aliases, notes FROM node_fts WHERE node_fts MATCH ?1
                 ORDER BY bm25(node_fts, 3.0, 2.0, 1.0) LIMIT ?2",
            )
            .map_err(db_err)?;
        let rows = stmt
            .query_map(params![expr, MAX_CANDIDATES], read)
            .and_then(|rows| rows.collect::<rusqlite::Result<Vec<_>>>())
            .map_err(db_err)?;
        docs.extend(rows);
    }
    for kw in kws.iter().filter(|k| k.chars().count() < 3) {
        let mut stmt = conn
            .prepare_cached(
                "SELECT rowid, title, aliases, notes FROM node_fts
                 WHERE title LIKE ?1 OR aliases LIKE ?1 LIMIT ?2",
            )
            .map_err(db_err)?;
        let rows = stmt
            .query_map(params![format!("%{kw}%"), MAX_CANDIDATES], read)
            .and_then(|rows| rows.collect::<rusqlite::Result<Vec<_>>>())
            .map_err(db_err)?;
        docs.extend(rows);
    }
    Ok(docs)
}

/// Every node's title and aliases (no notes), for the typo fallback.
fn scan_titles(conn: &Connection) -> Result<Vec<(NodeId, Doc)>, String> {
    let mut stmt = conn
        .prepare_cached("SELECT rowid, title, aliases FROM node_fts")
        .map_err(db_err)?;
    stmt.query_map([], |r| {
        Ok((
            NodeId(r.get(0)?),
            Doc {
                title: r.get(1)?,
                aliases: r.get(2)?,
                notes: String::new(),
            },
        ))
    })
    .and_then(|rows| rows.collect())
    .map_err(db_err)
}

/// The best strength of `kw` over the lines of one index column (each alias, and each
/// squashed form, is its own line).
fn best(kw: &str, column: &str, fuzzy: bool) -> f64 {
    column
        .lines()
        .map(|line| match_strength(kw, line, fuzzy))
        .fold(0.0, f64::max)
}

fn best_line<'a>(kw: &str, column: &'a str) -> Option<&'a str> {
    column
        .lines()
        .map(|line| (line, match_strength(kw, line, true)))
        .filter(|(_, s)| *s > 0.0)
        .max_by(|a, b| a.1.total_cmp(&b.1))
        .map(|(line, _)| line)
}

/// Scores one node. `phrase` is the whole normalized query padded with spaces: a
/// multi-word title or alias that appears in it whole (`my server` in "tasks for my
/// server") earns a bonus, so the user's own words beat a bare keyword hit.
fn score(id: NodeId, doc: &Doc, kws: &[String], phrase: &str) -> Option<Hit> {
    let mut total = 0.0;
    let mut title_hit = false;
    let mut via: Option<String> = None;
    let contains_phrase = |column: &str| {
        column
            .lines()
            .find(|line| line.contains(' ') && phrase.contains(&format!(" {line} ")))
            .map(str::to_string)
    };
    if contains_phrase(&doc.title).is_some() {
        total += W_TITLE;
        title_hit = true;
    }
    if let Some(alias) = contains_phrase(&doc.aliases) {
        total += W_ALIAS;
        via = Some(format!("alias \"{alias}\""));
    }
    for kw in kws {
        let options = [
            (W_TITLE * best(kw, &doc.title, true), Field::Title),
            (W_ALIAS * best(kw, &doc.aliases, true), Field::Alias),
            // Notes are long prose: exact words and fragments only, no typo matching.
            (W_NOTES * best(kw, &doc.notes, false), Field::Notes),
        ];
        let (s, field) = options
            .into_iter()
            .max_by(|a, b| a.0.total_cmp(&b.0))
            .expect("three options");
        if s <= 0.0 {
            continue;
        }
        total += s;
        match field {
            Field::Title => title_hit = true,
            Field::Alias if via.is_none() => {
                via = best_line(kw, &doc.aliases).map(|a| format!("alias \"{a}\""));
            }
            Field::Notes if via.is_none() => via = Some("in notes".to_string()),
            _ => {}
        }
    }
    (total > 0.0).then_some(Hit {
        id,
        score: total,
        via: if title_hit { None } else { via },
    })
}

fn load_meta(conn: &Connection) -> Result<HashMap<NodeId, Meta>, String> {
    let mut stmt = conn
        .prepare_cached(
            "SELECT id, parent_id, title, status, priority FROM nodes WHERE user_id = ?1",
        )
        .map_err(db_err)?;
    stmt.query_map(params![DEFAULT_USER], |r| {
        let status: String = r.get(3)?;
        Ok((
            NodeId(r.get(0)?),
            Meta {
                parent: r.get::<_, Option<i64>>(1)?.map(NodeId),
                title: r.get(2)?,
                status: status.parse().unwrap_or(Status::Open),
                priority: r.get(4)?,
            },
        ))
    })
    .and_then(|rows| rows.collect())
    .map_err(db_err)
}

/// Walks from `id` up to the root, yielding `id` first. Bounded so a corrupt cycle
/// cannot hang the call.
fn lineage(meta: &HashMap<NodeId, Meta>, id: NodeId) -> impl Iterator<Item = (NodeId, &Meta)> {
    let mut cur = Some(id);
    std::iter::from_fn(move || {
        let id = cur?;
        let m = meta.get(&id)?;
        cur = m.parent;
        Some((id, m))
    })
    .take(10_000)
}

fn within(meta: &HashMap<NodeId, Meta>, id: NodeId, scope: NodeId) -> bool {
    scope == ROOT_ID || lineage(meta, id).skip(1).any(|(a, _)| a == scope)
}

fn closed_here_or_above(meta: &HashMap<NodeId, Meta>, id: NodeId) -> bool {
    lineage(meta, id).any(|(_, m)| m.status.is_closed())
}

/// The node's own level, or the nearest ancestor's.
fn effective_level(meta: &HashMap<NodeId, Meta>, id: NodeId) -> Option<i64> {
    lineage(meta, id).find_map(|(_, m)| m.priority)
}

/// Breadcrumb below the root: `Hobbies / Homelab`.
fn path_of(meta: &HashMap<NodeId, Meta>, id: NodeId) -> String {
    let mut titles: Vec<&str> = lineage(meta, id)
        .skip(1)
        .filter(|(_, m)| m.parent.is_some())
        .map(|(_, m)| m.title.as_str())
        .collect();
    titles.reverse();
    titles.join(" / ")
}
