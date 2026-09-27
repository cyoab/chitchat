//! Shared memory: notes (the source of truth, in the DB), a read-only index of
//! the project's Markdown docs, full-text recall over both, and Markdown export.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail};
use rusqlite::{Connection, OptionalExtension, Row, TransactionBehavior, params};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::agents::Agent;
use crate::chat;
use crate::db::now_ms;
use crate::format;

pub const KINDS: &[&str] = &["note", "decision", "fact", "gotcha", "handoff", "plan"];
pub const MAX_BODY_CHARS: usize = 16_000;
const MAX_TITLE_CHARS: usize = 200;
const MAX_KEY_CHARS: usize = 120;
const MAX_TAGS: usize = 10;
const MAX_DOC_BYTES: u64 = 512 * 1024;
const EXPORT_MARKER: &str = "generated_by: chitchat";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum Scope {
    /// Only this project (default).
    #[default]
    Project,
    /// Every project on this machine: personal conventions, cross-repo facts.
    Global,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Note {
    pub id: i64,
    pub global: bool,
    pub key: String,
    pub kind: String,
    pub title: String,
    pub body: String,
    pub tags: Vec<String>,
    pub revision: i64,
    pub author: Option<String>,
    pub superseded_by: Option<String>,
    pub created_at: i64,
    pub updated_at: i64,
    pub deleted_at: Option<i64>,
}

impl Note {
    /// "decision/storage (decision, rev 3, @codex-1, 2h ago)".
    pub fn summary(&self) -> String {
        let mut parts = vec![self.kind.clone(), format!("rev {}", self.revision)];
        if let Some(author) = &self.author {
            parts.push(format!("@{author}"));
        }
        parts.push(format::ago(self.updated_at));
        if self.global {
            parts.push("global".into());
        }
        let superseded = match &self.superseded_by {
            Some(key) => format!(" (superseded by {key})"),
            None => String::new(),
        };
        format!(
            "{} \"{}\" ({}){superseded}",
            self.key,
            self.title,
            parts.join(", ")
        )
    }
}

const NOTE_COLUMNS: &str = "n.id, n.project_id IS NULL, n.key, n.kind, n.title, n.body, n.tags,
                            n.revision, a.handle, s.key, n.created_at, n.updated_at, n.deleted_at";
const NOTE_FROM: &str = "FROM notes n
                         LEFT JOIN agents a ON a.id = n.author_id
                         LEFT JOIN notes s ON s.id = n.superseded_by";

fn note_from_row(row: &Row) -> rusqlite::Result<Note> {
    let tags: String = row.get(6)?;
    Ok(Note {
        id: row.get(0)?,
        global: row.get(1)?,
        key: row.get(2)?,
        kind: row.get(3)?,
        title: row.get(4)?,
        body: row.get(5)?,
        tags: tags.split_whitespace().map(str::to_string).collect(),
        revision: row.get(7)?,
        author: row.get(8)?,
        superseded_by: row.get(9)?,
        created_at: row.get(10)?,
        updated_at: row.get(11)?,
        deleted_at: row.get(12)?,
    })
}

#[derive(Debug, Clone, Default)]
pub struct NoteInput {
    pub key: Option<String>,
    pub kind: Option<String>,
    pub title: String,
    pub body: String,
    pub tags: Vec<String>,
    pub scope: Scope,
    /// Required to change an existing note: the revision you last read.
    pub expected_revision: Option<i64>,
    /// Key of an older note this one replaces.
    pub supersedes: Option<String>,
    /// Skip the revision check (imports, where the source file is authoritative).
    /// Never set from agent input.
    pub overwrite: bool,
}

#[derive(Debug)]
pub enum Saved {
    Created(Note),
    Updated(Note),
    Unchanged(Note),
}

impl Saved {
    pub fn note(&self) -> &Note {
        match self {
            Saved::Created(n) | Saved::Updated(n) | Saved::Unchanged(n) => n,
        }
    }
}

pub fn remember(
    conn: &mut Connection,
    project_id: i64,
    author: &Agent,
    input: NoteInput,
) -> Result<Saved> {
    let kind = input
        .kind
        .as_deref()
        .unwrap_or("note")
        .trim()
        .to_ascii_lowercase();
    if !KINDS.contains(&kind.as_str()) {
        bail!("kind must be one of: {}", KINDS.join(", "));
    }
    let title = input.title.trim().to_string();
    if title.is_empty() || title.lines().count() > 1 || title.chars().count() > MAX_TITLE_CHARS {
        bail!("title must be a single non-empty line of at most {MAX_TITLE_CHARS} characters");
    }
    let body = input.body.trim().to_string();
    if body.is_empty() {
        bail!("body is empty");
    }
    if body.chars().count() > MAX_BODY_CHARS {
        bail!("body is longer than {MAX_BODY_CHARS} characters; split it into several notes");
    }
    let tags = normalize_tags(&input.tags)?;
    let key = match input.key.as_deref() {
        Some(key) => normalize_key(key)?,
        None => normalize_key(&format!("{kind}/{}", slug(&title)))?,
    };
    let scope_id = match input.scope {
        Scope::Project => Some(project_id),
        Scope::Global => None,
    };

    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let now = now_ms();
    let existing = find_exact(&tx, scope_id, &key)?;

    let id = match (&existing, input.expected_revision) {
        (None, Some(rev)) if rev > 0 => {
            bail!(
                "there is no note `{key}` to update (expected_revision={rev}); omit expected_revision to create it"
            )
        }
        (None, _) => {
            tx.execute(
                "INSERT INTO notes (project_id, key, kind, title, body, tags, author_id, created_at, updated_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?8)",
                params![scope_id, key, kind, title, body, tags, author.id, now],
            )?;
            tx.last_insert_rowid()
        }
        (Some(note), expected) => {
            let revived = note.deleted_at.is_some();
            if !revived && !input.overwrite {
                match expected {
                    None => bail!(
                        "note `{key}` already exists (revision {}, last updated by {} {}). Read it with \
                         get, merge your change into its content, and call remember again with \
                         expected_revision={}",
                        note.revision,
                        note.author
                            .as_deref()
                            .map_or("someone".into(), |a| format!("@{a}")),
                        format::ago(note.updated_at),
                        note.revision
                    ),
                    Some(rev) if rev != note.revision => bail!(
                        "note `{key}` changed since you read it: it is now at revision {} (by {}), you \
                         expected {rev}. Read it again with get and merge",
                        note.revision,
                        note.author
                            .as_deref()
                            .map_or("someone".into(), |a| format!("@{a}"))
                    ),
                    Some(_) => {}
                }
            }
            let same = !revived
                && note.title == title
                && note.body == body
                && note.kind == kind
                && note.tags.join(" ") == tags;
            if same {
                let note = note.clone();
                tx.commit()?;
                return Ok(Saved::Unchanged(note));
            }
            tx.execute(
                "UPDATE notes SET kind = ?2, title = ?3, body = ?4, tags = ?5, author_id = ?6,
                     revision = revision + 1, updated_at = ?7, deleted_at = NULL
                 WHERE id = ?1",
                params![note.id, kind, title, body, tags, author.id, now],
            )?;
            note.id
        }
    };

    if let Some(old_key) = &input.supersedes {
        let old_key = normalize_key(old_key)?;
        let Some(old) = find(&tx, project_id, &old_key)? else {
            bail!("supersedes: there is no note `{old_key}`");
        };
        if old.id == id {
            bail!("a note can't supersede itself");
        }
        tx.execute(
            "UPDATE notes SET superseded_by = ?2 WHERE id = ?1",
            params![old.id, id],
        )?;
    }

    let note = by_id(&tx, id)?;
    tx.commit()?;
    Ok(if existing.is_some() {
        Saved::Updated(note)
    } else {
        Saved::Created(note)
    })
}

fn by_id(conn: &Connection, id: i64) -> Result<Note> {
    Ok(conn.query_row(
        &format!("SELECT {NOTE_COLUMNS} {NOTE_FROM} WHERE n.id = ?1"),
        [id],
        note_from_row,
    )?)
}

/// A note in exactly this scope (project id, or `None` for global), deleted or not.
fn find_exact(conn: &Connection, scope_id: Option<i64>, key: &str) -> Result<Option<Note>> {
    Ok(conn
        .query_row(
            &format!(
                "SELECT {NOTE_COLUMNS} {NOTE_FROM}
                 WHERE coalesce(n.project_id, 0) = coalesce(?1, 0) AND n.key = ?2"
            ),
            params![scope_id, key],
            note_from_row,
        )
        .optional()?)
}

/// A live note visible from `project_id`: the project's own first, then global.
pub fn find(conn: &Connection, project_id: i64, key: &str) -> Result<Option<Note>> {
    let key = normalize_key(key)?;
    for scope in [Some(project_id), None] {
        if let Some(note) = find_exact(conn, scope, &key)?.filter(|n| n.deleted_at.is_none()) {
            return Ok(Some(note));
        }
    }
    Ok(None)
}

#[derive(Debug, Clone)]
pub struct Version {
    pub revision: i64,
    pub author: Option<String>,
    pub created_at: i64,
    pub title: String,
}

pub fn history(conn: &Connection, note_id: i64) -> Result<Vec<Version>> {
    let mut stmt = conn.prepare(
        "SELECT v.revision, a.handle, v.created_at, v.title
         FROM note_versions v LEFT JOIN agents a ON a.id = v.author_id
         WHERE v.note_id = ?1 ORDER BY v.revision DESC",
    )?;
    let rows = stmt
        .query_map([note_id], |r| {
            Ok(Version {
                revision: r.get(0)?,
                author: r.get(1)?,
                created_at: r.get(2)?,
                title: r.get(3)?,
            })
        })?
        .collect::<rusqlite::Result<_>>()?;
    Ok(rows)
}

/// Soft-deletes a note (it stays in history and can be revived by `remember`).
pub fn forget(conn: &Connection, project_id: i64, key: &str) -> Result<Note> {
    let Some(note) = find(conn, project_id, key)? else {
        bail!("there is no note `{key}`");
    };
    conn.execute(
        "UPDATE notes SET deleted_at = ?2 WHERE id = ?1",
        params![note.id, now_ms()],
    )?;
    Ok(note)
}

/// Most recently updated live notes visible from the project.
pub fn recent(
    conn: &Connection,
    project_id: i64,
    kind: Option<&str>,
    limit: usize,
) -> Result<Vec<Note>> {
    let mut stmt = conn.prepare(&format!(
        "SELECT {NOTE_COLUMNS} {NOTE_FROM}
         WHERE (n.project_id = ?1 OR n.project_id IS NULL) AND n.deleted_at IS NULL
         AND (?2 IS NULL OR n.kind = ?2)
         ORDER BY n.superseded_by IS NOT NULL, n.updated_at DESC LIMIT ?3"
    ))?;
    let rows = stmt
        .query_map(params![project_id, kind, limit as i64], note_from_row)?
        .collect::<rusqlite::Result<_>>()?;
    Ok(rows)
}

#[derive(Debug, Clone, Copy)]
pub struct Sources {
    pub notes: bool,
    pub docs: bool,
    pub messages: bool,
}

impl Default for Sources {
    fn default() -> Self {
        Sources {
            notes: true,
            docs: true,
            messages: false,
        }
    }
}

#[derive(Debug)]
pub enum Hit {
    Note(Note, String),
    Doc {
        path: String,
        title: String,
        snippet: String,
    },
    Message(chat::Message, String),
}

/// Full-text search. An empty query lists recent notes instead.
pub fn recall(
    conn: &Connection,
    project_id: i64,
    me: &Agent,
    text: &str,
    kind: Option<&str>,
    sources: Sources,
    limit: usize,
) -> Result<Vec<Hit>> {
    let limit = limit.clamp(1, 50);
    let Some(fts) = fts_query(text) else {
        return Ok(recent(conn, project_id, kind, limit)?
            .into_iter()
            .map(|n| Hit::Note(n, String::new()))
            .collect());
    };
    let mut hits = Vec::new();
    if sources.notes {
        let mut stmt = conn.prepare(&format!(
            "SELECT {NOTE_COLUMNS}, snippet(notes_fts, 1, '«', '»', '…', 16)
             FROM notes_fts JOIN notes n ON n.id = notes_fts.rowid
             LEFT JOIN agents a ON a.id = n.author_id
             LEFT JOIN notes s ON s.id = n.superseded_by
             WHERE notes_fts MATCH ?1 AND n.deleted_at IS NULL
             AND (n.project_id = ?2 OR n.project_id IS NULL) AND (?3 IS NULL OR n.kind = ?3)
             ORDER BY n.superseded_by IS NOT NULL, bm25(notes_fts, 5.0, 1.0, 2.0) LIMIT ?4"
        ))?;
        let rows = stmt.query_map(params![fts, project_id, kind, limit as i64], |r| {
            Ok(Hit::Note(note_from_row(r)?, r.get(13)?))
        })?;
        for row in rows {
            hits.push(row?);
        }
    }
    if sources.docs && kind.is_none() {
        let mut stmt = conn.prepare(
            "SELECT d.path, d.title, snippet(docs_fts, 1, '«', '»', '…', 16)
             FROM docs_fts JOIN docs d ON d.id = docs_fts.rowid
             WHERE docs_fts MATCH ?1 AND d.project_id = ?2
             ORDER BY bm25(docs_fts, 5.0, 1.0) LIMIT ?3",
        )?;
        let rows = stmt.query_map(params![fts, project_id, limit as i64], |r| {
            Ok(Hit::Doc {
                path: r.get(0)?,
                title: r.get(1)?,
                snippet: r.get(2)?,
            })
        })?;
        for row in rows {
            hits.push(row?);
        }
    }
    if sources.messages && kind.is_none() {
        for (m, snippet) in chat::search(conn, project_id, me, &fts, limit)? {
            hits.push(Hit::Message(m, snippet));
        }
    }
    Ok(hits)
}

/// Turns free text into a forgiving FTS5 query: every word as a quoted prefix
/// term, OR-ed together so BM25 ranks partial matches instead of dropping them.
/// Quoting also neutralizes FTS5 syntax characters in the input.
pub fn fts_query(text: &str) -> Option<String> {
    let mut seen = HashSet::new();
    let terms: Vec<String> = text
        .split(|c: char| !(c.is_alphanumeric() || c == '_'))
        .map(str::to_lowercase)
        .filter(|w| !w.is_empty() && seen.insert(w.clone()))
        .take(16)
        .map(|w| format!("\"{w}\"*"))
        .collect();
    (!terms.is_empty()).then(|| terms.join(" OR "))
}

/// Keys look like paths: "decision/storage", "gotcha/sqlite-busy".
pub fn normalize_key(key: &str) -> Result<String> {
    let key = key.trim().trim_matches('/').to_ascii_lowercase();
    let valid = !key.is_empty()
        && key.chars().count() <= MAX_KEY_CHARS
        && key.chars().all(|c| {
            c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '-' | '_' | '.' | '/')
        })
        && key
            .split('/')
            .all(|seg| !seg.is_empty() && seg != "." && seg != "..");
    if !valid {
        bail!(
            "keys are up to {MAX_KEY_CHARS} characters of lowercase letters, digits, '-', '_', '.' \
             and '/' separators, e.g. \"decision/storage\""
        );
    }
    Ok(key)
}

