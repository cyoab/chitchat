//! Claims (leases) on files and tasks, so parallel agents don't trample each
//! other. A claim expires on its own; claims held by agents whose process has
//! exited are ignored.

use std::path::{Component, Path, PathBuf};

use anyhow::{Result, bail};
use rusqlite::{Connection, TransactionBehavior, params};

use crate::agents::{self, Agent};
use crate::db::now_ms;
use crate::format;

pub const DEFAULT_MINUTES: u32 = 30;
pub const MAX_MINUTES: u32 = 8 * 60;
const MAX_RESOURCES: usize = 50;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Lease {
    pub resource: String,
    pub holder_id: i64,
    pub holder: String,
    pub exclusive: bool,
    pub reason: Option<String>,
    pub expires_at: i64,
}

impl Lease {
    /// "file:src/db.rs held by @codex-1 for 25 more min ("refactoring")".
    pub fn describe(&self) -> String {
        let mins = ((self.expires_at - now_ms()) / 60_000).max(1);
        let shared = if self.exclusive { "" } else { " (shared)" };
        let reason = match &self.reason {
            Some(r) => format!(" (\"{}\")", format::truncate(r, 80)),
            None => String::new(),
        };
        format!(
            "{}{shared} held by @{} for {mins} more min{reason}",
            self.resource, self.holder
        )
    }
}

/// Turns "src/db.rs", "./src/", "/abs/repo/src/db.rs" or "task:auth" into a
/// canonical resource: "file:src/db.rs", "file:src/", "task:auth".
pub fn normalize(resource: &str, root: &Path) -> Result<String> {
    let resource = resource.trim();
    if let Some(task) = resource.strip_prefix("task:") {
        let task = task.trim().to_ascii_lowercase();
        if task.is_empty() || task.chars().count() > 80 {
            bail!("task names are 1-80 characters");
        }
        return Ok(format!("task:{task}"));
    }
    let raw = resource.strip_prefix("file:").unwrap_or(resource);
    if raw.is_empty() {
        bail!("empty resource");
    }
    let is_dir = raw.ends_with('/') || root.join(raw).is_dir();
    let path = Path::new(raw);
    let rel = if path.is_absolute() {
        // Symlinked prefixes (macOS /tmp -> /private/tmp) mean the path and the
        // root may be spelled differently; compare canonical forms too.
        let canonical_root = root.canonicalize().unwrap_or_else(|_| root.to_path_buf());
        let canonical_path = canonicalize_lenient(path);
        [root, canonical_root.as_path()]
            .iter()
            .flat_map(|r| [path.strip_prefix(r), canonical_path.strip_prefix(r)])
            .find_map(Result::ok)
            .ok_or_else(|| anyhow::anyhow!("{raw} is outside this project ({})", root.display()))?
            .to_path_buf()
    } else {
        path.to_path_buf()
    };
    let mut parts = Vec::new();
    for c in rel.components() {
        match c {
            Component::Normal(p) => parts.push(p.to_string_lossy().into_owned()),
            Component::CurDir => {}
            _ => bail!("{raw}: use a path inside the project, without '..'"),
        }
    }
    let mut out = format!("file:{}", parts.join("/"));
    if parts.is_empty() {
        out.push('/'); // the whole project
    } else if is_dir {
        out.push('/');
    }
    Ok(out)
}

/// Canonicalizes a path that may not exist yet (a file about to be created) by
/// canonicalizing its nearest existing ancestor.
fn canonicalize_lenient(path: &Path) -> PathBuf {
    let mut existing = path;
    let mut rest = Vec::new();
    while let Some(parent) = existing.parent() {
        if let Ok(canonical) = existing.canonicalize() {
            return rest.iter().rev().fold(canonical, |p, c| p.join(c));
        }
        rest.push(existing.file_name().unwrap_or_default().to_os_string());
        existing = parent;
    }
    path.to_path_buf()
}

/// Two resources overlap if they're equal, or one is a directory containing the other.
fn overlaps(a: &str, b: &str) -> bool {
    if a == b {
        return true;
    }
    match (a.strip_prefix("file:"), b.strip_prefix("file:")) {
        (Some(a), Some(b)) => {
            (a.ends_with('/') && (b.starts_with(a) || a == "/"))
                || (b.ends_with('/') && (a.starts_with(b) || b == "/"))
        }
        _ => false,
    }
}

