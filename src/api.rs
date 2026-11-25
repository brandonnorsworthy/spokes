//! The read-only JSON API under `/api`, for the web view (`spokes-view`), which calls it
//! server to server:
//!
//! - `GET /api/tree?root=&depth=&include_closed=&include_dropped=`: a slice of the tree for lazy loading.
//! - `GET /api/nodes/{id}`: one node with its notes, breadcrumb and last events.
//! - `GET /api/history?root=`: a subtree's nodes (closed ones too) and all their events,
//!   oldest first, for replaying its history.
//! - `GET /api/search?q=&limit=&include_closed=&include_dropped=&scope=`: the `search` tool's ranked
//!   matches as data, each with its breadcrumb ids, for finding a node in the view.
//!
//! It reads through its own read-only SQLite connection (WAL lets it read while MCP
//! writes), so it can neither write nor hold the MCP connection's lock. Responses carry
//! a weak `ETag` and answer a matching `If-None-Match` with 304. Requests with an
//! `Origin` header are refused unless it is in `SPOKES_ALLOWED_ORIGINS`, and the `Host`
//! header must be in `SPOKES_ALLOWED_HOSTS` when that is set, as on `/mcp`.

use std::collections::HashMap;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::Context;
use axum::Router;
use axum::extract::rejection::QueryRejection;
use axum::extract::{Path as UrlPath, Query, Request, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, Uri, header};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use rusqlite::{Connection, OpenFlags};
use serde::Serialize;
use tower_http::compression::CompressionLayer;

use crate::config::Config;
use crate::format::today_in;
use crate::model::NodeId;
use crate::store::load;
use crate::store::search as find;
use crate::store::view::{
    self, DEFAULT_DEPTH, Detail, History, MAX_DEPTH, NODE_LIMIT, Slice, SliceQuery, ViewError,
};

#[derive(Clone)]
struct Api {
    /// The API's own read-only connection.
    conn: Arc<Mutex<Connection>>,
    tz: chrono_tz::Tz,
    allowed_hosts: Option<Vec<String>>,
    allowed_origins: Vec<String>,
    /// Differs on every start, so a tag never survives a restart (or a restore).
    boot: String,
}

/// The `/api` router. Call after the database has been opened and migrated.
pub fn router(config: &Config) -> anyhow::Result<Router> {
    let conn = open_read_only(&config.db_path)?;
    let boot = format!(
        "{:x}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or_default()
    );
    let state = Api {
        conn: Arc::new(Mutex::new(conn)),
        tz: config.tz,
        allowed_hosts: config.allowed_hosts.clone(),
        allowed_origins: config.allowed_origins.clone(),
        boot,
    };
    Ok(Router::new()
        .route("/tree", get(tree))
        .route("/nodes/{id}", get(node))
        .route("/history", get(history))
        .route("/search", get(search))
        .fallback(|| async {
            error(
                StatusCode::NOT_FOUND,
                "no such endpoint; use /api/tree, /api/nodes/{id}, /api/history or /api/search",
            )
        })
        .method_not_allowed_fallback(|| async {
            error(
                StatusCode::METHOD_NOT_ALLOWED,
                "the API is read-only; use GET",
            )
        })
        .layer(CompressionLayer::new())
        .layer(middleware::from_fn_with_state(state.clone(), guard))
        .with_state(state))
}

fn open_read_only(path: &Path) -> anyhow::Result<Connection> {
    let conn = Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .with_context(|| format!("opening {} read-only for the API", path.display()))?;
    conn.busy_timeout(Duration::from_secs(5))?;
    conn.execute_batch("PRAGMA query_only = ON")?;
    Ok(conn)
}

/// `{"error": "..."}` with a status.
struct ApiError(StatusCode, String);

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        #[derive(Serialize)]
        struct Body {
            error: String,
        }
        (self.0, axum::Json(Body { error: self.1 })).into_response()
    }
}

fn error(status: StatusCode, message: impl Into<String>) -> Response {
    ApiError(status, message.into()).into_response()
}

fn bad(message: impl Into<String>) -> ApiError {
    ApiError(StatusCode::BAD_REQUEST, message.into())
}

