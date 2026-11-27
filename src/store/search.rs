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
use super::{ToolResult, db_err, refuse_stage, require};
use crate::format::{due_flag, is_unplanned, parse_date, progress, short_date};
use crate::model::{DEFAULT_USER, Node, NodeId, ROOT_ID, Refinement, Status, WorkType};
use crate::text::{keywords, match_strength, normalize, squash, trigrams};
use crate::tools::args::SearchArgs;

const DEFAULT_LIMIT: i64 = 10;
const MAX_LIMIT: i64 = 50;
const MAX_KEYWORDS: usize = 12;
/// Furthest page start (`offset`) a search accepts.
pub const MAX_OFFSET: usize = 10_000;
/// FTS candidates scored per call; ranked by bm25 first, so the best survive the cut.
const MAX_CANDIDATES: i64 = 500;

/// How much a keyword hit counts per field: a title hit fully, an alias nearly, a notes
/// hit much less (long prose mentions many things, and feedback notes often quote whole
/// queries).
const W_TITLE: f64 = 1.0;
const W_ALIAS: f64 = 0.85;
const W_NOTES: f64 = 0.4;
/// The rarest keyword of a query weighs 1, the commonest no less than this.
const MIN_KEYWORD_WEIGHT: f64 = 0.25;
/// A node with children whose title or aliases hold at least a third of the matched
/// keywords: the usual "where do I add this" parent.
const PARENT_BONUS: f64 = 1.15;
/// Notes-only keyword hits add up to at most this (about one title word): a note that
/// mentions or quotes many of the words must not outrank a node named by them.
const NOTES_CAP: f64 = 1.2;
/// A parent whose own descendants also match is where those results cluster: each such
/// descendant lifts it by this share, up to `CLUSTER_MAX` of them.
const CLUSTER_STEP: f64 = 0.06;
const CLUSTER_MAX: usize = 5;
/// With two or more keywords, results scoring under this share of the best are left out
/// by default (`match: "best"`), apart from the first few.
const QUIET_SHARE: f64 = 0.4;
const QUIET_KEEP: usize = 3;

/// How keywords combine.
#[derive(Clone, Copy, Debug, PartialEq, Default)]
pub enum MatchMode {
    /// Any keyword, but drop the much weaker matches (the default).
    #[default]
    Best,
    /// Every node matching any keyword.
    Any,
    /// Only nodes matching every keyword.
    All,
}

impl MatchMode {
    pub fn parse(s: &str) -> Result<Self, String> {
        match s.trim().to_ascii_lowercase().as_str() {
            "best" | "" => Ok(MatchMode::Best),
            "any" => Ok(MatchMode::Any),
            "all" => Ok(MatchMode::All),
            other => Err(format!(
                "match {other:?} is not known. Use best (the default), any or all."
            )),
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            MatchMode::Best => "best",
            MatchMode::Any => "any",
            MatchMode::All => "all",
        }
    }
}

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
    work_type: Option<WorkType>,
    refinement: Option<Refinement>,
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
    /// The node's own id was typed in the query (`958`, `SPOKE-958`).
    id_match: bool,
    /// The keywords this node matched, in query order.
    matched: Vec<String>,
    /// Per keyword (aligned with the query's keywords): the field-weighted strength.
    strengths: Vec<f64>,
    /// Per keyword: matched in the title or an alias, not only in notes.
    head: Vec<bool>,
    /// The whole-phrase bonus.
    bonus: f64,
}

/// Keywords of a query as `search` uses them (empty when it has no words). A bare
/// `spoke` that only marks the number after it (`spoke 958`) is not a keyword.
pub(crate) fn query_keywords(query: &str) -> Vec<String> {
    let mut kws = keywords(query);
    if query_ids_parts(query).1 {
        kws.retain(|k| k != "spoke");
    }
    kws.truncate(MAX_KEYWORDS);
    kws
}

/// The node ids typed in a query, in the order typed: `958`, `#958`, `SPOKE-958`,
/// `spoke958`, or `spoke` followed by `958`.
pub(crate) fn query_ids(query: &str) -> Vec<NodeId> {
    query_ids_parts(query).0
}

/// [`query_ids`], plus whether a bare `spoke` word served only as an id marker.
fn query_ids_parts(query: &str) -> (Vec<NodeId>, bool) {
    let words: Vec<String> = query
        .split(|c: char| c.is_whitespace() || c == ',' || c == ';')
        .map(|w| squash(&normalize(w)))
        .filter(|w| !w.is_empty())
        .collect();
    let number = |w: &str| -> Option<i64> {
        (!w.is_empty() && w.len() <= 15 && w.bytes().all(|b| b.is_ascii_digit()))
            .then(|| w.parse().ok())
            .flatten()
    };
    let mut ids: Vec<NodeId> = Vec::new();
    let mut marker = false;
    let mut push = |n: i64| {
        if !ids.contains(&NodeId(n)) {
            ids.push(NodeId(n));
        }
    };
    for (i, w) in words.iter().enumerate() {
        if w == "spoke" {
            if words.get(i + 1).and_then(|next| number(next)).is_some() {
                marker = true;
            }
            continue;
        }
        if let Some(n) = number(w.strip_prefix("spoke").unwrap_or(w)) {
            push(n);
        }
    }
    (ids, marker)
}

