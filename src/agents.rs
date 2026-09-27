//! Participants: registering agents, resolving "who is calling", presence.

use anyhow::{Context, Result, bail};
use rusqlite::{Connection, OptionalExtension, Row, TransactionBehavior, params};

use crate::db::now_ms;
use crate::procs::{self, ProcInfo};
use crate::project::Project;
use crate::session::Vendor;

/// The human's handle in every project.
pub const HUMAN_HANDLE: &str = "user";

/// Agents without a known client process count as online this long after last contact.
const ONLINE_WINDOW_MS: i64 = 15 * 60 * 1000;

/// A shared Codex app-server can outlive the sessions it hosted, so a Codex agent
/// whose process is alive but that hasn't been heard from in this long is offline.
const CODEX_STALE_MS: i64 = 12 * 60 * 60 * 1000;

const MAX_STATUS_CHARS: usize = 160;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Agent {
    pub id: i64,
    pub project_id: i64,
    pub handle: String,
    pub vendor: Vendor,
    pub client_pid: Option<u32>,
    pub client_started_at: Option<i64>,
    pub session_id: Option<String>,
    pub cwd: Option<String>,
    pub status: Option<String>,
    pub created_at: i64,
    pub last_seen_at: i64,
}

impl Agent {
    /// Whether this participant is still around: is its client process running
    /// (for Codex, also: heard from recently)? Without a known process, recent
    /// activity decides. The human is always present.
    pub fn is_online(&self) -> bool {
        let quiet_for = now_ms() - self.last_seen_at;
        match (self.vendor, self.client_pid, self.client_started_at) {
            (Vendor::Human, _, _) => true,
            (Vendor::Codex, Some(pid), Some(started)) => {
                quiet_for < CODEX_STALE_MS && procs::is_alive(pid, started)
            }
            (_, Some(pid), Some(started)) => procs::is_alive(pid, started),
            _ => quiet_for < ONLINE_WINDOW_MS,
        }
    }

    pub fn is_human(&self) -> bool {
        self.vendor == Vendor::Human
    }

    pub fn at(&self) -> String {
        format!("@{}", self.handle)
    }
}

const COLUMNS: &str = "id, project_id, handle, vendor, client_pid, client_started_at, session_id, \
                       cwd, status, created_at, last_seen_at";

fn from_row(row: &Row) -> rusqlite::Result<Agent> {
    let vendor: String = row.get(3)?;
    Ok(Agent {
        id: row.get(0)?,
        project_id: row.get(1)?,
        handle: row.get(2)?,
        vendor: match vendor.as_str() {
            "claude" => Vendor::Claude,
            "codex" => Vendor::Codex,
            _ => Vendor::Human,
        },
        client_pid: row.get(4)?,
        client_started_at: row.get(5)?,
        session_id: row.get(6)?,
        cwd: row.get(7)?,
        status: row.get(8)?,
        created_at: row.get(9)?,
        last_seen_at: row.get(10)?,
    })
}

/// Inserts or refreshes the project row and returns its id.
pub fn ensure_project(conn: &Connection, project: &Project) -> Result<i64> {
    let now = now_ms();
    let root = project.root.to_string_lossy();
    conn.execute(
        "INSERT INTO projects (key, name, root, created_at, updated_at) VALUES (?1, ?2, ?3, ?4, ?4)
         ON CONFLICT (key) DO UPDATE SET root = excluded.root, updated_at = excluded.updated_at
         WHERE root IS NOT excluded.root",
        params![project.key, project.name, root, now],
    )?;
    Ok(conn.query_row(
        "SELECT id FROM projects WHERE key = ?1",
        [&project.key],
        |r| r.get(0),
    )?)
}

/// Everything known about the process calling into chitchat.
#[derive(Debug, Clone, Copy)]
pub struct Caller<'a> {
    pub vendor: Vendor,
    pub client: Option<&'a ProcInfo>,
    pub session_id: Option<&'a str>,
    /// Hooks see the client's current session id; an MCP server only knows the one
    /// it was spawned with, which goes stale after /clear.
    pub session_is_current: bool,
    pub cwd: Option<&'a str>,
}