/// Lowercase, dash-separated form of a title for keys.
pub fn slug(title: &str) -> String {
    let mut out = String::new();
    for c in title.chars() {
        if c.is_ascii_alphanumeric() {
            out.push(c.to_ascii_lowercase());
        } else if !out.ends_with('-') && !out.is_empty() {
            out.push('-');
        }
        if out.len() >= 60 {
            break;
        }
    }
    let out = out.trim_end_matches('-').to_string();
    if out.is_empty() {
        "untitled".into()
    } else {
        out
    }
}

fn normalize_tags(tags: &[String]) -> Result<String> {
    let mut out: Vec<String> = Vec::new();
    for tag in tags {
        let tag = tag.trim().trim_start_matches('#').to_ascii_lowercase();
        if tag.is_empty() {
            continue;
        }
        let valid = tag.len() <= 32
            && tag
                .chars()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '_');
        if !valid {
            bail!("tags are up to 32 characters: lowercase letters, digits, '-' and '_'");
        }
        if !out.contains(&tag) {
            out.push(tag);
        }
    }
    if out.len() > MAX_TAGS {
        bail!("at most {MAX_TAGS} tags");
    }
    Ok(out.join(" "))
}

/// Brings the docs index for `project_id` in line with the Markdown files in the
/// git worktree at `root` (tracked or untracked-but-not-ignored). Returns how many
/// documents changed. Not a git repo: nothing to index.
pub fn refresh_docs(conn: &mut Connection, project_id: i64, root: &Path) -> Result<usize> {
    let Some(paths) = list_markdown(root) else {
        return Ok(0);
    };
    let known: HashMap<String, (i64, i64)> = {
        let mut stmt =
            conn.prepare("SELECT path, mtime_ms, size FROM docs WHERE project_id = ?1")?;
        stmt.query_map([project_id], |r| Ok((r.get(0)?, (r.get(1)?, r.get(2)?))))?
            .collect::<rusqlite::Result<_>>()?
    };

    // Read files before taking the write lock.
    let mut changed = Vec::new();
    for rel in &paths {
        let full = root.join(rel);
        let Ok(meta) = std::fs::metadata(&full) else {
            continue;
        };
        if !meta.is_file() || meta.len() > MAX_DOC_BYTES {
            continue;
        }
        let mtime = meta
            .modified()
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map_or(0, |d| d.as_millis() as i64);
        let size = meta.len() as i64;
        if known.get(rel) == Some(&(mtime, size)) {
            continue;
        }
        let Ok(bytes) = std::fs::read(&full) else {
            continue;
        };
        let body = String::from_utf8_lossy(&bytes).into_owned();
        let title = doc_title(&body).unwrap_or_else(|| rel.clone());
        changed.push((rel.clone(), title, body, mtime, size));
    }
    let listed: HashSet<&String> = paths.iter().collect();
    let removed: Vec<&String> = known.keys().filter(|p| !listed.contains(p)).collect();
    if changed.is_empty() && removed.is_empty() {
        return Ok(0);
    }

    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let now = now_ms();
    for (path, title, body, mtime, size) in &changed {
        tx.execute(
            "INSERT INTO docs (project_id, path, title, body, mtime_ms, size, indexed_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
             ON CONFLICT (project_id, path) DO UPDATE SET title = excluded.title, body = excluded.body,
                 mtime_ms = excluded.mtime_ms, size = excluded.size, indexed_at = excluded.indexed_at",
            params![project_id, path, title, body, mtime, size, now],
        )?;
    }
    for path in &removed {
        tx.execute(
            "DELETE FROM docs WHERE project_id = ?1 AND path = ?2",
            params![project_id, path],
        )?;
    }
    tx.commit()?;
    Ok(changed.len() + removed.len())
}

