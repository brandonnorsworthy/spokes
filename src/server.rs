//! HTTP wiring: Streamable HTTP MCP on `/mcp` (stateless), the read-only JSON API on
//! `/api` (see `api.rs`) and a `/health` probe.

use std::sync::{Arc, Mutex};

use axum::Router;
use axum::http::StatusCode;
use axum::routing::get;
use rmcp::transport::streamable_http_server::session::never::NeverSessionManager;
use rmcp::transport::streamable_http_server::{StreamableHttpServerConfig, StreamableHttpService};

use crate::config::Config;
use crate::tools::{Db, Spokes};

/// Opens the database and builds the app router.
pub fn app(config: &Config) -> anyhow::Result<Router> {
    let conn = crate::db::open(&config.db_path)?;
    let db: Db = Arc::new(Mutex::new(conn));

    let mut mcp_config = StreamableHttpServerConfig::default()
        // Stateless: no protocol sessions, every request is self-contained.
        .with_legacy_session_mode(false)
        .with_json_response(true)
        .with_sse_keep_alive(None);
    mcp_config = match &config.allowed_hosts {
        Some(hosts) => mcp_config.with_allowed_hosts(hosts.clone()),
        None => mcp_config.disable_allowed_hosts(),
    };
    mcp_config = if config.allowed_origins.is_empty() {
        mcp_config.enforce_origin_validation()
    } else {
        mcp_config.with_allowed_origins(config.allowed_origins.clone())
    };

    let router = Spokes::router();
    let mcp_db = db.clone();
    let tz = config.tz;
    let mcp = StreamableHttpService::new(
        move || Ok(Spokes::new(mcp_db.clone(), router.clone(), tz)),
        Arc::new(NeverSessionManager::default()),
        mcp_config,
    );

    // Opened after `db::open` has created and migrated the file.
    let api = crate::api::router(config)?;

    Ok(Router::new()
        .nest_service("/mcp", mcp)
        .nest("/api", api)
        .route("/health", get(move || health(db.clone()))))
}

/// 200 "ok" when the database answers a trivial query.
async fn health(db: Db) -> (StatusCode, &'static str) {
    let ok = tokio::task::spawn_blocking(move || {
        let conn = db.lock().unwrap_or_else(|p| p.into_inner());
        conn.query_row("SELECT 1", [], |r| r.get::<_, i64>(0))
            .is_ok()
    })
    .await
    .unwrap_or(false);
    if ok {
        (StatusCode::OK, "ok")
    } else {
        (StatusCode::SERVICE_UNAVAILABLE, "database unavailable")
    }
}
