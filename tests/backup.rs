//! The in-process backup timer rotates files, and a backup restores into a working server.

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::time::Duration;

use http_body_util::{BodyExt, Full};
use hyper::Request;
use hyper::body::Bytes;
use hyper_util::client::legacy::Client;
use hyper_util::rt::TokioExecutor;
use serde_json::{Value, json};
use spokes::config::{BackupConfig, Config};
use tokio::sync::oneshot;
use tokio::task::JoinHandle;

struct Server {
    addr: SocketAddr,
    stop: oneshot::Sender<()>,
    task: JoinHandle<()>,
}

async fn start(db_path: &Path, backup: Option<BackupConfig>) -> Server {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let config = Config {
        bind: addr,
        db_path: db_path.to_path_buf(),
        allowed_hosts: None,
        allowed_origins: Vec::new(),
        tz: chrono_tz::UTC,
        backup,
        hints: true,
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

    async fn call(&self, name: &str, arguments: Value) -> String {
        let client = Client::builder(TokioExecutor::new()).build_http();
        let body = json!({"jsonrpc": "2.0", "id": 1, "method": "tools/call",
            "params": {"name": name, "arguments": arguments}});
        let req = Request::post(format!("http://{}/mcp", self.addr))
            .header("content-type", "application/json")
            .header("accept", "application/json, text/event-stream")
            .header("mcp-protocol-version", "2025-11-25")
            .body(Full::new(Bytes::from(body.to_string())))
            .unwrap();
        let res = client.request(req).await.unwrap();
        let bytes = res.into_body().collect().await.unwrap().to_bytes();
        let v: Value = serde_json::from_slice(&bytes).unwrap();
        let result = &v["result"];
        assert!(!result["isError"].as_bool().unwrap_or(false), "{v}");
        result["content"][0]["text"].as_str().unwrap().to_string()
    }
}

fn backups(dir: &Path) -> Vec<PathBuf> {
    let mut v: Vec<PathBuf> = std::fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().path())
        .filter(|p| {
            let n = p.file_name().unwrap().to_string_lossy();
            n.starts_with("spokes-") && n.ends_with(".db")
        })
        .collect();
    v.sort();
    v
}

#[tokio::test]
async fn timer_rotates_and_backup_restores() {
    let dir = std::env::temp_dir().join(format!("spokes-it-backup-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let db = dir.join("spokes.db");
    let bdir = dir.join("backups");

    let server = start(
        &db,
        Some(BackupConfig {
            dir: bdir.clone(),
            keep: 2,
            interval: Duration::from_secs(1),
        }),
    )
    .await;
    server.call("add", json!({"title": "Backed up"})).await;
    tokio::time::sleep(Duration::from_millis(3500)).await;
    let files = backups(&bdir);
    assert_eq!(files.len(), 2, "{files:?}");
    server.shutdown().await;

    for ext in ["", "-wal", "-shm"] {
        let mut p = db.clone().into_os_string();
        p.push(ext);
        let _ = std::fs::remove_file(p);
    }
    let newest = files.last().unwrap();
    spokes::backup::restore(newest, &db).unwrap();

    let server = start(&db, None).await;
    let text = server.call("get", json!({"id": 2})).await;
    assert!(text.contains("Backed up"), "{text}");
    server.shutdown().await;
    let _ = std::fs::remove_dir_all(&dir);
}
