//! Drives the real HTTP endpoint: initialize, tools/list and tools/call against a
//! server bound to a random port with a temp SQLite file, then restarts it on the
//! same file to check persistence.

use std::net::SocketAddr;
use std::path::{Path, PathBuf};

use http_body_util::{BodyExt, Full};
use hyper::body::Bytes;
use hyper::{Request, StatusCode};
use hyper_util::client::legacy::Client;
use hyper_util::client::legacy::connect::HttpConnector;
use hyper_util::rt::TokioExecutor;
use serde_json::{Value, json};
use spokes::config::Config;
use tokio::sync::oneshot;
use tokio::task::JoinHandle;

struct Server {
    addr: SocketAddr,
    stop: oneshot::Sender<()>,
    task: JoinHandle<()>,
}

async fn start(db_path: &Path) -> Server {
    start_tz(db_path, chrono_tz::UTC).await
}

async fn start_tz(db_path: &Path, tz: chrono_tz::Tz) -> Server {
    start_with(db_path, tz, true).await
}

async fn start_with(db_path: &Path, tz: chrono_tz::Tz, hints: bool) -> Server {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let config = Config {
        bind: addr,
        db_path: db_path.to_path_buf(),
        allowed_hosts: None,
        allowed_origins: Vec::new(),
        tz,
        backup: None,
        hints,
    };
    let (stop, stopped) = oneshot::channel::<()>();
    let task = tokio::spawn(async move {
        spokes::serve(listener, &config, async {
            let _ = stopped.await;
        })
        .await
        .unwrap();
    });
    Server { addr, stop, task }
}

impl Server {
    async fn shutdown(self) {
        let _ = self.stop.send(());
        self.task.await.unwrap();
    }
}

struct Mcp {
    client: Client<HttpConnector, Full<Bytes>>,
    url: String,
    base: String,
}

impl Mcp {
    fn new(addr: SocketAddr) -> Self {
        Mcp {
            client: Client::builder(TokioExecutor::new()).build_http(),
            url: format!("http://{addr}/mcp"),
            base: format!("http://{addr}"),
        }
    }

    async fn post(&self, body: Value, headers: &[(&str, &str)]) -> (StatusCode, String) {
        let mut req = Request::post(&self.url)
            .header("content-type", "application/json")
            .header("accept", "application/json, text/event-stream");
        for (k, v) in headers {
            req = req.header(*k, *v);
        }
        let req = req.body(Full::new(Bytes::from(body.to_string()))).unwrap();
        let res = self.client.request(req).await.unwrap();
        let status = res.status();
        let bytes = res.into_body().collect().await.unwrap().to_bytes();
        (status, String::from_utf8(bytes.to_vec()).unwrap())
    }

    /// A JSON-RPC request on the 2025-11-25 (initialize-based) protocol.
    async fn rpc(&self, method: &str, params: Value) -> Value {
        let (status, body) = self
            .post(
                json!({"jsonrpc": "2.0", "id": 1, "method": method, "params": params}),
                &[("mcp-protocol-version", "2025-11-25")],
            )
            .await;
        assert_eq!(status, StatusCode::OK, "{method}: {body}");
        serde_json::from_str(&body).unwrap_or_else(|e| panic!("{method}: {e}: {body}"))
    }

    /// Calls a tool; returns (text, is_error).
    async fn call(&self, name: &str, arguments: Value) -> (String, bool) {
        let v = self
            .rpc("tools/call", json!({"name": name, "arguments": arguments}))
            .await;
        let result = &v["result"];
        assert!(result.is_object(), "{name} returned no result: {v}");
        let text = result["content"][0]["text"].as_str().unwrap().to_string();
        (text, result["isError"].as_bool().unwrap_or(false))
    }

    async fn ok(&self, name: &str, arguments: Value) -> String {
        let (text, is_error) = self.call(name, arguments).await;
        assert!(!is_error, "{name} failed: {text}");
        text
    }

    async fn get_path(&self, path: &str) -> (StatusCode, String) {
        let req = Request::get(format!("{}{path}", self.base))
            .body(Full::new(Bytes::new()))
            .unwrap();
        let res = self.client.request(req).await.unwrap();
        let status = res.status();
        let bytes = res.into_body().collect().await.unwrap().to_bytes();
        (status, String::from_utf8(bytes.to_vec()).unwrap())
    }
}

fn temp_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("spokes-it-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    dir
}