/// Unexpired, unreleased leases whose holder is still online.
pub fn active(conn: &Connection, project_id: i64) -> Result<Vec<Lease>> {
    let mut stmt = conn.prepare(
        "SELECT l.resource, l.holder_id, a.handle, l.exclusive, l.reason, l.expires_at
         FROM leases l JOIN agents a ON a.id = l.holder_id
         WHERE l.project_id = ?1 AND l.released_at IS NULL AND l.expires_at > ?2
         ORDER BY l.resource",
    )?;
    let leases: Vec<Lease> = stmt
        .query_map(params![project_id, now_ms()], |r| {
            Ok(Lease {
                resource: r.get(0)?,
                holder_id: r.get(1)?,
                holder: r.get(2)?,
                exclusive: r.get(3)?,
                reason: r.get(4)?,
                expires_at: r.get(5)?,
            })
        })?
        .collect::<rusqlite::Result<_>>()?;
    let mut online = std::collections::HashMap::new();
    let mut out = Vec::new();
    for lease in leases {
        let alive = match online.get(&lease.holder_id) {
            Some(alive) => *alive,
            None => {
                let alive = agents::get(conn, lease.holder_id)?.is_online();
                online.insert(lease.holder_id, alive);
                alive
            }
        };
        if alive {
            out.push(lease);
        }
    }
    Ok(out)
}

#[derive(Debug, Default)]
pub struct ClaimOutcome {
    pub granted: Vec<String>,
    pub conflicts: Vec<Lease>,
}

#[derive(Debug, Clone, Copy)]
pub struct ClaimRequest<'a> {
    pub resources: &'a [String],
    /// Defaults to [`DEFAULT_MINUTES`], capped at [`MAX_MINUTES`].
    pub minutes: Option<u32>,
    pub exclusive: bool,
    pub reason: Option<&'a str>,
}

/// Claims all `resources` or none: if any overlaps someone else's claim (where
/// either side is exclusive), nothing is granted and the conflicts are returned.
/// Claiming something you already hold extends it.
pub fn claim(
    conn: &mut Connection,
    project_id: i64,
    me: &Agent,
    root: &Path,
    req: &ClaimRequest,
) -> Result<ClaimOutcome> {
    let ClaimRequest {
        resources,
        minutes,
        exclusive,
        reason,
    } = *req;
    if resources.is_empty() {
        bail!("nothing to claim");
    }
    if resources.len() > MAX_RESOURCES {
        bail!("claim at most {MAX_RESOURCES} resources at once (claim a directory instead)");
    }
    let wanted: Vec<String> = resources
        .iter()
        .map(|r| normalize(r, root))
        .collect::<Result<_>>()?;
    let minutes = minutes.unwrap_or(DEFAULT_MINUTES).clamp(1, MAX_MINUTES);
    let reason = reason.map(str::trim).filter(|r| !r.is_empty());

    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let held = active(&tx, project_id)?;
    let conflicts: Vec<Lease> = held
        .into_iter()
        .filter(|l| l.holder_id != me.id && (l.exclusive || exclusive))
        .filter(|l| wanted.iter().any(|w| overlaps(w, &l.resource)))
        .collect();
    if !conflicts.is_empty() {
        return Ok(ClaimOutcome {
            granted: Vec::new(),
            conflicts,
        });
    }
    let now = now_ms();
    let expires = now + i64::from(minutes) * 60_000;
    for resource in &wanted {
        tx.execute(
            "UPDATE leases SET released_at = ?4
             WHERE project_id = ?1 AND holder_id = ?2 AND resource = ?3 AND released_at IS NULL",
            params![project_id, me.id, resource, now],
        )?;
        tx.execute(
            "INSERT INTO leases (project_id, resource, holder_id, exclusive, reason, created_at, expires_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![project_id, resource, me.id, exclusive, reason, now, expires],
        )?;
    }
    tx.commit()?;
    Ok(ClaimOutcome {
        granted: wanted,
        conflicts: Vec::new(),
    })
}

/// Releases the given resources, or all of mine when `resources` is empty.
pub fn release(
    conn: &Connection,
    project_id: i64,
    me: &Agent,
    resources: &[String],
    root: &Path,
) -> Result<Vec<String>> {
    let now = now_ms();
    let mut released = Vec::new();
    if resources.is_empty() {
        let mut stmt = conn.prepare(
            "UPDATE leases SET released_at = ?3
             WHERE project_id = ?1 AND holder_id = ?2 AND released_at IS NULL AND expires_at > ?3
             RETURNING resource",
        )?;
        let rows = stmt.query_map(params![project_id, me.id, now], |r| r.get(0))?;
        for row in rows {
            released.push(row?);
        }
    } else {
        for resource in resources {
            let resource = normalize(resource, root)?;
            let n = conn.execute(
                "UPDATE leases SET released_at = ?4
                 WHERE project_id = ?1 AND holder_id = ?2 AND resource = ?3 AND released_at IS NULL",
                params![project_id, me.id, resource, now],
            )?;
            if n > 0 {
                released.push(resource);
            }
        }
    }
    released.sort();
    released.dedup();
    Ok(released)
}

