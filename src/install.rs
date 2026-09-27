//! `chitchat install|uninstall claude|codex`: registers the MCP server and hooks
//! with a client, at user scope, so every project gets chitchat.
//!
//! MCP servers are registered through the clients' own CLIs (`claude mcp`,
//! `codex mcp`). Hooks are merged into the clients' JSON hook files, preserving
//! everything else and keeping a `.bak` copy; chitchat's entries are recognized by
//! their command line, so installing twice replaces rather than duplicates them.

use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail};
use serde_json::{Map, Value, json};

use crate::hook::HookEvent;
use crate::session::Client;

const SERVER_NAME: &str = "chitchat";
/// Seconds. The hook itself is fast; this only bounds a stuck database lock.
const HOOK_TIMEOUT: u64 = 15;

pub fn install(client: Client, dry_run: bool, stop_hook: bool) -> Result<()> {
    let bin = binary_path()?;
    println!(
        "Installing chitchat for {} using {}",
        name(client),
        bin.display()
    );
    if bin.components().any(|c| c.as_os_str() == "target") {
        println!(
            "  note: this is a build output; if you move or rebuild the binary elsewhere, run install again"
        );
    }
    let path = hooks_file(client)?;
    if !dry_run && let Some(dir) = path.parent() {
        // `codex mcp add` refuses to run if CODEX_HOME doesn't exist yet.
        std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    }
    register_mcp(client, &bin, dry_run)?;
    let current = read_json(&path)?;
    let updated = with_hooks(current.clone(), client, &bin, stop_hook);
    write_json(&path, &current, &updated, dry_run)?;

    println!();
    match client {
        Client::Claude => println!(
            "Done. Start a new Claude Code session (existing ones don't pick up MCP servers or hooks)."
        ),
        Client::Codex => println!(
            "Done. Codex runs new hooks only after you trust them: start `codex`, run /hooks, and \
             trust the chitchat entries (again after every install, since the entries change)."
        ),
    }
    println!("Watch the chat with `chitchat tail`; check setup with `chitchat doctor`.");
    Ok(())
}

pub fn uninstall(client: Client, dry_run: bool) -> Result<()> {
    unregister_mcp(client, dry_run)?;
    let path = hooks_file(client)?;
    if path.exists() {
        let current = read_json(&path)?;
        let updated = without_hooks(current.clone(), client);
        write_json(&path, &current, &updated, dry_run)?;
    }
    println!(
        "Removed chitchat from {}. Your chitchat data in ~/.chitchat is untouched.",
        name(client)
    );
    Ok(())
}

/// What `chitchat doctor` reports per client.
#[derive(Debug)]
pub struct Status {
    pub mcp_registered: bool,
    pub mcp_config: PathBuf,
    pub hooks_installed: usize,
    pub hooks_file: PathBuf,
}

pub fn status(client: Client) -> Result<Status> {
    let hooks_file = hooks_file(client)?;
    let hooks_installed = read_json(&hooks_file)
        .ok()
        .and_then(|v| v.get("hooks").cloned())
        .map_or(0, |hooks| {
            HookEvent::ALL
                .iter()
                .filter(|e| {
                    hooks[event_key(**e)]
                        .as_array()
                        .into_iter()
                        .flatten()
                        .flat_map(|g| g["hooks"].as_array().cloned().unwrap_or_default())
                        .any(|h| is_ours(&h, client))
                })
                .count()
        });
    let (mcp_config, mcp_registered) = match client {
        Client::Claude => {
            // User-scope MCP servers live in ~/.claude.json (inside CLAUDE_CONFIG_DIR if set).
            let path = match std::env::var_os("CLAUDE_CONFIG_DIR") {
                Some(dir) => PathBuf::from(dir).join(".claude.json"),
                None => dirs::home_dir().unwrap_or_default().join(".claude.json"),
            };
            let found = read_json(&path)
                .ok()
                .is_some_and(|v| v["mcpServers"].get(SERVER_NAME).is_some());
            (path, found)
        }
        Client::Codex => {
            let path = hooks_file.with_file_name("config.toml");
            let found = std::fs::read_to_string(&path)
                .is_ok_and(|t| t.lines().any(|l| l.trim() == "[mcp_servers.chitchat]"));
            (path, found)
        }
    };
    Ok(Status {
        mcp_registered,
        mcp_config,
        hooks_installed,
        hooks_file,
    })
}

fn name(client: Client) -> &'static str {
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

fn binary_path() -> Result<PathBuf> {
    let exe = std::env::current_exe().context("locating the chitchat binary")?;
    Ok(exe.canonicalize().unwrap_or(exe))
}

/// `~/.claude/settings.json` (or under `CLAUDE_CONFIG_DIR`), `~/.codex/hooks.json`
/// (or under `CODEX_HOME`).
fn hooks_file(client: Client) -> Result<PathBuf> {
    let home = || dirs::home_dir().context("could not determine the home directory");
    Ok(match client {
        Client::Claude => match std::env::var_os("CLAUDE_CONFIG_DIR") {
            Some(dir) => PathBuf::from(dir),
            None => home()?.join(".claude"),
        }
        .join("settings.json"),
        Client::Codex => match std::env::var_os("CODEX_HOME") {
            Some(dir) => PathBuf::from(dir),
            None => home()?.join(".codex"),
        }
        .join("hooks.json"),
    })
}

// ---- MCP registration --------------------------------------------------------

