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
    /// `SPOKES_TZ`: IANA time zone name (e.g. `America/Chicago`), default `UTC`. Decides
    /// what "today" means for due dates and deferrals.
    pub tz: chrono_tz::Tz,
    /// Nightly backups of the database file; `None` when `SPOKES_BACKUP_DIR` is unset.
    pub backup: Option<BackupConfig>,
}

/// The in-process backup timer (the one scheduled job in spokes).
#[derive(Debug, Clone)]
pub struct BackupConfig {
    /// `SPOKES_BACKUP_DIR`: where backup files go (created if missing).
    pub dir: PathBuf,
    /// `SPOKES_BACKUP_KEEP`: how many backup files to keep, default 14 (at least 1).
    pub keep: usize,
    /// `SPOKES_BACKUP_INTERVAL_SECS`: seconds between backups, default one day.
    /// Test-only: shorten it to see a backup happen without waiting a day.
    pub interval: std::time::Duration,
}

pub const DEFAULT_BACKUP_KEEP: usize = 14;
pub const DEFAULT_BACKUP_INTERVAL_SECS: u64 = 24 * 60 * 60;

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
        let tz = get("SPOKES_TZ").unwrap_or_else(|| "UTC".to_string());
        let tz = tz.trim().parse().map_err(|_| {
            anyhow::anyhow!(
                "SPOKES_TZ '{tz}' is not an IANA time zone name, e.g. America/Chicago or UTC"
            )
        })?;
        let backup = match get("SPOKES_BACKUP_DIR") {
            None => None,
            Some(dir) => {
                let keep = match get("SPOKES_BACKUP_KEEP") {
                    None => DEFAULT_BACKUP_KEEP,
                    Some(k) => match k.trim().parse::<usize>() {
                        Ok(n) if n >= 1 => n,
                        _ => anyhow::bail!(
                            "SPOKES_BACKUP_KEEP '{k}' must be a whole number, 1 or more"
                        ),
                    },
                };
                let secs = match get("SPOKES_BACKUP_INTERVAL_SECS") {
                    None => DEFAULT_BACKUP_INTERVAL_SECS,
                    Some(v) => match v.trim().parse::<u64>() {
                        Ok(n) if n >= 1 => n,
                        _ => anyhow::bail!(
                            "SPOKES_BACKUP_INTERVAL_SECS '{v}' must be a whole number of seconds, 1 or more"
                        ),
                    },
                };
                Some(BackupConfig {
                    dir: PathBuf::from(dir.trim()),
                    keep,
                    interval: std::time::Duration::from_secs(secs),
                })
            }
        };
        Ok(Config {
            bind,
            db_path,
            allowed_hosts,
            allowed_origins,
            tz,
            backup,
        })
    }
}
