//! Commands the human runs in a terminal: talk in the project chat, watch it, and
//! browse shared memory. They act as the project's `@user`.

use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Result, bail};
use rusqlite::Connection;

use crate::agents::{self, Agent};
use crate::chat::{self, Intent, Message, NewMessage};
use crate::digest;
use crate::memory::{self, Sources};
use crate::project::Project;

const TAIL_POLL: Duration = Duration::from_millis(700);

struct Here {
    conn: Connection,
    project: Project,
    project_id: i64,
    me: Agent,
}

fn here() -> Result<Here> {
    let cwd = std::env::current_dir()?;
    let Some(project) = crate::project::detect(&cwd)? else {
        bail!(
            "{} is not in a chitchat workspace; run `chitchat init` in the project directory \
             (`chitchat workspaces` lists existing ones)",
            cwd.display()
        );
    };
    let conn = crate::db::open_default()?;
    let project_id = agents::ensure_project(&conn, &project)?;
    let me = agents::human(&conn, project_id)?;
    Ok(Here {
        conn,
        project,
        project_id,
        me,
    })
}

pub fn post(
    message: &str,
    to: Option<String>,
    room: Option<String>,
    request: bool,
    reply_to: Option<i64>,
) -> Result<()> {
    let mut h = here()?;
    let posted = chat::post(
        &mut h.conn,
        h.project_id,
        &h.me,
        NewMessage {
            body: message.to_string(),
            intent: if request {
                Intent::Request
            } else {
                Intent::Inform
            },
            to,
            room,
            reply_to,
        },
    )?;
    let to = if posted.recipients.is_empty() {
        "no agent is online right now".to_string()
    } else {
        posted
            .recipients
            .iter()
            .map(|r| format!("@{r}"))
            .collect::<Vec<_>>()
            .join(", ")
    };
    println!(
        "Posted #{} to {} ({to}).",
        posted.message.id,
        posted.message.destination()
    );
    if !posted.unknown_mentions.is_empty() {
        println!("Unknown: {}", posted.unknown_mentions.join(", "));
    }
    Ok(())
}

pub fn tail(room: Option<&str>, lines: usize, follow: bool) -> Result<()> {
    let h = here()?;
    let label = room.map_or("all rooms".to_string(), |r| {
        format!("#{}", r.trim_start_matches('#'))
    });
    println!(
        "chitchat · {} [{}] · {label}",
        h.project.name, h.project.key
    );
    let mut last = 0;
    for m in chat::history(&h.conn, h.project_id, 0, room, lines)? {
        print_message(&m);
        last = m.id;
    }
    if !follow {
        return Ok(());
    }
    loop {
        std::thread::sleep(TAIL_POLL);
        for m in chat::history(&h.conn, h.project_id, last, room, 500)? {
            print_message(&m);
            last = m.id;
        }
    }
}

fn print_message(m: &Message) {
    let for_user = m.recipient.as_deref() == Some(agents::HUMAN_HANDLE)
        || chat::parse_mentions(&m.body).contains(agents::HUMAN_HANDLE);
    let marker = if for_user { "» " } else { "  " };
    println!("{marker}{}", m.render(usize::MAX));
}

pub fn who() -> Result<()> {
    let h = here()?;
    println!("{}", digest::who(&h.conn, &h.project, h.project_id, None)?);
    Ok(())
}

pub fn notes(query: &str, kind: Option<&str>, messages: bool, limit: usize) -> Result<()> {
    let mut h = here()?;
    if !query.trim().is_empty() {
        memory::refresh_docs(&mut h.conn, h.project_id, &h.project.root)?;
    }
    let sources = Sources {
        notes: true,
        docs: true,
        messages,
    };
    let hits = memory::recall(&h.conn, h.project_id, &h.me, query, kind, sources, limit)?;
    println!(
        "{}",
        crate::mcp::render_hits(&hits, query.trim().is_empty())
    );
    Ok(())
}

pub fn note(key: &str, history: bool) -> Result<()> {
    let h = here()?;
    let Some(note) = memory::find(&h.conn, h.project_id, key)? else {
        bail!("there is no note `{key}`");
    };
    println!("{}", crate::mcp::render_note(&h.conn, &note, history)?);
    Ok(())
}

pub fn forget(key: &str) -> Result<()> {
    let h = here()?;
    let note = memory::forget(&h.conn, h.project_id, key)?;
    println!(
        "Forgot `{}` (it stays in history; remember revives it).",
        note.key
    );
    Ok(())
}

pub fn export(dir: Option<PathBuf>) -> Result<()> {
    let h = here()?;
    let dir = dir.unwrap_or_else(|| default_export_dir(&h.project.root));
    let out = memory::export(&h.conn, h.project_id, &dir)?;
    println!(
        "Exported to {}: {} written, {} unchanged, {} removed.",
        out.dir.display(),
        out.written,
        out.unchanged,
        out.removed
    );
    Ok(())
}

fn default_export_dir(root: &Path) -> PathBuf {
    root.join(".chitchat").join("notes")
}

pub fn backup(out: Option<&Path>, list: bool) -> Result<()> {
    if list {
        let backups = crate::backup::list()?;
        if backups.is_empty() {
            println!("No backups in {}.", crate::backup::dir()?.display());
        }
        for b in backups {
            let age = b.modified.elapsed().map_or("?".to_string(), |d| {
                crate::format::ago(crate::db::now_ms() - d.as_millis() as i64)
            });
            println!(
                "{}  {}  {}{}",
                b.path.display(),
                crate::backup::human_size(b.size),
                age,
                if b.is_auto() { "  (automatic)" } else { "" }
            );
        }
        return Ok(());
    }
    let conn = crate::db::open_default()?;
    let path = crate::backup::create(&conn, out, "")?;
    let size = std::fs::metadata(&path).map_or(0, |m| m.len());
    println!(
        "Backed up to {} ({}).",
        path.display(),
        crate::backup::human_size(size)
    );
    Ok(())
}

pub fn restore(which: &str) -> Result<()> {
    let src = if which == "latest" {
        crate::backup::list()?
            .into_iter()
            .find(|b| {
                !b.path
                    .file_name()
                    .is_some_and(|n| n.to_string_lossy().starts_with("pre-restore-"))
            })
            .map(|b| b.path)
            .ok_or_else(|| anyhow::anyhow!("there are no backups to restore"))?
    } else {
        PathBuf::from(which)
    };
    let saved = crate::backup::restore(&crate::paths::db_path()?, &src)?;
    println!("Restored {}.", src.display());
    println!(
        "The previous database was saved to {} (restore it to undo).",
        saved.display()
    );
    Ok(())
}