/// Env vars worth passing through to the MCP server. Codex starts MCP servers with
/// a scrubbed environment, so these must be set explicitly there.
fn passthrough_env() -> Vec<(String, String)> {
    ["CHITCHAT_HOME", "CHITCHAT_LOG"]
        .iter()
        .filter_map(|k| std::env::var(k).ok().map(|v| (k.to_string(), v)))
        .collect()
}

fn register_mcp(client: Client, bin: &Path, dry_run: bool) -> Result<()> {
    let bin_s = bin.to_string_lossy().into_owned();
    let env = passthrough_env();
    let (program, remove, add): (&str, Vec<String>, Vec<String>) = match client {
        Client::Claude => {
            let mut spec = json!({
                "type": "stdio",
                "command": bin_s,
                "args": ["mcp", "--client", "claude"],
            });
            if !env.is_empty() {
                spec["env"] =
                    Value::Object(env.iter().map(|(k, v)| (k.clone(), json!(v))).collect());
            }
            (
                "claude",
                args(&["mcp", "remove", "--scope", "user", SERVER_NAME]),
                args(&[
                    "mcp",
                    "add-json",
                    "--scope",
                    "user",
                    SERVER_NAME,
                    &spec.to_string(),
                ]),
            )
        }
        Client::Codex => {
            let mut add = args(&["mcp", "add", SERVER_NAME]);
            for (k, v) in &env {
                add.push("--env".into());
                add.push(format!("{k}={v}"));
            }
            add.extend(args(&["--", &bin_s, "mcp", "--client", "codex"]));
            ("codex", args(&["mcp", "remove", SERVER_NAME]), add)
        }
    };
    println!("MCP server:\n  {program} {}", shell_join(&add));
    if dry_run {
        return Ok(());
    }
    if !on_path(program) {
        bail!(
            "`{program}` is not on PATH; install {} first, or run the command above yourself",
            name(client)
        );
    }
    // Replace any previous registration (e.g. pointing at an old binary path).
    let _ = Command::new(program).args(&remove).output();
    let out = Command::new(program)
        .args(&add)
        .output()
        .with_context(|| format!("running {program}"))?;
    if !out.status.success() {
        bail!(
            "`{program} {}` failed: {}",
            add.first().map_or("", String::as_str),
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(())
}

fn unregister_mcp(client: Client, dry_run: bool) -> Result<()> {
    let (program, remove) = match client {
        Client::Claude => (
            "claude",
            args(&["mcp", "remove", "--scope", "user", SERVER_NAME]),
        ),
        Client::Codex => ("codex", args(&["mcp", "remove", SERVER_NAME])),
    };
    println!("MCP server:\n  {program} {}", shell_join(&remove));
    if !dry_run && on_path(program) {
        let _ = Command::new(program).args(&remove).output();
    }
    Ok(())
}

fn args(items: &[&str]) -> Vec<String> {
    items.iter().map(|s| s.to_string()).collect()
}

fn on_path(program: &str) -> bool {
    std::env::var_os("PATH")
        .is_some_and(|paths| std::env::split_paths(&paths).any(|dir| dir.join(program).is_file()))
}

// ---- hooks -------------------------------------------------------------------

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

fn shell_join(items: &[String]) -> String {
    items
        .iter()
        .map(|s| shell_quote(s))
        .collect::<Vec<_>>()
        .join(" ")
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

fn read_json(path: &Path) -> Result<Value> {
    match std::fs::read_to_string(path) {
        Ok(text) if text.trim().is_empty() => Ok(json!({})),
        Ok(text) => serde_json::from_str(&text)
            .with_context(|| format!("{} is not valid JSON; fix it and retry", path.display())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(json!({})),
        Err(e) => Err(e).with_context(|| format!("reading {}", path.display())),
    }
}

fn write_json(path: &Path, before: &Value, after: &Value, dry_run: bool) -> Result<()> {
    if before == after {
        println!("Hooks in {}: already up to date", path.display());
        return Ok(());
    }
    println!("Hooks in {}:", path.display());
    let hooks = after.get("hooks").cloned().unwrap_or(json!({}));
    for (event, groups) in hooks.as_object().into_iter().flatten() {
        for group in groups.as_array().into_iter().flatten() {
            for h in group
                .get("hooks")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
            {
                if let Some(cmd) = h.get("command").and_then(Value::as_str)
                    && cmd.contains("chitchat")
                    && cmd.contains(" hook ")
                {
                    println!("  {event}: {cmd}");
                }
            }
        }
    }
    if dry_run {
        println!("  (dry run: nothing written)");
        return Ok(());
    }
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    if path.exists() {
        let mut backup = path.as_os_str().to_owned();
        backup.push(".bak");
        std::fs::copy(path, &backup).with_context(|| format!("backing up {}", path.display()))?;
    }
    let mut text = serde_json::to_string_pretty(after)?;
    text.push('\n');
    let tmp = path.with_extension("json.chitchat-tmp");
    std::fs::write(&tmp, text)?;
    std::fs::rename(&tmp, path).with_context(|| format!("writing {}", path.display()))?;
    Ok(())
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
    fn quoting() {
        assert_eq!(
            shell_quote("/usr/local/bin/chitchat"),
            "/usr/local/bin/chitchat"
        );
        assert_eq!(shell_quote("/a b/it's"), r"'/a b/it'\''s'");
    }
}