/// Host and Origin checks, matching how `/mcp` behaves (see `server.rs`).
async fn guard(State(api): State<Api>, req: Request, next: Next) -> Response {
    if let Some(allowed) = &api.allowed_hosts {
        match request_host(req.uri(), req.headers()) {
            None => return error(StatusCode::BAD_REQUEST, "missing or malformed Host header"),
            Some(host) if !host_allowed(&host, allowed) => {
                tracing::warn!(?host, "API refused a request with a disallowed Host header");
                return error(StatusCode::FORBIDDEN, "Host header is not allowed");
            }
            Some(_) => {}
        }
    }
    if let Some(origin) = req.headers().get(header::ORIGIN) {
        let ok = origin
            .to_str()
            .ok()
            .and_then(parse_origin)
            .is_some_and(|o| origin_allowed(&o, &api.allowed_origins));
        if !ok {
            tracing::warn!(
                ?origin,
                "API refused a request with a disallowed Origin header"
            );
            return error(StatusCode::FORBIDDEN, "Origin header is not allowed");
        }
    }
    next.run(req).await
}

/// (host, port) from the `Host` header, or the URI authority (HTTP/2).
fn request_host(uri: &Uri, headers: &HeaderMap) -> Option<(String, Option<u16>)> {
    let authority = match headers.get(header::HOST) {
        Some(h) => axum::http::uri::Authority::try_from(h.to_str().ok()?).ok()?,
        None => uri.authority()?.clone(),
    };
    Some((norm_host(authority.host()), authority.port_u16()))
}

fn norm_host(host: &str) -> String {
    host.trim_matches(['[', ']']).to_ascii_lowercase()
}

/// An allowed entry without a port matches any port; with one, only that port.
fn host_allowed((host, port): &(String, Option<u16>), allowed: &[String]) -> bool {
    allowed.iter().any(|entry| {
        let entry = entry.trim();
        let (h, p) = match axum::http::uri::Authority::try_from(entry) {
            Ok(a) => (norm_host(a.host()), a.port_u16()),
            Err(_) => (norm_host(entry), None),
        };
        h == *host && p.is_none_or(|p| Some(p) == *port)
    })
}

/// A browser origin: `null`, or (scheme, host, port).
#[derive(Debug, PartialEq)]
enum Origin {
    Null,
    Tuple(String, String, Option<u16>),
}

fn parse_origin(value: &str) -> Option<Origin> {
    let value = value.trim();
    if value.eq_ignore_ascii_case("null") {
        return Some(Origin::Null);
    }
    let uri: Uri = value.parse().ok()?;
    let authority = uri.authority()?;
    Some(Origin::Tuple(
        uri.scheme_str()?.to_ascii_lowercase(),
        norm_host(authority.host()),
        authority.port_u16(),
    ))
}

/// The same rules as the MCP SDK: an entry's scheme and host must match; `:*` matches
/// any port, an entry without a port matches any port, an explicit port only that port
/// (the scheme's default when the origin omits it); `null` matches `Origin: null`.
fn origin_allowed(origin: &Origin, allowed: &[String]) -> bool {
    let default_port = |scheme: &str| match scheme {
        "http" => Some(80),
        "https" => Some(443),
        _ => None,
    };
    allowed.iter().any(|entry| {
        let entry = entry.trim();
        let (base, any_port) = match entry.strip_suffix(":*") {
            Some(base) => (base, true),
            None => (entry, false),
        };
        match (parse_origin(base), origin) {
            (Some(Origin::Null), Origin::Null) => !any_port,
            (Some(Origin::Tuple(s, h, p)), Origin::Tuple(os, oh, op)) => {
                s == *os
                    && h == *oh
                    && (any_port || p.is_none_or(|p| op.or_else(|| default_port(os)) == Some(p)))
            }
            _ => false,
        }
    })
}

/// Query parameters by name; unknown or repeated names are refused.
fn params(
    query: Result<Query<Vec<(String, String)>>, QueryRejection>,
    known: &[&str],
) -> Result<HashMap<String, String>, ApiError> {
    let Query(pairs) = query.map_err(|e| bad(format!("bad query string: {e}")))?;
    let mut out = HashMap::new();
    for (k, v) in pairs {
        if !known.contains(&k.as_str()) {
            return Err(bad(format!(
                "unknown parameter '{k}'; this endpoint takes {}",
                known.join(", ")
            )));
        }
        if out.insert(k.clone(), v).is_some() {
            return Err(bad(format!("parameter '{k}' is given more than once")));
        }
    }
    Ok(out)
}