/// Other agents' claims covering any of these (already normalized) files.
pub fn conflicts_for(
    conn: &Connection,
    project_id: i64,
    me: &Agent,
    files: &[String],
) -> Result<Vec<(String, Lease)>> {
    let mut out = Vec::new();
    for lease in active(conn, project_id)? {
        if lease.holder_id == me.id || !lease.exclusive {
            continue;
        }
        for file in files {
            if overlaps(file, &lease.resource) {
                out.push((file.clone(), lease.clone()));
            }
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agents::tests::{caller, project};

    #[test]
    fn normalizes_paths_and_tasks() {
        let root = Path::new("/repo");
        assert_eq!(normalize("src/db.rs", root).unwrap(), "file:src/db.rs");
        assert_eq!(normalize("./src/", root).unwrap(), "file:src/");
        assert_eq!(
            normalize("/repo/src/db.rs", root).unwrap(),
            "file:src/db.rs"
        );
        assert_eq!(normalize("file:src/db.rs", root).unwrap(), "file:src/db.rs");
        assert_eq!(
            normalize("task: Auth Refactor", root).unwrap(),
            "task:auth refactor"
        );
        assert!(normalize("../etc/passwd", root).is_err());
        assert!(normalize("/elsewhere/x", root).is_err());
    }

    #[test]
    fn directories_overlap_their_contents() {
        assert!(overlaps("file:src/", "file:src/db.rs"));
        assert!(overlaps("file:src/db.rs", "file:src/"));
        assert!(!overlaps("file:src/db.rs", "file:src/mcp.rs"));
        assert!(!overlaps("file:src2/x", "file:src/"));
        assert!(overlaps("file:/", "file:anything"));
        assert!(!overlaps("task:a", "file:a"));
    }

    #[test]
    fn exclusive_claims_conflict_until_released() {
        let dir = tempfile::tempdir().unwrap();
        let mut conn = crate::db::open(&dir.path().join("t.db")).unwrap();
        let pid = project(&conn);
        let me = crate::procs::info(std::process::id()).unwrap();
        let mut child = std::process::Command::new("sleep")
            .arg("60")
            .spawn()
            .unwrap();
        let other = crate::procs::info(child.id()).unwrap();
        let a = agents::resolve(&mut conn, pid, &caller("claude", Some(&me), None)).unwrap();
        let b = agents::resolve(&mut conn, pid, &caller("codex", Some(&other), None)).unwrap();
        let root = dir.path();

        let got = claim(
            &mut conn,
            pid,
            &a,
            root,
            &ClaimRequest {
                resources: &["src/".into()],
                minutes: None,
                exclusive: true,
                reason: Some("refactor"),
            },
        )
        .unwrap();
        assert_eq!(got.granted, ["file:src/"]);

        let denied = claim(
            &mut conn,
            pid,
            &b,
            root,
            &ClaimRequest {
                resources: &["src/db.rs".into(), "README.md".into()],
                minutes: None,
                exclusive: true,
                reason: None,
            },
        )
        .unwrap();
        assert!(denied.granted.is_empty());
        assert_eq!(denied.conflicts[0].holder, "claude-1");
        assert_eq!(
            conflicts_for(&conn, pid, &b, &["file:src/x.rs".into()])
                .unwrap()
                .len(),
            1
        );

        assert_eq!(release(&conn, pid, &a, &[], root).unwrap(), ["file:src/"]);
        let ok = claim(
            &mut conn,
            pid,
            &b,
            root,
            &ClaimRequest {
                resources: &["src/db.rs".into()],
                minutes: Some(5),
                exclusive: true,
                reason: None,
            },
        )
        .unwrap();
        assert_eq!(ok.granted, ["file:src/db.rs"]);

        // Claims of an agent whose process exited no longer count.
        child.kill().unwrap();
        child.wait().unwrap();
        assert!(active(&conn, pid).unwrap().is_empty());
    }
}
