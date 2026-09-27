//! `chitchat init`, `deinit` and `workspaces`: turning a directory into a chitchat
//! workspace, including one that agents have already been working in.
//!
//! `init` in a directory:
//! 1. creates `.chitchat/workspace.json` (or reuses the workspace it's already in,
//!    or, in a linked git worktree, joins the main worktree's workspace);
//! 2. registers it in the database, adopting chitchat data that older versions
//!    recorded for the same repository;
//! 3. configures Claude Code and Codex for this directory and every linked
//!    worktree of the repo (see [`crate::clients`]), keeping that config out of git;
//! 4. indexes the project's Markdown docs and imports what agents already know:
//!    Claude Code's memory files for these directories become shared notes.
//!
//! Running it again is safe: it refreshes the config and re-imports changed memories.

use std::path::{Path, PathBuf};

use anyhow::{Result, bail};
use rusqlite::{Connection, OptionalExtension, params};

use crate::agents;
use crate::clients;
use crate::db::now_ms;
use crate::format;
use crate::harness::{self, Harness};
use crate::memory;
use crate::project::{self, Marker, canonical};

#[derive(Debug, Clone)]
pub struct InitOptions {
    /// Clients to configure; empty means every one that's installed.
    pub clients: Vec<&'static Harness>,
    pub import: bool,
    pub stop_hook: bool,
    pub name: Option<String>,
}

enum Found {
    Created,
    Existing,
    /// A linked worktree joining the workspace in its main worktree (at this path).
    Joined(PathBuf),
}

pub fn init(path: Option<&Path>, opts: &InitOptions) -> Result<()> {
    let start = canonical(&match path {
        Some(p) => p.to_path_buf(),
        None => std::env::current_dir()?,
    });
    if !start.is_dir() {
        bail!("{} is not a directory", start.display());
    }
    let (dir, marker, found) = locate_or_create(&start, opts.name.as_deref())?;

    let mut conn = crate::db::open_default()?;
    let (project_id, adopted) = register(&conn, &marker, &dir)?;
    let user = agents::human(&conn, project_id)?;

    match &found {
        Found::Created => println!(
            "Initialized chitchat workspace \"{}\" at {}",
            marker.name,
            dir.display()
        ),
        Found::Existing => println!(
            "Refreshing chitchat workspace \"{}\" at {}",
            marker.name,
            dir.display()
        ),
        Found::Joined(main) => println!(
            "This worktree joins workspace \"{}\" (main worktree: {})",
            marker.name,
            main.display()
        ),
    }
    if let Some(key) = adopted {
        println!("  adopted chitchat history recorded earlier for {key}");
    }

    let targets = targets(&dir, &found);
    let clients = pick_clients(&opts.clients);
    let bin = clients::binary_path()?;
    for &h in &clients {
        println!("{}:", h.name);
        for target in &targets {
            match clients::configure(target, h, &bin, opts.stop_hook) {
                Ok(done) if done.is_empty() => println!("  {}: already set up", target.display()),
                Ok(done) => done.iter().for_each(|d| println!("  {d}")),
                Err(e) => println!("  {}: {e:#}", target.display()),
            }
        }
    }
    if opts.clients.is_empty() {
        let missing: Vec<&str> = harness::ALL
            .iter()
            .filter(|h| !clients.iter().any(|c| c.id == h.id))
            .map(|h| h.name)
            .collect();
        if !missing.is_empty() {
            println!("Not installed here (skipped): {}", missing.join(", "));
        }
    }

    let mut excluded = Vec::new();
    for target in &targets {
        let mut files: Vec<&str> = clients
            .iter()
            .flat_map(|c| clients::local_files(c))
            .collect();
        files.push(".chitchat/");
        excluded.extend(clients::exclude_from_git(target, &files)?);
    }
    if !excluded.is_empty() {
        println!(
            "Git: kept local agent config out of git (.git/info/exclude): {}",
            excluded.join(", ")
        );
    }

    let docs = memory::refresh_docs(&mut conn, project_id, &dir)?;
    let total_docs: i64 = conn.query_row(
        "SELECT count(*) FROM docs WHERE project_id = ?1",
        [project_id],
        |r| r.get(0),
    )?;
    println!("Docs: {total_docs} Markdown files indexed ({docs} changed)");

    if opts.import {
        let counts = crate::import::import_claude_memory(&mut conn, project_id, &user, &targets)?;
        if counts.seen > 0 {
            println!(
                "Imported Claude Code memory: {} new, {} updated, {} unchanged",
                counts.created, counts.updated, counts.unchanged
            );
        }
    }

    println!("\nNext:");
    println!("  - Start new agent sessions here (running ones don't pick up the change).");
    for h in &clients {
        if let Some(note) = h.setup_note {
            println!("  - {}: {note}", h.name);
        }
    }
    if matches!(found, Found::Created | Found::Existing) {
        println!("  - New git worktree later? Run `chitchat init` inside it.");
    }
    println!("  - Watch the agents: chitchat tail");
    Ok(())
}