/// Finds or registers the agent behind `caller`.
///
/// Claude Code agents are matched by client process (shared by the MCP server and
/// hooks, stable across /clear); a resumed session in a new process keeps its old
/// handle once that process is gone. Codex agents are matched by session id, since
/// one Codex process can host several sessions. Otherwise: a new agent with the
/// next free handle.
pub fn resolve(conn: &mut Connection, project_id: i64, caller: &Caller) -> Result<Agent> {
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let now = now_ms();

    let by_session = |only_if_gone: bool| -> Result<Option<Agent>> {
        let Some(session) = caller.session_id else {
            return Ok(None);
        };
        let found = tx
            .query_row(
                &format!(
                    "SELECT {COLUMNS} FROM agents
                     WHERE project_id = ?1 AND vendor = ?2 AND session_id = ?3
                     ORDER BY last_seen_at DESC LIMIT 1"
                ),
                params![project_id, caller.vendor.as_str(), session],
                from_row,
            )
            .optional()?;
        Ok(found.filter(|a| !only_if_gone || caller.client.is_none() || !a.is_online()))
    };
    let by_client = || -> Result<Option<Agent>> {
        let Some(client) = caller.client else {
            return Ok(None);
        };
        Ok(tx
            .query_row(
                &format!(
                    "SELECT {COLUMNS} FROM agents
                     WHERE project_id = ?1 AND vendor = ?2 AND client_pid = ?3
                       AND client_started_at = ?4
                     ORDER BY last_seen_at DESC LIMIT 1"
                ),
                params![
                    project_id,
                    caller.vendor.as_str(),
                    client.pid,
                    client.started_at
                ],
                from_row,
            )
            .optional()?)
    };
    let found = match caller.vendor {
        Vendor::Codex if caller.session_id.is_some() => by_session(false)?,
        _ => match by_client()? {
            Some(agent) => Some(agent),
            None => by_session(true)?,
        },
    };

    let id = if let Some(agent) = found {
        let update_session = caller.session_is_current || agent.session_id.is_none();
        tx.execute(
            "UPDATE agents SET
                 last_seen_at = ?2,
                 cwd = coalesce(?3, cwd),
                 session_id = CASE WHEN ?4 THEN coalesce(?5, session_id) ELSE session_id END,
                 client_pid = coalesce(?6, client_pid),
                 client_started_at = coalesce(?7, client_started_at)
             WHERE id = ?1",
            params![
                agent.id,
                now,
                caller.cwd,
                update_session,
                caller.session_id,
                caller.client.map(|c| c.pid),
                caller.client.map(|c| c.started_at),
            ],
        )?;
        agent.id
    } else {
        let handle = next_handle(&tx, project_id, caller.vendor)?;
        tx.execute(
            "INSERT INTO agents (project_id, handle, vendor, client_pid, client_started_at,
                                 session_id, cwd, created_at, last_seen_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?8)",
            params![
                project_id,
                handle,
                caller.vendor.as_str(),
                caller.client.map(|c| c.pid),
                caller.client.map(|c| c.started_at),
                caller.session_id,
                caller.cwd,
                now,
            ],
        )?;
        tx.last_insert_rowid()
    };
    let agent = get(&tx, id)?;
    tx.commit()?;
    Ok(agent)
}

/// The human participant of a project, created on first use.
pub fn human(conn: &Connection, project_id: i64) -> Result<Agent> {
    let now = now_ms();
    conn.execute(
        "INSERT INTO agents (project_id, handle, vendor, created_at, last_seen_at)
         VALUES (?1, ?2, 'human', ?3, ?3)
         ON CONFLICT (project_id, handle) DO UPDATE SET last_seen_at = excluded.last_seen_at",
        params![project_id, HUMAN_HANDLE, now],
    )?;
    let agent = conn.query_row(
        &format!("SELECT {COLUMNS} FROM agents WHERE project_id = ?1 AND handle = ?2"),
        params![project_id, HUMAN_HANDLE],
        from_row,
    )?;
    if !agent.is_human() {
        bail!("handle @{HUMAN_HANDLE} is taken by an agent in this project");
    }
    Ok(agent)
}

pub fn get(conn: &Connection, id: i64) -> Result<Agent> {
    conn.query_row(
        &format!("SELECT {COLUMNS} FROM agents WHERE id = ?1"),
        [id],
        from_row,
    )
    .with_context(|| format!("no agent with id {id}"))
}

