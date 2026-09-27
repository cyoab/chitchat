//! Backups of the chitchat database.
//!
//! A backup is a complete, compacted copy made with `VACUUM INTO`, which reads a
//! consistent snapshot even while agents keep writing. Restoring copies a backup
//! into the live database with SQLite's online backup API, so agents that have it
//! open see the restored data (instead of holding on to a replaced file). The
//! current database is always backed up first, so a restore can be undone.
//!
//! `chitchat mcp` also takes one automatic backup a day and keeps the newest
//! [`AUTO_KEEP`]; set `CHITCHAT_AUTO_BACKUP=0` to turn that off.

use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use anyhow::{Context, Result, bail};
use rusqlite::{Connection, MAIN_DB, OpenFlags};

use crate::db::{self, now_ms};
use crate::format;

pub const AUTO_KEEP: usize = 7;
const AUTO_EVERY: Duration = Duration::from_secs(24 * 60 * 60);
const AUTO_PREFIX: &str = "auto-";

pub fn dir() -> Result<PathBuf> {
    Ok(crate::paths::home()?.join("backups"))
}

#[derive(Debug, Clone)]
pub struct BackupFile {
    pub path: PathBuf,
    pub size: u64,
    pub modified: SystemTime,
}

impl BackupFile {
    pub fn is_auto(&self) -> bool {
        self.path
            .file_name()
            .is_some_and(|n| n.to_string_lossy().starts_with(AUTO_PREFIX))
    }
}

/// Writes a backup of `conn`'s database to `out`, or to a new timestamped file in
/// the backups directory (named `<prefix>chitchat-<UTC time>.db`).
pub fn create(conn: &Connection, out: Option<&Path>, prefix: &str) -> Result<PathBuf> {
    let path = match out {
        Some(p) => p.to_path_buf(),
        None => {
            let dir = dir()?;
            std::fs::create_dir_all(&dir)?;
            unique_name(&dir, prefix)
        }
    };
    if path.exists() {
        bail!("{} already exists", path.display());
    }
    if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
        std::fs::create_dir_all(parent)?;
    }
    conn.execute("VACUUM INTO ?1", [path.to_string_lossy()])
        .with_context(|| format!("writing backup {}", path.display()))?;
    Ok(path)
}

fn unique_name(dir: &Path, prefix: &str) -> PathBuf {
    let stamp = format::iso_utc(now_ms()).replace([':', '-'], "");
    let mut n = 0;
    loop {
        let suffix = if n == 0 {
            String::new()
        } else {
            format!("-{n}")
        };
        let path = dir.join(format!("{prefix}chitchat-{stamp}{suffix}.db"));
        if !path.exists() {
            return path;
        }
        n += 1;
    }
}

/// Backups in the backups directory, newest first.
pub fn list() -> Result<Vec<BackupFile>> {
    let dir = dir()?;
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return Ok(Vec::new());
    };
    let mut files: Vec<BackupFile> = entries
        .flatten()
        .filter(|e| e.path().extension().is_some_and(|x| x == "db"))
        .filter_map(|e| {
            let meta = e.metadata().ok()?;
            Some(BackupFile {
                path: e.path(),
                size: meta.len(),
                modified: meta.modified().ok()?,
            })
        })
        .collect();
    files.sort_by_key(|b| std::cmp::Reverse(b.modified));
    Ok(files)
}

/// Takes the daily automatic backup if the last one is older than a day, and
/// prunes old automatic backups. Manual backups are never pruned.
pub fn auto(conn: &Connection) -> Result<Option<PathBuf>> {
    if std::env::var("CHITCHAT_AUTO_BACKUP").is_ok_and(|v| v == "0") {
        return Ok(None);
    }
    let autos: Vec<BackupFile> = list()?.into_iter().filter(BackupFile::is_auto).collect();
    let fresh = autos
        .first()
        .and_then(|b| b.modified.elapsed().ok())
        .is_some_and(|age| age < AUTO_EVERY);
    if fresh {
        return Ok(None);
    }
    let path = create(conn, None, AUTO_PREFIX)?;
    for old in list()?.iter().filter(|b| b.is_auto()).skip(AUTO_KEEP) {
        let _ = std::fs::remove_file(&old.path);
    }
    Ok(Some(path))
}

/// Replaces the database at `db_path` with the backup at `src`, after checking the
/// backup and saving the current database. Returns where the current one was saved.
pub fn restore(db_path: &Path, src: &Path) -> Result<PathBuf> {
    restore_into(db_path, src, &dir()?)
}

fn restore_into(db_path: &Path, src: &Path, backups: &Path) -> Result<PathBuf> {
    check(src)?;
    let mut conn = db::open(db_path)?;
    std::fs::create_dir_all(backups)?;
    let saved = create(&conn, Some(&unique_name(backups, "pre-restore-")), "")?;
    conn.restore(MAIN_DB, src, None::<fn(rusqlite::backup::Progress)>)
        .with_context(|| format!("restoring {}", src.display()))?;
    db::migrate(&mut conn)?;
    Ok(saved)
}

/// Rejects files that aren't healthy chitchat databases this binary understands.
fn check(src: &Path) -> Result<()> {
    let conn = Connection::open_with_flags(src, OpenFlags::SQLITE_OPEN_READ_ONLY)
        .with_context(|| format!("opening {}", src.display()))?;
    let integrity: String = conn
        .query_row("PRAGMA integrity_check", [], |r| r.get(0))
        .with_context(|| format!("{} is not a SQLite database", src.display()))?;
    if integrity != "ok" {
        bail!("{} is damaged: {integrity}", src.display());
    }
    let version = db::schema_version(&conn)?;
    let has_projects: bool = conn.query_row(
        "SELECT count(*) > 0 FROM sqlite_master WHERE type = 'table' AND name = 'projects'",
        [],
        |r| r.get(0),
    )?;
    if version == 0 || !has_projects {
        bail!("{} is not a chitchat database", src.display());
    }
    if version > db::SCHEMA_VERSION {
        bail!(
            "{} has schema v{version}, newer than this chitchat (v{}); upgrade chitchat first",
            src.display(),
            db::SCHEMA_VERSION
        );
    }
    Ok(())
}

pub fn human_size(bytes: u64) -> String {
    match bytes {
        0..1024 => format!("{bytes} B"),
        1024..1_048_576 => format!("{:.1} KB", bytes as f64 / 1024.0),
        _ => format!("{:.1} MB", bytes as f64 / 1_048_576.0),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backup_and_restore_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("chitchat.db");
        let conn = db::open(&db_path).unwrap();
        conn.execute(
            "INSERT INTO projects (key, name, created_at, updated_at) VALUES ('ws:a', 'a', 0, 0)",
            [],
        )
        .unwrap();
        let backup = create(&conn, Some(&dir.path().join("b/one.db")), "").unwrap();
        conn.execute("DELETE FROM projects", []).unwrap();

        // An agent still holding a connection sees the restored data.
        let restored_from = dir.path().join("b/one.db");
        assert_eq!(backup, restored_from);
        let backups = dir.path().join("backups");
        let saved = restore_into(&db_path, &backup, &backups).unwrap();
        let n: i64 = conn
            .query_row("SELECT count(*) FROM projects", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n, 1);
        assert!(saved.starts_with(&backups));

        let junk = dir.path().join("junk.db");
        std::fs::write(&junk, "not a database").unwrap();
        assert!(restore_into(&db_path, &junk, &backups).is_err());
    }
}