fn locate_or_create(start: &Path, name: Option<&str>) -> Result<(PathBuf, Marker, Found)> {
    if let Some((dir, marker)) = project::find_marker(start) {
        return Ok((dir, marker, Found::Existing));
    }
    if let Some((top, main)) = project::linked_worktree(start) {
        let rel = start.strip_prefix(&top).unwrap_or(Path::new(""));
        if let Some((main_dir, marker)) = project::find_marker(&main.join(rel))
            && let Ok(sub) = main_dir.strip_prefix(&main)
        {
            return Ok((top.join(sub), marker, Found::Joined(main_dir)));
        }
    }
    let name = name
        .map(str::to_string)
        .unwrap_or_else(|| project::last_segment(&start.to_string_lossy()));
    let marker = Marker::new(&name);
    project::write_marker(start, &marker)?;
    Ok((start.to_path_buf(), marker, Found::Created))
}

/// This directory plus, for a workspace in a main worktree, the same directory in
/// every linked worktree of the repo.
fn targets(dir: &Path, found: &Found) -> Vec<PathBuf> {
    let mut out = vec![dir.to_path_buf()];
    if matches!(found, Found::Joined(_)) {
        return out;
    }
    let Some((top, common)) = project::git_dirs(dir) else {
        return out;
    };
    if common.parent().map(canonical).as_deref() != Some(top.as_path()) {
        return out; // `dir` itself is in a linked worktree
    }
    let sub = dir.strip_prefix(&top).unwrap_or(Path::new(""));
    for worktree in project::linked_worktrees(&top) {
        let equivalent = worktree.join(sub);
        if equivalent.is_dir() {
            out.push(equivalent);
        }
    }
    out
}

