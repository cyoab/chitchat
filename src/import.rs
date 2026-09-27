//! Copy harness memory files into versioned shared notes; source files stay untouched.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use rusqlite::{Connection, TransactionBehavior, params};

use crate::agents::{self, Agent};
use crate::format;
use crate::memory::{self, NoteInput, Saved, Scope};

#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum Source {
    Claude,
    Codex,
    Gemini,
}

impl Source {
    pub fn id(self) -> &'static str {
        match self {
            Self::Claude => "claude",
            Self::Codex => "codex",
            Self::Gemini => "gemini",
        }
    }
}

pub fn sources() -> &'static [Source] {
    &[Source::Claude, Source::Codex, Source::Gemini]
}

/// CLI selection includes `all`; the source registry contains real sources only.
#[derive(Debug, Clone, Copy)]
pub struct Selection(pub Option<Source>);

pub fn parse_source(value: &str) -> Result<Selection, String> {
    if value == "all" {
        return Ok(Selection(None));
    }
    sources()
        .iter()
        .find(|s| s.id() == value)
        .copied()
        .map(|s| Selection(Some(s)))
        .ok_or_else(|| "expected claude, codex, gemini, or all".into())
}

pub fn run(source: Option<Source>, dry_run: bool) -> Result<()> {
    let cwd = std::env::current_dir()?;
    let project =
        crate::project::detect(&cwd)?.context("not in a workspace; run chitchat init first")?;
    let mut dirs = vec![project.root.clone()];
    // Only bring in worktrees belonging to this workspace, including its subpath.
    if let Some((top, _)) = crate::project::git_dirs(&project.root) {
        let sub = project.root.strip_prefix(&top).unwrap_or(Path::new(""));
        for worktree in crate::project::linked_worktrees(&top) {
            let dir = worktree.join(sub);
            if dir.is_dir() && !dirs.contains(&dir) {
                dirs.push(dir);
            }
        }
    }
    // Serializes source-authoritative imports (including multi-part cleanup),
    // without holding a database transaction while reading files.
    let _lock = if dry_run { None } else { Some(import_lock()?) };
    let selected = source.map_or_else(|| sources().to_vec(), |s| vec![s]);
    let mut files = Vec::new();
    for source in selected {
        files.extend(collect(source, &dirs)?);
    }
    if dry_run {
        for file in &files {
            for note in &file.notes {
                println!(
                    "Would import {} ({}) from {}",
                    note.key.as_deref().unwrap_or_default(),
                    if note.scope == Scope::Global {
                        "global"
                    } else {
                        "project"
                    },
                    file.path.display()
                );
            }
        }
        println!(
            "Dry run: {} notes; no database changes",
            files.iter().map(|f| f.notes.len()).sum::<usize>()
        );
        return Ok(());
    }
    let mut conn = crate::db::open_default()?;
    let project_id = agents::ensure_project(&conn, &project)?;
    let user = agents::human(&conn, project_id)?;
    let mut counts = ImportCounts::default();
    for file in files {
        save_file(&mut conn, project_id, &user, file, &mut counts)?;
    }
    println!(
        "Imported memory: {} new, {} updated, {} unchanged",
        counts.created, counts.updated, counts.unchanged
    );
    Ok(())
}

struct MemoryFile {
    path: PathBuf,
    notes: Vec<NoteInput>,
    // Non-Claude files are split into parts. This identifies obsolete trailing parts.
    part_prefix: Option<String>,
    scope: Scope,
}

fn config_dir(env: &str, default: &str) -> Option<PathBuf> {
    std::env::var_os(env)
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .or_else(|| dirs::home_dir().map(|p| p.join(default)))
}