#[tokio::test]
async fn walking_skeleton_over_http() {
    let dir = temp_dir("skeleton");
    let db = dir.join("spokes.db");
    let server = start(&db).await;
    let mcp = Mcp::new(server.addr);

    assert_eq!(mcp.get_path("/health").await, (StatusCode::OK, "ok".into()));

    let init = mcp
        .rpc(
            "initialize",
            json!({
                "protocolVersion": "2025-11-25",
                "capabilities": {},
                "clientInfo": {"name": "it", "version": "1"}
            }),
        )
        .await;
    assert_eq!(init["result"]["serverInfo"]["name"], "spokes");
    assert!(init["result"]["capabilities"]["tools"].is_object());
    assert!(
        init["result"]["instructions"]
            .as_str()
            .unwrap()
            .contains("SPOKE-")
    );
    let (status, _) = mcp
        .post(
            json!({"jsonrpc": "2.0", "method": "notifications/initialized"}),
            &[("mcp-protocol-version", "2025-11-25")],
        )
        .await;
    assert_eq!(status, StatusCode::ACCEPTED);

    let tools = mcp.rpc("tools/list", json!({})).await;
    let mut names: Vec<&str> = tools["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["name"].as_str().unwrap())
        .collect();
    names.sort_unstable();
    assert_eq!(
        names,
        [
            "add",
            "answer",
            "ask",
            "claim",
            "focus",
            "get",
            "guide",
            "move",
            "next",
            "recent",
            "release",
            "search",
            "set_status",
            "update"
        ]
    );

    // The M0 "done when": add three areas and list them.
    for area in ["Home", "Work", "Hobbies"] {
        let out = mcp
            .ok(
                "add",
                json!({"parent": "SPOKE-1", "title": area, "kind": "area", "agent": "it"}),
            )
            .await;
        assert!(out.starts_with("Added SPOKE-"), "{out}");
    }
    assert_eq!(
        mcp.ok("get", json!({"id": "SPOKE-1"})).await,
        "SPOKE-1 area  To do\n  - SPOKE-2 area  Home\n  - SPOKE-3 area  Work\n  - SPOKE-4 area  Hobbies"
    );
    // No work listed, so no claim footer.
    assert_eq!(
        mcp.ok("next", json!({})).await,
        "Nothing actionable under SPOKE-1 To do. Add a task with add."
    );

    // A task shows up in the queue and leaves it when done; bare numeric ids work.
    mcp.ok(
        "add",
        json!({"parent": 4, "title": "Rewire garage outlet", "priority": 2}),
    )
    .await;
    // SPOKE-1006: a queue with work ends with the claim footer.
    assert_eq!(
        mcp.ok("next", json!({})).await,
        "UP NEXT\n  SPOKE-5  p2  Rewire garage outlet  unplanned\nNext: Claim the one you pick before starting."
    );
    assert_eq!(
        mcp.ok(
            "set_status",
            json!({"ids": ["SPOKE-5"], "status": "done", "agent": "it"})
        )
        .await,
        "SPOKE-5 done  Rewire garage outlet"
    );

    // Error paths come back as tool errors that say what to do.
    let (text, is_error) = mcp.call("get", json!({"id": "SPOKE-404"})).await;
    assert!(is_error && text.contains("does not exist"), "{text}");
    let (text, is_error) = mcp
        .call("set_status", json!({"ids": ["SPOKE-2"], "status": "done"}))
        .await;
    assert!(is_error && text.contains("areas never finish"), "{text}");

    // The 2026-07-28 stateless protocol works without initialize.
    let meta = json!({
        "io.modelcontextprotocol/protocolVersion": "2026-07-28",
        "io.modelcontextprotocol/clientCapabilities": {}
    });
    let (status, body) = mcp
        .post(
            json!({"jsonrpc": "2.0", "id": 9, "method": "tools/call",
                   "params": {"name": "get", "arguments": {"id": "4"}, "_meta": meta}}),
            &[
                ("mcp-protocol-version", "2026-07-28"),
                ("mcp-method", "tools/call"),
                ("mcp-name", "get"),
            ],
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(body.contains("SPOKE-5 done"), "{body}");

    // Browser-origin requests are refused by default.
    let (status, _) = mcp
        .post(
            json!({"jsonrpc": "2.0", "id": 1, "method": "tools/list"}),
            &[
                ("mcp-protocol-version", "2025-11-25"),
                ("origin", "http://evil.example"),
            ],
        )
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN);

    // Restart on the same file: everything is still there.
    server.shutdown().await;
    let server = start(&db).await;
    let mcp = Mcp::new(server.addr);
    let root = mcp.ok("get", json!({})).await;
    assert!(root.contains("SPOKE-4 area  Hobbies"), "{root}");
    let added = mcp.ok("add", json!({"title": "After restart"})).await;
    assert!(
        added.starts_with("Added SPOKE-6 "),
        "ids are never reused: {added}"
    );
    server.shutdown().await;
    let _ = std::fs::remove_dir_all(dir);
}

/// `SPOKES_HINTS=0`: the same state change gives no "Next: " line, and the rest of the
/// reply is unchanged (SPOKE-1078).
#[tokio::test]
async fn hints_off_over_http() {
    let dir = temp_dir("hints-off");
    let db = dir.join("spokes.db");
    for hints in [true, false] {
        let _ = std::fs::remove_dir_all(&dir);
        let server = start_with(&db, chrono_tz::UTC, hints).await;
        let mcp = Mcp::new(server.addr);
        mcp.ok(
            "add",
            json!({"title": "Plan", "ordered": true,
                   "children": [{"title": "One"}, {"title": "Two"}]}),
        )
        .await; // SPOKE-2..4
        let first = mcp
            .ok("set_status", json!({"ids": ["SPOKE-3"], "status": "done"}))
            .await;
        let last = mcp
            .ok("set_status", json!({"ids": ["SPOKE-4"], "status": "done"}))
            .await;
        // The read and notes hints (SPOKE-1006), on a fresh open task nobody holds.
        mcp.ok("add", json!({"title": "Loose"})).await; // SPOKE-5
        let read = mcp.ok("get", json!({"id": "SPOKE-5"})).await;
        let noted = mcp
            .ok("update", json!({"id": "SPOKE-5", "notes_append": "x"}))
            .await;
        let queued = mcp.ok("next", json!({})).await;
        let claim_hint = "Next: Working on this? Claim it first (that marks it in_progress). Finish with set_status done, or release it.";
        if hints {
            assert!(read.ends_with(&format!("\n{claim_hint}")), "{read}");
            assert!(
                noted.ends_with("\nNext: Recording progress? Claim it so other agents skip it."),
                "{noted}"
            );
            assert!(
                queued.ends_with("\nNext: Claim the one you pick before starting."),
                "{queued}"
            );
        } else {
            for out in [&read, &noted, &queued] {
                assert!(!out.contains("Next: "), "{out}");
            }
        }
        if hints {
            assert_eq!(
                first,
                "SPOKE-3 done  One\nNext: SPOKE-4 Two, the next step of SPOKE-2 Plan. Claim it before starting."
            );
            assert!(
                last.starts_with("SPOKE-4 done  Two\nNext: All steps of SPOKE-2 Plan are done."),
                "{last}"
            );
        } else {
            assert_eq!(first, "SPOKE-3 done  One");
            assert_eq!(last, "SPOKE-4 done  Two");
        }
        server.shutdown().await;
    }
    let _ = std::fs::remove_dir_all(dir);
}

/// The M1 "done when": a six-step plan is written and walked in three calls, then
/// move, update and notes round-trip and the tree's error paths answer usefully.
#[tokio::test]
async fn tree_over_http() {
    let dir = temp_dir("tree");
    let server = start(&dir.join("spokes.db")).await;
    let mcp = Mcp::new(server.addr);

    mcp.ok(
        "add",
        json!({"title": "Hobbies", "kind": "area", "children": [
            {"title": "Programming", "kind": "area", "children": [{"title": "Skyforge", "kind": "area"}]}
        ]}),
    )
    .await; // SPOKE-2..4

    // Call 1: the whole plan in one add.
    let steps = [
        "Map the pipeline",
        "Write the pipeline",
        "Add secrets",
        "Push and run the deployment",
        "Verify in production",
        "Retro",
    ];
    let children: Vec<Value> = steps.iter().map(|t| json!({"title": t})).collect();
    let added = mcp
        .ok(
            "add",
            json!({"parent": "SPOKE-4", "title": "Deploy game to production", "ordered": true,
                   "priority": 1, "due": "2030-11-20", "notes": "Target: the VPS.",
                   "agent": "it", "children": children}),
        )
        .await;
    assert_eq!(
        added,
        "Added SPOKE-5 task  Deploy game to production  (under SPOKE-4 Skyforge), with 6 beneath it in the order given: SPOKE-6..SPOKE-11"
    );

    // Call 2: the plan with its next step.
    let plan = "SPOKE-5 task p1 due Nov 20 2030 ordered (0/6) +notes  Deploy game to production\n\
                Hobbies / Programming / Skyforge (SPOKE-4)\n  \
                1. SPOKE-6 open  Map the pipeline  <- next\n  \
                2. SPOKE-7 open  Write the pipeline\n  \
                3. SPOKE-8 open  Add secrets\n  \
                4. SPOKE-9 open  Push and run the deployment\n  \
                5. SPOKE-10 open  Verify in production\n  \
                6. SPOKE-11 open  Retro";
    assert_eq!(mcp.ok("get", json!({"id": "SPOKE-5"})).await, plan);

    // Call 3: finish step 1; the reply and the outline both point at step 2.
    assert_eq!(
        mcp.ok("set_status", json!({"ids": ["SPOKE-6"], "status": "done"}))
            .await,
        "SPOKE-6 done  Map the pipeline\nNext: SPOKE-7 Write the pipeline, the next step of SPOKE-5 Deploy game to production. Claim it before starting."
    );
    let walked = mcp.ok("get", json!({"id": "SPOKE-5"})).await;
    assert!(walked.contains("(1/6)"), "{walked}");
    assert!(
        walked.contains("1. SPOKE-6 done  Map the pipeline\n"),
        "{walked}"
    );
    assert!(
        walked.contains("2. SPOKE-7 open  Write the pipeline  <- next"),
        "{walked}"
    );
    assert_eq!(walked.matches("<- next").count(), 1, "{walked}");
    assert_eq!(
        queue(mcp.ok("next", json!({})).await),
        "UP NEXT\n  SPOKE-5  p1 due Nov 20 2030  Deploy game to production (1/6)  next: SPOKE-7 Write the pipeline"
    );

    // Depth: the root outline grows one level per step and says what it left out.
    let d1 = mcp.ok("get", json!({})).await;
    assert_eq!(
        d1,
        "SPOKE-1 area  To do\n  - SPOKE-2 area  Hobbies\n(+9 more below depth 1. Pass a larger depth, or get a child.)"
    );
    let d5 = mcp.ok("get", json!({"depth": "5"})).await;
    assert!(
        d5.contains("\n        - SPOKE-5 open p1 due Nov 20 2030 ordered (1/6) +notes  Deploy game to production\n          1. SPOKE-6 done"),
        "{d5}"
    );
    assert!(!d5.contains("more below depth"), "{d5}");

    // A later due date on a step warns about the parent's deadline.
    mcp.ok("update", json!({"id": "SPOKE-8", "due": "2030-12-01"}))
        .await;
    let warned = mcp.ok("get", json!({"id": "SPOKE-5"})).await;
    assert!(
        warned.contains("SPOKE-8 open due Dec 1 2030 (after parent's Nov 20 2030)  Add secrets"),
        "{warned}"
    );

    // Notes: flagged in listings, the body only with notes: true.
    assert_eq!(
        mcp.ok(
            "update",
            json!({"id": "SPOKE-7", "notes_append": "Used deploy.yml.", "agent": "it"})
        )
        .await,
        "Updated SPOKE-7  Write the pipeline  (notes appended)\nNext: Recording progress? Claim it so other agents skip it."
    );
    // SPOKE-1006: get hints at claiming only the step to start, not a later one.
    let start = mcp.ok("get", json!({"id": "SPOKE-7"})).await;
    assert!(start.ends_with("\nNext: Working on this? Claim it first (that marks it in_progress). Finish with set_status done, or release it."), "{start}");
    let later = mcp.ok("get", json!({"id": "SPOKE-8"})).await;
    assert!(!later.contains("Next: "), "{later}");
    mcp.ok(
        "update",
        json!({"id": "SPOKE-7", "notes_append": "Added a cache step."}),
    )
    .await;
    let listing = mcp.ok("get", json!({"id": "SPOKE-5"})).await;
    assert!(
        listing.contains("SPOKE-7 open +notes  Write the pipeline"),
        "{listing}"
    );
    assert!(!listing.contains("Used deploy.yml"), "{listing}");
    assert!(!listing.contains("Target: the VPS"), "{listing}");
    let body = mcp.ok("get", json!({"id": "SPOKE-7", "notes": true})).await;
    assert!(
        body.ends_with("NOTES\nUsed deploy.yml.\n\nAdded a cache step.\nNext: Working on this? Claim it first (that marks it in_progress). Finish with set_status done, or release it."),
        "{body}"
    );
    mcp.ok("update", json!({"id": "SPOKE-7", "notes": "Replaced."}))
        .await;
    let body = mcp.ok("get", json!({"id": "SPOKE-7", "notes": true})).await;
    assert!(body.ends_with("NOTES\nReplaced.\nNext: Working on this? Claim it first (that marks it in_progress). Finish with set_status done, or release it."), "{body}");
    mcp.ok("update", json!({"id": "SPOKE-7", "notes": ""}))
        .await;
    let body = mcp.ok("get", json!({"id": "SPOKE-7", "notes": true})).await;
    assert!(
        body.ends_with("(no notes)\nNext: Working on this? Claim it first (that marks it in_progress). Finish with set_status done, or release it.") && !body.contains("+notes"),
        "{body}"
    );

    // Fields round-trip.
    assert_eq!(
        mcp.ok(
            "update",
            json!({"id": "SPOKE-11", "title": "Retro and write-up", "priority": 2})
        )
        .await,
        "Updated SPOKE-11  Retro and write-up  (title, priority)"
    );
    mcp.ok("update", json!({"id": "SPOKE-11", "priority": 0}))
        .await;
    let listing = mcp.ok("get", json!({"id": "SPOKE-5"})).await;
    assert!(
        listing.contains("6. SPOKE-11 open  Retro and write-up"),
        "{listing}"
    );

    // Move: reorder within the plan, then reparent.
    assert_eq!(
        mcp.ok("move", json!({"id": "SPOKE-8", "before": "SPOKE-7"}))
            .await,
        "Moved SPOKE-8  Add secrets  (under SPOKE-5 Deploy game to production, 2 of 6)"
    );
    let listing = mcp.ok("get", json!({"id": "SPOKE-5"})).await;
    assert!(
        listing.contains("2. SPOKE-8 open due Dec 1 2030 (after parent's Nov 20 2030)  Add secrets  <- next\n  3. SPOKE-7 open  Write the pipeline\n"),
        "{listing}"
    );
    mcp.ok("move", json!({"id": "SPOKE-11", "after": "SPOKE-6"}))
        .await;
    assert_eq!(
        mcp.ok("move", json!({"id": "SPOKE-11", "parent": "SPOKE-4"}))
            .await,
        "Moved SPOKE-11  Retro and write-up  (under SPOKE-4 Skyforge, 2 of 2)"
    );
    let sky = mcp.ok("get", json!({"id": "SPOKE-4"})).await;
    assert!(
        sky.contains("(1/5) +notes  Deploy game to production\n  - SPOKE-11 open unplanned  Retro and write-up\n"),
        "{sky}"
    );

    // Error paths say what to do next.
    mcp.ok("add", json!({"title": "Garden", "kind": "area"}))
        .await; // SPOKE-12
    for (tool, args, fragment) in [
        (
            "move",
            json!({"id": "SPOKE-1", "parent": "SPOKE-2"}),
            "is the root",
        ),
        (
            "move",
            json!({"id": "SPOKE-2", "parent": "SPOKE-5"}),
            "cannot move under itself",
        ),
        (
            "move",
            json!({"id": "SPOKE-5", "parent": "SPOKE-5"}),
            "own parent",
        ),
        (
            "move",
            json!({"id": "SPOKE-12", "parent": "SPOKE-5"}),
            "areas can only go under areas",
        ),
        (
            "move",
            json!({"id": "SPOKE-7", "parent": "SPOKE-6"}),
            "is done",
        ),
        ("move", json!({"id": "SPOKE-7"}), "nothing to do"),
        (
            "move",
            json!({"id": "SPOKE-7", "before": "SPOKE-8", "after": "SPOKE-9"}),
            "not both",
        ),
        (
            "move",
            json!({"id": "SPOKE-7", "parent": "SPOKE-4", "before": "SPOKE-9"}),
            "is not under SPOKE-4",
        ),
        (
            "move",
            json!({"id": "SPOKE-404", "parent": "SPOKE-2"}),
            "does not exist",
        ),
        ("update", json!({"id": "SPOKE-7"}), "nothing to update"),
        (
            "update",
            json!({"id": "SPOKE-7", "notes": "a", "notes_append": "b"}),
            "not both",
        ),
        (
            "update",
            json!({"id": "SPOKE-7", "kind": "area"}),
            "areas can only go under areas",
        ),
        (
            "update",
            json!({"id": "SPOKE-7", "due": "friday"}),
            "YYYY-MM-DD",
        ),
        (
            "get",
            json!({"id": "SPOKE-5", "depth": "lots"}),
            "whole number",
        ),
        (
            "add",
            json!({"parent": "SPOKE-5", "title": "Plan", "children": [{"title": "ok"}, {"title": "Bad", "kind": "area"}]}),
            "children[1] (\"Bad\")",
        ),
    ] {
        let (text, is_error) = mcp.call(tool, args.clone()).await;
        assert!(is_error && text.contains(fragment), "{tool} {args}: {text}");
    }
    // The refused nested add wrote nothing; ids continue from the last real node.
    let added = mcp
        .ok("add", json!({"parent": "SPOKE-4", "title": "Later"}))
        .await;
    assert!(added.starts_with("Added SPOKE-13 "), "{added}");

    // Finishing the last open step suggests closing the parent, but never closes it.
    let done = mcp
        .ok(
            "set_status",
            json!({"ids": ["SPOKE-8", "SPOKE-7", "SPOKE-9", "SPOKE-10"], "status": "done"}),
        )
        .await;
    assert!(
        done.ends_with("Next: All steps of SPOKE-5 Deploy game to production are done. If it is finished too, mark it done with set_status."),
        "{done}"
    );
    let parent = mcp.ok("get", json!({"id": "SPOKE-5"})).await;
    assert!(
        parent.starts_with("SPOKE-5 task p1"),
        "still open: {parent}"
    );

    server.shutdown().await;
    let _ = std::fs::remove_dir_all(dir);
}

// ---------------------------------------------------------------------------
// Search, priority, focus, `next` sections and time zones.
// ---------------------------------------------------------------------------

/// The first `SPOKE-n` in a reply, e.g. the id `add` just created.
fn first_id(text: &str) -> String {
    let at = text
        .find("SPOKE-")
        .unwrap_or_else(|| panic!("no id in {text}"));
    let digits: String = text[at + 6..]
        .chars()
        .take_while(char::is_ascii_digit)
        .collect();
    format!("SPOKE-{digits}")
}

/// Adds a node and returns its id.
async fn add_node(mcp: &Mcp, args: Value) -> String {
    first_id(&mcp.ok("add", args).await)
}

/// The line of `text` that contains `needle`.
fn line_with<'a>(text: &'a str, needle: &str) -> &'a str {
    text.lines()
        .find(|l| l.contains(needle))
        .unwrap_or_else(|| panic!("no line with {needle:?} in:\n{text}"))
}

fn pos(text: &str, needle: &str) -> usize {
    text.find(needle)
        .unwrap_or_else(|| panic!("{needle:?} missing from:\n{text}"))
}

/// Same format as `format::short_date`: `Nov 9`, plus the year outside today's year.
fn short(date: chrono::NaiveDate, today: chrono::NaiveDate) -> String {
    use chrono::Datelike;
    if date.year() == today.year() {
        date.format("%b %-d").to_string()
    } else {
        date.format("%b %-d %Y").to_string()
    }
}

fn iso(date: chrono::NaiveDate) -> String {
    date.format("%Y-%m-%d").to_string()
}

async fn search(mcp: &Mcp, query: &str) -> String {
    mcp.ok("search", json!({"query": query})).await
}