/// Looks up a participant by handle, with or without the leading "@".
pub fn find(conn: &Connection, project_id: i64, handle: &str) -> Result<Option<Agent>> {
    let handle = handle.trim().trim_start_matches('@').to_ascii_lowercase();
    Ok(conn
        .query_row(
            &format!("SELECT {COLUMNS} FROM agents WHERE project_id = ?1 AND handle = ?2"),
            params![project_id, handle],
            from_row,
        )
        .optional()?)
}

/// All participants in a project, agents first, most recently seen first.
pub fn list(conn: &Connection, project_id: i64) -> Result<Vec<Agent>> {
    let mut stmt = conn.prepare(&format!(
        "SELECT {COLUMNS} FROM agents WHERE project_id = ?1
         ORDER BY vendor = 'human', last_seen_at DESC"
    ))?;
    let agents = stmt
        .query_map([project_id], from_row)?
        .collect::<rusqlite::Result<_>>()?;
    Ok(agents)
}

pub fn touch(conn: &Connection, id: i64) -> Result<()> {
    conn.execute(
        "UPDATE agents SET last_seen_at = ?2 WHERE id = ?1",
        params![id, now_ms()],
    )?;
    Ok(())
}

pub fn set_status(conn: &Connection, id: i64, status: &str) -> Result<()> {
    let status = status.trim();
    if status.lines().count() > 1 || status.chars().count() > MAX_STATUS_CHARS {
        bail!("status must be a single line of at most {MAX_STATUS_CHARS} characters");
    }
    let status = (!status.is_empty()).then_some(status);
    conn.execute(
        "UPDATE agents SET status = ?2 WHERE id = ?1",
        params![id, status],
    )?;
    Ok(())
}

pub fn rename(conn: &Connection, agent: &Agent, handle: &str) -> Result<()> {
    let handle = handle.trim().trim_start_matches('@').to_ascii_lowercase();
    if handle == agent.handle {
        return Ok(());
    }
    validate_handle(&handle)?;
    if handle == HUMAN_HANDLE {
        bail!("@{HUMAN_HANDLE} is reserved for the human");
    }
    if find(conn, agent.project_id, &handle)?.is_some() {
        bail!("@{handle} is already taken in this project");
    }
    conn.execute(
        "UPDATE agents SET handle = ?2 WHERE id = ?1",
        params![agent.id, handle],
    )?;
    Ok(())
}

fn validate_handle(handle: &str) -> Result<()> {
    let valid = (2..=24).contains(&handle.len())
        && handle.starts_with(|c: char| c.is_ascii_lowercase())
        && handle
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-');
    if !valid {
        bail!(
            "handles are 2-24 characters: lowercase letters, digits and '-', starting with a letter"
        );
    }
    Ok(())
}