fn collect(source: Source, dirs: &[PathBuf]) -> Result<Vec<MemoryFile>> {
    let mut files = Vec::new();
    match source {
        Source::Claude => {
            if let Some(projects) = claude_projects_dir() {
                for dir in dirs {
                    for file in markdown_files(
                        &projects.join(claude_project_dir_name(dir)).join("memory"),
                        false,
                    )? {
                        if file.file_name().is_some_and(|n| n == "MEMORY.md") {
                            continue;
                        }
                        let text = read_memory(&file)?;
                        if let Some(note) = claude_memory_note(&file, &text) {
                            files.push(MemoryFile {
                                path: file,
                                notes: vec![note],
                                part_prefix: None,
                                scope: Scope::Project,
                            });
                        }
                    }
                }
            }
        }
        Source::Codex => {
            if let Some(home) = config_dir("CODEX_HOME", ".codex") {
                let root = home.join("memories");
                // Markdown is the supported export surface. Do not read Codex's
                // internal SQLite databases, transcripts, or executable skills.
                for file in markdown_files(&root, true)? {
                    let relative = file.strip_prefix(&root)?;
                    if relative
                        .components()
                        .any(|c| c.as_os_str() == "skills" || c.as_os_str() == "raw_memories")
                        || relative == Path::new("raw_memories.md")
                    {
                        continue;
                    }
                    let text = read_memory(&file)?;
                    // Rollout evidence can identify its project explicitly. Skip
                    // evidence for other workspaces; consolidated files are global.
                    let scope = if let Some(cwd) =
                        text.lines().take(20).find_map(|l| l.strip_prefix("cwd: "))
                    {
                        let cwd = crate::project::canonical(Path::new(cwd.trim()));
                        if !dirs
                            .iter()
                            .any(|dir| cwd.starts_with(crate::project::canonical(dir)))
                        {
                            continue;
                        }
                        Scope::Project
                    } else {
                        Scope::Global
                    };
                    files.push(markdown_note(
                        source,
                        &file,
                        &relative.to_string_lossy(),
                        &text,
                        scope,
                    ));
                }
            }
        }
        Source::Gemini => {
            // GEMINI_CLI_HOME replaces the user's home; Gemini appends .gemini.
            let home = std::env::var_os("GEMINI_CLI_HOME")
                .filter(|v| !v.is_empty())
                .map(PathBuf::from)
                .or_else(dirs::home_dir);
            if let Some(home) = home {
                let file = home.join(".gemini/GEMINI.md");
                if file.try_exists()? {
                    let text = read_memory(&file)?;
                    files.push(markdown_note(
                        source,
                        &file,
                        "added-memories",
                        &gemini_memories(&text),
                        Scope::Global,
                    ));
                }
            }
        }
    }
    Ok(files)
}

fn markdown_files(dir: &Path, recursive: bool) -> Result<Vec<PathBuf>> {
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e).with_context(|| format!("reading {}", dir.display())),
    };
    let mut files = Vec::new();
    for entry in entries {
        let entry = entry?;
        let ty = entry.file_type()?;
        if ty.is_file() && entry.path().extension().is_some_and(|x| x == "md") {
            files.push(entry.path());
        } else if recursive
            && ty.is_dir()
            && entry.file_name() != "skills"
            && entry.file_name() != "raw_memories"
        {
            files.extend(markdown_files(&entry.path(), true)?);
        }
    }
    files.sort();
    Ok(files)
}

fn read_memory(path: &Path) -> Result<String> {
    // Bound accidental large-file imports without silently dropping content.
    if std::fs::metadata(path)?.len() > 16 * 1024 * 1024 {
        bail!("memory file {} exceeds 16 MiB", path.display());
    }
    std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))
}

fn gemini_memories(text: &str) -> String {
    let mut lines = Vec::new();
    let mut inside = false;
    let mut fence = None;
    for line in text.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with("```") || trimmed.starts_with("~~~") {
            let marker = &trimmed[..3];
            if fence == Some(marker) {
                fence = None;
            } else if fence.is_none() {
                fence = Some(marker);
            }
        }
        if fence.is_none() && trimmed == "## Gemini Added Memories" {
            inside = true;
            continue;
        }
        if fence.is_none() && (trimmed.starts_with("## ") || trimmed.starts_with("# ")) {
            inside = false;
        }
        if inside {
            lines.push(line);
        }
    }
    lines.join("\n")
}

fn markdown_note(
    source: Source,
    file: &Path,
    relative: &str,
    text: &str,
    scope: Scope,
) -> MemoryFile {
    // Stable FNV-1a distinguishes paths whose slugs collide; never hash contents
    // into the key, since a source edit must update the existing note.
    let hash = relative.bytes().fold(0xcbf29ce484222325_u64, |h, b| {
        (h ^ u64::from(b)).wrapping_mul(0x100000001b3)
    });
    let base = format!(
        "imported/{}/{}-{hash:016x}",
        source.id(),
        memory::slug(relative).chars().take(50).collect::<String>()
    );
    let prefix = format!("{base}/part-");
    let chars: Vec<char> = text.trim().chars().collect();
    let title = text
        .lines()
        .find(|l| !l.trim().is_empty())
        .unwrap_or(relative)
        .trim_start_matches('#')
        .trim();
    let provenance = format!(
        "\n\n_Imported from {} memory (`{}`)._",
        source.id(),
        format::truncate(relative, 160)
    );
    let notes = chars
        .chunks(memory::MAX_BODY_CHARS - 300)
        .enumerate()
        .map(|(i, chunk)| NoteInput {
            key: Some(format!("{prefix}{}", i + 1)),
            title: format!("{} (part {})", format::truncate(title, 170), i + 1),
            body: format!(
                "Imported memory (part {}):\n\n{}{}",
                i + 1,
                chunk.iter().collect::<String>(),
                provenance
            ),
            tags: vec!["imported".into(), format!("{}-memory", source.id())],
            scope,
            overwrite: true,
            ..Default::default()
        })
        .collect();
    MemoryFile {
        path: file.to_path_buf(),
        notes,
        part_prefix: Some(prefix),
        scope,
    }
}

