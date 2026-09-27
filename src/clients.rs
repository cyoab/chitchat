//! Per-directory harness configuration written by `chitchat init`, driven by the
//! table in [`crate::harness`].
//!
//! Everything is local to this machine and kept out of git (via
//! `.git/info/exclude`), since it points at this machine's chitchat binary. Hook
//! and MCP files are merged, keeping everything else; chitchat's entries are
//! recognized by their command line, so re-running replaces rather than
//! duplicates them. Previous versions of edited files are saved under
//! `~/.chitchat/backups/config/`.

use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail};
use serde_json::{Map, Value, json};

use crate::harness::{Entry, Harness, Hooks, Layout, Mcp, Timeout};
use crate::hook::HookEvent;

pub const SERVER_NAME: &str = "chitchat";
/// Seconds. The hook itself is fast; this only bounds a stuck database lock.
const HOOK_TIMEOUT: u64 = 15;

const BLOCK_START: &str =
    "# >>> chitchat: managed by `chitchat init`; edits inside this block are overwritten";
const BLOCK_END: &str = "# <<< chitchat";

pub fn binary_path() -> Result<PathBuf> {
    let exe = std::env::current_exe().context("locating the chitchat binary")?;
    Ok(exe.canonicalize().unwrap_or(exe))
}

/// The chitchat skill (Agent Skills format), embedded in the binary.
pub const SKILL: &str = include_str!("../skills/chitchat/SKILL.md");
/// Present in the installed SKILL.md; a file without it is the user's own.
const SKILL_MARKER: &str = "<!-- Installed by `chitchat init`";

/// Files `configure` may create inside the workspace directory, relative to it.
pub fn local_files(h: &Harness) -> Vec<String> {
    let mut files = Vec::new();
    if let Some(hooks) = &h.hooks {
        files.push(hooks.file.to_string());
    }
    match &h.mcp {
        Mcp::TomlBlock { file } | Mcp::Json { file, .. } => files.push(file.to_string()),
        Mcp::ClaudeLocal | Mcp::Manual { .. } | Mcp::None => {}
    }
    if let Some(dir) = h.skills_dir {
        files.push(format!("{dir}/chitchat/"));
    }
    files.dedup();
    files
}

fn skill_path(target: &Path, h: &Harness) -> Option<PathBuf> {
    h.skills_dir
        .map(|dir| target.join(dir).join("chitchat").join("SKILL.md"))
}

/// Installs or refreshes the chitchat skill. Returns whether the file changed.
fn install_skill(path: &Path) -> Result<bool> {
    match std::fs::read_to_string(path) {
        Ok(current) if current == SKILL => Ok(false),
        Ok(current) if !current.contains(SKILL_MARKER) => bail!(
            "{} exists and wasn't installed by chitchat; leaving it alone",
            path.display()
        ),
        _ => {
            write_file(path, SKILL)?;
            Ok(true)
        }
    }
}

/// Removes the chitchat skill if chitchat installed it. Returns whether it did.
fn remove_skill(path: &Path) -> Result<bool> {
    match std::fs::read_to_string(path) {
        Ok(current) if current.contains(SKILL_MARKER) => {
            std::fs::remove_file(path)?;
            if let Some(dir) = path.parent() {
                let _ = std::fs::remove_dir(dir); // only if now empty
                if let Some(skills) = dir.parent() {
                    let _ = std::fs::remove_dir(skills);
                }
            }
            Ok(true)
        }
        _ => Ok(false),
    }
}

/// Env vars worth passing to the MCP server. Some harnesses (Codex) start MCP
/// servers with a scrubbed environment, so these must be set explicitly.
fn passthrough_env() -> Vec<(String, String)> {
    ["CHITCHAT_HOME", "CHITCHAT_LOG"]
        .iter()
        .filter_map(|k| std::env::var(k).ok().map(|v| (k.to_string(), v)))
        .collect()
}

/// The command line an MCP config launches.
fn mcp_args(h: &Harness) -> [String; 3] {
    ["mcp".to_string(), "--client".to_string(), h.id.to_string()]
}