fn list_markdown(root: &Path) -> Option<Vec<String>> {
    let out = Command::new("git")
        .arg("-C")
        .arg(root)
        .args([
            "ls-files",
            "-z",
            "--cached",
            "--others",
            "--exclude-standard",
            "--",
            "*.md",
            "*.mdx",
            "*.markdown",
        ])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let mut paths: Vec<String> = out
        .stdout
        .split(|b| *b == 0)
        .filter(|p| !p.is_empty())
        .map(|p| String::from_utf8_lossy(p).into_owned())
        .collect();
    paths.sort();
    paths.dedup();
    Some(paths)
}

fn doc_title(body: &str) -> Option<String> {
    body.lines()
        .map(str::trim)
        .find_map(|l| l.strip_prefix("# "))
        .map(|t| format::truncate(t.trim(), MAX_TITLE_CHARS))
}

#[derive(Debug, Default)]
pub struct Exported {
    pub written: usize,
    pub unchanged: usize,
    pub removed: usize,
    pub dir: PathBuf,
}

/// Writes every live note visible from the project as Markdown under `dir`
/// (global notes under `_global/`), plus an INDEX.md. One-way: the files are a
/// derived view; editing them changes nothing. Stale files this export wrote
/// earlier are removed; other files are left alone.
pub fn export(conn: &Connection, project_id: i64, dir: &Path) -> Result<Exported> {
    let notes = recent(conn, project_id, None, 100_000)?;
    std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    let mut summary = Exported {
        dir: dir.to_path_buf(),
        ..Default::default()
    };
    let mut expected = HashSet::new();

    for note in &notes {
        let rel = note_path(note);
        let path = dir.join(&rel);
        expected.insert(path.clone());
        let content = render_markdown(note);
        if std::fs::read_to_string(&path).ok().as_deref() == Some(content.as_str()) {
            summary.unchanged += 1;
            continue;
        }
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(&path, content).with_context(|| format!("writing {}", path.display()))?;
        summary.written += 1;
    }

    let index = dir.join("INDEX.md");
    expected.insert(index.clone());
    let index_content = render_index(&notes);
    if std::fs::read_to_string(&index).ok().as_deref() != Some(index_content.as_str()) {
        std::fs::write(&index, index_content)?;
    }

    for path in walk_markdown(dir) {
        if expected.contains(&path) {
            continue;
        }
        let ours = std::fs::read_to_string(&path).is_ok_and(|c| c.contains(EXPORT_MARKER));
        if ours {
            std::fs::remove_file(&path)?;
            summary.removed += 1;
        }
    }
    Ok(summary)
}