fn save_file(
    conn: &mut Connection,
    project_id: i64,
    author: &Agent,
    file: MemoryFile,
    counts: &mut ImportCounts,
) -> Result<()> {
    let keys: Vec<String> = file.notes.iter().filter_map(|n| n.key.clone()).collect();
    // Each note is versioned in a short transaction; cleanup holds its own
    // immediate lock so it cannot race a writer while deciding what to delete.
    for note in file.notes {
        counts.seen += 1;
        match memory::remember(conn, project_id, author, note)? {
            Saved::Created(_) => counts.created += 1,
            Saved::Updated(_) => counts.updated += 1,
            Saved::Unchanged(_) => counts.unchanged += 1,
        }
    }
    if let Some(prefix) = file.part_prefix {
        let scope_id = (file.scope == Scope::Project).then_some(project_id);
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let old: Vec<(i64, String)> = {
            let mut stmt = tx.prepare("SELECT id, key FROM notes WHERE project_id IS ?1 AND substr(key, 1, length(?2)) = ?2 AND deleted_at IS NULL")?;
            stmt.query_map(params![scope_id, prefix], |r| Ok((r.get(0)?, r.get(1)?)))?
                .collect::<rusqlite::Result<_>>()?
        };
        for (id, key) in old {
            if !keys.contains(&key) {
                tx.execute(
                    "UPDATE notes SET deleted_at = ?2 WHERE id = ?1",
                    params![id, crate::db::now_ms()],
                )?;
            }
        }
        tx.commit()?;
    }
    Ok(())
}

#[derive(Debug, Default, PartialEq, Eq)]
pub struct ImportCounts {
    pub seen: usize,
    pub created: usize,
    pub updated: usize,
    pub unchanged: usize,
}

fn import_lock() -> Result<std::fs::File> {
    let home = crate::paths::home()?;
    let mut builder = std::fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    builder.create(&home)?;
    let lock = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(home.join("import.lock"))?;
    lock.lock()?;
    Ok(lock)
}

/// `~/.claude/projects` (or under `CLAUDE_CONFIG_DIR`).
fn claude_projects_dir() -> Option<PathBuf> {
    match std::env::var_os("CLAUDE_CONFIG_DIR") {
        Some(dir) => Some(PathBuf::from(dir).join("projects")),
        None => dirs::home_dir().map(|h| h.join(".claude").join("projects")),
    }
}

/// Claude Code names a project's data directory after its path, with every
/// character that isn't an ASCII letter or digit replaced by '-'.
pub fn claude_project_dir_name(path: &Path) -> String {
    path.to_string_lossy()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect()
}

/// Imports Claude Code's memory files for `dirs` as shared project notes under
/// `imported/claude/<name>`. The file is the source of truth for these notes, so
/// changed files update them in place.
pub fn import_claude_memory(
    conn: &mut Connection,
    project_id: i64,
    author: &Agent,
    dirs: &[PathBuf],
) -> Result<ImportCounts> {
    let _lock = import_lock()?;
    let mut counts = ImportCounts::default();
    let Some(projects) = claude_projects_dir() else {
        return Ok(counts);
    };
    for dir in dirs {
        let memory_dir = projects.join(claude_project_dir_name(dir)).join("memory");
        let Ok(entries) = std::fs::read_dir(&memory_dir) else {
            continue;
        };
        let mut files: Vec<PathBuf> = entries
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.extension().is_some_and(|x| x == "md"))
            .filter(|p| p.file_name().is_some_and(|n| n != "MEMORY.md"))
            .collect();
        files.sort();
        for file in files {
            let text = std::fs::read_to_string(&file)
                .with_context(|| format!("reading {}", file.display()))?;
            let Some(input) = claude_memory_note(&file, &text) else {
                continue;
            };
            counts.seen += 1;
            match memory::remember(conn, project_id, author, input)? {
                Saved::Created(_) => counts.created += 1,
                Saved::Updated(_) => counts.updated += 1,
                Saved::Unchanged(_) => counts.unchanged += 1,
            }
        }
    }
    Ok(counts)
}