fn parse_id(value: &str, name: &str) -> Result<NodeId, ApiError> {
    value.parse().map_err(|_| {
        bad(format!(
            "{name} '{value}' is not a node id; use SPOKE-12 or 12"
        ))
    })
}

/// `include_closed` (done nodes, and dropped ones too when `include_dropped` is absent)
/// and `include_dropped`.
fn visibility(p: &std::collections::HashMap<String, String>) -> Result<(bool, bool), ApiError> {
    let include_closed = parse_bool(p.get("include_closed"), "include_closed")?;
    let include_dropped = match p.get("include_dropped") {
        None => include_closed,
        v => parse_bool(v, "include_dropped")?,
    };
    Ok((include_closed, include_dropped))
}

fn parse_bool(value: Option<&String>, name: &str) -> Result<bool, ApiError> {
    match value.map(|v| v.trim().to_ascii_lowercase()).as_deref() {
        None => Ok(false),
        Some("true" | "1") => Ok(true),
        Some("false" | "0") => Ok(false),
        Some(_) => Err(bad(format!("{name} must be true or false"))),
    }
}

/// Fields every response starts with.
#[derive(Serialize)]
struct Meta {
    rev: i64,
    version: &'static str,
    time_zone: String,
    today: String,
}

#[derive(Serialize)]
struct TreeBody {
    #[serde(flatten)]
    meta: Meta,
    root: i64,
    depth: i64,
    include_closed: bool,
    include_dropped: bool,
    node_limit: usize,
    #[serde(flatten)]
    slice: Slice,
}

#[derive(Serialize)]
struct NodeBody {
    #[serde(flatten)]
    meta: Meta,
    #[serde(flatten)]
    detail: Detail,
}

#[derive(Serialize)]
struct HistoryBody {
    #[serde(flatten)]
    meta: Meta,
    root: i64,
    #[serde(flatten)]
    history: History,
}

async fn tree(
    State(api): State<Api>,
    headers: HeaderMap,
    query: Result<Query<Vec<(String, String)>>, QueryRejection>,
) -> Result<Response, ApiError> {
    let p = params(
        query,
        &["root", "depth", "include_closed", "include_dropped"],
    )?;
    let root = match p.get("root") {
        None => crate::model::ROOT_ID,
        Some(v) => parse_id(v, "root")?,
    };
    let depth = match p.get("depth") {
        None => DEFAULT_DEPTH,
        Some(v) => v
            .trim()
            .parse::<i64>()
            .ok()
            .filter(|d| (0..=MAX_DEPTH).contains(d))
            .ok_or_else(|| {
                bad(format!(
                    "depth must be a whole number from 0 to {MAX_DEPTH}"
                ))
            })?,
    };
    let (include_closed, include_dropped) = visibility(&p)?;
    let q = SliceQuery {
        root,
        depth,
        include_closed,
        include_dropped,
    };
    let tag = format!(
        "t.{}.{depth}.{}{}",
        root.0,
        u8::from(include_closed),
        u8::from(include_dropped)
    );
    Ok(respond(api, headers, tag, move |conn, meta, today, now| {
        let slice = view::slice(conn, q, today, now)?;
        Ok(TreeBody {
            meta,
            root: root.0,
            depth,
            include_closed,
            include_dropped,
            node_limit: NODE_LIMIT,
            slice,
        })
    })
    .await)
}

async fn node(
    State(api): State<Api>,
    headers: HeaderMap,
    UrlPath(id): UrlPath<String>,
    query: Result<Query<Vec<(String, String)>>, QueryRejection>,
) -> Result<Response, ApiError> {
    let p = params(query, &["include_closed", "include_dropped"])?;
    let id = parse_id(&id, "id")?;
    let (include_closed, include_dropped) = visibility(&p)?;
    let tag = format!(
        "n.{}.{}{}",
        id.0,
        u8::from(include_closed),
        u8::from(include_dropped)
    );
    Ok(respond(api, headers, tag, move |conn, meta, today, now| {
        let detail = view::detail(conn, id, include_closed, include_dropped, today, now)?;
        Ok(NodeBody { meta, detail })
    })
    .await)
}