/// Sets `h` up to use chitchat in `target`. Returns a line per change made.
pub fn configure(target: &Path, h: &Harness, bin: &Path, stop_hook: bool) -> Result<Vec<String>> {
    let mut done = Vec::new();
    if let Some(hooks) = &h.hooks {
        let path = target.join(hooks.file);
        let before = read_json(&path)?;
        let after = with_hooks(before.clone(), h, bin, stop_hook);
        if write_json(&path, &before, &after)? {
            done.push(format!("hooks → {}", path.display()));
        }
    }
    if let Some(path) = skill_path(target, h) {
        match install_skill(&path) {
            Ok(true) => done.push(format!("skill → {}", path.display())),
            Ok(false) => {}
            Err(e) => done.push(format!("skill: {e:#}")),
        }
    }
    match &h.mcp {
        Mcp::ClaudeLocal => {
            claude_mcp(target, Some(bin))?;
            done.push(format!(
                "MCP server → {} local scope for this directory",
                h.name
            ));
        }
        Mcp::TomlBlock { file } => {
            let path = target.join(file);
            let before = std::fs::read_to_string(&path).unwrap_or_default();
            let after = with_toml_block(&before, &toml_block(h, bin))?;
            if after != before {
                write_file(&path, &after)?;
                done.push(format!("MCP server → {}", path.display()));
            }
        }
        Mcp::Json {
            file,
            servers,
            entry,
        } => {
            let path = target.join(file);
            let before = read_json(&path)?;
            let after = with_json_server(before.clone(), servers, json_entry(h, bin, *entry));
            if write_json(&path, &before, &after)? {
                done.push(format!("MCP server → {}", path.display()));
            }
        }
        Mcp::Manual { file } => {
            if !manual_registered(file, h) {
                done.push(format!(
                    "MCP server: {} has no per-project config; add this to {file}:\n{}",
                    h.name,
                    manual_snippet(h, bin)
                ));
            }
        }
        Mcp::None => {}
    }
    Ok(done)
}

