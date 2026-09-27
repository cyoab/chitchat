//! Per-directory client configuration written by `chitchat init`.
//!
//! Everything is local to this machine and kept out of git (via
//! `.git/info/exclude`), since it points at this machine's chitchat binary:
//! - Claude Code: the MCP server at *local* scope (`claude mcp add-json --scope
//!   local`, stored in ~/.claude.json for this directory only) and hooks in
//!   `.claude/settings.local.json`.
//! - Codex: a managed `[mcp_servers.chitchat]` block in `.codex/config.toml` and
//!   hooks in `.codex/hooks.json` (Codex loads both once the project is trusted).
//!
//! Hook files are merged, keeping everything else; chitchat's entries are
//! recognized by their command line, so re-running replaces rather than duplicates.

use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail};
use serde_json::{Map, Value, json};

use crate::hook::HookEvent;
use crate::session::Client;

pub const SERVER_NAME: &str = "chitchat";
/// Seconds. The hook itself is fast; this only bounds a stuck database lock.
const HOOK_TIMEOUT: u64 = 15;

const BLOCK_START: &str =
    "# >>> chitchat: managed by `chitchat init`; edits inside this block are overwritten";
const BLOCK_END: &str = "# <<< chitchat";

pub fn name(client: Client) -> &'static str {
    match client {
        Client::Claude => "Claude Code",
        Client::Codex => "Codex",
    }
}

fn client_arg(client: Client) -> &'static str {
    match client {
        Client::Claude => "claude",
        Client::Codex => "codex",
    }
}

/// Whether the client's CLI is installed (on PATH).
pub fn available(client: Client) -> bool {
    on_path(client_arg(client))
}

pub fn binary_path() -> Result<PathBuf> {
    let exe = std::env::current_exe().context("locating the chitchat binary")?;
    Ok(exe.canonicalize().unwrap_or(exe))
}

fn hooks_file(target: &Path, client: Client) -> PathBuf {
    match client {
        Client::Claude => target.join(".claude").join("settings.local.json"),
        Client::Codex => target.join(".codex").join("hooks.json"),
    }
}

fn codex_config(target: &Path) -> PathBuf {
    target.join(".codex").join("config.toml")
}

/// Files `configure` may create inside `target`, relative to it.
pub fn local_files(client: Client) -> &'static [&'static str] {
    match client {
        Client::Claude => &[".claude/settings.local.json"],
        Client::Codex => &[".codex/hooks.json", ".codex/config.toml"],
    }
}

/// Env vars worth passing to the MCP server. Codex starts MCP servers with a
/// scrubbed environment, so these must be set explicitly there.
fn passthrough_env() -> Vec<(String, String)> {
    ["CHITCHAT_HOME", "CHITCHAT_LOG"]
        .iter()
        .filter_map(|k| std::env::var(k).ok().map(|v| (k.to_string(), v)))
        .collect()
}

/// Sets `client` up to use chitchat in `target`. Returns a line per change made.
pub fn configure(
    target: &Path,
    client: Client,
    bin: &Path,
    stop_hook: bool,
) -> Result<Vec<String>> {
    let mut done = Vec::new();
    let hooks_path = hooks_file(target, client);
    let before = read_json(&hooks_path)?;
    let after = with_hooks(before.clone(), client, bin, stop_hook);
    if write_json(&hooks_path, &before, &after)? {
        done.push(format!("hooks → {}", hooks_path.display()));
    }
    match client {
        Client::Claude => {
            claude_mcp(target, Some(bin))?;
            done.push("MCP server → Claude Code local scope for this directory".to_string());
        }
        Client::Codex => {
            let path = codex_config(target);
            let before = std::fs::read_to_string(&path).unwrap_or_default();
            let after = with_codex_block(&before, &codex_block(bin))?;
            if after != before {
                write_file(&path, &after)?;
                done.push(format!("MCP server → {}", path.display()));
            }
        }
    }
    Ok(done)
}