fn note_path(note: &Note) -> PathBuf {
    let mut path = PathBuf::new();
    if note.global {
        path.push("_global");
    }
    for seg in note.key.split('/') {
        path.push(seg);
    }
    path.set_extension("md");
    path
}

fn render_markdown(note: &Note) -> String {
    let mut out = String::from("---\n");
    out.push_str(&format!("key: {}\n", note.key));
    out.push_str(&format!("kind: {}\n", note.kind));
    out.push_str(&format!(
        "title: {}\n",
        serde_json::to_string(&note.title).unwrap_or_default()
    ));
    out.push_str(&format!("revision: {}\n", note.revision));
    if let Some(author) = &note.author {
        out.push_str(&format!("author: \"@{author}\"\n"));
    }
    out.push_str(&format!("updated: {}\n", format::iso_utc(note.updated_at)));
    if !note.tags.is_empty() {
        out.push_str(&format!("tags: [{}]\n", note.tags.join(", ")));
    }
    if let Some(key) = &note.superseded_by {
        out.push_str(&format!("superseded_by: {key}\n"));
    }
    if note.global {
        out.push_str("scope: global\n");
    }
    out.push_str(EXPORT_MARKER);
    out.push_str("\n---\n\n");
    out.push_str(&format!("# {}\n\n{}\n", note.title, note.body.trim()));
    out
}