/// Ranked matches before they are formatted: shared by the MCP tool and `/api/search`.
struct Found {
    /// Best first.
    hits: Vec<Hit>,
    hidden_closed: usize,
    /// Weaker matches left out by `match: "best"`.
    weaker_hidden: usize,
    meta: HashMap<NodeId, Meta>,
}

/// Which closed nodes a search may return: done ones, dropped ones (each with what lies
/// under it).
#[derive(Clone, Copy, Debug, Default)]
pub struct Show {
    pub done: bool,
    pub dropped: bool,
}

/// Type and refinement filters: a node's own values must equal every filter given.
#[derive(Clone, Copy, Default)]
struct Filters {
    work_type: Option<WorkType>,
    refinement: Option<Refinement>,
}

impl Filters {
    fn is_empty(&self) -> bool {
        self.work_type.is_none() && self.refinement.is_none()
    }
}

/// Scores every candidate for `kws` and ranks them, keeping only nodes under `scope`
/// that pass `filters` and, unless `include_done`, nodes with nothing closed at or above
/// them. With no keywords (filters only), every node passing the filters matches.
fn find(
    conn: &Connection,
    query: &str,
    kws: &[String],
    filters: Filters,
    mode: MatchMode,
    scope: NodeId,
    show: Show,
) -> Result<Found, String> {
    let meta = load_meta(conn)?;
    let mut hits = if kws.is_empty() {
        meta.keys()
            .map(|id| Hit {
                id: *id,
                score: 0.0,
                via: None,
                id_match: false,
                matched: Vec::new(),
                strengths: Vec::new(),
                head: Vec::new(),
                bonus: 0.0,
            })
            .collect()
    } else {
        let mut hits = keyword_hits(conn, query, kws)?;
        weigh(&mut hits, kws.len(), meta.len());
        let live = live_keywords(&hits, kws.len());
        parent_bonus(&mut hits, &meta, live);
        if mode == MatchMode::All {
            hits.retain(|h| h.matched.len() == kws.len());
        }
        hits
    };
    // A typed id is unambiguous: that node comes first, in the order typed, even when
    // it is done or dropped (every node stays reachable by id). Scope and the type and
    // refinement filters still apply.
    let ids: Vec<NodeId> = query_ids(query)
        .into_iter()
        .filter(|id| {
            *id != ROOT_ID
                && meta.contains_key(id)
                && within(&meta, *id, scope)
                && matches_filters(&meta, *id, filters)
        })
        .collect();
    hits.retain(|h| !ids.contains(&h.id));
    let id_hits: Vec<Hit> = ids
        .iter()
        .map(|id| Hit {
            id: *id,
            score: f64::INFINITY,
            via: Some("id".to_string()),
            id_match: true,
            matched: kws
                .iter()
                .filter(|k| k.strip_prefix("spoke").unwrap_or(k) == id.0.to_string())
                .cloned()
                .collect(),
            strengths: Vec::new(),
            head: Vec::new(),
            bonus: 0.0,
        })
        .collect();
    let mut hidden_closed = 0;
    hits.retain(|h| {
        if h.id == ROOT_ID || !within(&meta, h.id, scope) || !matches_filters(&meta, h.id, filters)
        {
            return false;
        }
        if hidden_here_or_above(&meta, h.id, show.done, show.dropped) {
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
    let mut weaker_hidden = 0;
    if mode == MatchMode::Best
        && kws.len() >= 2
        && let Some(top) = hits.first().map(|h| h.score)
    {
        let cut = top * QUIET_SHARE;
        let before = hits.len();
        let mut i = 0;
        hits.retain(|h| {
            i += 1;
            i <= QUIET_KEEP || h.score >= cut
        });
        weaker_hidden = before - hits.len();
    }
    hits.splice(0..0, id_hits);
    Ok(Found {
        hits,
        hidden_closed,
        weaker_hidden,
        meta,
    })
}

/// Candidate hits for keywords: FTS scoring plus the typo fallback.
fn keyword_hits(conn: &Connection, query: &str, kws: &[String]) -> Result<Vec<Hit>, String> {
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
    Ok(hits)
}

/// Final scores: each keyword counts by its rarity among the matches (a word half the
/// tree mentions says little), times how strongly and where it matched, plus the
/// phrase bonus. A node matching more of the rarer keywords therefore ranks first.
fn weigh(hits: &mut [Hit], n_kws: usize, n_nodes: usize) {
    let n = n_nodes.max(1) as f64;
    let raw: Vec<f64> = (0..n_kws)
        .map(|i| {
            let df = hits.iter().filter(|h| h.strengths[i] > 0.0).count().max(1) as f64;
            (1.0 + n / df).ln()
        })
        .collect();
    let top = raw.iter().copied().fold(0.0, f64::max);
    let weights: Vec<f64> = raw
        .iter()
        .map(|w| {
            let share = if top > 0.0 { w / top } else { 1.0 };
            MIN_KEYWORD_WEIGHT + (1.0 - MIN_KEYWORD_WEIGHT) * share
        })
        .collect();
    for h in hits.iter_mut() {
        let (mut named, mut notes) = (0.0, 0.0);
        for ((s, w), head) in h.strengths.iter().zip(&weights).zip(&h.head) {
            if *head {
                named += s * w;
            } else {
                notes += s * w;
            }
        }
        h.score = named + notes.min(NOTES_CAP) + h.bonus;
    }
}

/// How many keywords matched at least one node (a keyword nothing has does not count
/// against anyone).
fn live_keywords(hits: &[Hit], n_kws: usize) -> usize {
    (0..n_kws)
        .filter(|i| {
            hits.iter()
                .any(|h| h.strengths.get(*i).is_some_and(|s| *s > 0.0))
        })
        .count()
}

/// A parent (any node with children) whose title or aliases carry at least a third of
/// the matched keywords is the likely place to add under, so it gets a small lift.
fn parent_bonus(hits: &mut [Hit], meta: &HashMap<NodeId, Meta>, n_kws: usize) {
    let parents: std::collections::HashSet<NodeId> =
        meta.values().filter_map(|m| m.parent).collect();
    let matched: std::collections::HashSet<NodeId> = hits.iter().map(|h| h.id).collect();
    let mut below: HashMap<NodeId, usize> = HashMap::new();
    for h in hits.iter() {
        for (a, _) in lineage(meta, h.id).skip(1) {
            if matched.contains(&a) {
                *below.entry(a).or_default() += 1;
            }
        }
    }
    for h in hits.iter_mut() {
        let head = h.head.iter().filter(|b| **b).count();
        if parents.contains(&h.id) && head > 0 && head * 3 >= n_kws {
            let cluster = below.get(&h.id).copied().unwrap_or(0).min(CLUSTER_MAX);
            h.score *= PARENT_BONUS + CLUSTER_STEP * cluster as f64;
        }
    }
}

/// The node's own type and refinement must equal every filter given.
fn matches_filters(meta: &HashMap<NodeId, Meta>, id: NodeId, filters: Filters) -> bool {
    let Some(m) = meta.get(&id) else {
        return false;
    };
    filters.work_type.is_none_or(|t| m.work_type == Some(t))
        && filters.refinement.is_none_or(|s| m.refinement == Some(s))
}

/// `type bug, refinement planned`.
fn filter_label(filters: Filters) -> String {
    let mut parts = Vec::new();
    if let Some(t) = filters.work_type {
        parts.push(format!("type {t}"));
    }
    if let Some(s) = filters.refinement {
        parts.push(format!("refinement {s}"));
    }
    parts.join(", ")
}

pub fn search(conn: &Connection, args: SearchArgs, today: NaiveDate) -> ToolResult {
    refuse_stage(args.stage.as_ref())?;
    let filters = Filters {
        work_type: args
            .work_type
            .as_deref()
            .map(WorkType::parse_loose)
            .transpose()?,
        refinement: args
            .refinement
            .as_deref()
            .map(Refinement::parse_loose)
            .transpose()?,
    };
    let kws = query_keywords(&args.query);
    if kws.is_empty() && filters.is_empty() {
        return Err(
            "query has no words. Pass a few keywords as alternatives, e.g. \"server homelab nas\"."
                .to_string(),
        );
    }
    let full_paths = args.full_paths.unwrap_or(false);
    let limit = match &args.limit {
        Some(l) => l.parse("limit")?.clamp(1, MAX_LIMIT),
        None => DEFAULT_LIMIT,
    } as usize;
    let offset = match &args.offset {
        Some(o) => {
            let o = o.parse("offset")?;
            if !(0..=MAX_OFFSET as i64).contains(&o) {
                return Err(format!("offset must be 0 to {MAX_OFFSET}."));
            }
            o as usize
        }
        None => 0,
    };
    let scope = match &args.scope {
        Some(s) => require(conn, s.parse()?)?.id,
        None => ROOT_ID,
    };
    let include_done = args.include_done.unwrap_or(false);
    let mode = match &args.match_mode {
        Some(m) => MatchMode::parse(m)?,
        None => MatchMode::Best,
    };
    let Found {
        hits,
        hidden_closed,
        weaker_hidden,
        meta,
    } = find(
        conn,
        &args.query,
        &kws,
        filters,
        mode,
        scope,
        Show {
            done: include_done,
            dropped: include_done,
        },
    )?;
    let weaker_note = (weaker_hidden > 0).then(|| {
        format!(
            "({weaker_hidden} weaker matches, on fewer or commoner keywords, left out; pass match: \"any\" to list them, or match: \"all\" to narrow.)"
        )
    });

    let hidden_note = (hidden_closed > 0).then(|| {
        format!(
            "({hidden_closed} done or dropped matches hidden; pass include_done: true to see them.)"
        )
    });
    let subject = if kws.is_empty() {
        filter_label(filters)
    } else {
        kws.join(", ")
    };
    if hits.is_empty() {
        let mut out = if kws.is_empty() {
            format!("No matches for {subject}. Browse with get {ROOT_ID}.")
        } else {
            format!(
                "No matches for {subject}. Try synonyms or other spellings, or browse with get {ROOT_ID}."
            )
        };
        if let Some(note) = hidden_note {
            out.push('\n');
            out.push_str(&note);
        }
        if mode == MatchMode::All && kws.len() > 1 {
            out.push_str(
                "\n(No node has every keyword; drop match: \"all\" to see partial matches.)",
            );
        }
        return Ok(out);
    }

    let focus = focus_numbers(conn)?;
    // Keywords some match has: one nothing matched is not "missing" from every result.
    let live = kws
        .iter()
        .filter(|k| hits.iter().any(|h| h.matched.contains(k)))
        .count();
    let shown_end = (offset + limit).min(hits.len());
    let range = if offset > 0 && offset < hits.len() {
        format!(" (showing {}-{})", offset + 1, shown_end)
    } else {
        String::new()
    };
    let mut out = vec![format!(
        "{} {} for {}{range}:",
        hits.len(),
        if hits.len() == 1 { "match" } else { "matches" },
        subject
    )];
    if offset >= hits.len() {
        out.push(format!(
            "(offset {offset} is past the last match; pass a smaller offset, or none.)"
        ));
    }
    for hit in hits.iter().skip(offset).take(limit) {
        let node = require(conn, hit.id)?;
        out.push(format!(
            "  {}",
            result_line(&node, &meta, &focus, hit, live, today, full_paths)
        ));
    }
    if hits.len() > shown_end && offset < hits.len() {
        out.push(format!(
            "(+{} more matches. Pass offset: {shown_end} for the next page, or add keywords or scope.)",
            hits.len() - shown_end
        ));
    }
    if let Some(note) = weaker_note {
        out.push(note);
    }
    if let Some(note) = hidden_note {
        out.push(note);
    }
    Ok(out.join("\n"))
}

/// Which slice of the ranked matches to send.
#[derive(Clone, Copy, Debug)]
pub struct Page {
    pub offset: usize,
    pub limit: usize,
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
    /// Why it matched when the title did not: `alias "nas"`, `in notes` or `id`.
    pub via: Option<String>,
    /// The node's own id was typed in the query.
    pub id_match: bool,
    /// The keywords (as in `keywords`) this node matched.
    pub matched: Vec<String>,
    /// The "show" switches (`done`, `dropped`) this request left off that hide the node
    /// or an ancestor. Only an id match can be hidden like this.
    pub hidden_by: Vec<&'static str>,
}

/// The `/api/search` body, apart from the common fields.
#[derive(Debug, Serialize)]
pub struct ApiSearch {
    pub keywords: Vec<String>,
    /// How keywords combined: `best`, `any` or `all`.
    #[serde(rename = "match")]
    pub match_mode: &'static str,
    /// Where this page starts in the ranked matches.
    pub offset: usize,
    /// Matches found (before `offset` and `limit`).
    pub total: usize,
    pub hidden_closed: usize,
    /// Weaker matches left out by `match=best`.
    pub weaker_hidden: usize,
    pub results: Vec<ApiHit>,
}

/// The same ranking as the `search` tool, as data. `kws` comes from [`query_keywords`]
/// and is non-empty; `scope` must exist (checked by the caller).
pub fn api_search(
    conn: &Connection,
    query: &str,
    kws: Vec<String>,
    mode: MatchMode,
    scope: NodeId,
    show: Show,
    page: Page,
) -> Result<ApiSearch, String> {
    let Found {
        hits,
        hidden_closed,
        weaker_hidden,
        meta,
    } = find(conn, query, &kws, Filters::default(), mode, scope, show)?;
    let focus = focus_numbers(conn)?;
    let mut results = Vec::new();
    for hit in hits.iter().skip(page.offset).take(page.limit) {
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
            id_match: hit.id_match,
            matched: hit.matched.clone(),
            hidden_by: hidden_by(&meta, hit.id, show.done, show.dropped),
        });
    }
    Ok(ApiSearch {
        keywords: kws,
        match_mode: mode.as_str(),
        offset: page.offset,
        total: hits.len(),
        hidden_closed,
        weaker_hidden,
        results,
    })
}

/// `SPOKE-30 task p1 (0/4)  Homelab server migration  in Hobbies (SPOKE-2) / Homelab (SPOKE-9)  (alias "nas")`.
fn result_line(
    node: &Node,
    meta: &HashMap<NodeId, Meta>,
    focus: &HashMap<NodeId, usize>,
    hit: &Hit,
    n_kws: usize,
    today: NaiveDate,
    full_paths: bool,
) -> String {
    let mut flags = vec![node.id.to_string(), node.kind.as_str().to_string()];
    if node.status != Status::Open {
        flags.push(node.status.as_str().to_string());
    }
    if let Some(level) = effective_level(meta, node.id) {
        flags.push(format!("p{level}"));
    }
    if let Some(t) = node.work_type {
        flags.push(t.to_string());
    }
    if let Some(st) = node.refinement {
        flags.push(st.to_string());
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
    if node.has_source {
        flags.push("+source".to_string());
    }
    if is_unplanned(node) {
        flags.push("unplanned".to_string());
    }
    let path = path_of(meta, node.id, full_paths);
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
    let mut why: Vec<String> = hit.via.iter().cloned().collect();
    if !hit.id_match && n_kws > 1 && hit.matched.len() < n_kws {
        why.push(format!(
            "matched {} of {n_kws}: {}",
            hit.matched.len(),
            hit.matched.join(", ")
        ));
    }
    if !why.is_empty() {
        line.push_str(&format!("  ({})", why.join("; ")));
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
    let mut bonus = 0.0;
    let mut title_hit = false;
    let mut via: Option<String> = None;
    let mut matched: Vec<String> = Vec::new();
    let mut strengths: Vec<f64> = Vec::with_capacity(kws.len());
    let mut head: Vec<bool> = Vec::with_capacity(kws.len());
    let contains_phrase = |column: &str| {
        column
            .lines()
            .find(|line| line.contains(' ') && phrase.contains(&format!(" {line} ")))
            .map(str::to_string)
    };
    if contains_phrase(&doc.title).is_some() {
        bonus += W_TITLE;
        title_hit = true;
    }
    if let Some(alias) = contains_phrase(&doc.aliases) {
        bonus += W_ALIAS;
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
        strengths.push(s.max(0.0));
        head.push(s > 0.0 && field != Field::Notes);
        if s <= 0.0 {
            continue;
        }
        total += s;
        matched.push(kw.clone());
        match field {
            Field::Title => title_hit = true,
            Field::Alias if via.is_none() => {
                via = best_line(kw, &doc.aliases).map(|a| format!("alias \"{a}\""));
            }
            Field::Notes if via.is_none() => via = Some("in notes".to_string()),
            _ => {}
        }
    }
    (total + bonus > 0.0).then_some(Hit {
        id,
        score: total + bonus,
        via: if title_hit { None } else { via },
        id_match: false,
        matched,
        strengths,
        head,
        bonus,
    })
}

fn load_meta(conn: &Connection) -> Result<HashMap<NodeId, Meta>, String> {
    let mut stmt = conn
        .prepare_cached(
            "SELECT id, parent_id, title, status, priority, work_type, refinement
             FROM nodes WHERE user_id = ?1",
        )
        .map_err(db_err)?;
    stmt.query_map(params![DEFAULT_USER], |r| {
        let status: String = r.get(3)?;
        let status: Status = status.parse().unwrap_or(Status::Open);
        Ok((
            NodeId(r.get(0)?),
            Meta {
                parent: r.get::<_, Option<i64>>(1)?.map(NodeId),
                title: r.get(2)?,
                status,
                priority: r.get(4)?,
                work_type: r
                    .get::<_, Option<String>>(5)?
                    .and_then(|t| WorkType::parse_loose(&t).ok()),
                refinement: r
                    .get::<_, Option<String>>(6)?
                    .and_then(|t| Refinement::parse_loose(&t).ok())
                    // Closed nodes have no refinement to match (see `node_from_row`).
                    .filter(|_| !status.is_closed()),
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

/// A done node (unless `include_done`) or a dropped one (unless `include_dropped`) at or
/// above `id`.
fn hidden_here_or_above(
    meta: &HashMap<NodeId, Meta>,
    id: NodeId,
    include_done: bool,
    include_dropped: bool,
) -> bool {
    lineage(meta, id).any(|(_, m)| match m.status {
        Status::Done => !include_done,
        Status::Dropped => !include_dropped,
        _ => false,
    })
}

/// Which of `done` and `dropped` hide `id` (itself or an ancestor) for these switches.
fn hidden_by(
    meta: &HashMap<NodeId, Meta>,
    id: NodeId,
    include_done: bool,
    include_dropped: bool,
) -> Vec<&'static str> {
    let statuses: Vec<Status> = lineage(meta, id).map(|(_, m)| m.status).collect();
    let mut out = Vec::new();
    if !include_done && statuses.contains(&Status::Done) {
        out.push("done");
    }
    if !include_dropped && statuses.contains(&Status::Dropped) {
        out.push("dropped");
    }
    out
}

/// The node's own level, or the nearest ancestor's.
fn effective_level(meta: &HashMap<NodeId, Meta>, id: NodeId) -> Option<i64> {
    lineage(meta, id).find_map(|(_, m)| m.priority)
}

/// Ancestors shown per result unless `full_paths` is set.
const SHORT_PATH: usize = 3;

/// Ancestors below the root with ids: `… / Spokes (SPOKE-5) / agent-feedback (SPOKE-182)`.
/// Only the last `SHORT_PATH` unless `full`; an empty string for a top-level node.
fn path_of(meta: &HashMap<NodeId, Meta>, id: NodeId, full: bool) -> String {
    let mut up: Vec<String> = lineage(meta, id)
        .skip(1)
        .filter(|(_, m)| m.parent.is_some())
        .map(|(a, m)| format!("{} ({a})", m.title))
        .collect();
    up.reverse();
    if full || up.len() <= SHORT_PATH {
        return up.join(" / ");
    }
    format!("… / {}", up[up.len() - SHORT_PATH..].join(" / "))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Kind;
    use crate::store::add::{self, tests::args};
    use crate::store::test_support::temp_db;

    fn search_args(query: &str, work_type: Option<&str>, refinement: Option<&str>) -> SearchArgs {
        SearchArgs {
            stage: None,
            query: query.into(),
            scope: None,
            include_done: None,
            limit: None,
            work_type: work_type.map(String::from),
            refinement: refinement.map(String::from),
            full_paths: None,
            match_mode: None,
            offset: None,
        }
    }

    fn today() -> NaiveDate {
        NaiveDate::from_ymd_opt(2026, 1, 1).unwrap()
    }

    fn seeded(name: &str) -> (Connection, std::path::PathBuf) {
        let (mut conn, dir) = temp_db(name);
        add::add(&mut conn, args(None, "Crash bug", Kind::Task)).unwrap();
        add::add(&mut conn, args(None, "Crash feature", Kind::Task)).unwrap();
        add::add(&mut conn, args(None, "Plain crash", Kind::Task)).unwrap();
        conn.execute(
            "UPDATE nodes SET work_type = 'bug', refinement = 'ready' WHERE title = 'Crash bug'",
            [],
        )
        .unwrap();
        conn.execute(
            "UPDATE nodes SET work_type = 'feature', refinement = 'idea' WHERE title = 'Crash feature'",
            [],
        )
        .unwrap();
        (conn, dir)
    }

    #[test]
    fn empty_query_with_refinement_lists_only_that_refinement() {
        let (conn, dir) = seeded("search-refinement");
        let out = search(&conn, search_args("", None, Some("Ready")), today()).unwrap();
        assert!(out.starts_with("1 match for refinement ready:"), "{out}");
        assert!(out.contains("Crash bug"));
        assert!(!out.contains("Crash feature") && !out.contains("Plain crash"));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn empty_query_combines_filters_with_and() {
        let (conn, dir) = seeded("search-and");
        let out = search(&conn, search_args("", Some("bug"), Some("idea")), today()).unwrap();
        assert!(
            out.starts_with("No matches for type bug, refinement idea."),
            "{out}"
        );
        let out = search(&conn, search_args("", Some("bug"), Some("ready")), today()).unwrap();
        assert!(
            out.starts_with("1 match for type bug, refinement ready:"),
            "{out}"
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn type_filter_drops_non_matching_keyword_hits() {
        let (conn, dir) = seeded("search-type");
        let out = search(&conn, search_args("crash", None, None), today()).unwrap();
        assert!(out.starts_with("3 matches for crash:"), "{out}");
        let out = search(&conn, search_args("crash", Some("bug"), None), today()).unwrap();
        assert!(out.starts_with("1 match for crash:"), "{out}");
        assert!(out.contains("Crash bug") && !out.contains("Crash feature"));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn bad_filter_value_errors() {
        let (conn, dir) = seeded("search-bad");
        let err = search(&conn, search_args("crash", Some("gremlin"), None), today()).unwrap_err();
        assert!(err.contains("Use one of"), "{err}");
        let err = search(&conn, search_args("", None, Some("nope")), today()).unwrap_err();
        assert!(err.contains("Use one of"), "{err}");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn empty_query_without_filter_still_errors() {
        let (conn, dir) = seeded("search-empty");
        let err = search(&conn, search_args("", None, None), today()).unwrap_err();
        assert!(err.contains("query has no words"), "{err}");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn result_lines_show_type_and_refinement() {
        let (conn, dir) = seeded("search-flags");
        let out = search(&conn, search_args("crash bug", None, None), today()).unwrap();
        let line = out.lines().find(|l| l.contains("Crash bug")).unwrap();
        assert!(line.contains(" task bug ready "), "{line}");
        let _ = std::fs::remove_dir_all(dir);
    }

    fn first_result(out: &str) -> &str {
        out.lines().nth(1).unwrap_or("")
    }

    #[test]
    fn every_id_form_puts_that_node_first() {
        let (conn, dir) = seeded("search-id-forms");
        // Root is SPOKE-1, then Crash bug 2, Crash feature 3, Plain crash 4.
        for q in [
            "4", "#4", "SPOKE-4", "spoke-4", "spoke4", "SPOKE 4", "spoke #4",
        ] {
            let out = search(&conn, search_args(q, None, None), today()).unwrap();
            let line = first_result(&out);
            assert!(line.starts_with("  SPOKE-4 task"), "{q}: {out}");
            assert!(line.ends_with("(id)"), "{q}: {line}");
        }
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn id_comes_before_text_matches_and_text_still_follows() {
        let (conn, dir) = seeded("search-id-text");
        let out = search(&conn, search_args("crash 3", None, None), today()).unwrap();
        assert!(out.starts_with("3 matches for crash, 3:"), "{out}");
        assert!(first_result(&out).starts_with("  SPOKE-3 "), "{out}");
        // Two ids keep the order typed.
        let out = search(&conn, search_args("4 2", None, None), today()).unwrap();
        let lines: Vec<&str> = out.lines().collect();
        assert!(
            lines[1].starts_with("  SPOKE-4 ") && lines[2].starts_with("  SPOKE-2 "),
            "{out}"
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn spoke_marker_is_not_searched_as_a_word() {
        assert_eq!(query_keywords("spoke 958"), vec!["958"]);
        assert_eq!(query_keywords("SPOKE-958"), vec!["spoke958"]);
        assert_eq!(query_keywords("spoke wheel"), vec!["spoke", "wheel"]);
        assert_eq!(
            query_ids("spoke 958, #12 SPOKE-7 x9 958"),
            vec![NodeId(958), NodeId(12), NodeId(7)]
        );
        assert!(query_ids("spokes 958x 1234567890123456").is_empty());
    }

    #[test]
    fn unknown_id_and_root_fall_back_to_text() {
        let (conn, dir) = seeded("search-id-missing");
        let out = search(&conn, search_args("999", None, None), today()).unwrap();
        assert!(out.starts_with("No matches for 999."), "{out}");
        let out = search(&conn, search_args("1", None, None), today()).unwrap();
        assert!(!out.contains("SPOKE-1 "), "{out}");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn closed_node_is_found_by_id_but_not_by_text() {
        let (conn, dir) = seeded("search-id-closed");
        conn.execute("UPDATE nodes SET status = 'done' WHERE id = 3", [])
            .unwrap();
        let out = search(&conn, search_args("3", None, None), today()).unwrap();
        assert!(
            first_result(&out).starts_with("  SPOKE-3 task done "),
            "{out}"
        );
        let out = search(&conn, search_args("crash", None, None), today()).unwrap();
        assert!(!out.contains("SPOKE-3 "), "{out}");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn id_respects_scope_and_filters() {
        let (conn, dir) = seeded("search-id-scope");
        let mut a = search_args("3", None, None);
        a.scope = Some(crate::model::IdArg::Number(2));
        let out = search(&conn, a, today()).unwrap();
        assert!(!out.contains("SPOKE-3 "), "{out}");
        let out = search(&conn, search_args("3", Some("bug"), None), today()).unwrap();
        assert!(!out.contains("SPOKE-3 "), "{out}");
        let out = search(&conn, search_args("3", Some("feature"), None), today()).unwrap();
        assert!(first_result(&out).starts_with("  SPOKE-3 "), "{out}");
        let _ = std::fs::remove_dir_all(dir);
    }

    /// Alpha (SPOKE-2) > Bravo (3) > Charlie (4) > Delta (5) > Echo (6) > `Zebra leaf` (7),
    /// plus top-level `Zebra solo` (8) and `Zebra pair` (9) under Bravo.
    fn chain(name: &str) -> (Connection, std::path::PathBuf) {
        let (mut conn, dir) = temp_db(name);
        add::add(&mut conn, args(None, "Alpha", Kind::Area)).unwrap();
        for (i, t) in ["Bravo", "Charlie", "Delta", "Echo"].iter().enumerate() {
            add::add(&mut conn, args(Some(i as i64 + 2), t, Kind::Area)).unwrap();
        }
        add::add(&mut conn, args(Some(6), "Zebra leaf", Kind::Task)).unwrap();
        add::add(&mut conn, args(None, "Zebra solo", Kind::Task)).unwrap();
        add::add(&mut conn, args(Some(3), "Zebra pair", Kind::Task)).unwrap();
        (conn, dir)
    }

    fn line_of(out: &str, title: &str) -> String {
        out.lines().find(|l| l.contains(title)).unwrap().to_string()
    }

    #[test]
    fn deep_paths_keep_the_last_three_ancestors_with_ids() {
        let (conn, dir) = chain("search-deep");
        let out = search(&conn, search_args("zebra", None, None), today()).unwrap();
        assert!(
            line_of(&out, "Zebra leaf")
                .ends_with("in … / Charlie (SPOKE-4) / Delta (SPOKE-5) / Echo (SPOKE-6)"),
            "{out}"
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn short_and_top_level_paths_have_no_ellipsis() {
        let (conn, dir) = chain("search-short");
        let out = search(&conn, search_args("zebra", None, None), today()).unwrap();
        assert!(
            line_of(&out, "Zebra pair").ends_with("in Alpha (SPOKE-2) / Bravo (SPOKE-3)"),
            "{out}"
        );
        assert!(
            line_of(&out, "Zebra solo").ends_with("in To do (top level)"),
            "{out}"
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn full_paths_shows_every_ancestor_with_ids() {
        let (conn, dir) = chain("search-full");
        let mut a = search_args("zebra", None, None);
        a.full_paths = Some(true);
        let out = search(&conn, a, today()).unwrap();
        assert!(
            line_of(&out, "Zebra leaf").ends_with(
                "in Alpha (SPOKE-2) / Bravo (SPOKE-3) / Charlie (SPOKE-4) / Delta (SPOKE-5) / Echo (SPOKE-6)"
            ),
            "{out}"
        );
        assert!(!out.contains('…'), "{out}");
        let _ = std::fs::remove_dir_all(dir);
    }

    /// A small tree with the shapes that made loose queries noisy (SPOKE-774, SPOKE-226):
    /// a YouTube area with video ideas, a Discord status bot project with a child whose
    /// aliases match many words, a feedback task whose notes quote the whole query, and
    /// filler that makes "video", "ideas" and "content" common words.
    fn loose_tree(name: &str) -> (Connection, std::path::PathBuf) {
        use crate::tools::args::NodeSpec;
        let (mut conn, dir) = temp_db(name);
        let mut put =
            |parent: Option<i64>, title: &str, kind: Kind, aliases: &[&str], notes: &str| {
                let mut a = args(parent, title, kind);
                a.node = NodeSpec {
                    aliases: (!aliases.is_empty())
                        .then(|| aliases.iter().map(|s| s.to_string()).collect()),
                    notes: (!notes.is_empty()).then(|| notes.to_string()),
                    ..a.node
                };
                add::add(&mut conn, a).unwrap();
            };
        put(None, "Hobbies", Kind::Area, &[], ""); // 2
        put(
            Some(2),
            "YouTube",
            Kind::Area,
            &["youtube channel", "video content"],
            "",
        ); // 3
        put(
            Some(3),
            "Video idea: a skit",
            Kind::Task,
            &[],
            "content ideas",
        ); // 4
        put(
            Some(3),
            "Editing idea: transition by holding a picture",
            Kind::Task,
            &["video editing"],
            "",
        ); // 5
        put(None, "Projects", Kind::Area, &[], ""); // 6
        put(
            Some(6),
            "norsworthy-discord-status",
            Kind::Area,
            &["discord status bot", "discord bot"],
            "",
        ); // 7
        put(
            Some(7),
            "Duplicate tile after a container recreate",
            Kind::Task,
            &["duplicate server entries", "status image duplicate tiles"],
            "",
        ); // 8
        put(
            Some(7),
            "Re-run CI on the status image PRs",
            Kind::Task,
            &[],
            "",
        ); // 9
        put(
            Some(6),
            "Search ranks too loosely",
            Kind::Task,
            &[],
            "I searched youtube video editing transition ideas video content channel and \
             discord status bot server tile chart image offline and got dozens of hits",
        ); // 10
        for i in 0..12 {
            put(
                Some(6),
                &format!("Filler {i}"),
                Kind::Task,
                &[],
                "video ideas and content for some server image",
            ); // 11..22
        }
        (conn, dir)
    }

    fn ids_in(out: &str) -> Vec<String> {
        out.lines()
            .skip(1)
            .filter(|l| l.starts_with("  SPOKE-"))
            .map(|l| l.split_whitespace().next().unwrap().to_string())
            .collect()
    }

    #[test]
    fn loose_youtube_query_finds_the_area_first_and_stays_short() {
        let (conn, dir) = loose_tree("search-loose-yt");
        let q = "youtube video editing transition ideas video content channel";
        let out = search(&conn, search_args(q, None, None), today()).unwrap();
        let ids = ids_in(&out);
        assert_eq!(ids[0], "SPOKE-3", "{out}");
        assert!(ids.len() <= 5, "{out}");
        // Notes that quote the query rank below every node the words name.
        let pos = |id: &str| ids.iter().position(|i| i == id).unwrap_or(usize::MAX);
        assert!(
            pos("SPOKE-10") > pos("SPOKE-5"),
            "quoted notes must not win: {out}"
        );
        assert!(out.contains("weaker matches"), "{out}");
        // match: any lists everything again.
        let mut a = search_args(q, None, None);
        a.match_mode = Some("any".into());
        let all = search(&conn, a, today()).unwrap();
        assert!(
            ids_in(&all).len() > ids.len() || all.contains("more matches"),
            "{all}"
        );
        assert!(!all.contains("weaker matches"), "{all}");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn loose_discord_query_puts_the_parent_first() {
        let (conn, dir) = loose_tree("search-loose-discord");
        let q = "discord status bot server tile chart image offline";
        let out = search(&conn, search_args(q, None, None), today()).unwrap();
        assert_eq!(ids_in(&out)[0], "SPOKE-7", "{out}");
        assert!(ids_in(&out).len() <= 6, "{out}");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn match_all_keeps_only_nodes_with_every_keyword() {
        let (conn, dir) = loose_tree("search-all");
        let mut a = search_args("duplicate tile", None, None);
        a.match_mode = Some("ALL".into());
        let out = search(&conn, a, today()).unwrap();
        assert_eq!(ids_in(&out), vec!["SPOKE-8"], "{out}");
        let mut a = search_args("duplicate zzqqxx", None, None);
        a.match_mode = Some("all".into());
        let out = search(&conn, a, today()).unwrap();
        assert!(out.starts_with("No matches"), "{out}");
        let mut a = search_args("tile", None, None);
        a.match_mode = Some("most".into());
        assert!(
            search(&conn, a, today())
                .unwrap_err()
                .contains("best (the default), any or all")
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn partial_matches_say_which_keywords_they_had() {
        let (conn, dir) = loose_tree("search-matched");
        let out = search(&conn, search_args("duplicate rerun", None, None), today()).unwrap();
        let line = line_of(&out, "Duplicate tile");
        assert!(line.ends_with("(matched 1 of 2: duplicate)"), "{line}");
        // A keyword nothing has does not count: no note when every live keyword matched.
        let out = search(&conn, search_args("duplicate zzqqxx", None, None), today()).unwrap();
        assert!(
            !line_of(&out, "Duplicate tile").contains("matched"),
            "{out}"
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn offset_pages_through_matches_without_gaps_or_overlap() {
        let (mut conn, dir) = temp_db("search-pages");
        for i in 0..25 {
            add::add(
                &mut conn,
                args(None, &format!("Pager item {i:02}"), Kind::Task),
            )
            .unwrap();
        }
        let page = |offset: Option<i64>| {
            let mut a = search_args("pager", None, None);
            a.offset = offset.map(crate::model::NumArg::Number);
            search(&conn, a, today()).unwrap()
        };
        let p1 = page(None);
        assert!(p1.starts_with("25 matches for pager:"), "{p1}");
        assert!(
            p1.contains("(+15 more matches. Pass offset: 10 for the next page"),
            "{p1}"
        );
        let p2 = page(Some(10));
        assert!(
            p2.starts_with("25 matches for pager (showing 11-20):"),
            "{p2}"
        );
        assert!(p2.contains("Pass offset: 20"), "{p2}");
        let p3 = page(Some(20));
        assert!(
            p3.starts_with("25 matches for pager (showing 21-25):"),
            "{p3}"
        );
        assert!(!p3.contains("more matches"), "{p3}");
        let mut seen: Vec<String> = [&p1, &p2, &p3].iter().flat_map(|p| ids_in(p)).collect();
        assert_eq!(seen.len(), 25);
        seen.sort();
        seen.dedup();
        assert_eq!(seen.len(), 25, "no overlap");
        let past = page(Some(40));
        assert!(past.contains("offset 40 is past the last match"), "{past}");
        let mut a = search_args("pager", None, None);
        a.offset = Some(crate::model::NumArg::Number(-1));
        assert!(
            search(&conn, a, today())
                .unwrap_err()
                .contains("offset must be 0 to")
        );
        let _ = std::fs::remove_dir_all(dir);
    }
}