async fn history(
    State(api): State<Api>,
    headers: HeaderMap,
    query: Result<Query<Vec<(String, String)>>, QueryRejection>,
) -> Result<Response, ApiError> {
    let p = params(query, &["root"])?;
    let root = match p.get("root") {
        None => crate::model::ROOT_ID,
        Some(v) => parse_id(v, "root")?,
    };
    let tag = format!("h.{}", root.0);
    Ok(respond(api, headers, tag, move |conn, meta, today, now| {
        let history = view::history(conn, root, today, now)?;
        Ok(HistoryBody {
            meta,
            root: root.0,
            history,
        })
    })
    .await)
}

/// Longest `q` accepted, in characters.
const MAX_QUERY_CHARS: usize = 200;

#[derive(Serialize)]
struct SearchBody {
    #[serde(flatten)]
    meta: Meta,
    query: String,
    scope: i64,
    include_closed: bool,
    include_dropped: bool,
    #[serde(flatten)]
    found: find::ApiSearch,
}

async fn search(
    State(api): State<Api>,
    headers: HeaderMap,
    query: Result<Query<Vec<(String, String)>>, QueryRejection>,
) -> Result<Response, ApiError> {
    let p = params(
        query,
        &["q", "limit", "include_closed", "include_dropped", "scope"],
    )?;
    let q = p.get("q").map(|v| v.trim().to_string()).unwrap_or_default();
    if q.chars().count() > MAX_QUERY_CHARS {
        return Err(bad(format!(
            "q is longer than {MAX_QUERY_CHARS} characters"
        )));
    }
    let kws = find::query_keywords(&q);
    if kws.is_empty() {
        return Err(bad(
            "q needs at least one word, e.g. q=server homelab (words are alternatives)",
        ));
    }
    let limit = match p.get("limit") {
        None => find::API_DEFAULT_LIMIT,
        Some(v) => v
            .trim()
            .parse::<usize>()
            .ok()
            .filter(|l| (1..=find::API_MAX_LIMIT).contains(l))
            .ok_or_else(|| {
                bad(format!(
                    "limit must be a whole number from 1 to {}",
                    find::API_MAX_LIMIT
                ))
            })?,
    };
    let (include_closed, include_dropped) = visibility(&p)?;
    let scope = match p.get("scope") {
        None => crate::model::ROOT_ID,
        Some(v) => parse_id(v, "scope")?,
    };
    // The query text goes into the ETag as a hash: it is free text, a header is not.
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    std::hash::Hash::hash(&q, &mut hasher);
    let tag = format!(
        "s.{:x}.{limit}.{}{}.{}",
        std::hash::Hasher::finish(&hasher),
        u8::from(include_closed),
        u8::from(include_dropped),
        scope.0
    );
    Ok(respond(api, headers, tag, move |conn, meta, _today, _now| {
        if load(conn, scope)?.is_none() {
            return Err(ViewError::NotFound(format!("{scope} does not exist")));
        }
        let found = find::api_search(conn, &q, kws, scope, include_closed, include_dropped, limit)?;
        Ok(SearchBody {
            meta,
            query: q,
            scope: scope.0,
            include_closed,
            include_dropped,
            found,
        })
    })
    .await)
}

