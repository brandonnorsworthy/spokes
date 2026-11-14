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
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let config = Config {
        bind: addr,
        db_path: db_path.to_path_buf(),
        allowed_hosts: None,
        allowed_origins: Vec::new(),
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
        ["add", "get", "move", "next", "set_status", "update"]
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
    assert_eq!(
        mcp.ok("next", json!({})).await,
        "UP NEXT\n  SPOKE-5  p2  Rewire garage outlet  unplanned"
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
                Hobbies / Programming / Skyforge\n  \
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
        "SPOKE-6 done  Map the pipeline\nNext step of SPOKE-5 Deploy game to production: SPOKE-7 Write the pipeline"
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
        mcp.ok("next", json!({})).await,
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
        "Updated SPOKE-7  Write the pipeline  (notes appended)"
    );
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
        body.ends_with("NOTES\nUsed deploy.yml.\n\nAdded a cache step."),
        "{body}"
    );
    mcp.ok("update", json!({"id": "SPOKE-7", "notes": "Replaced."}))
        .await;
    let body = mcp.ok("get", json!({"id": "SPOKE-7", "notes": true})).await;
    assert!(body.ends_with("NOTES\nReplaced."), "{body}");
    mcp.ok("update", json!({"id": "SPOKE-7", "notes": ""}))
        .await;
    let body = mcp.ok("get", json!({"id": "SPOKE-7", "notes": true})).await;
    assert!(
        body.ends_with("(no notes)") && !body.contains("+notes"),
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
        done.ends_with("All steps of SPOKE-5 Deploy game to production are done. If it is finished too, mark it done with set_status."),
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