#[tokio::test]
async fn search_over_http() {
    let dir = temp_dir("search");
    let server = start(&dir.join("spokes.db")).await;
    let mcp = Mcp::new(server.addr);

    let work = add_node(
        &mcp,
        json!({"title": "Work", "kind": "area", "priority": 2}),
    )
    .await;
    mcp.ok(
        "add",
        json!({"parent": work, "title": "Server", "aliases": ["work server", "prod box"]}),
    )
    .await;
    let hobbies = add_node(&mcp, json!({"title": "Hobbies", "kind": "area"})).await;
    let homelab = add_node(
        &mcp,
        json!({"parent": hobbies, "title": "Homelab", "kind": "area", "aliases": ["home lab"]}),
    )
    .await;
    let migration = add_node(
        &mcp,
        json!({"parent": homelab, "title": "Homelab server migration", "priority": 3,
               "aliases": ["nas", "move to new box"]}),
    )
    .await;
    mcp.ok(
        "add",
        json!({"parent": homelab, "title": "Proxmox host rebuild", "priority": 1, "ordered": true,
               "aliases": ["my server", "home server", "proxmox"],
               "children": [{"title": "Back up VMs"}, {"title": "Reinstall"}, {"title": "Restore VMs"}]}),
    )
    .await;
    mcp.ok("add", json!({"parent": hobbies, "title": "PC build"}))
        .await;
    let programming = add_node(
        &mcp,
        json!({"parent": hobbies, "title": "Programming", "kind": "area"}),
    )
    .await;
    mcp.ok(
        "add",
        json!({"parent": programming, "title": "Skyforj", "aliases": ["skyforge", "the game"]}),
    )
    .await;
    let home = add_node(&mcp, json!({"title": "Home", "kind": "area"})).await;
    for title in ["TV mount", "Café-shelf build", "Upcoming dentist", "Zebra"] {
        mcp.ok("add", json!({"parent": home, "title": title})).await;
    }
    mcp.ok(
        "add",
        json!({"parent": home, "title": "Striped animal", "aliases": ["zebra"]}),
    )
    .await;
    mcp.ok(
        "add",
        json!({"parent": home, "title": "Safari trip", "notes": "See the zebra crossing."}),
    )
    .await;

    // The model's natural phrasing finds the alias, with breadcrumb and level.
    let out = search(&mcp, "priority tasks for my server").await;
    let first = out.lines().nth(1).unwrap();
    assert!(
        first.contains("Proxmox host rebuild")
            && first.contains(" p1")
            && first.contains(&format!("in Hobbies ({hobbies}) / Homelab ({homelab})"))
            && first.contains("(alias \"my server\")"),
        "{out}"
    );
    let line = line_with(&out, "Server  in Work");
    assert!(
        line.contains(" p2") && line.ends_with(&format!("in Work ({work})")),
        "{line}"
    );

    // Normalization: hyphens, spacing, case and accents do not matter.
    for q in ["home-lab", "homelab", "home lab", "HOME LAB"] {
        let out = search(&mcp, q).await;
        assert!(out.contains("Homelab server migration"), "{q}: {out}");
    }
    for q in ["cafe shelf", "CAFÉ", "cafe-shelf"] {
        let out = search(&mcp, q).await;
        assert!(out.contains("Café-shelf build"), "{q}: {out}");
    }

    // Typos.
    for (q, title) in [
        ("sevrer", "Server  in Work"),
        ("skyforj", "Skyforj"),
        ("sky forge", "Skyforj"),
        ("proxmx", "Proxmox host rebuild"),
    ] {
        let out = search(&mcp, q).await;
        assert!(out.contains(title), "{q}: {out}");
    }

    // Short keywords match whole words only.
    let out = search(&mcp, "pc").await;
    assert!(out.contains("PC build"), "{out}");
    assert!(!out.contains("Upcoming dentist"), "{out}");
    let out = search(&mcp, "tv").await;
    assert!(out.contains("TV mount"), "{out}");

    // Ranking: title > alias > notes.
    let out = search(&mcp, "zebra").await;
    let (t, a, n) = (
        pos(&out, "Zebra  in Home"),
        pos(&out, "Striped animal"),
        pos(&out, "Safari trip"),
    );
    assert!(t < a && a < n, "{out}");
    assert!(line_with(&out, "Striped animal").contains("(alias \"zebra\")"));
    assert!(line_with(&out, "Safari trip").contains("(in notes)"));

    // Scope.
    let out = mcp
        .ok("search", json!({"query": "server", "scope": hobbies}))
        .await;
    assert!(out.contains("Homelab server migration"), "{out}");
    assert!(!out.contains("in Work"), "{out}");
    let out = search(&mcp, "server").await;
    assert!(out.contains("in Work"), "{out}");

    // Done and dropped nodes hide, with a note; their descendants too.
    mcp.ok("set_status", json!({"ids": [migration], "status": "done"}))
        .await;
    let out = search(&mcp, "migration").await;
    assert!(out.starts_with("No matches"), "{out}");
    assert!(
        out.ends_with("(1 done or dropped matches hidden; pass include_done: true to see them.)"),
        "{out}"
    );
    let out = mcp
        .ok(
            "search",
            json!({"query": "migration", "include_done": true}),
        )
        .await;
    assert!(
        line_with(&out, "Homelab server migration").contains(" done "),
        "{out}"
    );
    let parent = add_node(
        &mcp,
        json!({"parent": home, "title": "Quokka plan",
        "children": [{"title": "Quokka feeding"}]}),
    )
    .await;
    assert!(search(&mcp, "quokka").await.contains("Quokka feeding"));
    mcp.ok("set_status", json!({"ids": [parent], "status": "dropped"}))
        .await;
    let out = search(&mcp, "quokka").await;
    assert!(
        out.starts_with("No matches")
            && out.contains("(2 done or dropped matches hidden; pass include_done: true"),
        "{out}"
    );
    let out = mcp
        .ok("search", json!({"query": "quokka", "include_done": true}))
        .await;
    assert!(line_with(&out, "Quokka feeding").contains("task dropped") || out.contains("dropped"));

    // Cap.
    let out = mcp
        .ok("search", json!({"query": "server", "limit": 1}))
        .await;
    assert_eq!(out.lines().filter(|l| l.starts_with("  SPOKE-")).count(), 1);
    assert!(
        out.contains(
            "\n(+1 more matches. Pass offset: 1 for the next page, or add keywords or scope.)"
        ),
        "{out}"
    );

    // The index follows updates.
    let toolbox = add_node(&mcp, json!({"parent": home, "title": "Oldtitle gadget"})).await;
    assert!(search(&mcp, "oldtitle").await.contains("Oldtitle gadget"));
    mcp.ok(
        "update",
        json!({"id": toolbox, "title": "Newname gadget", "aliases": ["wrench rack"],
               "notes": "Includes a plumbus."}),
    )
    .await;
    assert!(search(&mcp, "newname").await.contains("Newname gadget"));
    assert!(search(&mcp, "wrench").await.contains("Newname gadget"));
    assert!(search(&mcp, "plumbus").await.contains("Newname gadget"));
    assert!(search(&mcp, "oldtitle").await.starts_with("No matches"));

    // A query with no words is an error.
    let (text, is_error) = mcp.call("search", json!({"query": "  !! "})).await;
    assert!(is_error && text.contains("no words"), "{text}");

    server.shutdown().await;
    let _ = std::fs::remove_dir_all(dir);
}

#[tokio::test]
async fn priority_inheritance_over_http() {
    let dir = temp_dir("priority");
    let server = start(&dir.join("spokes.db")).await;
    let mcp = Mcp::new(server.addr);

    // 2 Urgent area (p1): 3 Inherits, 4 Plain task holding 5..7, 8 Ordered holding 9, 10.
    let urgent = add_node(
        &mcp,
        json!({"title": "Urgent", "kind": "area", "priority": 1, "children": [
            {"title": "Inherits"},
            {"title": "Container", "children": [
                {"title": "Deep plain"},
                {"title": "Deep own level", "priority": 3},
                {"title": "Deep dated", "due": "2099-01-01"}]},
            {"title": "Pipeline", "ordered": true, "children": [
                {"title": "Step one"},
                {"title": "Step two", "priority": 2}]}
        ]}),
    )
    .await;
    assert_eq!(urgent, "SPOKE-2");
    // 11 Calm area (p3) > 12; 13 Loose area > 14 (p4), 15 (p2), 16 (unset), 17 (p2, later).
    mcp.ok(
        "add",
        json!({"title": "Calm", "kind": "area", "priority": 3, "children": [{"title": "Calm task"}]}),
    )
    .await;
    mcp.ok(
        "add",
        json!({"title": "Loose", "kind": "area", "children": [
            {"title": "Four", "priority": 4},
            {"title": "Two", "priority": 2},
            {"title": "Unset"},
            {"title": "Two later", "priority": 2}]}),
    )
    .await;

    // Level 1 (inherited from the area), then 2, 3, 4, then unset; oldest first within a
    // level. "Deep own level" shows its own p3 under the p1 area. "Deep plain" and
    // "Step one" carry no level or date, and blocked "Step two" stays hidden despite its p2.
    let out = queue(mcp.ok("next", json!({"limit": 50})).await);
    assert_eq!(
        out,
        "UP NEXT\n\
         \x20 SPOKE-3   p1                 Inherits  unplanned\n\
         \x20 SPOKE-4   p1                 Container (0/3)  next: SPOKE-5 Deep plain\n\
         \x20 SPOKE-7   p1 due Jan 1 2099  Deep dated  unplanned\n\
         \x20 SPOKE-8   p1                 Pipeline (0/2)  next: SPOKE-9 Step one\n\
         \x20 SPOKE-15  p2                 Two  unplanned\n\
         \x20 SPOKE-17  p2                 Two later  unplanned\n\
         \x20 SPOKE-6   p3                 Deep own level  unplanned\n\
         \x20 SPOKE-12  p3                 Calm task  unplanned\n\
         \x20 SPOKE-14  p4                 Four  unplanned\n\
         \x20 SPOKE-16                     Unset  unplanned"
    );
    for hidden in ["Deep plain  ", "Step one  ", "Step two"] {
        assert!(!out.contains(hidden), "{hidden}: {out}");
    }

    // Raising the ordered task's first step makes it a work item, but its parent's line
    // (p1, higher up) already points at it, so it is not listed twice. Later steps stay
    // blocked.
    mcp.ok("update", json!({"id": "SPOKE-9", "priority": 4}))
        .await;
    mcp.ok("update", json!({"id": "SPOKE-10", "due": "2099-01-02"}))
        .await;
    let out = queue(mcp.ok("next", json!({"limit": 50})).await);
    assert_eq!(out.matches("Step one").count(), 1, "{out}");
    assert!(
        out.contains("Pipeline (0/2)  next: SPOKE-9 Step one"),
        "{out}"
    );
    assert!(!out.contains("Step two"), "{out}");

    server.shutdown().await;
    let _ = std::fs::remove_dir_all(dir);
}

async fn focus_err(mcp: &Mcp, args: Value, fragment: &str) {
    let (text, is_error) = mcp.call("focus", args.clone()).await;
    assert!(is_error && text.contains(fragment), "{args}: {text}");
}

#[tokio::test]
async fn focus_over_http() {
    let dir = temp_dir("focus");
    let server = start(&dir.join("spokes.db")).await;
    let mcp = Mcp::new(server.addr);

    // 2 Home, 3 Alpha, 4 Beta > 5 Beta step, 6 Garden (area), 7 Delta, 8 Echo.
    mcp.ok(
        "add",
        json!({"title": "Home", "kind": "area", "children": [
            {"title": "Alpha"},
            {"title": "Beta", "children": [{"title": "Beta step"}]},
            {"title": "Garden", "kind": "area"},
            {"title": "Delta"},
            {"title": "Echo"}]}),
    )
    .await;

    let focus = |args: Value| {
        let mcp = &mcp;
        async move { mcp.ok("focus", args).await }
    };
    assert_eq!(
        focus(json!({"id": 3})).await,
        "SPOKE-3 Alpha is now #1 of 1 on the focus list."
    );
    assert_eq!(
        focus(json!({"id": 5})).await,
        "SPOKE-5 Beta step is now #2 of 2 on the focus list."
    );
    assert_eq!(
        focus(json!({"id": 6})).await,
        "SPOKE-6 Garden is now #3 of 3 on the focus list."
    );
    assert_eq!(
        focus(json!({"id": 3})).await,
        "SPOKE-3 Alpha is already #1 of 3 on the focus list. Pass rank, before or after to move it."
    );
    // List: 3 5 6. Beta to rank 2 -> 3 4 5 6.
    assert_eq!(
        focus(json!({"id": 4, "rank": 2})).await,
        "SPOKE-4 Beta is now #2 of 4 on the focus list."
    );
    // Garden before Alpha -> 6 3 4 5.
    assert_eq!(
        focus(json!({"id": 6, "before": "SPOKE-3"})).await,
        "SPOKE-6 Garden is now #1 of 4 on the focus list."
    );
    // Alpha after Beta step -> 6 4 5 3.
    assert_eq!(
        focus(json!({"id": 3, "after": "SPOKE-5"})).await,
        "SPOKE-3 Alpha is now #4 of 4 on the focus list."
    );
    // A rank past the end clamps to last -> 4 5 3 6.
    assert_eq!(
        focus(json!({"id": 6, "rank": 99})).await,
        "SPOKE-6 Garden is now #4 of 4 on the focus list."
    );
    // Garden up to the top again -> 6 4 5 3.
    focus(json!({"id": 6, "rank": 1})).await;

    // Errors.
    focus_err(
        &mcp,
        json!({"id": 5, "before": "SPOKE-7"}),
        "is not on the focus list",
    )
    .await;
    focus_err(
        &mcp,
        json!({"id": 5, "after": "SPOKE-7"}),
        "is not on the focus list",
    )
    .await;
    focus_err(
        &mcp,
        json!({"id": 5, "before": "SPOKE-5"}),
        "cannot be placed before or after itself",
    )
    .await;
    focus_err(&mcp, json!({"id": 1}), "is the root").await;
    focus_err(&mcp, json!({"id": 7, "rank": 0}), "rank must be 1 or more").await;
    focus_err(
        &mcp,
        json!({"id": 7, "rank": 1, "before": "SPOKE-3"}),
        "pass only one of rank, before or after",
    )
    .await;
    focus_err(
        &mcp,
        json!({"id": 7, "before": "SPOKE-3", "after": "SPOKE-4"}),
        "pass only one of rank, before or after",
    )
    .await;
    focus_err(
        &mcp,
        json!({"id": 3, "remove": true, "rank": 1}),
        "on its own",
    )
    .await;
    focus_err(&mcp, json!({"id": 404}), "does not exist").await;
    mcp.ok("set_status", json!({"ids": [8], "status": "done"}))
        .await;
    focus_err(&mcp, json!({"id": 8}), "is done").await;

    // `next`: FOCUS first, numbered in rank order, and no repeats further down.
    let out = queue(mcp.ok("next", json!({"limit": 20})).await);
    assert_eq!(
        out,
        "FOCUS\n\
         \x20 1. SPOKE-6  Garden\n\
         \x20 2. SPOKE-4  Beta (0/1)  next: SPOKE-5 Beta step\n\
         \x20 3. SPOKE-5  Beta step  unplanned\n\
         \x20 4. SPOKE-3  Alpha  unplanned\n\
         UP NEXT\n\
         \x20 SPOKE-7  Delta  unplanned"
    );

    // Removing and a non-member.
    assert_eq!(
        focus(json!({"id": 6, "remove": true})).await,
        "Took SPOKE-6 Garden off the focus list (3 left)."
    );
    assert_eq!(
        focus(json!({"id": 6, "remove": true})).await,
        "SPOKE-6 Garden is not on the focus list."
    );

    // Finishing a node takes it off the list.
    let out = mcp
        .ok("set_status", json!({"ids": [3], "status": "done"}))
        .await;
    assert!(out.contains("(taken off the focus list)"), "{out}");
    let out = queue(mcp.ok("next", json!({"limit": 20})).await);
    assert!(out.starts_with("FOCUS\n  1. SPOKE-4  Beta"), "{out}");
    assert!(!out.contains("Alpha"), "{out}");

    // More than ten entries draws a warning from both `focus` and `next`.
    for i in 0..8 {
        let id = add_node(&mcp, json!({"parent": 2, "title": format!("Extra {i}")})).await;
        let out = focus(json!({"id": id})).await;
        assert!(!out.contains("keep it to about 10"), "{out}");
    }
    // The list holds Beta, Beta step and eight extras: exactly 10, no warning yet.
    let id = add_node(&mcp, json!({"parent": 2, "title": "One too many"})).await;
    let out = focus(json!({"id": id})).await;
    assert!(
        out.ends_with("The focus list has 11 entries; keep it to about 10. Take some off with focus remove: true."),
        "{out}"
    );
    let out = queue(mcp.ok("next", json!({"limit": 50})).await);
    assert!(
        out.ends_with("(The focus list has 11 entries; keep it to about 10. Take some off with focus remove: true.)"),
        "{out}"
    );

    server.shutdown().await;
    let _ = std::fs::remove_dir_all(dir);
}

