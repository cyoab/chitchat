//! SQLite storage: opening connections and applying migrations.
//!
//! Several processes (one `chitchat mcp` per agent, plus short-lived hook and CLI
//! invocations) share one database file, so every connection runs in WAL mode with
//! a busy timeout, and writers should use `BEGIN IMMEDIATE` transactions.

use std::fs::{File, OpenOptions};
use std::path::Path;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use rusqlite::{Connection, TransactionBehavior};

/// Schema migrations, applied in order. Index `i` brings the schema to version `i + 1`.
const MIGRATIONS: &[&str] = &[
    include_str!("../migrations/0001_init.sql"),
    include_str!("../migrations/0002_any_harness.sql"),
];

pub const SCHEMA_VERSION: u32 = MIGRATIONS.len() as u32;

const BUSY_TIMEOUT: Duration = Duration::from_secs(5);

/// Opens (creating if needed) the database at `path` and migrates it to the latest schema.
pub fn open(path: &Path) -> Result<Connection> {
    if let Some(dir) = path.parent() {
        create_private_dir(dir)?;
    }
    let _setup = setup_lock(path)?;
    let mut conn =
        Connection::open(path).with_context(|| format!("opening database {}", path.display()))?;
    configure(&conn)?;
    migrate(&mut conn)?;
    Ok(conn)
}

/// Switching a new database to WAL needs exclusive access, and when several
/// connections race for it SQLite fails with "database is locked" straight away
/// instead of waiting out the busy timeout. An OS lock on a sidecar file serializes
/// connection setup (WAL switch + migrations) across processes; it is released when
/// `open` returns, so ordinary reads and writes never take it.
fn setup_lock(db: &Path) -> Result<File> {
    let mut name = db.as_os_str().to_owned();
    name.push(".lock");
    let file = OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(&name)
        .with_context(|| format!("opening lock file {}", Path::new(&name).display()))?;
    file.lock().context("locking the database for setup")?;
    Ok(file)
}

/// Opens the database at the default location (see [`crate::paths`]).
pub fn open_default() -> Result<Connection> {
    open(&crate::paths::db_path()?)
}

fn configure(conn: &Connection) -> Result<()> {
    // The busy timeout must be set first: switching to WAL needs a brief lock.
    conn.busy_timeout(BUSY_TIMEOUT)?;
    let mode: String =
        conn.pragma_update_and_check(None, "journal_mode", "wal", |row| row.get(0))?;
    if !mode.eq_ignore_ascii_case("wal") {
        bail!(
            "could not enable WAL mode (journal_mode is {mode}); is the database on a network filesystem?"
        );
    }
    conn.pragma_update(None, "synchronous", "NORMAL")?;
    conn.pragma_update(None, "foreign_keys", true)?;
    Ok(())
}

/// Brings the schema up to [`SCHEMA_VERSION`].
///
/// Safe to call from several processes at once: the version is re-read after taking
/// the write lock, so a process that loses the race sees the finished migration and
/// applies nothing.
pub fn migrate(conn: &mut Connection) -> Result<()> {
    if schema_version(conn)? == SCHEMA_VERSION {
        return Ok(());
    }
    // Rebuilding a table (the only way to change a constraint in SQLite) needs
    // foreign-key enforcement off, and that can only be switched outside a
    // transaction. Integrity is checked before committing instead.
    conn.pragma_update(None, "foreign_keys", false)?;
    let result = apply_pending(conn);
    conn.pragma_update(None, "foreign_keys", true)?;
    result
}

fn apply_pending(conn: &mut Connection) -> Result<()> {
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let current = schema_version(&tx)?;
    if current > SCHEMA_VERSION {
        bail!(
            "database schema is v{current} but this chitchat only knows v{SCHEMA_VERSION}; upgrade chitchat"
        );
    }
    for (i, sql) in MIGRATIONS.iter().enumerate().skip(current as usize) {
        tx.execute_batch(sql)
            .with_context(|| format!("applying migration {}", i + 1))?;
    }
    let broken: i64 = tx.query_row("SELECT count(*) FROM pragma_foreign_key_check", [], |r| {
        r.get(0)
    })?;
    if broken > 0 {
        bail!("migration left {broken} broken foreign key references; not applied");
    }
    tx.pragma_update(None, "user_version", SCHEMA_VERSION)?;
    tx.commit()?;
    Ok(())
}

pub fn schema_version(conn: &Connection) -> Result<u32> {
    Ok(conn.pragma_query_value(None, "user_version", |row| row.get(0))?)
}

/// Current time as unix milliseconds, the unit used by every timestamp column.
pub fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as i64)
}