/// Removes chitchat's configuration for `client` from `target`.
pub fn unconfigure(target: &Path, client: Client) -> Result<Vec<String>> {
    let mut done = Vec::new();
    let hooks_path = hooks_file(target, client);
    if hooks_path.exists() {
        let before = read_json(&hooks_path)?;
        let after = without_hooks(before.clone(), client);
        if write_json(&hooks_path, &before, &after)? {
            done.push(format!("removed hooks from {}", hooks_path.display()));
        }
    }
    match client {
        Client::Claude => {
            if claude_mcp_registered(target) {
                claude_mcp(target, None)?;
                done.push("removed the local-scope MCP server".to_string());
            }
        }
        Client::Codex => {
            let path = codex_config(target);
            if let Ok(before) = std::fs::read_to_string(&path) {
                let after = without_codex_block(&before);
                if after != before {
                    write_file(&path, &after)?;
                    done.push(format!("removed the MCP server from {}", path.display()));
                }
            }
        }
    }
    Ok(done)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Status {
    pub mcp: bool,
    pub hooks: usize,
}

pub fn status(target: &Path, client: Client) -> Status {
    let hooks = read_json(&hooks_file(target, client))
        .ok()
        .map_or(0, |v| count_hooks(&v, client));
    let mcp = match client {
        Client::Claude => claude_mcp_registered(target),
        Client::Codex => std::fs::read_to_string(codex_config(target))
            .is_ok_and(|t| t.lines().any(|l| l.trim() == BLOCK_START)),
    };
    Status { mcp, hooks }
}

// ---- Claude Code: local-scope MCP server -------------------------------------

/// Adds (with `bin`) or removes (without) the local-scope server for `target`.
fn claude_mcp(target: &Path, bin: Option<&Path>) -> Result<()> {
    if !available(Client::Claude) {
        bail!("`claude` is not on PATH");
    }
    let run = |args: &[String]| {
        Command::new("claude")
            .args(args)
            .current_dir(target)
            .output()
            .context("running claude")
    };
    let remove = strings(&["mcp", "remove", "--scope", "local", SERVER_NAME]);
    let _ = run(&remove); // replace any previous entry (e.g. an old binary path)
    let Some(bin) = bin else {
        return Ok(());
    };
    let mut spec = json!({
        "type": "stdio",
        "command": bin.to_string_lossy(),
        "args": ["mcp", "--client", "claude"],
    });
    let env = passthrough_env();
    if !env.is_empty() {
        spec["env"] = Value::Object(env.into_iter().map(|(k, v)| (k, json!(v))).collect());
    }
    let add = strings(&[
        "mcp",
        "add-json",
        "--scope",
        "local",
        SERVER_NAME,
        &spec.to_string(),
    ]);
    let out = run(&add)?;
    if !out.status.success() {
        bail!(
            "`claude mcp add-json` failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(())
}

/// Claude Code keeps local-scope servers in ~/.claude.json (inside
/// CLAUDE_CONFIG_DIR if set) under `projects["<dir>"].mcpServers`.
fn claude_mcp_registered(target: &Path) -> bool {
    let path = match std::env::var_os("CLAUDE_CONFIG_DIR") {
        Some(dir) => PathBuf::from(dir).join(".claude.json"),
        None => dirs::home_dir().unwrap_or_default().join(".claude.json"),
    };
    read_json(&path).ok().is_some_and(|v| {
        v["projects"][target.to_string_lossy().as_ref()]["mcpServers"]
            .get(SERVER_NAME)
            .is_some()
    })
}

// ---- Codex: managed block in .codex/config.toml ------------------------------

fn codex_block(bin: &Path) -> String {
    // TOML basic strings accept JSON string escapes.
    let q = |s: &str| serde_json::to_string(s).unwrap_or_default();
    let mut block = format!(
        "{BLOCK_START}\n[mcp_servers.{SERVER_NAME}]\ncommand = {}\nargs = [\"mcp\", \"--client\", \"codex\"]\n",
        q(&bin.to_string_lossy())
    );
    let env = passthrough_env();
    if !env.is_empty() {
        let pairs: Vec<String> = env.iter().map(|(k, v)| format!("{k} = {}", q(v))).collect();
        block.push_str(&format!("env = {{ {} }}\n", pairs.join(", ")));
    }
    block.push_str(BLOCK_END);
    block.push('\n');
    block
}

/// Replaces chitchat's block in a config.toml, or appends it.
pub fn with_codex_block(existing: &str, block: &str) -> Result<String> {
    let stripped = without_codex_block(existing);
    if stripped
        .lines()
        .any(|l| l.trim() == format!("[mcp_servers.{SERVER_NAME}]"))
    {
        bail!(
            "config.toml already defines [mcp_servers.{SERVER_NAME}] outside chitchat's managed \
             block; remove it and run chitchat init again"
        );
    }
    let mut out = stripped.trim_end().to_string();
    if !out.is_empty() {
        out.push_str("\n\n");
    }
    out.push_str(block);
    Ok(out)
}

pub fn without_codex_block(existing: &str) -> String {
    let mut out = Vec::new();
    let mut inside = false;
    for line in existing.lines() {
        match line.trim() {
            l if l == BLOCK_START => inside = true,
            l if l == BLOCK_END && inside => inside = false,
            _ if inside => {}
            _ => out.push(line),
        }
    }
    let mut text = out.join("\n").trim_end().to_string();
    if !text.is_empty() {
        text.push('\n');
    }
    text
}

// ---- hooks ---------------------------------------------------------------------

/// Quotes for `sh -c` / `zsh -c` when needed.
fn shell_quote(s: &str) -> String {
    let plain = !s.is_empty()
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || "/._-+=:@,".contains(c));
    if plain {
        s.to_string()
    } else {
        format!("'{}'", s.replace('\'', r"'\''"))
    }
}

fn hook_command(bin: &Path, event: HookEvent, client: Client) -> String {
    format!(
        "{} hook {} --client {}",
        shell_quote(&bin.to_string_lossy()),
        event.arg(),
        client_arg(client)
    )
}

/// Whether a hook handler was written by chitchat for this client.
fn is_ours(handler: &Value, client: Client) -> bool {
    handler
        .get("command")
        .and_then(Value::as_str)
        .is_some_and(|c| {
            c.contains("chitchat")
                && c.contains(" hook ")
                && c.ends_with(&format!("--client {}", client_arg(client)))
        })
}

fn count_hooks(settings: &Value, client: Client) -> usize {
    HookEvent::ALL
        .iter()
        .filter(|e| {
            settings["hooks"][event_key(**e)]
                .as_array()
                .into_iter()
                .flatten()
                .flat_map(|g| g["hooks"].as_array().cloned().unwrap_or_default())
                .any(|h| is_ours(&h, client))
        })
        .count()
}

/// Returns `settings` with chitchat's hook entries (re)added. Claude Code and Codex
/// share this shape: `{"hooks": {"<Event>": [{"matcher"?, "hooks": [handler]}]}}`.
pub fn with_hooks(settings: Value, client: Client, bin: &Path, stop_hook: bool) -> Value {
    let mut settings = without_hooks(settings, client);
    let root = ensure_object(&mut settings);
    let hooks = ensure_object(root.entry("hooks").or_insert_with(|| json!({})));
    for event in HookEvent::ALL {
        if event == HookEvent::Stop && !stop_hook {
            continue;
        }
        let handler = json!({
            "type": "command",
            "command": hook_command(bin, event, client),
            "timeout": HOOK_TIMEOUT,
        });
        let mut group = Map::new();
        // PostToolUse runs after every tool, so urgent messages arrive mid-turn.
        if event == HookEvent::PostToolUse {
            group.insert("matcher".into(), json!("*"));
        }
        group.insert("hooks".into(), json!([handler]));
        let list = hooks.entry(event_key(event)).or_insert_with(|| json!([]));
        if let Some(list) = list.as_array_mut() {
            list.push(Value::Object(group));
        }
    }
    settings
}

/// Returns `settings` with every chitchat hook entry for `client` removed, dropping
/// groups and event lists that become empty.
pub fn without_hooks(mut settings: Value, client: Client) -> Value {
    let Some(hooks) = settings.get_mut("hooks").and_then(Value::as_object_mut) else {
        return settings;
    };
    for groups in hooks.values_mut() {
        let Some(groups) = groups.as_array_mut() else {
            continue;
        };
        for group in groups.iter_mut() {
            if let Some(handlers) = group.get_mut("hooks").and_then(Value::as_array_mut) {
                handlers.retain(|h| !is_ours(h, client));
            }
        }
        groups.retain(|g| {
            g.get("hooks")
                .and_then(Value::as_array)
                .is_none_or(|h| !h.is_empty())
        });
    }
    hooks.retain(|_, groups| groups.as_array().is_none_or(|g| !g.is_empty()));
    if hooks.is_empty()
        && let Some(root) = settings.as_object_mut()
    {
        root.remove("hooks");
    }
    settings
}

fn event_key(event: HookEvent) -> String {
    match event {
        HookEvent::SessionStart => "SessionStart",
        HookEvent::UserPromptSubmit => "UserPromptSubmit",
        HookEvent::PostToolUse => "PostToolUse",
        HookEvent::Stop => "Stop",
    }
    .to_string()
}

fn ensure_object(value: &mut Value) -> &mut Map<String, Value> {
    if !value.is_object() {
        *value = json!({});
    }
    value.as_object_mut().expect("just made an object")
}

// ---- files and git -------------------------------------------------------------

fn read_json(path: &Path) -> Result<Value> {
    match std::fs::read_to_string(path) {
        Ok(text) if text.trim().is_empty() => Ok(json!({})),
        Ok(text) => serde_json::from_str(&text)
            .with_context(|| format!("{} is not valid JSON; fix it and retry", path.display())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(json!({})),
        Err(e) => Err(e).with_context(|| format!("reading {}", path.display())),
    }
}

/// Writes `after` if it differs from `before`; a file left as `{}` is removed.
/// Returns whether anything changed.
fn write_json(path: &Path, before: &Value, after: &Value) -> Result<bool> {
    if before == after {
        return Ok(false);
    }
    let text = if after.as_object().is_some_and(Map::is_empty) {
        String::new()
    } else {
        let mut text = serde_json::to_string_pretty(after)?;
        text.push('\n');
        text
    };
    write_file(path, &text)?;
    Ok(true)
}

/// Replaces (or, for empty `text`, removes) a config file. The previous version is
/// saved under ~/.chitchat/backups/config/ rather than next to it, so nothing new
/// appears in the user's repository.
fn write_file(path: &Path, text: &str) -> Result<()> {
    if path.exists() {
        let dir = crate::paths::home()?.join("backups").join("config");
        std::fs::create_dir_all(&dir)?;
        let name = crate::workspace::claude_project_dir_name(path);
        std::fs::copy(path, dir.join(format!("{name}.bak")))
            .with_context(|| format!("backing up {}", path.display()))?;
    }
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    if text.is_empty() {
        let _ = std::fs::remove_file(path);
        return Ok(());
    }
    let mut tmp = path.as_os_str().to_owned();
    tmp.push(".chitchat-tmp");
    std::fs::write(&tmp, text)?;
    std::fs::rename(&tmp, path).with_context(|| format!("writing {}", path.display()))
}

/// Adds untracked chitchat files under `target` to the repo's `.git/info/exclude`
/// (local to this clone, unlike .gitignore). Returns the patterns added.
pub fn exclude_from_git(target: &Path, files: &[&str]) -> Result<Vec<String>> {
    let Some((top, common)) = crate::project::git_dirs(target) else {
        return Ok(Vec::new());
    };
    let rel = target.strip_prefix(&top).unwrap_or(Path::new(""));
    let exclude = common.join("info").join("exclude");
    let current = std::fs::read_to_string(&exclude).unwrap_or_default();
    let mut added = Vec::new();
    for file in files {
        let rel_file = rel.join(file);
        let rel_file = rel_file.to_string_lossy().replace('\\', "/");
        let tracked = Command::new("git")
            .arg("-C")
            .arg(&top)
            .args(["ls-files", "--error-unmatch", "--", &rel_file])
            .output()
            .is_ok_and(|o| o.status.success());
        let pattern = format!("/{rel_file}");
        if !tracked && !current.lines().any(|l| l.trim() == pattern) && !added.contains(&pattern) {
            added.push(pattern);
        }
    }
    if added.is_empty() {
        return Ok(added);
    }
    let mut text = current.trim_end().to_string();
    if !text.is_empty() {
        text.push('\n');
    }
    if !text.contains("# chitchat") {
        text.push_str("# chitchat: local agent config (see `chitchat init`)\n");
    }
    for pattern in &added {
        text.push_str(pattern);
        text.push('\n');
    }
    std::fs::create_dir_all(exclude.parent().expect("info dir"))?;
    std::fs::write(&exclude, text)?;
    Ok(added)
}

fn strings(items: &[&str]) -> Vec<String> {
    items.iter().map(|s| s.to_string()).collect()
}

fn on_path(program: &str) -> bool {
    std::env::var_os("PATH")
        .is_some_and(|paths| std::env::split_paths(&paths).any(|dir| dir.join(program).is_file()))
}

#[cfg(test)]
mod tests {
    use super::*;

    const BIN: &str = "/Users/me/My Tools/chitchat";

    #[test]
    fn merging_preserves_other_settings_and_is_idempotent() {
        let existing = json!({
            "model": "opus",
            "hooks": {
                "Stop": [{"hooks": [{"type": "command", "command": "say done"}]}],
                "PostToolUse": [{"matcher": "Bash", "hooks": [{"type": "command", "command": "lint"}]}]
            }
        });
        let once = with_hooks(existing.clone(), Client::Claude, Path::new(BIN), true);
        let twice = with_hooks(once.clone(), Client::Claude, Path::new(BIN), true);
        assert_eq!(once, twice);
        assert_eq!(once["model"], "opus");
        assert_eq!(once["hooks"]["Stop"].as_array().unwrap().len(), 2);
        assert_eq!(
            once["hooks"]["UserPromptSubmit"][0]["hooks"][0]["command"],
            "'/Users/me/My Tools/chitchat' hook user-prompt-submit --client claude"
        );
        assert_eq!(once["hooks"]["PostToolUse"][1]["matcher"], "*");
        assert_eq!(count_hooks(&once, Client::Claude), 4);
        // Keys keep their original order (serde_json preserve_order).
        let keys: Vec<&String> = once.as_object().unwrap().keys().collect();
        assert_eq!(keys, ["model", "hooks"]);

        assert_eq!(without_hooks(once, Client::Claude), existing);
    }

    #[test]
    fn clients_do_not_touch_each_others_entries() {
        let both = with_hooks(
            with_hooks(json!({}), Client::Claude, Path::new(BIN), true),
            Client::Codex,
            Path::new(BIN),
            false,
        );
        let codex_only = without_hooks(both, Client::Claude);
        let text = codex_only.to_string();
        assert!(text.contains("--client codex") && !text.contains("--client claude"));
        assert!(codex_only["hooks"].get("Stop").is_none());
        assert_eq!(without_hooks(codex_only, Client::Codex), json!({}));
    }

    #[test]
    fn codex_block_replaces_and_removes_cleanly() {
        let user = "model = \"gpt-6\"\n\n[mcp_servers.other]\ncommand = \"other\"\n";
        let block = codex_block(Path::new(BIN));
        let once = with_codex_block(user, &block).unwrap();
        let twice = with_codex_block(&once, &block).unwrap();
        assert_eq!(once, twice);
        assert!(once.contains("command = \"/Users/me/My Tools/chitchat\""));
        assert_eq!(without_codex_block(&once), user);
        assert_eq!(without_codex_block(&block), "");

        let clash = "[mcp_servers.chitchat]\ncommand = \"x\"\n";
        assert!(with_codex_block(clash, &block).is_err());
    }

    #[test]
    fn quoting() {
        assert_eq!(
            shell_quote("/usr/local/bin/chitchat"),
            "/usr/local/bin/chitchat"
        );
        assert_eq!(shell_quote("/a b/it's"), r"'/a b/it'\''s'");
    }
}