/// "claude-1", "claude-2", ...: the lowest number not already used in the project.
fn next_handle(conn: &Connection, project_id: i64, vendor: Vendor) -> Result<String> {
    let prefix = format!("{}-", vendor.as_str());
    let mut stmt = conn.prepare("SELECT handle FROM agents WHERE project_id = ?1")?;
    let used: Vec<u32> = stmt
        .query_map([project_id], |r| r.get::<_, String>(0))?
        .filter_map(|h| h.ok()?.strip_prefix(&prefix)?.parse().ok())
        .collect();
    let n = (1..).find(|n| !used.contains(n)).unwrap_or(1);
    Ok(format!("{prefix}{n}"))
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    pub fn project(conn: &Connection) -> i64 {
        ensure_project(
            conn,
            &Project {
                key: "example.com/team/demo".into(),
                name: "demo".into(),
                root: "/tmp/demo".into(),
            },
        )
        .unwrap()
    }

    pub fn proc(pid: u32) -> ProcInfo {
        ProcInfo {
            pid,
            ppid: 1,
            name: "fake".into(),
            started_at: 42,
        }
    }

    pub fn caller<'a>(
        vendor: Vendor,
        client: Option<&'a ProcInfo>,
        session: Option<&'a str>,
    ) -> Caller<'a> {
        Caller {
            vendor,
            client,
            session_id: session,
            session_is_current: true,
            cwd: None,
        }
    }

    fn db() -> (tempfile::TempDir, Connection) {
        let dir = tempfile::tempdir().unwrap();
        let conn = crate::db::open(&dir.path().join("t.db")).unwrap();
        (dir, conn)
    }

    #[test]
    fn same_client_process_is_the_same_agent() {
        let (_dir, mut conn) = db();
        let pid = project(&conn);
        let client = proc(1000);

        // The MCP server (stale session id) and a hook (current id) agree.
        let from_mcp = resolve(
            &mut conn,
            pid,
            &Caller {
                session_is_current: false,
                ..caller(Vendor::Claude, Some(&client), Some("s0"))
            },
        )
        .unwrap();
        let from_hook = resolve(
            &mut conn,
            pid,
            &caller(Vendor::Claude, Some(&client), Some("s1")),
        )
        .unwrap();
        assert_eq!(from_mcp.id, from_hook.id);
        assert_eq!(from_hook.handle, "claude-1");
        assert_eq!(from_hook.session_id.as_deref(), Some("s1"));

        // A stale id from the MCP server doesn't overwrite the current one.
        let again = resolve(
            &mut conn,
            pid,
            &Caller {
                session_is_current: false,
                ..caller(Vendor::Claude, Some(&client), Some("s0"))
            },
        )
        .unwrap();
        assert_eq!(again.session_id.as_deref(), Some("s1"));
    }

    #[test]
    fn handles_are_numbered_per_vendor() {
        let (_dir, mut conn) = db();
        let pid = project(&conn);
        let (a, b, c) = (proc(1), proc(2), proc(3));
        let h = |conn: &mut Connection, v, p| {
            resolve(conn, pid, &caller(v, Some(p), None))
                .unwrap()
                .handle
        };
        assert_eq!(h(&mut conn, Vendor::Claude, &a), "claude-1");
        assert_eq!(h(&mut conn, Vendor::Claude, &b), "claude-2");
        assert_eq!(h(&mut conn, Vendor::Codex, &c), "codex-1");
    }

    #[test]
    fn resumed_session_in_a_new_process_keeps_its_handle() {
        let (_dir, mut conn) = db();
        let pid = project(&conn);
        // The first process (fake pid, never alive) registers with session s1.
        let old = resolve(
            &mut conn,
            pid,
            &caller(Vendor::Codex, Some(&proc(999_999)), Some("s1")),
        )
        .unwrap();
        let me = procs::info(std::process::id()).unwrap();
        let new = resolve(
            &mut conn,
            pid,
            &caller(Vendor::Codex, Some(&me), Some("s1")),
        )
        .unwrap();
        assert_eq!(old.id, new.id);
        assert_eq!(new.client_pid, Some(me.pid));
    }

    #[test]
    fn codex_sessions_sharing_an_app_server_are_distinct() {
        let (_dir, mut conn) = db();
        let pid = project(&conn);
        let server = proc(4242);
        let a = resolve(
            &mut conn,
            pid,
            &caller(Vendor::Codex, Some(&server), Some("thread-a")),
        )
        .unwrap();
        let b = resolve(
            &mut conn,
            pid,
            &caller(Vendor::Codex, Some(&server), Some("thread-b")),
        )
        .unwrap();
        assert_ne!(a.id, b.id);
        // The hook and the MCP server of session A agree on the session id alone.
        let a_again = resolve(
            &mut conn,
            pid,
            &caller(Vendor::Codex, None, Some("thread-a")),
        )
        .unwrap();
        assert_eq!(a.id, a_again.id);
    }

    #[test]
    fn rename_validates_and_reserves_user() {
        let (_dir, mut conn) = db();
        let pid = project(&conn);
        let a = resolve(
            &mut conn,
            pid,
            &caller(Vendor::Claude, Some(&proc(1)), None),
        )
        .unwrap();
        let b = resolve(&mut conn, pid, &caller(Vendor::Codex, Some(&proc(2)), None)).unwrap();
        rename(&conn, &a, "@api").unwrap();
        assert_eq!(get(&conn, a.id).unwrap().handle, "api");
        assert!(rename(&conn, &b, "api").is_err());
        assert!(rename(&conn, &b, "user").is_err());
        assert!(rename(&conn, &b, "Bad Name").is_err());
        assert_eq!(human(&conn, pid).unwrap().handle, "user");
    }
}