/// The database holds other agents' messages, so keep its directory private.
fn create_private_dir(dir: &Path) -> Result<()> {
    if dir.as_os_str().is_empty() || dir.exists() {
        return Ok(());
    }
    std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn open_creates_latest_schema_in_wal_mode() {
        let dir = tempfile::tempdir().unwrap();
        let conn = open(&dir.path().join("nested/chitchat.db")).unwrap();

        assert_eq!(schema_version(&conn).unwrap(), SCHEMA_VERSION);
        let mode: String = conn
            .pragma_query_value(None, "journal_mode", |r| r.get(0))
            .unwrap();
        assert_eq!(mode, "wal");
        let fk: bool = conn
            .pragma_query_value(None, "foreign_keys", |r| r.get(0))
            .unwrap();
        assert!(fk);
    }

    #[test]
    fn reopening_is_a_no_op() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("chitchat.db");
        drop(open(&path).unwrap());
        let conn = open(&path).unwrap();
        assert_eq!(schema_version(&conn).unwrap(), SCHEMA_VERSION);
    }

    #[test]
    fn concurrent_first_opens_all_succeed() {
        // Several rounds, because the race only shows up some of the time.
        for _ in 0..20 {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("chitchat.db");
            let barrier = std::sync::Arc::new(std::sync::Barrier::new(8));

            let handles: Vec<_> = (0..8)
                .map(|_| {
                    let (path, barrier) = (path.clone(), barrier.clone());
                    std::thread::spawn(move || {
                        barrier.wait();
                        open(&path).map(|conn| schema_version(&conn).unwrap())
                    })
                })
                .collect();

            for handle in handles {
                assert_eq!(handle.join().unwrap().unwrap(), SCHEMA_VERSION);
            }
        }
    }

    #[test]
    fn upgrading_from_v1_keeps_agents_and_their_messages() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("v1.db");
        // Build a v1 database with an agent that has sent a message.
        {
            let mut conn = Connection::open(&path).unwrap();
            conn.pragma_update(None, "foreign_keys", true).unwrap();
            let tx = conn.transaction().unwrap();
            tx.execute_batch(MIGRATIONS[0]).unwrap();
            tx.pragma_update(None, "user_version", 1).unwrap();
            tx.execute_batch(
                "INSERT INTO projects (id, key, name, created_at, updated_at) VALUES (1, 'ws:x', 'x', 0, 0);
                 INSERT INTO agents (id, project_id, handle, vendor, created_at, last_seen_at)
                     VALUES (7, 1, 'codex-1', 'codex', 0, 0);
                 INSERT INTO messages (project_id, sender_id, room, intent, body, created_at)
                     VALUES (1, 7, 'general', 'inform', 'hi', 0);",
            )
            .unwrap();
            tx.commit().unwrap();
        }

        let conn = open(&path).unwrap();
        assert_eq!(schema_version(&conn).unwrap(), SCHEMA_VERSION);
        let (handle, body): (String, String) = conn
            .query_row(
                "SELECT a.handle, m.body FROM messages m JOIN agents a ON a.id = m.sender_id",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!((handle.as_str(), body.as_str()), ("codex-1", "hi"));
        // Any harness id is now accepted, and foreign keys are enforced again.
        conn.execute(
            "INSERT INTO agents (project_id, handle, vendor, created_at, last_seen_at)
             VALUES (1, 'gemini-1', 'gemini', 0, 0)",
            [],
        )
        .unwrap();
        assert!(
            conn.execute(
                "INSERT INTO agents (project_id, handle, vendor, created_at, last_seen_at)
                 VALUES (99, 'x-1', 'x', 0, 0)",
                [],
            )
            .is_err()
        );
    }

    #[test]
    fn rejects_a_newer_schema() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("chitchat.db");
        let conn = open(&path).unwrap();
        conn.pragma_update(None, "user_version", SCHEMA_VERSION + 1)
            .unwrap();
        drop(conn);

        let err = open(&path).unwrap_err().to_string();
        assert!(err.contains("upgrade chitchat"), "{err}");
    }

    #[test]
    fn note_revisions_are_versioned_and_searchable() {
        let dir = tempfile::tempdir().unwrap();
        let conn = open(&dir.path().join("chitchat.db")).unwrap();
        let now = now_ms();
        conn.execute(
            "INSERT INTO notes (key, kind, title, body, created_at, updated_at)
             VALUES ('decision/storage', 'decision', 'Storage', 'Use SQLite in WAL mode', ?1, ?1)",
            [now],
        )
        .unwrap();
        conn.execute(
            "UPDATE notes SET body = 'Use SQLite via rusqlite', revision = revision + 1
             WHERE key = 'decision/storage'",
            [],
        )
        .unwrap();

        let versions: i64 = conn
            .query_row("SELECT count(*) FROM note_versions", [], |r| r.get(0))
            .unwrap();
        assert_eq!(versions, 2);

        let hit: String = conn
            .query_row(
                "SELECT n.body FROM notes_fts JOIN notes n ON n.id = notes_fts.rowid
                 WHERE notes_fts MATCH 'rusqlite'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(hit, "Use SQLite via rusqlite");
        let stale: i64 = conn
            .query_row(
                "SELECT count(*) FROM notes_fts WHERE notes_fts MATCH 'wal'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(stale, 0);
    }
}