/// One Claude Code memory file (frontmatter with name / description /
/// metadata.type, then Markdown) as a note.
fn claude_memory_note(file: &Path, text: &str) -> Option<NoteInput> {
    let stem = file.file_stem()?.to_string_lossy().into_owned();
    let (front, body) = split_frontmatter(text);
    let field = |k: &str| {
        front.lines().find_map(|l| {
            let (key, value) = l.trim().split_once(':')?;
            (key.trim() == k).then(|| value.trim().trim_matches(['"', '\'']).to_string())
        })
    };
    let name = field("name").filter(|n| !n.is_empty()).unwrap_or(stem);
    let kind = match field("type").as_deref() {
        Some("feedback") => "gotcha",
        Some("project") | Some("reference") => "fact",
        _ => "note",
    };
    let body = body.trim();
    if body.is_empty() {
        return None;
    }
    let title = field("description")
        .filter(|d| !d.is_empty())
        .unwrap_or_else(|| name.clone());
    let source = file.file_name()?.to_string_lossy().into_owned();
    let mut body = format::truncate(body, memory::MAX_BODY_CHARS - 200);
    body.push_str(&format!(
        "\n\n_Imported from Claude Code memory (`{source}`)._"
    ));
    let mut tags = vec!["imported".to_string(), "claude-memory".to_string()];
    if let Some(t) = field("type").filter(|t| t.chars().all(|c| c.is_ascii_lowercase())) {
        tags.push(t);
    }
    Some(NoteInput {
        key: Some(format!("imported/claude/{}", memory::slug(&name))),
        kind: Some(kind.to_string()),
        title: format::truncate(title.lines().next().unwrap_or_default(), 200),
        body,
        tags,
        scope: Scope::Project,
        expected_revision: None,
        supersedes: None,
        overwrite: true,
    })
}

fn split_frontmatter(text: &str) -> (&str, &str) {
    let Some(rest) = text
        .strip_prefix("---\n")
        .or_else(|| text.strip_prefix("---\r\n"))
    else {
        return ("", text);
    };
    match rest.find("\n---") {
        Some(end) => {
            let after = &rest[end + 4..];
            (&rest[..end], after.split_once('\n').map_or("", |(_, b)| b))
        }
        None => ("", text),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generated_keys_handle_long_and_colliding_paths() {
        let long = format!("rollout_summaries/{}.md", "a".repeat(120));
        let file = markdown_note(
            Source::Codex,
            Path::new(&long),
            &long,
            "A memory",
            Scope::Global,
        );
        memory::normalize_key(file.notes[0].key.as_deref().unwrap()).unwrap();
        let a = markdown_note(
            Source::Codex,
            Path::new("a_b.md"),
            "a_b.md",
            "A memory",
            Scope::Global,
        );
        let b = markdown_note(
            Source::Codex,
            Path::new("a-b.md"),
            "a-b.md",
            "A memory",
            Scope::Global,
        );
        assert_ne!(a.notes[0].key, b.notes[0].key);
    }

    #[test]
    fn gemini_only_imports_saved_memory_outside_example_fences() {
        let text = "```md\n## Gemini Added Memories\nExample, not memory\n```\n## Gemini Added Memories\n- Real memory\n```md\n## A heading inside memory\n```\n## Instructions\nNot memory";
        assert_eq!(
            gemini_memories(text),
            "- Real memory\n```md\n## A heading inside memory\n```"
        );
        assert!(gemini_memories("# Instructions only\nNo saved memories").is_empty());
    }
    #[test]
    fn claude_dir_names_match_claude_codes_encoding() {
        assert_eq!(
            claude_project_dir_name(Path::new("/Users/yoab/Desktop/Projects/chitchat")),
            "-Users-yoab-Desktop-Projects-chitchat"
        );
        assert_eq!(
            claude_project_dir_name(Path::new("/tmp/claude-501/-Users-x/my.repo")),
            "-tmp-claude-501--Users-x-my-repo"
        );
    }

    #[test]
    fn memory_files_become_notes() {
        let text = "---\nname: no-claude-coauthor\ndescription: Never add a Claude co-author trailer\nmetadata:\n  type: feedback\n---\n\nDon't add the trailer.\n";
        let note = claude_memory_note(Path::new("/m/no-claude-coauthor.md"), text).unwrap();
        assert_eq!(
            note.key.as_deref(),
            Some("imported/claude/no-claude-coauthor")
        );
        assert_eq!(note.kind.as_deref(), Some("gotcha"));
        assert_eq!(note.title, "Never add a Claude co-author trailer");
        assert!(note.body.starts_with("Don't add the trailer."));
        assert_eq!(note.tags, ["imported", "claude-memory", "feedback"]);
        assert!(note.overwrite);

        let plain = claude_memory_note(Path::new("/m/Plain Note.md"), "just text").unwrap();
        assert_eq!(plain.key.as_deref(), Some("imported/claude/plain-note"));
        assert_eq!(plain.kind.as_deref(), Some("note"));
        assert!(claude_memory_note(Path::new("/m/empty.md"), "---\nname: x\n---\n").is_none());
    }
}
