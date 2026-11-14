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
    assert_eq!(names, ["add", "get", "next", "set_status"]);

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
