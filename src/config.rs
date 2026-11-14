//! Runtime configuration, read from environment variables only.

use std::net::SocketAddr;
use std::path::PathBuf;

use anyhow::Context;

#[derive(Debug, Clone)]
pub struct Config {
    /// `SPOKES_BIND`, default `127.0.0.1:8787`.
    pub bind: SocketAddr,
    /// `SPOKES_DB_PATH`, default `data/spokes.db` (relative to the working directory).
    pub db_path: PathBuf,
    /// `SPOKES_ALLOWED_HOSTS`: comma-separated `Host` values to accept. Unset or `*`
    /// accepts any host, which LAN access by IP or name needs.
    pub allowed_hosts: Option<Vec<String>>,
    /// `SPOKES_ALLOWED_ORIGINS`: comma-separated browser origins to accept. By default
    /// any request carrying an `Origin` header is refused, which blocks browser-driven
    /// attacks (CSRF, DNS rebinding); MCP clients such as Claude Code send no Origin.
    pub allowed_origins: Vec<String>,
}

pub const DEFAULT_BIND: &str = "127.0.0.1:8787";

impl Config {
    pub fn from_env() -> anyhow::Result<Self> {
        let get = |k: &str| std::env::var(k).ok().filter(|v| !v.trim().is_empty());
        let bind = get("SPOKES_BIND").unwrap_or_else(|| DEFAULT_BIND.to_string());
        let bind = bind
            .trim()
            .parse()
            .with_context(|| format!("SPOKES_BIND '{bind}' is not an ip:port address"))?;
        let db_path =
            get("SPOKES_DB_PATH").map_or_else(|| PathBuf::from("data/spokes.db"), PathBuf::from);
        let list = |v: String| -> Vec<String> {
            v.split(',')
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_string)
                .collect()
        };
        let allowed_hosts = get("SPOKES_ALLOWED_HOSTS")
            .filter(|v| v.trim() != "*")
            .map(list);
        let allowed_origins = get("SPOKES_ALLOWED_ORIGINS").map(list).unwrap_or_default();
        Ok(Config {
            bind,
            db_path,
            allowed_hosts,
            allowed_origins,
        })
    }
}
