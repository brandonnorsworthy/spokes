//! Database backups: consistent snapshots via `VACUUM INTO`, rotation, the in-process
//! timer, and restore for the CLI.

use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, bail};
use chrono::{NaiveDateTime, Utc};
use rusqlite::{Connection, OpenFlags};

use crate::config::BackupConfig;

const PREFIX: &str = "spokes-";
const SUFFIX: &str = ".db";
const STAMP: &str = "%Y%m%d-%H%M%S";

fn stamp_now() -> String {
    Utc::now().format(STAMP).to_string()
}

/// `spokes-YYYYMMDD-HHMMSS.db` for the current UTC time.
pub fn backup_file_name() -> String {
    format!("{PREFIX}{}{SUFFIX}", stamp_now())
}

fn open_read_only(path: &Path) -> anyhow::Result<Connection> {
    let conn = Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .with_context(|| format!("opening {} read-only", path.display()))?;
    conn.busy_timeout(Duration::from_secs(5))?;
    Ok(conn)
}

fn with_suffix(path: &Path, suffix: &str) -> PathBuf {
    let mut s = path.as_os_str().to_owned();
    s.push(suffix);
    PathBuf::from(s)
}

/// Writes a consistent snapshot of the live database to `dest` and returns its size.
/// Uses its own read-only connection so server tool calls are never blocked.
pub fn write_backup(db_path: &Path, dest: &Path) -> anyhow::Result<u64> {
    if dest.exists() {
        bail!(
            "backup file {} already exists; not overwriting",
            dest.display()
        );
    }
    let partial = with_suffix(dest, ".partial");
    match std::fs::remove_file(&partial) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(e).with_context(|| format!("removing {}", partial.display())),
    }
    let result = (|| -> anyhow::Result<u64> {
        let conn = open_read_only(db_path)?;
        conn.execute("VACUUM INTO ?1", [partial.to_string_lossy().as_ref()])
            .with_context(|| format!("snapshotting {}", db_path.display()))?;
        drop(conn);
        let bytes = std::fs::metadata(&partial)?.len();
        std::fs::rename(&partial, dest)
            .with_context(|| format!("moving backup into place at {}", dest.display()))?;
        Ok(bytes)
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&partial);
    }
    result
}

fn is_backup_name(name: &str) -> bool {
    name.starts_with(PREFIX) && name.ends_with(SUFFIX)
}

/// Backup file names in `dir`, oldest first (names sort chronologically).
fn list_backups(dir: &Path) -> anyhow::Result<Vec<String>> {
    let mut names = Vec::new();
    for entry in std::fs::read_dir(dir).with_context(|| format!("reading {}", dir.display()))? {
        let entry = entry?;
        if let Some(name) = entry.file_name().to_str()
            && is_backup_name(name)
            && entry.file_type()?.is_file()
        {
            names.push(name.to_string());
        }
    }
    names.sort();
    Ok(names)
}

/// Deletes the oldest `spokes-*.db` beyond `keep`, plus leftover `spokes-*.partial`
/// files. Returns how many files were removed. Other files are never touched.
pub fn rotate(dir: &Path, keep: usize) -> anyhow::Result<usize> {
    let mut removed = 0;
    let names = list_backups(dir)?;
    let excess = names.len().saturating_sub(keep);
    for name in &names[..excess] {
        std::fs::remove_file(dir.join(name))
            .with_context(|| format!("removing old backup {name}"))?;
        removed += 1;
    }
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        if let Some(name) = entry.file_name().to_str()
            && name.starts_with(PREFIX)
            && name.ends_with(".partial")
        {
            std::fs::remove_file(entry.path())?;
            removed += 1;
        }
    }
    Ok(removed)
}

/// One timer run: back up into `cfg.dir` and rotate. Returns (path, bytes, removed).
fn run_once(db_path: &Path, cfg: &BackupConfig) -> anyhow::Result<Option<(PathBuf, u64, usize)>> {
    let dest = cfg.dir.join(backup_file_name());
    if dest.exists() {
        tracing::warn!(path = %dest.display(), "backup file already exists, skipping this run");
        return Ok(None);
    }
    let bytes = write_backup(db_path, &dest)?;
    let removed = rotate(&cfg.dir, cfg.keep)?;
    Ok(Some((dest, bytes, removed)))
}

/// How long until the next backup is due, judged by the newest backup's name timestamp.
fn initial_delay(dir: &Path, interval: Duration) -> Duration {
    let Ok(names) = list_backups(dir) else {
        return Duration::ZERO;
    };
    let Some(newest) = names.last() else {
        return Duration::ZERO;
    };
    let stamp = &newest[PREFIX.len()..newest.len() - SUFFIX.len()];
    let Ok(when) = NaiveDateTime::parse_from_str(stamp, STAMP) else {
        return Duration::ZERO;
    };
    let age = Utc::now().naive_utc() - when;
    match age.to_std() {
        Ok(age) => interval.saturating_sub(age),
        Err(_) => interval, // timestamp in the future: wait a full interval
    }
}

