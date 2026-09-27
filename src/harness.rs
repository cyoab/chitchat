//! The agent harnesses chitchat can set up, as data.
//!
//! Each entry says how the harness identifies an agent, where its MCP server is
//! registered, where its hooks live and what hook output it accepts. Adding a
//! harness is adding an entry here; the config writers (`clients`), the hook
//! handler (`hook`) and identity resolution (`agents`) all read this table.

use std::path::Path;

use crate::hook::HookEvent;

#[derive(Debug, PartialEq, Eq)]
pub struct Harness {
    /// Stable id: the `--client` value, the `agents.vendor` column and the handle
    /// prefix ("claude-1").
    pub id: &'static str,
    pub name: &'static str,
    /// Executables whose presence on PATH means the harness is installed.
    pub binaries: &'static [&'static str],
    pub identity: Identity,
    /// Environment variable the harness sets, in hook processes, to its own pid.
    pub hook_pid_env: Option<&'static str>,
    /// Environment variable the harness sets, in its MCP server, to the session id.
    pub mcp_session_env: Option<&'static str>,
    pub mcp: Mcp,
    pub hooks: Option<Hooks>,
    /// What the user still has to do after `chitchat init`, if anything.
    pub setup_note: Option<&'static str>,
}

/// What makes two chitchat processes the same agent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Identity {
    /// One agent per harness process (its MCP server and hooks are both children
    /// of it). Survives /clear-style session resets.
    Process,
    /// One agent per session id, which hooks and MCP calls both carry. Needed when
    /// one harness process hosts several sessions (Codex's app-server).
    Session,
}

/// Where the MCP server is registered for a workspace.
#[derive(Debug, PartialEq, Eq)]
pub enum Mcp {
    /// `claude mcp add-json --scope local` (stored in ~/.claude.json per directory).
    ClaudeLocal,
    /// A managed `[mcp_servers.chitchat]` block in a TOML file (Codex).
    TomlBlock { file: &'static str },
}

/// A harness's hook configuration and protocol.
#[derive(Debug, PartialEq, Eq)]
pub struct Hooks {
    /// Hook file, relative to the workspace directory.
    pub file: &'static str,
    /// The harness's name for each chitchat event it supports.
    pub events: &'static [(HookEvent, &'static str)],
    /// The matcher that makes the after-tool hook run for every tool, if the
    /// harness uses matchers there.
    pub all_tools_matcher: Option<&'static str>,
    pub output: Output,
}

impl Hooks {
    pub fn event_name(&self, event: HookEvent) -> Option<&'static str> {
        self.events
            .iter()
            .find(|(e, _)| *e == event)
            .map(|(_, name)| *name)
    }
}

/// The stdout contract of a harness's hooks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Output {
    /// Claude Code: `{"hookSpecificOutput": {"hookEventName", "additionalContext"}}`
    /// on every event; Stop blocks with `{"decision": "block", "reason"}`.
    Claude,
    /// Like Claude, but every output struct rejects unknown fields and Stop only
    /// accepts `decision` / `reason` (Codex).
    Codex,
}

const CLAUDE_STYLE_EVENTS: &[(HookEvent, &str)] = &[
    (HookEvent::SessionStart, "SessionStart"),
    (HookEvent::UserPromptSubmit, "UserPromptSubmit"),
    (HookEvent::PostToolUse, "PostToolUse"),
    (HookEvent::Stop, "Stop"),
];

pub static CLAUDE: Harness = Harness {
    id: "claude",
    name: "Claude Code",
    binaries: &["claude"],
    identity: Identity::Process,
    hook_pid_env: Some("CLAUDE_PID"),
    mcp_session_env: Some("CLAUDE_CODE_SESSION_ID"),
    mcp: Mcp::ClaudeLocal,
    hooks: Some(Hooks {
        file: ".claude/settings.local.json",
        events: CLAUDE_STYLE_EVENTS,
        all_tools_matcher: Some("*"),
        output: Output::Claude,
    }),
    setup_note: None,
};

pub static CODEX: Harness = Harness {
    id: "codex",
    name: "Codex",
    binaries: &["codex"],
    identity: Identity::Session,
    hook_pid_env: None,
    mcp_session_env: None,
    mcp: Mcp::TomlBlock {
        file: ".codex/config.toml",
    },
    hooks: Some(Hooks {
        file: ".codex/hooks.json",
        events: CLAUDE_STYLE_EVENTS,
        all_tools_matcher: Some("*"),
        output: Output::Codex,
    }),
    setup_note: Some(
        "trust this folder when Codex asks, then run /hooks and trust the chitchat hooks.",
    ),
};

/// Every supported harness, in the order `init` and `doctor` list them.
pub static ALL: &[&Harness] = &[&CLAUDE, &CODEX];

pub fn find(id: &str) -> Option<&'static Harness> {
    ALL.iter().copied().find(|h| h.id == id)
}

/// clap value parser for `--client`.
pub fn parse(id: &str) -> Result<&'static Harness, String> {
    find(&id.to_ascii_lowercase()).ok_or_else(|| {
        let ids: Vec<&str> = ALL.iter().map(|h| h.id).collect();
        format!("unknown client `{id}`; supported: {}", ids.join(", "))
    })
}

impl Harness {
    /// Whether the harness is installed (one of its executables is on PATH).
    pub fn available(&self) -> bool {
        self.binaries.iter().any(|b| on_path(b))
    }

    /// Best-effort match of a process name ("claude", "codex", ...) to a harness.
    pub fn from_process_name(name: &str) -> Option<&'static Harness> {
        let name = name.to_ascii_lowercase();
        ALL.iter()
            .copied()
            .find(|h| h.binaries.iter().any(|b| name.contains(b)))
    }
}

fn on_path(program: &str) -> bool {
    std::env::var_os("PATH").is_some_and(|paths| {
        std::env::split_paths(&paths).any(|dir| is_executable(&dir.join(program)))
    })
}

fn is_executable(path: &Path) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        path.metadata()
            .is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
    }
    #[cfg(not(unix))]
    {
        path.is_file()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_are_unique_lowercase_and_short() {
        for (i, h) in ALL.iter().enumerate() {
            assert!(
                h.id.len() <= 16 && h.id.chars().all(|c| c.is_ascii_lowercase()),
                "{}",
                h.id
            );
            assert!(
                ALL[i + 1..].iter().all(|o| o.id != h.id),
                "duplicate {}",
                h.id
            );
            assert_ne!(h.id, "human");
        }
    }

    #[test]
    fn parse_is_case_insensitive_and_lists_ids_on_error() {
        assert_eq!(parse("Claude").unwrap().id, "claude");
        let err = parse("nope").unwrap_err();
        assert!(err.contains("claude") && err.contains("codex"), "{err}");
    }
}