/// The leading `SPOKE-n` of every item line (not headers or the footer).
/// A `next` reply without its claim footer (checked in `walking_skeleton_over_http`).
fn queue(out: String) -> String {
    match out.strip_suffix("\nNext: Claim the one you pick before starting.") {
        Some(body) => body.to_string(),
        None => out,
    }
}

fn listed_ids(out: &str) -> Vec<String> {
    out.lines()
        .filter(|l| l.starts_with("  "))
        .map(|l| {
            let l = l.trim_start();
            let l = l.split_once(". ").map_or(l, |(n, rest)| {
                if n.chars().all(|c| c.is_ascii_digit()) {
                    rest
                } else {
                    l
                }
            });
            l.split_whitespace().next().unwrap().to_string()
        })
        .collect()
}

#[tokio::test]
async fn due_and_sections_over_http() {
    use chrono::Duration;
    let dir = temp_dir("sections");
    let db = dir.join("spokes.db");
    let server = start(&db).await;
    let mcp = Mcp::new(server.addr);
    let today = chrono::Utc::now().date_naive();
    let due = |days: i64| iso(today + Duration::days(days));

    // 2 Home; 3 overdue, 4 today, 5 soon, 6 far, 7 plain, 8 plain p1, 9 waiting,
    // 10 stalled, 11..14 stale, 15 focused.
    mcp.ok(
        "add",
        json!({"title": "Home", "kind": "area", "children": [
            {"title": "Overdue job", "due": due(-3)},
            {"title": "Due today job", "due": due(0)},
            {"title": "Soon job", "due": due(5)},
            {"title": "Far job", "due": due(20)},
            {"title": "Plain job"},
            {"title": "Plain p1 job", "priority": 1},
            {"title": "Waiting job"},
            {"title": "Stalled job"},
            {"title": "Stale one"},
            {"title": "Stale two"},
            {"title": "Stale three"},
            {"title": "Stale four"},
            {"title": "Focused job"}]}),
    )
    .await;
    mcp.ok("focus", json!({"id": 15})).await;

    // Without NEEDS YOU or STALE yet: DUE SOON order, and the +20 day task is UP NEXT.
    let out = queue(mcp.ok("next", json!({"limit": 50})).await);
    let (d_over, d_soon, d_far) = (
        short(today - Duration::days(3), today),
        short(today + Duration::days(5), today),
        short(today + Duration::days(20), today),
    );
    let due_block = out
        .split("DUE SOON\n")
        .nth(1)
        .unwrap()
        .split("\nUP NEXT")
        .next()
        .unwrap();
    let fw = format!("overdue {d_over}").len();
    assert_eq!(
        due_block.lines().collect::<Vec<_>>(),
        [
            format!(
                "  SPOKE-3   {:<fw$}  Overdue job  unplanned",
                format!("overdue {d_over}")
            ),
            format!("  SPOKE-4   {:<fw$}  Due today job  unplanned", "due today"),
            format!(
                "  SPOKE-5   {:<fw$}  Soon job  unplanned",
                format!("due {d_soon}")
            ),
        ],
        "{out}"
    );
    let up_next = out.split("UP NEXT\n").nth(1).unwrap();
    assert!(up_next.contains(&format!("due {d_far}")), "{out}");
    assert!(up_next.contains("Far job"), "{out}");
    assert!(!due_block.contains("Far job"), "{out}");

    // Simulate NEEDS YOU and STALE by editing the database file under the running server.
    let conn = rusqlite::Connection::open(&db).unwrap();
    conn.busy_timeout(std::time::Duration::from_secs(5))
        .unwrap();
    conn.execute_batch(
        "UPDATE nodes SET status='waiting', waiting_on='Which disk?', claimed_by='sonnet-2' WHERE id=9;
         UPDATE nodes SET status='in_progress', claimed_by='opus-red',
                claim_expires='2020-01-01T00:00:00Z' WHERE id=10;
         UPDATE nodes SET touched_at='2020-01-01T00:00:00Z' WHERE id IN (11, 12, 13, 14);",
    )
    .unwrap();
    drop(conn);

    let out = queue(mcp.ok("next", json!({"limit": 8})).await);
    let days = (chrono::Utc::now()
        - "2020-01-01T00:00:00Z"
            .parse::<chrono::DateTime<chrono::Utc>>()
            .unwrap())
    .num_days();
    // Ids are padded to SPOKE-15 (8 wide) in every section; flags within a section.
    let overdue = format!("overdue {d_over}");
    let untouched = format!("untouched {days}d");
    let uw = untouched.len();
    let up_flags = format!("due {d_far}");
    let upw = up_flags.len().max(2);
    let lines: Vec<&str> = out.lines().collect();
    let headers: Vec<&str> = lines
        .iter()
        .copied()
        .filter(|l| !l.starts_with(' ') && !l.starts_with('('))
        .collect();
    assert_eq!(
        headers,
        ["NEEDS YOU", "FOCUS", "DUE SOON", "UP NEXT", "STALE"],
        "{out}"
    );
    let expected_tail = [
        "NEEDS YOU".to_string(),
        String::new(), // waiting line, checked below
        format!("  SPOKE-10  stalled  Stalled job  (claim by opus-red expired {days}d ago)"),
        "FOCUS".to_string(),
        "  1. SPOKE-15  Focused job  unplanned".to_string(),
        "DUE SOON".to_string(),
        format!("  SPOKE-3   {overdue:<fw$}  Overdue job  unplanned"),
        format!("  SPOKE-4   {:<fw$}  Due today job  unplanned", "due today"),
        format!(
            "  SPOKE-5   {:<fw$}  Soon job  unplanned",
            format!("due {d_soon}")
        ),
        "UP NEXT".to_string(),
        format!("  SPOKE-8   {:<upw$}  Plain p1 job  unplanned", "p1"),
        format!("  SPOKE-6   {up_flags:<upw$}  Far job  unplanned"),
        "STALE".to_string(),
        format!("  SPOKE-11  {untouched:<uw$}  Stale one"),
        format!("  SPOKE-12  {untouched:<uw$}  Stale two"),
        format!("  SPOKE-13  {untouched:<uw$}  Stale three"),
        "(+2 more open. Pass scope or limit.)".to_string(),
    ];
    assert_eq!(lines.len(), expected_tail.len(), "{out}");
    for (i, (got, want)) in lines.iter().zip(&expected_tail).enumerate() {
        if i == 1 {
            assert!(
                got.starts_with("  SPOKE-9   waiting  Waiting job: \"Which disk?\" (sonnet-2, ")
                    && got.ends_with(')'),
                "{out}"
            );
        } else {
            assert_eq!(got, want, "line {i} of:\n{out}");
        }
    }
    let ids = listed_ids(&out);
    let unique: std::collections::HashSet<_> = ids.iter().collect();
    assert_eq!(ids.len(), unique.len(), "an id repeats: {out}");
    // Stale 4 and Plain job are over the limit and only counted.
    assert!(
        !out.contains("Stale four") && !out.contains("Plain job"),
        "{out}"
    );

    // With room for everything, nothing repeats and STALE stays at three lines.
    let out = queue(mcp.ok("next", json!({"limit": 50})).await);
    let ids = listed_ids(&out);
    let unique: std::collections::HashSet<_> = ids.iter().collect();
    assert_eq!(ids.len(), unique.len(), "an id repeats: {out}");
    assert!(!out.contains("(+"), "{out}");
    let stale = out.split("STALE\n").nth(1).map_or(0, |s| s.lines().count());
    assert!(stale <= 3, "{out}");

    // Scope: an area lists only what is under it.
    let work = add_node(&mcp, json!({"title": "Work", "kind": "area"})).await;
    mcp.ok("add", json!({"parent": work, "title": "Elsewhere job"}))
        .await;
    assert!(queue(mcp.ok("next", json!({"limit": 50})).await).contains("Elsewhere job"));
    let scoped = queue(mcp.ok("next", json!({"scope": 2, "limit": 50})).await);
    assert!(
        !scoped.contains("Elsewhere job") && scoped.contains("Soon job"),
        "{scoped}"
    );
    assert_eq!(
        queue(mcp.ok("next", json!({"scope": work})).await),
        "UP NEXT\n  SPOKE-17  Elsewhere job  unplanned"
    );

    server.shutdown().await;
    let _ = std::fs::remove_dir_all(dir);
}

#[tokio::test]
async fn timezone_over_http() {
    use chrono_tz::Pacific::{Kiritimati, Pago_Pago};
    let dir = temp_dir("tz");
    let utc_now = chrono::Utc::now();
    let kiri_today = utc_now.with_timezone(&Kiritimati).date_naive();
    let pago_today = utc_now.with_timezone(&Pago_Pago).date_naive();
    assert!(kiri_today > pago_today, "UTC+14 is always ahead of UTC-11");

    let kiri = start_tz(&dir.join("kiri.db"), Kiritimati).await;
    let pago = start_tz(&dir.join("pago.db"), Pago_Pago).await;
    let (k, p) = (Mcp::new(kiri.addr), Mcp::new(pago.addr));
    for mcp in [&k, &p] {
        mcp.ok(
            "add",
            json!({"title": "Home", "kind": "area", "children": [
            {"title": "Kiritimati day", "due": iso(kiri_today)},
            {"title": "Pago Pago day", "due": iso(pago_today)}]}),
        )
        .await;
    }

    // Two rows of DUE SOON, earliest date first, flags padded to the wider one.
    let due_soon = |rows: [(&str, String, &str); 2]| {
        let w = rows.iter().map(|r| r.1.len()).max().unwrap();
        let line = |(id, flags, title): &(&str, String, &str)| {
            format!("  {id}  {flags:<w$}  {title}  unplanned")
        };
        format!("DUE SOON\n{}\n{}", line(&rows[0]), line(&rows[1]))
    };

    // Kiritimati: its own today is "due today"; Pago Pago's today is already past.
    assert_eq!(
        queue(k.ok("next", json!({})).await),
        due_soon([
            (
                "SPOKE-4",
                format!("overdue {}", short(pago_today, kiri_today)),
                "Pago Pago day"
            ),
            ("SPOKE-3", "due today".to_string(), "Kiritimati day"),
        ])
    );
    // Pago Pago: its own today is "due today"; Kiritimati's today is still ahead.
    assert_eq!(
        queue(p.ok("next", json!({})).await),
        due_soon([
            ("SPOKE-4", "due today".to_string(), "Pago Pago day"),
            (
                "SPOKE-3",
                format!("due {}", short(kiri_today, pago_today)),
                "Kiritimati day"
            ),
        ])
    );

    kiri.shutdown().await;
    pago.shutdown().await;
    let _ = std::fs::remove_dir_all(dir);
}

// ---------------------------------------------------------------------------
// M3: claims, leases, questions and answers, and the event log.
// ---------------------------------------------------------------------------

/// A second connection to the server's database file, to read state and to move
/// timestamps into the past (lease expiry, question age) without waiting.
fn db(path: &Path) -> rusqlite::Connection {
    let conn = rusqlite::Connection::open(path).unwrap();
    conn.busy_timeout(std::time::Duration::from_secs(5))
        .unwrap();
    conn
}

fn ts_ago(minutes: i64) -> String {
    (chrono::Utc::now() - chrono::Duration::minutes(minutes))
        .to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
}

fn claim_expires(conn: &rusqlite::Connection, id: &str) -> String {
    let n: i64 = id.trim_start_matches("SPOKE-").parse().unwrap();
    conn.query_row("SELECT claim_expires FROM nodes WHERE id = ?1", [n], |r| {
        r.get(0)
    })
    .unwrap()
}