/// Runs a read on the blocking pool inside one read transaction (a consistent
/// snapshot), answering 304 when the client's tag is still current.
///
/// The tag folds in everything a response depends on besides the query: the boot id,
/// SQLite's change counter for this connection and the revision (any write), "today"
/// in `SPOKES_TZ` (dates and deferrals) and the number of lapsed claims (a lease that
/// runs out turns a node `stalled` with no write).
async fn respond<T, F>(api: Api, headers: HeaderMap, query_tag: String, build: F) -> Response
where
    T: Serialize + Send + 'static,
    F: FnOnce(&Connection, Meta, chrono::NaiveDate, &str) -> Result<T, ViewError> + Send + 'static,
{
    let if_none_match = headers
        .get(header::IF_NONE_MATCH)
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);
    let outcome = tokio::task::spawn_blocking(move || -> Result<Reply, ViewError> {
        let mut conn = api.conn.lock().unwrap_or_else(|p| p.into_inner());
        let tx = conn.transaction()?;
        let today = today_in(api.tz);
        let now = crate::db::now();
        let changes: i64 = tx.query_row("PRAGMA data_version", [], |r| r.get(0))?;
        let (rev, lapsed) = view::freshness(&tx, &now)?;
        let etag = format!(
            "W/\"{query_tag}.{}.{changes}.{rev}.{lapsed}.{today}\"",
            api.boot
        );
        if if_none_match
            .as_deref()
            .is_some_and(|inm| tag_matches(inm, &etag))
        {
            return Ok(Reply::NotModified(etag));
        }
        let meta = Meta {
            rev,
            version: env!("CARGO_PKG_VERSION"),
            time_zone: api.tz.name().to_string(),
            today: today.to_string(),
        };
        let body = build(&tx, meta, today, &now)?;
        drop(tx);
        drop(conn);
        let json = serde_json::to_vec(&body).map_err(|e| {
            tracing::error!(error = %e, "API response did not serialize");
            ViewError::Internal
        })?;
        Ok(Reply::Body(etag, json))
    })
    .await;
    let reply = match outcome {
        Ok(Ok(reply)) => reply,
        Ok(Err(ViewError::NotFound(msg))) => return error(StatusCode::NOT_FOUND, msg),
        Ok(Err(ViewError::Internal)) => {
            return error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "internal database error; check the server log",
            );
        }
        Err(e) => {
            tracing::error!(error = %e, "API task failed");
            return error(StatusCode::INTERNAL_SERVER_ERROR, "internal error");
        }
    };
    let (mut res, etag) = match reply {
        Reply::NotModified(etag) => (StatusCode::NOT_MODIFIED.into_response(), etag),
        Reply::Body(etag, json) => {
            let mut res = json.into_response();
            res.headers_mut().insert(
                header::CONTENT_TYPE,
                HeaderValue::from_static("application/json"),
            );
            (res, etag)
        }
    };
    let h = res.headers_mut();
    h.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-cache"));
    if let Ok(v) = HeaderValue::from_str(&etag) {
        h.insert(header::ETAG, v);
    }
    res
}

enum Reply {
    NotModified(String),
    Body(String, Vec<u8>),
}

/// Weak comparison against an `If-None-Match` list (`*` matches anything).
fn tag_matches(if_none_match: &str, etag: &str) -> bool {
    let opaque = |t: &str| t.trim().trim_start_matches("W/").to_string();
    let ours = opaque(etag);
    if_none_match
        .split(',')
        .any(|t| t.trim() == "*" || opaque(t) == ours)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn origin_rules_match_the_mcp_sdk() {
        let allow = |list: &[&str], o: &str| {
            let list: Vec<String> = list.iter().map(|s| s.to_string()).collect();
            parse_origin(o).is_some_and(|o| origin_allowed(&o, &list))
        };
        assert!(!allow(&[], "http://evil.example"));
        assert!(allow(&["http://view.lan:8080"], "http://view.lan:8080"));
        assert!(!allow(&["http://view.lan:8080"], "http://view.lan:9090"));
        assert!(!allow(&["http://view.lan:8080"], "https://view.lan:8080"));
        assert!(allow(&["http://view.lan:*"], "http://view.lan:1234"));
        assert!(allow(&["http://view.lan"], "http://view.lan:1234"));
        assert!(allow(&["https://view.lan:443"], "https://VIEW.lan"));
        assert!(allow(&["null"], "null"));
        assert!(!allow(&["http://view.lan"], "null"));
        assert!(parse_origin("not a url").is_none());
    }

    #[test]
    fn host_rules() {
        let list = vec!["spokes.lan".to_string(), "192.168.5.100:10050".to_string()];
        assert!(host_allowed(&("spokes.lan".into(), Some(8787)), &list));
        assert!(host_allowed(&("192.168.5.100".into(), Some(10050)), &list));
        assert!(!host_allowed(&("192.168.5.100".into(), Some(80)), &list));
        assert!(!host_allowed(&("evil.example".into(), None), &list));
    }

    #[test]
    fn if_none_match_is_weak_and_listy() {
        let tag = "W/\"t.1.3.0.x.1.2.0.2026-11-15\"";
        assert!(tag_matches(tag, tag));
        assert!(tag_matches("\"t.1.3.0.x.1.2.0.2026-11-15\"", tag));
        assert!(tag_matches(
            "\"other\", W/\"t.1.3.0.x.1.2.0.2026-11-15\"",
            tag
        ));
        assert!(tag_matches("*", tag));
        assert!(!tag_matches("W/\"t.1.3.0.x.1.2.0.2026-11-16\"", tag));
    }
}