/// The in-process backup timer: backs up now (or after the remainder of the interval if
/// the newest backup is recent), then every `interval`. Failures are logged, never fatal.
pub fn spawn(db_path: PathBuf, cfg: BackupConfig) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        if let Err(e) = std::fs::create_dir_all(&cfg.dir) {
            tracing::error!(dir = %cfg.dir.display(), "creating backup directory: {e}");
        }
        let delay = initial_delay(&cfg.dir, cfg.interval);
        if !delay.is_zero() {
            tracing::info!(
                wait_secs = delay.as_secs(),
                "recent backup exists; next one later"
            );
        }
        tokio::time::sleep(delay).await;
        loop {
            let (db, c) = (db_path.clone(), cfg.clone());
            match tokio::task::spawn_blocking(move || run_once(&db, &c)).await {
                Ok(Ok(Some((path, bytes, removed)))) => {
                    tracing::info!(path = %path.display(), bytes, removed, "backup written");
                }
                Ok(Ok(None)) => {}
                Ok(Err(e)) => tracing::error!("backup failed: {e:#}"),
                Err(e) => tracing::error!("backup task failed: {e}"),
            }
            tokio::time::sleep(cfg.interval).await;
        }
    })
}

fn validate(backup: &Path) -> anyhow::Result<()> {
    let conn = open_read_only(backup)?;
    let check: String = conn
        .query_row("PRAGMA integrity_check", [], |r| r.get(0))
        .with_context(|| format!("{} is not a readable SQLite database", backup.display()))?;
    if check != "ok" {
        bail!("integrity check failed for {}: {check}", backup.display());
    }
    let has_nodes: bool = conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = 'nodes')",
        [],
        |r| r.get(0),
    )?;
    if !has_nodes {
        bail!(
            "{} has no nodes table; it is not a spokes backup",
            backup.display()
        );
    }
    let version: i64 = conn.query_row("PRAGMA user_version", [], |r| r.get(0))?;
    if version > crate::db::schema_version() {
        bail!(
            "{} has schema version {version}, newer than this build supports ({}); upgrade spokes",
            backup.display(),
            crate::db::schema_version()
        );
    }
    Ok(())
}

/// Replaces the database at `db_path` with `backup`, after validating it. The current
/// files are moved aside, not deleted. The server must not be running.
pub fn restore(backup: &Path, db_path: &Path) -> anyhow::Result<String> {
    validate(backup)?;
    let mut msg = String::new();
    if let Some(dir) = db_path.parent()
        && !dir.as_os_str().is_empty()
    {
        std::fs::create_dir_all(dir)?;
    }
    if db_path.exists() {
        let aside = with_suffix(db_path, &format!(".before-restore-{}", stamp_now()));
        for ext in ["", "-wal", "-shm"] {
            let from = with_suffix(db_path, ext);
            if from.exists() {
                let to = with_suffix(&aside, ext);
                std::fs::rename(&from, &to)
                    .with_context(|| format!("moving {} aside", from.display()))?;
            }
        }
        msg.push_str(&format!(
            "Moved the current database aside to {}. To undo: stop spokes, then move that file (and its -wal/-shm siblings, if any) back to {}.\n",
            aside.display(),
            db_path.display()
        ));
    } else {
        // A stray WAL from a deleted DB would corrupt the restored file.
        for ext in ["-wal", "-shm"] {
            let _ = std::fs::remove_file(with_suffix(db_path, ext));
        }
    }
    let tmp = with_suffix(db_path, ".restoring");
    std::fs::copy(backup, &tmp)
        .with_context(|| format!("copying {} to {}", backup.display(), tmp.display()))?;
    std::fs::rename(&tmp, db_path)?;
    msg.push_str(&format!(
        "Restored {} to {}.",
        backup.display(),
        db_path.display()
    ));
    Ok(msg)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("spokes-bk-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn rotate_keeps_newest_and_ignores_foreign_files() {
        let dir = temp_dir("rotate");
        for n in [
            "spokes-20260101-000000.db",
            "spokes-20260102-000000.db",
            "spokes-20260103-000000.db",
            "spokes-20260104-000000.db.partial",
            "notes.txt",
            "other-20250101.db",
        ] {
            std::fs::write(dir.join(n), "x").unwrap();
        }
        assert_eq!(rotate(&dir, 2).unwrap(), 2);
        let mut left: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .collect();
        left.sort();
        assert_eq!(
            left,
            [
                "notes.txt",
                "other-20250101.db",
                "spokes-20260102-000000.db",
                "spokes-20260103-000000.db"
            ]
        );
    }

    #[test]
    fn write_backup_snapshots_and_refuses_existing_dest() {
        let dir = temp_dir("write");
        let db = dir.join("live.db");
        drop(crate::db::open(&db).unwrap());
        let dest = dir.join("b.db");
        assert!(write_backup(&db, &dest).unwrap() > 0);
        assert!(!with_suffix(&dest, ".partial").exists());
        let err = write_backup(&db, &dest).unwrap_err().to_string();
        assert!(err.contains("already exists"), "{err}");
        validate(&dest).unwrap();
    }

    #[test]
    fn restore_refuses_garbage() {
        let dir = temp_dir("garbage");
        let bad = dir.join("bad.db");
        std::fs::write(&bad, "this is not a database at all, just text padding").unwrap();
        let db = dir.join("live.db");
        assert!(restore(&bad, &db).is_err());
        assert!(!db.exists());
    }
}