fn events(conn: &rusqlite::Connection, id: &str) -> Vec<(String, Option<String>, String)> {
    let n: i64 = id.trim_start_matches("SPOKE-").parse().unwrap();
    let mut stmt = conn
        .prepare(
            "SELECT type, agent, COALESCE(detail, '') FROM events WHERE node_id = ?1 ORDER BY id",
        )
        .unwrap();
    stmt.query_map([n], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap()
}

async fn err(mcp: &Mcp, tool: &str, args: Value) -> String {
    let (text, is_error) = mcp.call(tool, args).await;
    assert!(is_error, "{tool} should have failed: {text}");
    text
}

/// The spec's "done when": two terminals asking "what's next" get different tasks.
#[tokio::test]
async fn two_agents_get_different_tasks_over_http() {
    let dir = temp_dir("agents");
    let path = dir.join("spokes.db");
    let tz: chrono_tz::Tz = "America/Chicago".parse().unwrap();
    let server = start_tz(&path, tz).await;
    let red = Mcp::new(server.addr);
    let blue = Mcp::new(server.addr);

    add_node(
        &red,
        json!({"title": "Work", "kind": "area", "children": [
            {"title": "Fix login bug", "priority": 1},
            {"title": "Write release notes", "priority": 2},
            {"title": "Tidy backlog", "priority": 3}]}),
    )
    .await; // 2: 3, 4, 5

    // Both terminals see the same top task.
    let first = queue(red.ok("next", json!({})).await);
    assert_eq!(first, queue(blue.ok("next", json!({})).await));
    let top = listed_ids(&first)[0].clone();
    assert_eq!(top, "SPOKE-3");

    // Red claims it; blue's next no longer has it and offers the next task.
    let out = red
        .ok("claim", json!({"id": top, "agent": "opus-red"}))
        .await;
    let expires = claim_expires(&db(&path), &top);
    let until = spokes::format::clock(&expires, tz);
    assert_eq!(
        out,
        format!(
            "Claimed SPOKE-3 Fix login bug for opus-red until {until}. It is hidden from next for everyone else. Finish with set_status done, release, or ask."
        )
    );
    let lease =
        chrono::DateTime::parse_from_rfc3339(&expires).unwrap() - chrono::Utc::now().fixed_offset();
    assert!((58..=60).contains(&lease.num_minutes()), "{lease}");

    let seen = queue(blue.ok("next", json!({})).await);
    assert!(!seen.contains("SPOKE-3 "), "{seen}");
    let blue_top = listed_ids(&seen)[0].clone();
    assert_eq!(blue_top, "SPOKE-4");
    blue.ok("claim", json!({"id": blue_top, "agent": "sonnet-blue"}))
        .await;
    let seen = queue(red.ok("next", json!({})).await);
    assert_eq!(listed_ids(&seen), ["SPOKE-5"], "{seen}");

    // Blue cannot take red's task; the error says who, until when, and what to do.
    let e = err(
        &blue,
        "claim",
        json!({"id": "SPOKE-3", "agent": "sonnet-blue"}),
    )
    .await;
    assert_eq!(
        e,
        format!(
            "SPOKE-3 is claimed by opus-red until {until}. Pick another task, or ask the user."
        )
    );
    // Red claiming again renews its own lease.
    let out = red
        .ok(
            "claim",
            json!({"id": "SPOKE-3", "agent": "opus-red", "lease_minutes": 120}),
        )
        .await;
    assert!(
        out.starts_with("Renewed SPOKE-3 Fix login bug for opus-red until "),
        "{out}"
    );
    let lease = chrono::DateTime::parse_from_rfc3339(&claim_expires(&db(&path), "SPOKE-3"))
        .unwrap()
        - chrono::Utc::now().fixed_offset();
    assert!((118..=120).contains(&lease.num_minutes()), "{lease}");

    // get shows who holds it.
    let shown = blue.ok("get", json!({"id": "SPOKE-3"})).await;
    assert!(shown.starts_with("SPOKE-3 task in_progress p1"), "{shown}");
    assert!(shown.contains("\nclaimed by opus-red until "), "{shown}");

    // Release puts it back with the note; only the holder can release.
    let e = err(
        &blue,
        "release",
        json!({"id": "SPOKE-3", "agent": "sonnet-blue"}),
    )
    .await;
    assert!(e.ends_with("Only opus-red can release it."), "{e}");
    let out = red
        .ok(
            "release",
            json!({"id": "SPOKE-3", "agent": "opus-red", "note": "Repro found; fix needs a schema change."}),
        )
        .await;
    assert_eq!(
        out,
        "Released SPOKE-3 Fix login bug; it is open in the queue again. Your note was added to its notes."
    );
    let notes = blue.ok("get", json!({"id": 3, "notes": true})).await;
    assert!(
        notes.contains("Released by opus-red (")
            && notes.contains("): Repro found; fix needs a schema change."),
        "{notes}"
    );
    assert_eq!(
        listed_ids(&queue(blue.ok("next", json!({})).await)),
        ["SPOKE-3", "SPOKE-5"]
    );
    let e = err(
        &red,
        "release",
        json!({"id": "SPOKE-3", "agent": "opus-red"}),
    )
    .await;
    assert!(e.contains("is not claimed (open)"), "{e}");

    // Done clears the claim.
    blue.ok(
        "set_status",
        json!({"ids": ["SPOKE-4"], "status": "done", "agent": "sonnet-blue"}),
    )
    .await;
    let (by, exp): (Option<String>, Option<String>) = db(&path)
        .query_row(
            "SELECT claimed_by, claim_expires FROM nodes WHERE id = 4",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    assert_eq!((by, exp), (None, None));

    let log = events(&db(&path), "SPOKE-3");
    let kinds: Vec<&str> = log.iter().map(|e| e.0.as_str()).collect();
    assert_eq!(kinds, ["created", "claimed", "claimed", "released"]);
    assert_eq!(log[1].1.as_deref(), Some("opus-red"));
    assert!(log[2].2.starts_with("renewed until "), "{:?}", log[2]);
    assert_eq!(log[3].2, "Repro found; fix needs a schema change.");

    server.shutdown().await;
    let _ = std::fs::remove_dir_all(dir);
}

#[tokio::test]
async fn claim_rules_and_lease_renewal_over_http() {
    let dir = temp_dir("claim-rules");
    let path = dir.join("spokes.db");
    let server = start(&path).await;
    let mcp = Mcp::new(server.addr);

    // 2 Home area; 3 Garage project (ordered) with 4, 5; 6 Shed with 7; 8 Done task.
    add_node(
        &mcp,
        json!({"title": "Home", "kind": "area", "children": [
            {"title": "Garage", "ordered": true, "children": [{"title": "Clear"}, {"title": "Paint"}]},
            {"title": "Shed", "children": [{"title": "Roof"}]},
            {"title": "Old"}]}),
    )
    .await;
    mcp.ok("set_status", json!({"ids": [8], "status": "done"}))
        .await;

    let e = err(&mcp, "claim", json!({"id": 2, "agent": "red"})).await;
    assert_eq!(
        e,
        "SPOKE-2 Home is an area; areas are never claimed. Claim a task under it (next with scope SPOKE-2)."
    );
    let e = err(&mcp, "claim", json!({"id": 1, "agent": "red"})).await;
    assert!(e.contains("is the root"), "{e}");
    let e = err(&mcp, "claim", json!({"id": 8, "agent": "red"})).await;
    assert_eq!(
        e,
        "SPOKE-8 Old is done. Reopen it with set_status open first, or pick another task."
    );
    let e = err(&mcp, "claim", json!({"id": 404, "agent": "red"})).await;
    assert!(e.starts_with("SPOKE-404 does not exist"), "{e}");
    let e = err(&mcp, "claim", json!({"id": 3, "agent": "  "})).await;
    assert!(e.starts_with("agent is empty"), "{e}");
    let e = err(
        &mcp,
        "claim",
        json!({"id": 3, "agent": "red", "lease_minutes": 0}),
    )
    .await;
    assert!(e.starts_with("lease_minutes must be 1 to 1440"), "{e}");

    // A claimed project hides its whole subtree. The orchestrator pattern: red holds the
    // project, a worker (blue) claims a step inside it under its own label. The step
    // stays hidden from next (its ancestor is claimed); get shows the worker's claim.
    mcp.ok("claim", json!({"id": 3, "agent": "red"})).await;
    let q = queue(mcp.ok("next", json!({})).await);
    assert!(!q.contains("Garage") && !q.contains("Clear"), "{q}");
    let c = mcp.ok("claim", json!({"id": 4, "agent": "blue"})).await;
    assert!(
        c.starts_with("Claimed SPOKE-4 Clear for blue until ")
            && c.contains("It is a step of SPOKE-3 Garage, which red holds as the orchestrator."),
        "{c}"
    );
    let q = queue(mcp.ok("next", json!({})).await);
    assert!(!q.contains("Clear"), "{q}");
    let g = mcp.ok("get", json!({"id": 4})).await;
    assert!(g.contains("\nclaimed by blue until "), "{g}");
    let g = mcp.ok("get", json!({"id": 3})).await;
    assert!(g.contains("SPOKE-4 in_progress by blue  Clear"), "{g}");
    // A node someone else holds directly is still refused, to the orchestrator too.
    let e = err(&mcp, "claim", json!({"id": 4, "agent": "red"})).await;
    assert!(e.starts_with("SPOKE-4 is claimed by blue until "), "{e}");
    // The holder can still claim a free step of its own project.
    mcp.ok("claim", json!({"id": 5, "agent": "red"})).await;
    mcp.ok("release", json!({"id": 5, "agent": "red"})).await;

    // A step claimed by one agent blocks another from claiming its parent.
    mcp.ok("claim", json!({"id": 7, "agent": "blue"})).await;
    let e = err(&mcp, "claim", json!({"id": 6, "agent": "red"})).await;
    assert!(
        e.starts_with("SPOKE-7 Roof under SPOKE-6 is claimed by blue until "),
        "{e}"
    );

    // Writes renew the writer's own claims only: the worker's on the step, the
    // orchestrator's on the project.
    let conn = db(&path);
    let soon = (chrono::Utc::now() + chrono::Duration::minutes(5))
        .to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
    let left = |id: &str| {
        (chrono::DateTime::parse_from_rfc3339(&claim_expires(&conn, id)).unwrap()
            - chrono::Utc::now().fixed_offset())
        .num_minutes()
    };
    conn.execute(
        "UPDATE nodes SET claim_expires = ?1 WHERE id IN (3, 4)",
        [&soon],
    )
    .unwrap();
    mcp.ok(
        "update",
        json!({"id": 4, "notes_append": "half done", "agent": "blue"}),
    )
    .await;
    assert!(left("SPOKE-4") >= 58, "worker renewed");
    assert_eq!(claim_expires(&conn, "SPOKE-3"), soon);
    conn.execute("UPDATE nodes SET claim_expires = ?1 WHERE id = 4", [&soon])
        .unwrap();
    mcp.ok(
        "update",
        json!({"id": 4, "notes_append": "checked", "agent": "red"}),
    )
    .await;
    assert!(left("SPOKE-3") >= 58, "orchestrator renewed");
    assert_eq!(claim_expires(&conn, "SPOKE-4"), soon);
    // add, move, focus and set_status renew too (here: on the parent project).
    for (tool, args) in [
        (
            "add",
            json!({"parent": 3, "title": "Varnish", "agent": "red"}),
        ),
        ("move", json!({"id": 9, "before": 5, "agent": "red"})),
        ("focus", json!({"id": 5, "agent": "red"})),
        (
            "set_status",
            json!({"ids": [4], "status": "done", "agent": "red"}),
        ),
    ] {
        conn.execute("UPDATE nodes SET claim_expires = ?1 WHERE id = 3", [&soon])
            .unwrap();
        mcp.ok(tool, args).await;
        let left = chrono::DateTime::parse_from_rfc3339(&claim_expires(&conn, "SPOKE-3")).unwrap()
            - chrono::Utc::now().fixed_offset();
        assert!(left.num_minutes() >= 58, "{tool}: {left}");
    }

    server.shutdown().await;
    let _ = std::fs::remove_dir_all(dir);
}

/// Many agents claim the same task at the same moment: exactly one wins.
#[tokio::test]
async fn concurrent_claims_have_one_winner_over_http() {
    let dir = temp_dir("claim-race");
    let path = dir.join("spokes.db");
    let server = start(&path).await;
    let mcp = Mcp::new(server.addr);
    add_node(&mcp, json!({"title": "Contested"})).await; // 2

    let addr = server.addr;
    let tasks: Vec<_> = (0..24)
        .map(|i| {
            tokio::spawn(async move {
                let mcp = Mcp::new(addr);
                let (text, is_error) = mcp
                    .call("claim", json!({"id": 2, "agent": format!("agent-{i}")}))
                    .await;
                (i, text, is_error)
            })
        })
        .collect();
    let mut winners = Vec::new();
    for t in tasks {
        let (i, text, is_error) = t.await.unwrap();
        if is_error {
            assert!(text.starts_with("SPOKE-2 is claimed by agent-"), "{text}");
        } else {
            winners.push((i, text));
        }
    }
    assert_eq!(winners.len(), 1, "{winners:?}");
    let holder: String = db(&path)
        .query_row("SELECT claimed_by FROM nodes WHERE id = 2", [], |r| {
            r.get(0)
        })
        .unwrap();
    assert_eq!(holder, format!("agent-{}", winners[0].0));
    let claims = events(&db(&path), "SPOKE-2")
        .iter()
        .filter(|e| e.0 == "claimed")
        .count();
    assert_eq!(claims, 1);

    server.shutdown().await;
    let _ = std::fs::remove_dir_all(dir);
}

#[tokio::test]
async fn ask_answer_and_stalled_leases_over_http() {
    let dir = temp_dir("ask");
    let path = dir.join("spokes.db");
    let server = start(&path).await;
    let mcp = Mcp::new(server.addr);

    // 2 Hobbies area: 3 Set up emulator (p2), 4 Workbench (p1), 5 Paint shed (p3).
    add_node(
        &mcp,
        json!({"title": "Hobbies", "kind": "area", "children": [
            {"title": "Set up emulator", "priority": 2},
            {"title": "Workbench", "priority": 1},
            {"title": "Paint shed", "priority": 3}]}),
    )
    .await;

    // ask -> top of NEEDS YOU with the question, agent and age.
    mcp.ok("claim", json!({"id": 3, "agent": "sonnet-2"})).await;
    let e = err(
        &mcp,
        "ask",
        json!({"id": 3, "agent": "opus-red", "question": "Which drive?"}),
    )
    .await;
    assert!(
        e.starts_with("SPOKE-3 is claimed by sonnet-2 until "),
        "{e}"
    );
    let e = err(
        &mcp,
        "ask",
        json!({"id": 3, "agent": "sonnet-2", "question": "  "}),
    )
    .await;
    assert!(e.starts_with("question is empty"), "{e}");
    let out = mcp
        .ok(
            "ask",
            json!({"id": 3, "agent": "sonnet-2", "question": "Install to D: or E:?"}),
        )
        .await;
    assert_eq!(
        out,
        "SPOKE-3 Set up emulator is waiting on the user: \"Install to D: or E:?\". It is at the top of next under NEEDS YOU. Stop work on it until it is answered."
    );
    let conn = db(&path);
    conn.execute(
        "UPDATE events SET at = ?1 WHERE node_id = 3 AND type = 'asked'",
        [ts_ago(185)],
    )
    .unwrap();
    let q = queue(mcp.ok("next", json!({})).await);
    assert_eq!(
        q,
        "NEEDS YOU\n\
         \x20 SPOKE-3  waiting  Set up emulator: \"Install to D: or E:?\" (sonnet-2, 3h ago)\n\
         UP NEXT\n\
         \x20 SPOKE-4  p1  Workbench  unplanned\n\
         \x20 SPOKE-5  p3  Paint shed  unplanned"
    );
    let e = err(&mcp, "claim", json!({"id": 3, "agent": "opus-red"})).await;
    assert_eq!(
        e,
        "SPOKE-3 Set up emulator is waiting on the user: \"Install to D: or E:?\". Ask them, record the reply with answer, then claim it."
    );
    let shown = mcp.ok("get", json!({"id": 3})).await;
    assert!(
        shown.contains("\nwaiting on the user: \"Install to D: or E:?\" (asked by sonnet-2)"),
        "{shown}"
    );

    // answer -> Q&A in the notes, open again, kept under NEEDS YOU as answered.
    let e = err(&mcp, "answer", json!({"id": 4, "text": "E:"})).await;
    assert!(e.contains("is not waiting on a question (open)"), "{e}");
    let out = mcp
        .ok("answer", json!({"id": 3, "text": "E:, it has more space."}))
        .await;
    assert!(
        out.starts_with("Answered SPOKE-3 Set up emulator; it is open again"),
        "{out}"
    );
    let notes = mcp.ok("get", json!({"id": 3, "notes": true})).await;
    assert!(notes.contains("Question from sonnet-2 ("), "{notes}");
    assert!(
        notes.contains("): Install to D: or E:?\nAnswer from the user ("),
        "{notes}"
    );
    assert!(notes.contains("): E:, it has more space."), "{notes}");
    assert!(notes.contains("\nanswered "), "{notes}");
    let q = queue(mcp.ok("next", json!({})).await);
    assert_eq!(
        q,
        "NEEDS YOU\n\
         \x20 SPOKE-3  answered  Set up emulator  (answered just now; claim it and read the notes)\n\
         UP NEXT\n\
         \x20 SPOKE-4  p1  Workbench  unplanned\n\
         \x20 SPOKE-5  p3  Paint shed  unplanned"
    );
    // The answer is searchable (notes are indexed).
    assert!(search(&mcp, "more space").await.contains("SPOKE-3"));
    // Claiming it clears the answered mark and points at the notes.
    let out = mcp.ok("claim", json!({"id": 3, "agent": "opus-red"})).await;
    assert!(
        out.ends_with("The user answered a question on it: read the notes (get notes: true)."),
        "{out}"
    );
    assert_eq!(
        listed_ids(&queue(mcp.ok("next", json!({})).await)),
        ["SPOKE-4", "SPOKE-5"]
    );

    // Lease expiry: stalled under NEEDS YOU (after questions), then a takeover.
    conn.execute(
        "UPDATE nodes SET claim_expires = ?1 WHERE id = 3",
        [ts_ago(125)],
    )
    .unwrap();
    mcp.ok("claim", json!({"id": 5, "agent": "sonnet-2"})).await;
    mcp.ok(
        "ask",
        json!({"id": 5, "agent": "sonnet-2", "question": "Which colour?"}),
    )
    .await;
    let q = queue(mcp.ok("next", json!({})).await);
    assert_eq!(
        q,
        "NEEDS YOU\n\
         \x20 SPOKE-5  waiting  Paint shed: \"Which colour?\" (sonnet-2, just now)\n\
         \x20 SPOKE-3  stalled  Set up emulator  (claim by opus-red expired 2h ago)\n\
         UP NEXT\n\
         \x20 SPOKE-4  p1  Workbench  unplanned"
    );
    let shown = mcp.ok("get", json!({"id": 3})).await;
    assert!(
        shown.contains("\nstalled: claim by opus-red expired "),
        "{shown}"
    );
    let out = mcp
        .ok("claim", json!({"id": 3, "agent": "sonnet-blue"}))
        .await;
    assert!(
        out.contains("(took over from opus-red, whose lease expired ")
            && out.contains("Read its notes (get notes: true) for where opus-red got to."),
        "{out}"
    );
    assert!(!queue(mcp.ok("next", json!({})).await).contains("SPOKE-3"));

    // The history on get: newest first, with who did what.
    let shown = mcp.ok("get", json!({"id": 3, "history": true})).await;
    let hist = &shown[pos(&shown, "HISTORY (newest first)\n")..];
    let lines: Vec<&str> = hist.lines().skip(1).collect();
    assert!(
        lines[0].contains("claimed by sonnet-blue: took over from opus-red, whose lease expired"),
        "{hist}"
    );
    assert!(lines[1].contains("claimed by opus-red: until "), "{hist}");
    assert!(
        lines[2].contains("answered: E:, it has more space."),
        "{hist}"
    );
    assert!(
        lines[3].contains("asked by sonnet-2: Install to D: or E:?"),
        "{hist}"
    );
    assert!(lines[4].contains("claimed by sonnet-2: until "), "{hist}");
    assert!(
        lines[5].contains("created by it: Set up emulator")
            || lines[5].contains("created: Set up emulator"),
        "{hist}"
    );
    let kinds: Vec<String> = events(&conn, "SPOKE-3").into_iter().map(|e| e.0).collect();
    assert_eq!(
        kinds,
        [
            "created", "claimed", "asked", "answered", "claimed", "claimed"
        ]
    );

    // The asking agent can withdraw its question with release; others must answer it.
    let e = err(&mcp, "release", json!({"id": 5, "agent": "opus-red"})).await;
    assert!(
        e.contains("Record the user's reply with answer instead."),
        "{e}"
    );
    mcp.ok(
        "release",
        json!({"id": 5, "agent": "sonnet-2", "note": "Will pick a colour later."}),
    )
    .await;
    assert!(!queue(mcp.ok("next", json!({})).await).contains("waiting"));

    server.shutdown().await;
    let _ = std::fs::remove_dir_all(dir);
}

/// Retro from the M2 usage test: a container whose next step is already shown higher up
/// (by a sub-project with a due date, or the step itself) is not listed again.
#[tokio::test]
async fn next_lists_each_actionable_step_once_over_http() {
    let dir = temp_dir("dedupe");
    let server = start(&dir.join("spokes.db")).await;
    let mcp = Mcp::new(server.addr);
    let today = chrono::Utc::now().date_naive();
    let soon = iso(today + chrono::Duration::days(3));
    let later = iso(today + chrono::Duration::days(5));

    // Hobbies > Skyforge (project) > Deploy Skyforge (ordered, due soon) > 6 steps, the
    // first done. Work > Server (project) > Migrate (due soon). Plus a plain task.
    add_node(
        &mcp,
        json!({"title": "Hobbies", "kind": "area", "children": [
            {"title": "Skyforge", "children": [
                {"title": "Deploy Skyforge", "ordered": true, "due": soon, "children": [
                    {"title": "Map the pipeline"}, {"title": "Write the pipeline"},
                    {"title": "Add secrets"}, {"title": "Push"}, {"title": "Verify"}, {"title": "Retro"}]}]}]}),
    )
    .await; // 2 > 3 > 4 > 5..10
    mcp.ok("set_status", json!({"ids": [5], "status": "done"}))
        .await;
    add_node(
        &mcp,
        json!({"title": "Work", "kind": "area", "children": [
            {"title": "Server", "children": [{"title": "Migrate", "due": later}]},
            {"title": "Expenses"}]}),
    )
    .await; // 11 > 12 > 13; 14

    let q = queue(mcp.ok("next", json!({"limit": 20})).await);
    assert_eq!(
        listed_ids(&q),
        ["SPOKE-4", "SPOKE-13", "SPOKE-14"],
        "Skyforge (SPOKE-3) and Server (SPOKE-12) point at steps already shown:\n{q}"
    );
    assert!(
        line_with(&q, "SPOKE-4 ")
            .contains("Deploy Skyforge (1/6)  next: SPOKE-6 Write the pipeline"),
        "{q}"
    );
    assert!(
        !q.contains("more open"),
        "dropped duplicates are not counted as more: {q}"
    );

    // Once the deploy step moves on to another sub-step, still one line per step.
    mcp.ok("set_status", json!({"ids": [6], "status": "done"}))
        .await;
    let q = queue(mcp.ok("next", json!({"limit": 20})).await);
    assert_eq!(listed_ids(&q), ["SPOKE-4", "SPOKE-13", "SPOKE-14"], "{q}");
    // Without the due sub-project the parent project carries the line again.
    mcp.ok("update", json!({"id": 4, "due": ""})).await;
    let q = queue(mcp.ok("next", json!({"limit": 20})).await);
    assert_eq!(listed_ids(&q), ["SPOKE-13", "SPOKE-3", "SPOKE-14"], "{q}");
    assert!(
        line_with(&q, "SPOKE-3 ").contains("next: SPOKE-7 Add secrets"),
        "{q}"
    );

    server.shutdown().await;
    let _ = std::fs::remove_dir_all(dir);
}

/// A project whose only open work is claimed by someone else is not offered at all.
#[tokio::test]
async fn project_with_all_steps_claimed_is_not_offered_over_http() {
    let dir = temp_dir("all-claimed");
    let server = start(&dir.join("spokes.db")).await;
    let mcp = Mcp::new(server.addr);
    add_node(
        &mcp,
        json!({"title": "Hobbies", "kind": "area", "children": [
            {"title": "Skyforge", "children": [{"title": "Deploy", "children": [{"title": "Push"}]}]},
            {"title": "Other"}]}),
    )
    .await; // 2 > 3 > 4 > 5; 6
    assert_eq!(
        listed_ids(&queue(mcp.ok("next", json!({})).await)),
        ["SPOKE-3", "SPOKE-6"]
    );
    mcp.ok("claim", json!({"id": 4, "agent": "opus-red"})).await;
    assert_eq!(
        listed_ids(&queue(mcp.ok("next", json!({})).await)),
        ["SPOKE-6"]
    );
    mcp.ok("release", json!({"id": 4, "agent": "opus-red"}))
        .await;
    mcp.ok("claim", json!({"id": 5, "agent": "opus-red"})).await;
    assert_eq!(
        listed_ids(&queue(mcp.ok("next", json!({})).await)),
        ["SPOKE-6"]
    );
    server.shutdown().await;
    let _ = std::fs::remove_dir_all(dir);
}

// ---------------------------------------------------------------------------
// M4: recurrence, defer_until, staleness, local times in history.
// ---------------------------------------------------------------------------

fn node_dates(conn: &rusqlite::Connection, id: &str) -> (String, Option<String>, Option<String>) {
    let n: i64 = id.trim_start_matches("SPOKE-").parse().unwrap();
    conn.query_row(
        "SELECT status, due_at, defer_until FROM nodes WHERE id = ?1",
        [n],
        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
    )
    .unwrap()
}

#[tokio::test]
async fn recurrence_over_http() {
    let dir = temp_dir("recur");
    let path = dir.join("spokes.db");
    let server = start(&path).await;
    let mcp = Mcp::new(server.addr);
    let today = chrono::Utc::now().date_naive();
    let days = |n: i64| today + chrono::Duration::days(n);

    let home = add_node(&mcp, json!({"title": "Home", "kind": "area"})).await;
    let conn = db(&path);
    add_node(&mcp, json!({"parent": home, "title": "Something else"})).await;

    // From completion: every 2 weeks, back 3 days before it is due.
    let oil = add_node(
        &mcp,
        json!({"parent": home, "title": "Check oil",
               "recur": {"every": 2, "unit": "week", "from": "completion"}}),
    )
    .await;
    let q = queue(mcp.ok("next", json!({})).await);
    assert!(line_with(&q, "Check oil").contains("repeats"), "{q}");
    mcp.ok("claim", json!({"id": oil, "agent": "red"})).await;
    let out = mcp
        .ok(
            "set_status",
            json!({"ids": [oil], "status": "done", "agent": "red"}),
        )
        .await;
    assert_eq!(
        out,
        format!(
            "{oil} done  Check oil  (repeats every 2 weeks, counted from completion: back in the queue {}, due {})",
            short(days(11), today),
            short(days(14), today)
        )
    );
    assert_eq!(
        node_dates(&conn, &oil),
        ("open".into(), Some(iso(days(14))), Some(iso(days(11))))
    );
    let claimed: Option<String> = conn
        .query_row(
            "SELECT claimed_by FROM nodes WHERE id = ?1",
            [oil.trim_start_matches("SPOKE-")],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(claimed, None, "claim cleared");
    let q = queue(mcp.ok("next", json!({})).await);
    assert!(!q.contains("Check oil"), "deferred until relevant: {q}");
    let g = mcp.ok("get", json!({"id": oil, "history": true})).await;
    assert!(
        g.starts_with(&format!(
            "{oil} task due {} deferred until {} repeats  Check oil",
            short(days(14), today),
            short(days(11), today)
        )),
        "{g}"
    );
    assert!(
        g.contains("\nrepeats every 2 weeks, counted from completion"),
        "{g}"
    );
    assert!(
        g.contains(" recurred by red: in_progress -> done -> open; next due "),
        "{g}"
    );
    assert!(events(&conn, &oil).iter().any(|e| e.0 == "recurred"));

    // From schedule, month ends clamp and the day-of-month anchor is kept.
    let rent = add_node(
        &mcp,
        json!({"parent": home, "title": "Pay rent", "due": "2027-01-31",
               "recur": "every month from schedule"}),
    )
    .await;
    mcp.ok("set_status", json!({"ids": [rent], "status": "done"}))
        .await;
    assert_eq!(
        node_dates(&conn, &rent),
        (
            "open".into(),
            Some("2027-02-28".into()),
            Some("2027-02-21".into())
        )
    );
    let out = mcp
        .ok("set_status", json!({"ids": [rent], "status": "done"}))
        .await;
    assert!(out.contains("counted from the due date"), "{out}");
    assert_eq!(
        node_dates(&conn, &rent),
        (
            "open".into(),
            Some("2027-03-31".into()),
            Some("2027-03-24".into())
        )
    );

    // A late completion of a schedule rule skips the periods gone by.
    let bin = add_node(
        &mcp,
        json!({"parent": home, "title": "Bins", "due": iso(days(-10)),
               "recur": {"every": 1, "unit": "week", "from": "schedule"}}),
    )
    .await;
    mcp.ok("set_status", json!({"ids": [bin], "status": "done"}))
        .await;
    let (_, due, _) = node_dates(&conn, &bin);
    let due = chrono::NaiveDate::parse_from_str(&due.unwrap(), "%Y-%m-%d").unwrap();
    assert!(due > today && due <= days(7), "{due}");

    // A repeating checklist: its finished steps reopen with it.
    let review = add_node(
        &mcp,
        json!({"parent": home, "title": "Weekly review", "recur": "weekly",
               "children": [{"title": "Inbox zero"}, {"title": "Plan week"}]}),
    )
    .await;
    let step = mcp.ok("get", json!({"id": review})).await;
    let inbox = first_id(line_with(&step, "Inbox zero"));
    mcp.ok("set_status", json!({"ids": [inbox], "status": "done"}))
        .await;
    let out = mcp
        .ok("set_status", json!({"ids": [review], "status": "done"}))
        .await;
    assert!(out.ends_with("Its 1 finished steps were reopened"), "{out}");
    assert_eq!(node_dates(&conn, &inbox).0, "open");

    // Dropping ends it.
    let out = mcp
        .ok("set_status", json!({"ids": [review], "status": "dropped"}))
        .await;
    assert_eq!(
        out,
        format!("{review} dropped  Weekly review  (it stops repeating)")
    );

    // Rules: areas never repeat; steps of an ordered parent do not repeat on their own.
    let e = err(
        &mcp,
        "add",
        json!({"title": "Garden", "kind": "area", "recur": "weekly"}),
    )
    .await;
    assert!(
        e.contains("areas never finish, so they cannot repeat"),
        "{e}"
    );
    let e = err(
        &mcp,
        "add",
        json!({"title": "Oil", "recur": "every fortnight"}),
    )
    .await;
    assert!(e.contains("is not day, week, month or year"), "{e}");
    let e = err(
        &mcp,
        "add",
        json!({"title": "Plan", "ordered": true, "children": [{"title": "S1", "recur": "daily"}]}),
    )
    .await;
    assert!(
        e.starts_with("children[0] (\"S1\"): its parent Plan has ordered steps"),
        "{e}"
    );
    let plan = add_node(
        &mcp,
        json!({"title": "Pipeline", "ordered": true, "children": [{"title": "A"}]}),
    )
    .await;
    let e = err(
        &mcp,
        "add",
        json!({"parent": plan, "title": "B", "recur": "daily"}),
    )
    .await;
    assert!(e.contains("Put recur on Pipeline instead"), "{e}");
    let bag = add_node(
        &mcp,
        json!({"title": "Bag", "children": [{"title": "Loop", "recur": "daily"}]}),
    )
    .await;
    let e = err(&mcp, "update", json!({"id": bag, "ordered": true})).await;
    assert!(e.contains("Loop repeats"), "{e}");
    let g = mcp.ok("get", json!({"id": bag})).await;
    let looped = first_id(line_with(&g, "Loop"));
    let e = err(&mcp, "move", json!({"id": looped, "parent": plan})).await;
    assert!(e.contains("has ordered steps"), "{e}");
    let out = mcp.ok("update", json!({"id": looped, "recur": ""})).await;
    assert!(out.ends_with("(recur cleared)"), "{out}");
    mcp.ok("update", json!({"id": bag, "ordered": true})).await;
    let out = mcp
        .ok("set_status", json!({"ids": [looped], "status": "done"}))
        .await;
    assert!(
        out.starts_with(&format!("{looped} done  Loop")) && !out.contains("repeats"),
        "{out}"
    );

    server.shutdown().await;
    let _ = std::fs::remove_dir_all(dir);
}

#[tokio::test]
async fn defer_until_over_http() {
    let dir = temp_dir("defer");
    let path = dir.join("spokes.db");
    let server = start(&path).await;
    let mcp = Mcp::new(server.addr);
    let today = chrono::Utc::now().date_naive();
    let later = iso(today + chrono::Duration::days(3));

    let home = add_node(&mcp, json!({"title": "Home", "kind": "area"})).await;
    // An ordered project whose first step is deferred offers nothing yet.
    let proj = add_node(
        &mcp,
        json!({"parent": home, "title": "Paint", "ordered": true,
               "children": [{"title": "Buy paint", "defer_until": later}, {"title": "Paint wall"}]}),
    )
    .await;
    let solo = add_node(
        &mcp,
        json!({"parent": home, "title": "Later thing", "defer_until": later}),
    )
    .await;
    add_node(&mcp, json!({"parent": home, "title": "Now thing"})).await;
    let q = queue(mcp.ok("next", json!({})).await);
    assert!(
        !q.contains("Paint") && !q.contains("Later thing") && q.contains("Now thing"),
        "{q}"
    );
    let g = mcp.ok("get", json!({"id": proj})).await;
    assert!(!g.contains("<- next"), "{g}");
    assert!(
        g.contains(&format!(
            "deferred until {}",
            short(today + chrono::Duration::days(3), today)
        )),
        "{g}"
    );
    // A question on a deferred task still reaches the user.
    mcp.ok(
        "ask",
        json!({"id": solo, "agent": "red", "question": "Which color?"}),
    )
    .await;
    let q = queue(mcp.ok("next", json!({})).await);
    assert!(line_with(&q, "Later thing").contains("waiting"), "{q}");
    // Clearing the deferral brings it back.
    mcp.ok("update", json!({"id": proj, "defer_until": ""}))
        .await;
    let g = mcp.ok("get", json!({"id": proj})).await;
    let buy = first_id(line_with(&g, "Buy paint"));
    mcp.ok("update", json!({"id": buy, "defer_until": ""}))
        .await;
    let q = queue(mcp.ok("next", json!({})).await);
    assert!(
        line_with(&q, "Paint").contains(&format!("next: {buy} Buy paint")),
        "{q}"
    );

    server.shutdown().await;
    let _ = std::fs::remove_dir_all(dir);
}

#[tokio::test]
async fn staleness_over_http() {
    let dir = temp_dir("stale");
    let path = dir.join("spokes.db");
    let server = start(&path).await;
    let mcp = Mcp::new(server.addr);
    let today = chrono::Utc::now().date_naive();

    let home = add_node(&mcp, json!({"title": "Home", "kind": "area"})).await;
    let conn = db(&path);
    let mut ids = Vec::new();
    for t in [
        "Fresh",
        "Aged 35",
        "Aged 50",
        "Aged 60",
        "Aged 41",
        "Project 45",
    ] {
        ids.push(add_node(&mcp, json!({"parent": home, "title": t})).await);
    }
    let deferred = add_node(
        &mcp,
        json!({"parent": home, "title": "Deferred 90",
               "defer_until": iso(today + chrono::Duration::days(5))}),
    )
    .await;
    let age = |id: &str, d: i64| {
        let n: i64 = id.trim_start_matches("SPOKE-").parse().unwrap();
        conn.execute(
            "UPDATE nodes SET touched_at = ?1 WHERE id = ?2",
            rusqlite::params![ts_ago(d * 24 * 60), n],
        )
        .unwrap();
    };
    // The area is old too, but areas are never stale.
    age(&home, 400);
    for (id, d) in ids.iter().zip([0, 35, 50, 60, 41, 45]) {
        age(id, d);
    }
    age(&deferred, 90);
    let q = queue(mcp.ok("next", json!({"limit": 1})).await);
    // UP NEXT shows Fresh (oldest created first); STALE the three oldest of the rest.
    assert!(q.contains("UP NEXT\n"), "{q}");
    let stale = &q[pos(&q, "STALE\n")..];
    let lines: Vec<&str> = stale
        .lines()
        .skip(1)
        .take_while(|l| l.starts_with("  "))
        .collect();
    assert_eq!(lines.len(), 3, "{q}");
    assert!(
        lines[0].contains("untouched 60d") && lines[0].ends_with("Aged 60"),
        "{q}"
    );
    assert!(
        lines[1].contains("untouched 50d") && lines[1].ends_with("Aged 50"),
        "{q}"
    );
    assert!(
        lines[2].contains("untouched 45d") && lines[2].ends_with("Project 45"),
        "{q}"
    );
    assert!(!q.contains("Deferred 90"), "{q}");

    // Activity beneath a project touches it; deferring or dropping stops the nudge.
    add_node(&mcp, json!({"parent": ids[5], "title": "First step"})).await;
    mcp.ok(
        "update",
        json!({"id": ids[3], "defer_until": iso(today + chrono::Duration::days(30))}),
    )
    .await;
    mcp.ok("set_status", json!({"ids": [ids[2]], "status": "dropped"}))
        .await;
    let q = queue(mcp.ok("next", json!({"limit": 1})).await);
    let stale = &q[pos(&q, "STALE\n")..];
    let lines: Vec<&str> = stale
        .lines()
        .skip(1)
        .take_while(|l| l.starts_with("  "))
        .collect();
    assert_eq!(lines.len(), 2, "{q}");
    assert!(
        lines[0].contains("untouched 41d") && lines[0].ends_with("Aged 41"),
        "{q}"
    );
    assert!(
        lines[1].contains("untouched 35d") && lines[1].ends_with("Aged 35"),
        "{q}"
    );

    server.shutdown().await;
    let _ = std::fs::remove_dir_all(dir);
}

#[tokio::test]
async fn orchestrator_workers_and_local_history_over_http() {
    let dir = temp_dir("orchestrator");
    let path = dir.join("spokes.db");
    let tz: chrono_tz::Tz = "America/Chicago".parse().unwrap();
    let server = start_tz(&path, tz).await;
    let mcp = Mcp::new(server.addr);

    let proj = add_node(
        &mcp,
        json!({"title": "Deploy", "children": [{"title": "Secrets"}, {"title": "Docs"}]}),
    )
    .await;
    mcp.ok("claim", json!({"id": proj, "agent": "opus"})).await;
    let g = mcp.ok("get", json!({"id": proj})).await;
    let secrets = first_id(line_with(&g, "Secrets"));
    let docs = first_id(line_with(&g, "Docs"));
    mcp.ok("claim", json!({"id": secrets, "agent": "sonnet-1"}))
        .await;
    mcp.ok("claim", json!({"id": docs, "agent": "sonnet-2"}))
        .await;
    // The orchestrator cannot be claimed over by a worker, nor can a worker's step.
    let e = err(&mcp, "claim", json!({"id": proj, "agent": "sonnet-1"})).await;
    assert!(
        e.starts_with(&format!("{proj} is claimed by opus until ")),
        "{e}"
    );
    let e = err(&mcp, "claim", json!({"id": docs, "agent": "sonnet-1"})).await;
    assert!(
        e.starts_with(&format!("{docs} is claimed by sonnet-2 until ")),
        "{e}"
    );
    // A worker's question surfaces under NEEDS YOU although the project is claimed.
    mcp.ok(
        "ask",
        json!({"id": secrets, "agent": "sonnet-1", "question": "Which vault?"}),
    )
    .await;
    let q = queue(mcp.ok("next", json!({})).await);
    assert!(line_with(&q, "Secrets").contains("waiting"), "{q}");
    assert!(!q.contains("Docs"), "{q}");
    mcp.ok(
        "set_status",
        json!({"ids": [docs], "status": "done", "agent": "sonnet-2"}),
    )
    .await;

    // History details show times in SPOKES_TZ, not raw UTC.
    let h = mcp.ok("get", json!({"id": proj, "history": true})).await;
    let line = line_with(&h, "claimed by opus: until ");
    assert!(!line.contains('T') || !line.contains('Z'), "{line}");
    let until = line.rsplit("until ").next().unwrap();
    assert!(
        until.len() == 5 || until.contains(' '),
        "wall-clock time expected: {line}"
    );
    let h = mcp.ok("get", json!({"id": docs, "history": true})).await;
    assert!(!h.contains(":00Z") && !h.chars().any(|c| c == 'Z'), "{h}");

    server.shutdown().await;
    let _ = std::fs::remove_dir_all(dir);
}

/// Claiming a task with open children names them and says to claim the subtask too
/// (SPOKE-692); silent on a renewal, on a leaf, and once the agent holds a subtask.
#[tokio::test]
async fn claim_on_a_parent_hints_to_claim_the_subtask_over_http() {
    let dir = temp_dir("hint-parent-claim");
    let server = start(&dir.join("spokes.db")).await;
    let mcp = Mcp::new(server.addr);

    let proj = add_node(
        &mcp,
        json!({"title": "Deploy", "ordered": true,
               "children": [{"title": "Map"}, {"title": "Write"}]}),
    )
    .await;
    let g = mcp.ok("get", json!({"id": proj})).await;
    let map = first_id(line_with(&g, "Map"));
    let out = mcp
        .ok("claim", json!({"id": proj, "agent": "opus-orch"}))
        .await;
    assert!(
        out.ends_with(&format!(
            "\nNext: {proj} has 2 open subtasks (the next step is {map} Map). Working on one? Claim that one too. Orchestrating? Keep this claim; workers can claim their own subtasks inside it."
        )),
        "{out}"
    );
    let renewed = mcp
        .ok("claim", json!({"id": proj, "agent": "opus-orch"}))
        .await;
    assert!(!renewed.contains("Next: "), "{renewed}");
    let leaf = mcp
        .ok("claim", json!({"id": map, "agent": "opus-orch"}))
        .await;
    assert!(!leaf.contains("Next: "), "{leaf}");

    // An unordered parent lists its open children; holding one of them silences it.
    let bag = add_node(
        &mcp,
        json!({"title": "Chores", "children": [{"title": "Dishes"}, {"title": "Laundry"}]}),
    )
    .await;
    let g = mcp.ok("get", json!({"id": bag})).await;
    let dishes = first_id(line_with(&g, "Dishes"));
    let laundry = first_id(line_with(&g, "Laundry"));
    let out = mcp.ok("claim", json!({"id": bag, "agent": "w1"})).await;
    assert!(
        out.contains(&format!(
            "\nNext: {bag} has 2 open subtasks ({dishes} Dishes, {laundry} Laundry). "
        )),
        "{out}"
    );
    mcp.ok("release", json!({"id": bag, "agent": "w1"})).await;
    mcp.ok("claim", json!({"id": dishes, "agent": "w1"})).await;
    let out = mcp.ok("claim", json!({"id": bag, "agent": "w1"})).await;
    assert!(!out.contains("Next: "), "{out}");
    let _ = std::fs::remove_dir_all(dir);
}

/// M4 retro: a worker finishing a step inside an ordered project that the orchestrator
/// holds claimed gets the same "Next: ..., the next step of ..." hint as in an unclaimed project.
#[tokio::test]
async fn next_step_hint_inside_a_claimed_project_over_http() {
    let dir = temp_dir("hint-claimed");
    let server = start(&dir.join("spokes.db")).await;
    let mcp = Mcp::new(server.addr);

    let proj = add_node(
        &mcp,
        json!({"title": "Deploy", "ordered": true,
               "children": [{"title": "Map"}, {"title": "Write"}, {"title": "Secrets"}]}),
    )
    .await;
    let g = mcp.ok("get", json!({"id": proj})).await;
    let map = first_id(line_with(&g, "Map"));
    let write = first_id(line_with(&g, "Write"));
    let secrets = first_id(line_with(&g, "Secrets"));
    mcp.ok("claim", json!({"id": proj, "agent": "opus-orch"}))
        .await;

    // A worker claims its step, notes progress and finishes it.
    for (step, next, worker) in [(&map, &write, "sonnet-w1"), (&write, &secrets, "sonnet-w2")] {
        mcp.ok("claim", json!({"id": step, "agent": worker})).await;
        mcp.ok(
            "update",
            json!({"id": step, "notes_append": "Done.", "agent": worker}),
        )
        .await;
        let out = mcp
            .ok(
                "set_status",
                json!({"ids": [step], "status": "done", "agent": worker}),
            )
            .await;
        assert!(
            out.ends_with(&format!(
                "\nNext: {next} {}, the next step of {proj} Deploy. Claim it before starting.",
                if next == &write { "Write" } else { "Secrets" }
            )),
            "{out}"
        );
    }
    // The orchestrator noting on a step under its own claim gets no claim nudge (SPOKE-1006).
    let noted = mcp
        .ok(
            "update",
            json!({"id": secrets, "notes_append": "Plan: rotate keys.", "agent": "opus-orch"}),
        )
        .await;
    assert!(!noted.contains("Next: "), "{noted}");
    // The orchestrator finishing a step itself gets the closing hint too.
    let out = mcp
        .ok(
            "set_status",
            json!({"ids": [secrets], "status": "done", "agent": "opus-orch"}),
        )
        .await;
    assert!(
        out.ends_with(&format!("Next: All steps of {proj} Deploy are done. If it is finished too, mark it done with set_status.")),
        "{out}"
    );

    server.shutdown().await;
    let _ = std::fs::remove_dir_all(dir);
}

/// SPOKE-181: the user's raw words live apart from notes, are kept as typed, and only grow.
#[tokio::test]
async fn source_field_over_http() {
    let dir = temp_dir("source");
    let path = dir.join("spokes.db");
    let server = start(&path).await;
    let m = Mcp::new(server.addr);

    // Nested add: the parent and a child each carry their own source.
    let out = m
        .ok(
            "add",
            json!({"title": "Idea", "source": "  can you make a zebra thing?  ", "notes": "my plan",
                   "children": [{"title": "Step", "source": "and also stripes"}]}),
        )
        .await;
    let idea = first_id(&out);
    let step = format!("SPOKE-{}", idea[6..].parse::<i64>().unwrap() + 1);

    // Listings only flag it; `get` shows it with notes:true, apart from the notes and untouched.
    let plain = m.ok("get", json!({"id": idea})).await;
    assert!(plain.contains("+notes +source"), "{plain}");
    assert!(!plain.contains("zebra"), "{plain}");
    let full = m.ok("get", json!({"id": idea, "notes": true})).await;
    assert!(
        full.contains("SOURCE (the user's words)\ncan you make a zebra thing?\nNOTES\nmy plan"),
        "{full}"
    );
    let kid = m.ok("get", json!({"id": step, "notes": true})).await;
    assert!(
        kid.contains("SOURCE (the user's words)\nand also stripes"),
        "{kid}"
    );

    // A node without a source shows no SOURCE block and no flag.
    let bare = add_node(&m, json!({"title": "Bare"})).await;
    let bare_full = m.ok("get", json!({"id": bare, "notes": true})).await;
    assert!(
        !bare_full.contains("SOURCE") && !bare_full.contains("+source"),
        "{bare_full}"
    );

    // Append only: a later message is added after a blank line; the first words stay.
    let out = m
        .ok(
            "update",
            json!({"id": idea, "source_append": "follow-up: make it blue"}),
        )
        .await;
    assert!(out.contains("source appended"), "{out}");
    let full = m.ok("get", json!({"id": idea, "notes": true})).await;
    assert!(
        full.contains("can you make a zebra thing?\n\nfollow-up: make it blue\nNOTES"),
        "{full}"
    );
    // A node with no source can start one by appending.
    m.ok("update", json!({"id": bare, "source_append": "late words"}))
        .await;
    let bare_full = m.ok("get", json!({"id": bare, "notes": true})).await;
    assert!(
        bare_full.contains("SOURCE (the user's words)\nlate words"),
        "{bare_full}"
    );

    // Never replaced or cleared, never empty-appended, and notes tools do not touch it.
    for args in [
        json!({"id": idea, "source": "rewritten"}),
        json!({"id": idea, "source": ""}),
    ] {
        let e = err(&m, "update", args).await;
        assert!(e.contains("source_append"), "{e}");
    }
    let e = err(&m, "update", json!({"id": idea, "source_append": "   "})).await;
    assert!(e.contains("empty"), "{e}");
    m.ok("update", json!({"id": idea, "notes": ""})).await;
    let full = m.ok("get", json!({"id": idea, "notes": true})).await;
    assert!(
        full.contains("zebra") && full.contains("follow-up"),
        "{full}"
    );

    // Too long is refused on add and on append, naming the child that failed.
    let huge = "x".repeat(100_001);
    let e = err(&m, "add", json!({"title": "Big", "source": huge})).await;
    assert!(e.contains("source is longer"), "{e}");
    let e = err(
        &m,
        "add",
        json!({"title": "P", "children": [{"title": "Kid", "source": "x".repeat(100_001)}]}),
    )
    .await;
    assert!(
        e.contains("children[0]") && e.contains("source is longer"),
        "{e}"
    );

    // Search finds a node by a phrase from its source, and shows the +source flag.
    let found = search(&m, "zebra").await;
    assert!(line_with(&found, &idea).contains("+source"), "{found}");

    // The read-only API: has_source on tree nodes, the words on node detail.
    let (_, body) = m.get_path(&format!("/api/nodes/{idea}")).await;
    let v: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(v["node"]["has_source"], true);
    assert_eq!(
        v["node"]["source"],
        "can you make a zebra thing?\n\nfollow-up: make it blue"
    );
    let (_, body) = m.get_path("/api/tree?depth=3").await;
    let tree: Value = serde_json::from_str(&body).unwrap();
    let flag = |title: &str| {
        tree["nodes"]
            .as_array()
            .unwrap()
            .iter()
            .find(|n| n["title"] == title)
            .map(|n| n["has_source"].clone())
    };
    assert_eq!(flag("Idea"), Some(json!(true)));
    assert_eq!(flag("Step"), Some(json!(true)));
    let (_, body) = m.get_path(&format!("/api/nodes/{bare}")).await;
    let v: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(v["node"]["source"], "late words");

    // The words survive a restart.
    server.shutdown().await;
    let server = start(&path).await;
    let m = Mcp::new(server.addr);
    let full = m.ok("get", json!({"id": idea, "notes": true})).await;
    assert!(
        full.contains("zebra") && full.contains("follow-up"),
        "{full}"
    );
    server.shutdown().await;
    let _ = std::fs::remove_dir_all(&dir);
}

/// The old `stage` argument is refused everywhere, so a caller never thinks it was saved.
#[tokio::test]
async fn old_stage_argument_is_refused_and_writes_nothing() {
    let dir = temp_dir("stage-refused");
    let server = start(&dir.join("spokes.db")).await;
    let m = Mcp::new(server.addr);

    let kept = first_id(&m.ok("add", json!({"title": "Kept"})).await);
    for (tool, args) in [
        ("add", json!({"title": "Zebra", "stage": "ready"})),
        (
            "add",
            json!({"title": "Zebra", "children": [{"title": "Stripe", "stage": "ready"}]}),
        ),
        ("update", json!({"id": kept, "title": "Zebra", "stage": ""})),
        ("search", json!({"query": "kept", "stage": "ready"})),
        ("next", json!({"stage": "ready"})),
    ] {
        let e = err(&m, tool, args).await;
        assert!(e.contains("renamed to refinement"), "{tool}: {e}");
    }
    let found = m.ok("search", json!({"query": "zebra stripe kept"})).await;
    assert!(found.contains("Kept"), "{found}");
    assert!(!found.contains("Zebra"), "{found}");
    server.shutdown().await;
}

/// A closed node shows no refinement anywhere and matches no refinement filter; the stored
/// value is kept and shows again when the node is reopened. Its type is unaffected.
#[tokio::test]
async fn closed_nodes_hide_refinement_until_reopened() {
    let dir = temp_dir("closed-refinement");
    let server = start(&dir.join("spokes.db")).await;
    let m = Mcp::new(server.addr);

    let done = first_id(
        &m.ok(
            "add",
            json!({"title": "Zebra one", "type": "bug", "refinement": "ready"}),
        )
        .await,
    );
    let dropped = first_id(
        &m.ok(
            "add",
            json!({"title": "Zebra two", "type": "bug", "refinement": "ready"}),
        )
        .await,
    );
    m.ok("set_status", json!({"ids": [done], "status": "done"}))
        .await;
    m.ok("set_status", json!({"ids": [dropped], "status": "dropped"}))
        .await;

    for id in [&done, &dropped] {
        let got = m.ok("get", json!({"id": id})).await;
        assert!(got.contains(" bug"), "{got}");
        assert!(!got.contains("ready"), "{got}");
    }
    let all = m
        .ok(
            "search",
            json!({"query": "", "type": "bug", "include_done": true}),
        )
        .await;
    assert!(
        all.contains("Zebra one") && all.contains("Zebra two"),
        "{all}"
    );
    assert!(!all.contains("ready"), "{all}");
    let filtered = m
        .ok(
            "search",
            json!({"query": "", "refinement": "ready", "include_done": true}),
        )
        .await;
    assert!(!filtered.contains("Zebra"), "{filtered}");

    m.ok("set_status", json!({"ids": [done], "status": "open"}))
        .await;
    let back = m.ok("get", json!({"id": done})).await;
    assert!(back.contains("bug ready"), "{back}");
    server.shutdown().await;
}

/// The tracked mark: set on add (top level and nested) and by update, shown as a flag and as
/// "Tracked since" in the user's time zone, found by search, cleared by update, and logged.
/// It changes nothing about status or priority.
#[tokio::test]
async fn tracked_mark_is_set_shown_found_and_cleared() {
    let dir = temp_dir("tracked");
    let path = dir.join("spokes.db");
    let server = start_tz(&path, chrono_tz::Pacific::Auckland).await;
    let m = Mcp::new(server.addr);

    let text = m
        .ok(
            "add",
            json!({"title": "Zebra epic", "tracked": true, "children": [
                {"title": "Zebra kid", "tracked": true}, {"title": "Zebra plain"}]}),
        )
        .await;
    let epic = first_id(&text);
    let find = |q: &str| json!({"query": q, "tracked": true});
    let found = m.ok("search", find("")).await;
    assert!(
        found.contains("Zebra epic") && found.contains("Zebra kid"),
        "{found}"
    );
    assert!(!found.contains("Zebra plain"), "{found}");
    assert!(found.contains(" tracked "), "{found}");
    let found = m.ok("search", find("zebra")).await;
    assert!(
        found.contains("Zebra epic") && !found.contains("Zebra plain"),
        "{found}"
    );

    // Another node, tracked by update; tracking twice keeps the first time.
    let other = first_id(&m.ok("add", json!({"title": "Zebra other"})).await);
    let reply = m.ok("update", json!({"id": other, "tracked": true})).await;
    assert!(reply.ends_with("(tracked)"), "{reply}");
    let n: i64 = other.trim_start_matches("SPOKE-").parse().unwrap();
    // 2026-11-27 23:30 UTC is 2026-11-28 12:30 in Auckland (UTC+13).
    db(&path)
        .execute(
            "UPDATE nodes SET tracked_at = '2026-11-27T23:30:00Z' WHERE id = ?1",
            [n],
        )
        .unwrap();
    m.ok("update", json!({"id": other, "tracked": true})).await;
    let got = m.ok("get", json!({"id": other})).await;
    assert!(got.contains("Tracked since 2026-11-28"), "{got}");
    assert!(got.lines().next().unwrap().contains(" tracked "), "{got}");
    let outline = m.ok("get", json!({"id": epic})).await;
    assert!(
        outline.contains("Zebra kid") && outline.contains("tracked"),
        "{outline}"
    );
    assert!(
        !m.ok("get", json!({"id": "SPOKE-1"}))
            .await
            .contains("Tracked since"),
    );

    // Cleared, and logged in the node's events.
    let reply = m.ok("update", json!({"id": other, "tracked": false})).await;
    assert!(reply.ends_with("(untracked)"), "{reply}");
    let found = m.ok("search", find("")).await;
    assert!(!found.contains("Zebra other"), "{found}");
    let got = m.ok("get", json!({"id": other})).await;
    assert!(
        !got.contains("Tracked since") && !got.contains(" tracked "),
        "{got}"
    );
    let ev: Vec<String> = events(&db(&path), &other)
        .into_iter()
        .filter(|e| e.0 == "updated")
        .map(|e| e.2)
        .collect();
    assert_eq!(ev, vec!["tracked", "tracked", "untracked"]);
    server.shutdown().await;
}
