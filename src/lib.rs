//! Spokes Todo MCP: one self-hosted tree of to-dos served over MCP Streamable HTTP.

pub mod config;
pub mod db;
pub mod format;
pub mod model;
pub mod server;
pub mod store;
pub mod text;
pub mod tools;

use tokio::net::TcpListener;

/// Serves the app on an already-bound listener until `shutdown` resolves.
pub async fn serve(
    listener: TcpListener,
    config: &config::Config,
    shutdown: impl Future<Output = ()> + Send + 'static,
) -> anyhow::Result<()> {
    let app = server::app(config)?;
    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown)
        .await?;
    Ok(())
}