fn render_index(notes: &[Note]) -> String {
    let mut out = format!(
        "---\n{EXPORT_MARKER}\n---\n\n# Shared notes\n\nExported by `chitchat export`. The chitchat \
         database is the source of truth; edits here are not read back.\n"
    );
    for kind in KINDS {
        let of_kind: Vec<&Note> = notes.iter().filter(|n| n.kind == *kind).collect();
        if of_kind.is_empty() {
            continue;
        }
        out.push_str(&format!("\n## {kind}\n\n"));
        for n in of_kind {
            let link = note_path(n).to_string_lossy().replace('\\', "/");
            let superseded = if n.superseded_by.is_some() {
                " (superseded)"
            } else {
                ""
            };
            out.push_str(&format!("- [{}]({link}){superseded}\n", n.title));
        }
    }
    out
}

fn walk_markdown(dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&d) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
            } else if path.extension().is_some_and(|e| e == "md") {
                out.push(path);
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agents::{
        self,
        tests::{caller, proc, project},
    };

    fn setup() -> (tempfile::TempDir, Connection, i64, Agent, Agent) {
        let dir = tempfile::tempdir().unwrap();
        let mut conn = crate::db::open(&dir.path().join("t.db")).unwrap();
        let pid = project(&conn);
        let a = agents::resolve(&mut conn, pid, &caller("claude", Some(&proc(1)), None)).unwrap();
        let b = agents::resolve(&mut conn, pid, &caller("codex", Some(&proc(2)), None)).unwrap();
        (dir, conn, pid, a, b)
    }

    fn input(title: &str, body: &str) -> NoteInput {
        NoteInput {
            kind: Some("decision".into()),
            title: title.into(),
            body: body.into(),
            ..Default::default()
        }
    }

    #[test]
    fn keys_default_to_kind_and_slug() {
        let (_d, mut conn, pid, a, _) = setup();
        let saved = remember(&mut conn, pid, &a, input("Use SQLite (WAL)!", "because")).unwrap();
        assert_eq!(saved.note().key, "decision/use-sqlite-wal");
        assert!(matches!(saved, Saved::Created(_)));
    }

    #[test]
    fn updates_require_the_current_revision() {
        let (_d, mut conn, pid, a, b) = setup();
        remember(&mut conn, pid, &a, input("Storage", "SQLite")).unwrap();

        let blind = remember(&mut conn, pid, &b, input("Storage", "Postgres"));
        assert!(
            blind
                .unwrap_err()
                .to_string()
                .contains("expected_revision=1")
        );

        let ok = remember(
            &mut conn,
            pid,
            &b,
            NoteInput {
                expected_revision: Some(1),
                ..input("Storage", "SQLite, bundled")
            },
        )
        .unwrap();
        assert!(matches!(ok, Saved::Updated(_)));
        assert_eq!(ok.note().revision, 2);
        assert_eq!(ok.note().author.as_deref(), Some("codex-1"));

        let stale = remember(
            &mut conn,
            pid,
            &a,
            NoteInput {
                expected_revision: Some(1),
                ..input("Storage", "x")
            },
        );
        assert!(
            stale
                .unwrap_err()
                .to_string()
                .contains("changed since you read it")
        );

        let same = remember(
            &mut conn,
            pid,
            &a,
            NoteInput {
                expected_revision: Some(2),
                ..input("Storage", "SQLite, bundled")
            },
        )
        .unwrap();
        assert!(matches!(same, Saved::Unchanged(_)));
        assert_eq!(history(&conn, ok.note().id).unwrap().len(), 2);
    }

    #[test]
    fn recall_finds_notes_and_marks_superseded() {
        let (_d, mut conn, pid, a, _) = setup();
        remember(&mut conn, pid, &a, input("Old storage", "MySQL server")).unwrap();
        remember(
            &mut conn,
            pid,
            &a,
            NoteInput {
                supersedes: Some("decision/old-storage".into()),
                ..input("New storage", "SQLite file, no server")
            },
        )
        .unwrap();

        let hits = recall(&conn, pid, &a, "server", None, Sources::default(), 10).unwrap();
        let keys: Vec<String> = hits
            .iter()
            .map(|h| match h {
                Hit::Note(n, _) => n.key.clone(),
                _ => unreachable!(),
            })
            .collect();
        assert_eq!(keys, ["decision/new-storage", "decision/old-storage"]);
        let Hit::Note(old, _) = &hits[1] else {
            unreachable!()
        };
        assert_eq!(old.superseded_by.as_deref(), Some("decision/new-storage"));
    }

    #[test]
    fn fts_query_is_injection_safe() {
        assert_eq!(
            fts_query("foo-bar AND \"x\"").unwrap(),
            "\"foo\"* OR \"bar\"* OR \"and\"* OR \"x\"*"
        );
        assert_eq!(fts_query("  ?!  "), None);
    }

    #[test]
    fn keys_reject_traversal() {
        for bad in ["../etc", "a/../b", "a//b", "Upper Case", ""] {
            assert!(normalize_key(bad).is_err(), "{bad:?}");
        }
        assert_eq!(
            normalize_key("/Decision/Storage/").unwrap(),
            "decision/storage"
        );
    }

    #[test]
    fn export_writes_and_prunes_only_its_own_files() {
        let (d, mut conn, pid, a, _) = setup();
        remember(&mut conn, pid, &a, input("Storage", "SQLite")).unwrap();
        remember(
            &mut conn,
            pid,
            &a,
            NoteInput {
                scope: Scope::Global,
                ..input("Style", "Short commits")
            },
        )
        .unwrap();
        let out = d.path().join("notes");
        std::fs::create_dir_all(&out).unwrap();
        std::fs::write(out.join("mine.md"), "hand written").unwrap();

        let first = export(&conn, pid, &out).unwrap();
        assert_eq!(first.written, 2);
        assert!(out.join("decision/storage.md").exists());
        assert!(out.join("_global/decision/style.md").exists());

        forget(&conn, pid, "decision/storage").unwrap();
        let second = export(&conn, pid, &out).unwrap();
        assert_eq!(
            (second.written, second.unchanged, second.removed),
            (0, 1, 1)
        );
        assert!(!out.join("decision/storage.md").exists());
        assert!(out.join("mine.md").exists());
    }

    #[test]
    fn docs_index_follows_the_worktree() {
        let (d, mut conn, pid, a, _) = setup();
        let repo = d.path().join("repo");
        std::fs::create_dir_all(repo.join("docs")).unwrap();
        assert!(
            Command::new("git")
                .arg("-C")
                .arg(&repo)
                .args(["init", "-q"])
                .status()
                .unwrap()
                .success()
        );
        std::fs::write(
            repo.join("README.md"),
            "# Demo\n\nThe frobnicator lives here.",
        )
        .unwrap();
        std::fs::write(repo.join("docs/ops.md"), "# Ops\n\nRestart with care.").unwrap();

        assert_eq!(refresh_docs(&mut conn, pid, &repo).unwrap(), 2);
        assert_eq!(refresh_docs(&mut conn, pid, &repo).unwrap(), 0);
        let hits = recall(&conn, pid, &a, "frobnicator", None, Sources::default(), 10).unwrap();
        assert!(
            matches!(&hits[..], [Hit::Doc { path, title, .. }] if path == "README.md" && title == "Demo")
        );

        std::fs::remove_file(repo.join("docs/ops.md")).unwrap();
        assert_eq!(refresh_docs(&mut conn, pid, &repo).unwrap(), 1);
    }
}