/// Removes chitchat's configuration for `h` from `target`.
pub fn unconfigure(target: &Path, h: &Harness) -> Result<Vec<String>> {
    let mut done = Vec::new();
    if let Some(hooks) = &h.hooks {
        let path = target.join(hooks.file);
        if path.exists() {
            let before = read_json(&path)?;
            let after = without_hooks(before.clone(), h);
            if write_json(&path, &before, &after)? {
                done.push(format!("removed hooks from {}", path.display()));
            }
        }
    }
    if let Some(path) = skill_path(target, h)
        && remove_skill(&path)?
    {
        done.push(format!("removed the skill {}", path.display()));
    }
    match &h.mcp {
        Mcp::ClaudeLocal => {
            if claude_mcp_registered(target) {
                claude_mcp(target, None)?;
                done.push("removed the local-scope MCP server".to_string());
            }
        }
        Mcp::TomlBlock { file } => {
            let path = target.join(file);
            if let Ok(before) = std::fs::read_to_string(&path) {
                let after = without_toml_block(&before);
                if after != before {
                    write_file(&path, &after)?;
                    done.push(format!("removed the MCP server from {}", path.display()));
                }
            }
        }
        Mcp::Json { file, servers, .. } => {
            let path = target.join(file);
            if path.exists() {
                let before = read_json(&path)?;
                let after = without_json_server(before.clone(), servers);
                if write_json(&path, &before, &after)? {
                    done.push(format!("removed the MCP server from {}", path.display()));
                }
            }
        }
        Mcp::Manual { file } => {
            if manual_registered(file, h) {
                done.push(format!("remove the chitchat entry from {file} yourself"));
            }
        }
        Mcp::None => {}
    }
    Ok(done)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Status {
    pub mcp: bool,
    pub hooks: usize,
    /// How many chitchat events this harness supports hooks for.
    pub hook_events: usize,
    /// None when the harness has no skills support.
    pub skill: Option<bool>,
}

impl Status {
    pub fn configured(&self) -> bool {
        self.mcp || self.hooks > 0 || self.skill == Some(true)
    }
}

pub fn status(target: &Path, h: &Harness) -> Status {
    let (hooks, hook_events) = match &h.hooks {
        Some(spec) => (
            read_json(&target.join(spec.file))
                .ok()
                .map_or(0, |v| count_hooks(&v, h)),
            spec.events.len(),
        ),
        None => (0, 0),
    };
    let mcp = match &h.mcp {
        Mcp::ClaudeLocal => claude_mcp_registered(target),
        Mcp::TomlBlock { file } => std::fs::read_to_string(target.join(file))
            .is_ok_and(|t| t.lines().any(|l| l.trim() == BLOCK_START)),
        Mcp::Json { file, servers, .. } => read_json(&target.join(file)).ok().is_some_and(|v| {
            servers_object(&v, servers).is_some_and(|o| o.contains_key(SERVER_NAME))
        }),
        Mcp::Manual { file } => manual_registered(file, h),
        Mcp::None => false,
    };
    let skill = skill_path(target, h)
        .map(|p| std::fs::read_to_string(p).is_ok_and(|t| t.contains(SKILL_MARKER)));
    Status {
        mcp,
        hooks,
        hook_events,
        skill,
    }
}

// ---- Claude Code: local-scope MCP server -------------------------------------

/// Adds (with `bin`) or removes (without) the local-scope server for `target`.
fn claude_mcp(target: &Path, bin: Option<&Path>) -> Result<()> {
    let h = &crate::harness::CLAUDE;
    if !h.available() {
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
        "args": mcp_args(h),
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

// ---- JSON MCP configs ------------------------------------------------------------

/// One MCP server entry in the harness's JSON shape.
fn json_entry(h: &Harness, bin: &Path, entry: Entry) -> Value {
    let command = bin.to_string_lossy().into_owned();
    let args = mcp_args(h);
    let env: Map<String, Value> = passthrough_env()
        .into_iter()
        .map(|(k, v)| (k, json!(v)))
        .collect();
    let mut value = match entry {
        Entry::Plain => json!({ "command": command, "args": args }),
        Entry::Typed(kind) => json!({ "type": kind, "command": command, "args": args }),
        Entry::Copilot => json!({
            "type": "local", "command": command, "args": args, "env": env.clone(), "tools": ["*"]
        }),
        Entry::OpenCode => {
            let mut cmd = vec![command];
            cmd.extend(args);
            let mut v = json!({ "type": "local", "command": cmd, "enabled": true });
            if !env.is_empty() {
                v["environment"] = Value::Object(env.clone());
            }
            return v;
        }
    };
    if !env.is_empty() && entry != Entry::Copilot {
        value["env"] = Value::Object(env);
    }
    value
}

fn servers_object<'a>(settings: &'a Value, path: &[&str]) -> Option<&'a Map<String, Value>> {
    path.iter()
        .try_fold(settings, |v, key| v.get(*key))?
        .as_object()
}

/// Returns `settings` with the chitchat server set at `path`.
pub fn with_json_server(mut settings: Value, path: &[&str], entry: Value) -> Value {
    let mut node = ensure_object(&mut settings);
    for key in path {
        node = ensure_object(node.entry(*key).or_insert_with(|| json!({})));
    }
    node.insert(SERVER_NAME.to_string(), entry);
    settings
}

/// Returns `settings` without the chitchat server at `path`, dropping objects
/// that become empty.
pub fn without_json_server(mut settings: Value, path: &[&str]) -> Value {
    fn remove(node: &mut Value, path: &[&str]) {
        let Some(obj) = node.as_object_mut() else {
            return;
        };
        match path.split_first() {
            None => {
                obj.remove(SERVER_NAME);
            }
            Some((key, rest)) => {
                if let Some(child) = obj.get_mut(*key) {
                    remove(child, rest);
                    if child.as_object().is_some_and(Map::is_empty) {
                        obj.remove(*key);
                    }
                }
            }
        }
    }
    remove(&mut settings, path);
    settings
}

// ---- harnesses configured by hand -------------------------------------------------

fn manual_snippet(h: &Harness, bin: &Path) -> String {
    let q = |s: &str| serde_json::to_string(s).unwrap_or_default();
    let mut out = format!(
        "  mcp_servers:\n    {SERVER_NAME}:\n      command: {}\n      args: [mcp, --client, {}]\n",
        q(&bin.to_string_lossy()),
        h.id
    );
    let env = passthrough_env();
    if !env.is_empty() {
        out.push_str("      env:\n");
        for (k, v) in env {
            out.push_str(&format!("        {k}: {}\n", q(&v)));
        }
    }
    out.trim_end().to_string()
}

fn manual_registered(file: &str, h: &Harness) -> bool {
    let path = match file.strip_prefix("~/") {
        Some(rest) => dirs::home_dir().unwrap_or_default().join(rest),
        None => PathBuf::from(file),
    };
    std::fs::read_to_string(path).is_ok_and(|t| t.contains(&format!("--client, {}]", h.id)))
}

// ---- managed block in a TOML config (Codex) ------------------------------------

fn toml_block(h: &Harness, bin: &Path) -> String {
    // TOML basic strings accept JSON string escapes.
    let q = |s: &str| serde_json::to_string(s).unwrap_or_default();
    let args: Vec<String> = mcp_args(h).iter().map(|a| q(a)).collect();
    let mut block = format!(
        "{BLOCK_START}\n[mcp_servers.{SERVER_NAME}]\ncommand = {}\nargs = [{}]\n",
        q(&bin.to_string_lossy()),
        args.join(", ")
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
pub fn with_toml_block(existing: &str, block: &str) -> Result<String> {
    let stripped = without_toml_block(existing);
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

pub fn without_toml_block(existing: &str) -> String {
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

pub fn hook_command(bin: &Path, event: HookEvent, h: &Harness) -> String {
    format!(
        "{} hook {} --client {}",
        shell_quote(&bin.to_string_lossy()),
        event.arg(),
        h.id
    )
}

/// Whether a hook handler was written by chitchat for this harness.
fn is_ours(handler: &Value, h: &Harness) -> bool {
    ["command", "bash"].iter().any(|field| {
        handler
            .get(*field)
            .and_then(Value::as_str)
            .is_some_and(|c| {
                c.contains("chitchat")
                    && c.contains(" hook ")
                    && c.ends_with(&format!("--client {}", h.id))
            })
    })
}

/// Handlers listed under one event: nested in groups (`Grouped`) or directly.
fn handlers(event_list: &Value) -> Vec<Value> {
    event_list
        .as_array()
        .into_iter()
        .flatten()
        .flat_map(|item| match item.get("hooks").and_then(Value::as_array) {
            Some(nested) => nested.clone(),
            None => vec![item.clone()],
        })
        .collect()
}

fn count_hooks(settings: &Value, h: &Harness) -> usize {
    let Some(spec) = &h.hooks else {
        return 0;
    };
    spec.events
        .iter()
        .filter(|(_, name)| {
            handlers(&settings["hooks"][*name])
                .iter()
                .any(|handler| is_ours(handler, h))
        })
        .count()
}

/// Returns `settings` with chitchat's hook entries (re)added in the harness's
/// layout (see [`Layout`]).
pub fn with_hooks(settings: Value, h: &Harness, bin: &Path, stop_hook: bool) -> Value {
    let Some(spec) = &h.hooks else {
        return settings;
    };
    let mut settings = without_hooks(settings, h);
    let root = ensure_object(&mut settings);
    if spec.layout != Layout::Grouped {
        root.entry("version").or_insert(json!(1));
    }
    let hooks = ensure_object(root.entry("hooks").or_insert_with(|| json!({})));
    for &(event, name) in spec.events {
        if event == HookEvent::Stop && !stop_hook {
            continue;
        }
        if let Some(list) = hooks
            .entry(name)
            .or_insert_with(|| json!([]))
            .as_array_mut()
        {
            list.push(hook_group(spec, event, bin, h));
        }
    }
    settings
}

fn hook_group(spec: &Hooks, event: HookEvent, bin: &Path, h: &Harness) -> Value {
    let command = hook_command(bin, event, h);
    let timeout = match spec.timeout {
        Timeout::Seconds => HOOK_TIMEOUT,
        Timeout::Millis => HOOK_TIMEOUT * 1000,
    };
    match spec.layout {
        Layout::Copilot => {
            return json!({ "type": "command", "bash": command, "timeoutSec": timeout });
        }
        Layout::Flat => return json!({ "command": command, "timeout": timeout }),
        Layout::Grouped => {}
    }
    let handler = json!({
        "type": "command",
        "command": command,
        "timeout": timeout,
    });
    let mut group = Map::new();
    // After-tool hooks run for every tool, so urgent messages arrive mid-turn.
    if event == HookEvent::PostToolUse
        && let Some(matcher) = spec.all_tools_matcher
    {
        group.insert("matcher".into(), json!(matcher));
    }
    group.insert("hooks".into(), json!([handler]));
    Value::Object(group)
}

/// Returns `settings` with every chitchat hook entry for `h` removed, dropping
/// groups and event lists that become empty.
pub fn without_hooks(mut settings: Value, h: &Harness) -> Value {
    let Some(hooks) = settings.get_mut("hooks").and_then(Value::as_object_mut) else {
        return settings;
    };
    for groups in hooks.values_mut() {
        let Some(groups) = groups.as_array_mut() else {
            continue;
        };
        for group in groups.iter_mut() {
            if let Some(handlers) = group.get_mut("hooks").and_then(Value::as_array_mut) {
                handlers.retain(|handler| !is_ours(handler, h));
            }
        }
        // Flat layouts list handlers directly; grouped ones drop emptied groups.
        groups.retain(|g| !is_ours(g, h));
        groups.retain(|g| {
            g.get("hooks")
                .and_then(Value::as_array)
                .is_none_or(|list| !list.is_empty())
        });
    }
    hooks.retain(|_, groups| groups.as_array().is_none_or(|g| !g.is_empty()));
    if hooks.is_empty()
        && let Some(root) = settings.as_object_mut()
    {
        root.remove("hooks");
        // A file left with only a layout version held nothing but chitchat's hooks.
        if root.len() == 1 && root.contains_key("version") {
            root.clear();
        }
    }
    settings
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
pub fn write_file(path: &Path, text: &str) -> Result<()> {
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::harness::{CLAUDE, CODEX};

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
        let once = with_hooks(existing.clone(), &CLAUDE, Path::new(BIN), true);
        let twice = with_hooks(once.clone(), &CLAUDE, Path::new(BIN), true);
        assert_eq!(once, twice);
        assert_eq!(once["model"], "opus");
        assert_eq!(once["hooks"]["Stop"].as_array().unwrap().len(), 2);
        assert_eq!(
            once["hooks"]["UserPromptSubmit"][0]["hooks"][0]["command"],
            "'/Users/me/My Tools/chitchat' hook user-prompt-submit --client claude"
        );
        assert_eq!(once["hooks"]["PostToolUse"][1]["matcher"], "*");
        assert_eq!(count_hooks(&once, &CLAUDE), 4);
        // Keys keep their original order (serde_json preserve_order).
        let keys: Vec<&String> = once.as_object().unwrap().keys().collect();
        assert_eq!(keys, ["model", "hooks"]);

        assert_eq!(without_hooks(once, &CLAUDE), existing);
    }

    #[test]
    fn harnesses_do_not_touch_each_others_entries() {
        let both = with_hooks(
            with_hooks(json!({}), &CLAUDE, Path::new(BIN), true),
            &CODEX,
            Path::new(BIN),
            false,
        );
        let codex_only = without_hooks(both, &CLAUDE);
        let text = codex_only.to_string();
        assert!(text.contains("--client codex") && !text.contains("--client claude"));
        assert!(codex_only["hooks"].get("Stop").is_none());
        assert_eq!(without_hooks(codex_only, &CODEX), json!({}));
    }

    #[test]
    fn toml_block_replaces_and_removes_cleanly() {
        let user = "model = \"gpt-6\"\n\n[mcp_servers.other]\ncommand = \"other\"\n";
        let block = toml_block(&CODEX, Path::new(BIN));
        assert!(
            block.contains("args = [\"mcp\", \"--client\", \"codex\"]"),
            "{block}"
        );
        let once = with_toml_block(user, &block).unwrap();
        let twice = with_toml_block(&once, &block).unwrap();
        assert_eq!(once, twice);
        assert!(once.contains("command = \"/Users/me/My Tools/chitchat\""));
        assert_eq!(without_toml_block(&once), user);
        assert_eq!(without_toml_block(&block), "");

        let clash = "[mcp_servers.chitchat]\ncommand = \"x\"\n";
        assert!(with_toml_block(clash, &block).is_err());
    }

    #[test]
    fn skill_is_installed_refreshed_and_removed_but_never_clobbers_the_users() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(".claude/skills/chitchat/SKILL.md");
        assert!(SKILL.starts_with("---\nname: chitchat\ndescription: "));
        assert!(install_skill(&path).unwrap());
        assert!(!install_skill(&path).unwrap());
        // An older chitchat version's copy is refreshed.
        std::fs::write(&path, format!("old text\n{SKILL_MARKER} -->\n")).unwrap();
        assert!(install_skill(&path).unwrap());
        assert!(remove_skill(&path).unwrap());
        assert!(!dir.path().join(".claude/skills").exists());

        // A skill the user wrote under the same name stays.
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, "my own chitchat notes").unwrap();
        assert!(install_skill(&path).is_err());
        assert!(!remove_skill(&path).unwrap());
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "my own chitchat notes"
        );
    }

    #[test]
    fn copilot_and_cursor_layouts_round_trip() {
        use crate::harness::{COPILOT, CURSOR, GEMINI};
        let copilot = with_hooks(json!({}), &COPILOT, Path::new(BIN), true);
        assert_eq!(copilot["version"], 1);
        assert_eq!(copilot["hooks"]["sessionStart"][0]["timeoutSec"], 15);
        assert!(
            copilot["hooks"]["agentStop"][0]["bash"]
                .as_str()
                .unwrap()
                .ends_with("--client copilot")
        );
        assert!(copilot["hooks"].get("userPromptSubmitted").is_none());
        assert_eq!(count_hooks(&copilot, &COPILOT), 3);
        assert_eq!(without_hooks(copilot, &COPILOT), json!({}));

        let user = json!({"version": 1, "hooks": {"stop": [{"command": "notify-send done"}]}});
        let cursor = with_hooks(user.clone(), &CURSOR, Path::new(BIN), true);
        assert_eq!(cursor["hooks"]["stop"].as_array().unwrap().len(), 2);
        assert_eq!(cursor["hooks"]["postToolUse"][0]["timeout"], 15);
        assert_eq!(without_hooks(cursor, &CURSOR), user);

        let gemini = with_hooks(json!({}), &GEMINI, Path::new(BIN), true);
        assert_eq!(
            gemini["hooks"]["BeforeAgent"][0]["hooks"][0]["timeout"],
            15_000
        );
        assert_eq!(gemini["hooks"]["AfterTool"][0]["matcher"], "*");
    }

    #[test]
    fn json_mcp_entries_match_each_harness_and_remove_cleanly() {
        use crate::harness::{AMP, COPILOT, OPENCODE, ZED};
        let bin = Path::new("/bin/chitchat");
        let plain = json_entry(&ZED, bin, Entry::Plain);
        assert_eq!(
            plain,
            json!({"command": "/bin/chitchat", "args": ["mcp", "--client", "zed"]})
        );
        assert_eq!(
            json_entry(&COPILOT, bin, Entry::Copilot)["tools"],
            json!(["*"])
        );
        let oc = json_entry(&OPENCODE, bin, Entry::OpenCode);
        assert_eq!(
            oc["command"],
            json!(["/bin/chitchat", "mcp", "--client", "opencode"])
        );
        assert_eq!(oc["type"], "local");

        let existing = json!({"theme": "dark", "amp.mcpServers": {"other": {"command": "x"}}});
        let with = with_json_server(existing.clone(), &["amp.mcpServers"], plain.clone());
        assert_eq!(with["amp.mcpServers"]["chitchat"], plain);
        assert_eq!(without_json_server(with, &["amp.mcpServers"]), existing);
        let fresh = with_json_server(json!({}), &["context_servers"], plain);
        assert_eq!(without_json_server(fresh, &["context_servers"]), json!({}));
        let _ = &AMP;
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