fn pick_clients(requested: &[&'static Harness]) -> Vec<&'static Harness> {
    if !requested.is_empty() {
        return requested.to_vec();
    }
    harness::ALL
        .iter()
        .copied()
        .filter(|h| h.available())
        .collect()
}

/// Finds or creates the project row for `marker`, taking over the row an older
/// chitchat version created for the same repository if there is one. Returns the
/// project id and the legacy key that was adopted, if any.
fn register(conn: &Connection, marker: &Marker, dir: &Path) -> Result<(i64, Option<String>)> {
    let key = marker.key();
    let root = dir.to_string_lossy();
    let now = now_ms();
    let find = |k: &str| -> Result<Option<i64>> {
        Ok(conn
            .query_row("SELECT id FROM projects WHERE key = ?1", [k], |r| r.get(0))
            .optional()?)
    };
    if let Some(id) = find(&key)? {
        conn.execute(
            "UPDATE projects SET name = ?2, root = ?3, updated_at = ?4 WHERE id = ?1",
            params![id, marker.name, root, now],
        )?;
        return Ok((id, None));
    }
    for legacy in project::legacy_keys(dir) {
        if let Some(id) = find(&legacy)? {
            conn.execute(
                "UPDATE projects SET key = ?2, name = ?3, root = ?4, updated_at = ?5 WHERE id = ?1",
                params![id, key, marker.name, root, now],
            )?;
            return Ok((id, Some(legacy)));
        }
    }
    conn.execute(
        "INSERT INTO projects (key, name, root, created_at, updated_at) VALUES (?1, ?2, ?3, ?4, ?4)",
        params![key, marker.name, root, now],
    )?;
    Ok((conn.last_insert_rowid(), None))
}

pub use crate::import::claude_project_dir_name;

/// Turns chitchat off for the workspace containing `path` (and its worktrees),
/// keeping its data.
pub fn deinit(path: Option<&Path>, requested: &[&'static Harness]) -> Result<()> {
    let start = canonical(&match path {
        Some(p) => p.to_path_buf(),
        None => std::env::current_dir()?,
    });
    let Some(project) = project::detect_workspace(&start) else {
        bail!("{} is not in a chitchat workspace", start.display());
    };
    let found = if project::find_marker(&start).is_some() {
        Found::Existing
    } else {
        Found::Joined(PathBuf::new())
    };
    let clients: Vec<&'static Harness> = if requested.is_empty() {
        harness::ALL.to_vec()
    } else {
        requested.to_vec()
    };
    for target in targets(&project.root, &found) {
        for h in &clients {
            match clients::unconfigure(&target, h) {
                Ok(done) => done.iter().for_each(|d| println!("{}: {d}", h.name)),
                Err(e) => println!("{}: {e:#}", h.name),
            }
        }
    }
    println!(
        "chitchat is off in \"{}\". Its chat and notes are kept; `chitchat init` turns it back on.",
        project.name
    );
    Ok(())
}

/// Every workspace in the database, with activity counts.
pub fn list() -> Result<()> {
    let conn = crate::db::open_default()?;
    let mut stmt = conn.prepare(
        "SELECT p.name, p.root,
                (SELECT count(*) FROM agents a WHERE a.project_id = p.id AND a.vendor != 'human'),
                (SELECT count(*) FROM messages m WHERE m.project_id = p.id),
                (SELECT count(*) FROM notes n WHERE n.project_id = p.id AND n.deleted_at IS NULL),
                (SELECT max(created_at) FROM messages m WHERE m.project_id = p.id),
                p.updated_at
         FROM projects p WHERE p.key LIKE 'ws:%' ORDER BY p.name",
    )?;
    let rows = stmt
        .query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, Option<String>>(1)?,
                r.get::<_, i64>(2)?,
                r.get::<_, i64>(3)?,
                r.get::<_, i64>(4)?,
                r.get::<_, Option<i64>>(5)?,
                r.get::<_, i64>(6)?,
            ))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    if rows.is_empty() {
        println!("No workspaces yet. Run `chitchat init` in a project directory.");
        return Ok(());
    }
    for (name, root, agents, messages, notes, last_message, updated) in rows {
        let root = root.unwrap_or_default();
        let missing = if Path::new(&root).join(project::MARKER_DIR).is_dir() {
            ""
        } else {
            "  (directory or marker missing)"
        };
        println!(
            "{name}\n  {root}{missing}\n  {agents} agent{} · {messages} message{} · {notes} note{} · active {}",
            crate::digest::plural(agents as usize),
            crate::digest::plural(messages as usize),
            crate::digest::plural(notes as usize),
            format::ago(last_message.unwrap_or(updated))
        );
    }
    Ok(())
}

/// `chitchat clients`: every supported harness, whether it's installed, and how
/// much of chitchat it gets (MCP tools, automatic delivery through hooks).
pub fn list_clients() -> Result<()> {
    let here = std::env::current_dir()
        .ok()
        .and_then(|cwd| project::detect(&cwd).ok().flatten());
    for h in harness::ALL {
        let installed = if h.available() {
            "installed"
        } else {
            "not installed"
        };
        let delivery = match &h.hooks {
            Some(spec) => format!("hooks: {}", spec.events.len()),
            None => "no hooks (tools only)".to_string(),
        };
        let here = match &here {
            Some(p) if clients::status(&p.root, h).configured() => ", set up here",
            _ => "",
        };
        println!("{:<9} {:<18} {installed}{here} · {delivery}", h.id, h.name);
    }
    println!("\nSet up a workspace for one of them with `chitchat init --client <id>`.");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn register_adopts_data_recorded_before_workspaces() {
        let dir = tempfile::tempdir().unwrap();
        let conn = crate::db::open(&dir.path().join("t.db")).unwrap();
        let ws_dir = canonical(dir.path());
        // Older versions keyed a non-git directory by its path.
        let legacy = format!("path:{}", ws_dir.display());
        conn.execute(
            "INSERT INTO projects (key, name, created_at, updated_at) VALUES (?1, 'old', 0, 0)",
            [&legacy],
        )
        .unwrap();
        let old_id = conn.last_insert_rowid();

        let marker = Marker::new("demo");
        let (id, adopted) = register(&conn, &marker, &ws_dir).unwrap();
        assert_eq!((id, adopted.as_deref()), (old_id, Some(legacy.as_str())));
        let key: String = conn
            .query_row("SELECT key FROM projects WHERE id = ?1", [id], |r| r.get(0))
            .unwrap();
        assert_eq!(key, marker.key());
        // Registering again finds the adopted row.
        assert_eq!(register(&conn, &marker, &ws_dir).unwrap(), (old_id, None));
    }
}
