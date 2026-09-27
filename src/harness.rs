//! The agent harnesses chitchat can set up, as data.
//!
//! Each entry says how the harness identifies an agent, where its MCP server is
//! registered, where its hooks live, what hook output it accepts and where it
//! loads skills. Adding a harness is adding an entry here; the config writers
//! (`clients`), the hook handler (`hook`) and identity resolution (`agents`) all
//! read this table.
//!
//! Entries with `verified: false` were built from the harness's documentation and
//! source (researched 2026-09-27) but not yet run end to end; `chitchat clients`
//! marks them experimental.

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
    /// Directory (relative to the workspace) the harness loads Agent Skills from;
    /// `chitchat init` installs the chitchat skill there.
    pub skills_dir: Option<&'static str>,
    /// What the user still has to do after `chitchat init`, if anything.
    pub setup_note: Option<&'static str>,
    /// Tested end to end with the real harness.
    pub verified: bool,
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
    /// An entry in a JSON config file inside the workspace. `servers` is the path
    /// of keys to the object that holds servers by name.
    Json {
        file: &'static str,
        servers: &'static [&'static str],
        entry: Entry,
    },
    /// No per-project MCP config: `init` prints what to add to `file` (a
    /// user-level config).
    Manual { file: &'static str },
    /// No MCP support: agents run `chitchat tool` through their shell, as the
    /// installed skill explains.
    None,
}

/// The shape of one MCP server entry in a JSON config.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Entry {
    /// `{"command", "args", "env"}`
    Plain,
    /// `{"type": <type>, "command", "args", "env"}`
    Typed(&'static str),
    /// Copilot CLI: `{"type": "local", "command", "args", "env", "tools": ["*"]}`
    Copilot,
    /// OpenCode: `{"type": "local", "command": [program, args...], "environment",
    /// "enabled": true}`
    OpenCode,
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
    pub layout: Layout,
    pub timeout: Timeout,
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

/// How hook entries are laid out in the hook file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Layout {
    /// `{"hooks": {"<Event>": [{"matcher"?, "hooks": [{"type": "command",
    /// "command", "timeout"}]}]}}`: Claude Code and the harnesses that copied it.
    Grouped,
    /// `{"version": 1, "hooks": {"<event>": [{"type": "command", "bash",
    /// "timeoutSec"}]}}`: Copilot CLI.
    Copilot,
    /// `{"version": 1, "hooks": {"<event>": [{"command", "timeout"}]}}`: Cursor.
    Flat,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Timeout {
    Seconds,
    Millis,
}

/// The stdout contract of a harness's hooks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Output {
    /// `{"hookSpecificOutput": {"hookEventName", "additionalContext"}}`; Stop
    /// blocks with `{"decision": "block", "reason"}`.
    Claude,
    /// Like Claude, but unknown fields are rejected and Stop only accepts
    /// `decision` / `reason` (Codex).
    Codex,
    /// Like Claude, but the stop event blocks with `{"decision": "deny", "reason"}`.
    Gemini,
    /// `{"additionalContext"}`; stop blocks with `{"decision": "block", "reason"}`.
    Copilot,
    /// `{"additional_context"}`; stop continues with `{"followup_message"}`.
    Cursor,
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
        layout: Layout::Grouped,
        timeout: Timeout::Seconds,
        output: Output::Claude,
    }),
    skills_dir: Some(".claude/skills"),
    setup_note: None,
    verified: true,
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
        layout: Layout::Grouped,
        timeout: Timeout::Seconds,
        output: Output::Codex,
    }),
    skills_dir: Some(".agents/skills"),
    setup_note: Some(
        "trust this folder when Codex asks, then run /hooks and trust the chitchat hooks.",
    ),
    verified: true,
};

/// Gemini CLI: MCP servers and hooks share `.gemini/settings.json`; hook
/// timeouts are milliseconds; the prompt event is `BeforeAgent`.
pub static GEMINI: Harness = Harness {
    id: "gemini",
    name: "Gemini CLI",
    binaries: &["gemini"],
    identity: Identity::Process,
    hook_pid_env: None,
    mcp_session_env: None,
    mcp: Mcp::Json {
        file: ".gemini/settings.json",
        servers: &["mcpServers"],
        entry: Entry::Plain,
    },
    hooks: Some(Hooks {
        file: ".gemini/settings.json",
        events: &[
            (HookEvent::SessionStart, "SessionStart"),
            (HookEvent::UserPromptSubmit, "BeforeAgent"),
            (HookEvent::PostToolUse, "AfterTool"),
            (HookEvent::Stop, "AfterAgent"),
        ],
        all_tools_matcher: Some("*"),
        layout: Layout::Grouped,
        timeout: Timeout::Millis,
        output: Output::Gemini,
    }),
    skills_dir: Some(".agents/skills"),
    setup_note: Some(
        "if folder trust is on, trust this folder (`gemini trust`); Gemini asks before running new project hooks.",
    ),
    verified: false,
};

/// GitHub Copilot CLI. Its per-prompt hook can't add context, so messages arrive
/// at session start, after tool calls and at stop.
pub static COPILOT: Harness = Harness {
    id: "copilot",
    name: "GitHub Copilot CLI",
    binaries: &["copilot"],
    identity: Identity::Process,
    hook_pid_env: None,
    mcp_session_env: None,
    // Not `.mcp.json`: Claude Code reads that file too.
    mcp: Mcp::Json {
        file: ".github/mcp.json",
        servers: &["mcpServers"],
        entry: Entry::Copilot,
    },
    hooks: Some(Hooks {
        file: ".github/hooks/chitchat.json",
        events: &[
            (HookEvent::SessionStart, "sessionStart"),
            (HookEvent::PostToolUse, "postToolUse"),
            (HookEvent::Stop, "agentStop"),
        ],
        all_tools_matcher: None,
        layout: Layout::Copilot,
        timeout: Timeout::Seconds,
        output: Output::Copilot,
    }),
    skills_dir: Some(".agents/skills"),
    setup_note: Some("confirm folder trust when Copilot asks, so it loads project MCP servers."),
    verified: false,
};

/// Cursor (IDE and `cursor-agent`). Cursor also runs Claude Code's hooks by
/// default; `chitchat hook` recognizes Cursor payloads and leaves them to the
/// Cursor hooks, so nothing is delivered twice.
pub static CURSOR: Harness = Harness {
    id: "cursor",
    name: "Cursor",
    binaries: &["cursor-agent", "cursor"],
    identity: Identity::Process,
    hook_pid_env: None,
    mcp_session_env: None,
    mcp: Mcp::Json {
        file: ".cursor/mcp.json",
        servers: &["mcpServers"],
        entry: Entry::Typed("stdio"),
    },
    hooks: Some(Hooks {
        file: ".cursor/hooks.json",
        events: &[
            (HookEvent::SessionStart, "sessionStart"),
            (HookEvent::PostToolUse, "postToolUse"),
            (HookEvent::Stop, "stop"),
        ],
        all_tools_matcher: None,
        layout: Layout::Flat,
        timeout: Timeout::Seconds,
        output: Output::Cursor,
    }),
    skills_dir: Some(".agents/skills"),
    setup_note: Some(
        "enable the chitchat MCP server when Cursor asks (CLI: `cursor-agent mcp enable chitchat`).",
    ),
    verified: false,
};

/// Qwen Code: Claude-compatible hooks in `.qwen/settings.json`.
pub static QWEN: Harness = Harness {
    id: "qwen",
    name: "Qwen Code",
    binaries: &["qwen"],
    identity: Identity::Process,
    hook_pid_env: None,
    mcp_session_env: None,
    mcp: Mcp::Json {
        file: ".qwen/settings.json",
        servers: &["mcpServers"],
        entry: Entry::Plain,
    },
    hooks: Some(Hooks {
        file: ".qwen/settings.json",
        events: CLAUDE_STYLE_EVENTS,
        all_tools_matcher: Some("*"),
        layout: Layout::Grouped,
        timeout: Timeout::Seconds,
        output: Output::Claude,
    }),
    skills_dir: Some(".qwen/skills"),
    setup_note: None,
    verified: false,
};

/// Factory Droid: Claude-compatible hooks in its project settings.
pub static DROID: Harness = Harness {
    id: "droid",
    name: "Factory Droid",
    binaries: &["droid"],
    identity: Identity::Process,
    hook_pid_env: None,
    mcp_session_env: None,
    mcp: Mcp::Json {
        file: ".factory/mcp.json",
        servers: &["mcpServers"],
        entry: Entry::Typed("stdio"),
    },
    hooks: Some(Hooks {
        file: ".factory/settings.json",
        events: CLAUDE_STYLE_EVENTS,
        all_tools_matcher: Some("*"),
        layout: Layout::Grouped,
        timeout: Timeout::Seconds,
        output: Output::Claude,
    }),
    skills_dir: Some(".factory/skills"),
    setup_note: None,
    verified: false,
};

/// OpenCode: MCP only (its hooks are JS plugins).
pub static OPENCODE: Harness = Harness {
    id: "opencode",
    name: "OpenCode",
    binaries: &["opencode"],
    identity: Identity::Process,
    hook_pid_env: None,
    mcp_session_env: None,
    mcp: Mcp::Json {
        file: "opencode.json",
        servers: &["mcp"],
        entry: Entry::OpenCode,
    },
    hooks: None,
    skills_dir: Some(".agents/skills"),
    setup_note: None,
    verified: false,
};

/// Amp: MCP only (its hooks are TS plugins).
pub static AMP: Harness = Harness {
    id: "amp",
    name: "Amp",
    binaries: &["amp"],
    identity: Identity::Process,
    hook_pid_env: None,
    mcp_session_env: None,
    mcp: Mcp::Json {
        file: ".amp/settings.json",
        servers: &["amp.mcpServers"],
        entry: Entry::Plain,
    },
    hooks: None,
    skills_dir: Some(".agents/skills"),
    setup_note: Some("approve the workspace server: `amp mcp approve chitchat`."),
    verified: false,
};

/// Kiro: MCP only (its hooks don't inject context).
pub static KIRO: Harness = Harness {
    id: "kiro",
    name: "Kiro",
    binaries: &["kiro-cli", "kiro"],
    identity: Identity::Process,
    hook_pid_env: None,
    mcp_session_env: None,
    mcp: Mcp::Json {
        file: ".kiro/settings/mcp.json",
        servers: &["mcpServers"],
        entry: Entry::Plain,
    },
    hooks: None,
    skills_dir: None,
    setup_note: None,
    verified: false,
};

/// Crush: MCP only.
pub static CRUSH: Harness = Harness {
    id: "crush",
    name: "Crush",
    binaries: &["crush"],
    identity: Identity::Process,
    hook_pid_env: None,
    mcp_session_env: None,
    mcp: Mcp::Json {
        file: ".crush.json",
        servers: &["mcp"],
        entry: Entry::Typed("stdio"),
    },
    hooks: None,
    skills_dir: Some(".agents/skills"),
    setup_note: None,
    verified: false,
};

/// Zed's agent: MCP only, as a context server.
pub static ZED: Harness = Harness {
    id: "zed",
    name: "Zed",
    binaries: &["zed"],
    identity: Identity::Process,
    hook_pid_env: None,
    mcp_session_env: None,
    mcp: Mcp::Json {
        file: ".zed/settings.json",
        servers: &["context_servers"],
        entry: Entry::Plain,
    },
    hooks: None,
    skills_dir: Some(".agents/skills"),
    setup_note: None,
    verified: false,
};

/// Hermes Agent (Nous Research): MCP servers are configured per user only.
pub static HERMES: Harness = Harness {
    id: "hermes",
    name: "Hermes Agent",
    binaries: &["hermes"],
    identity: Identity::Process,
    hook_pid_env: None,
    mcp_session_env: None,
    mcp: Mcp::Manual {
        file: "~/.hermes/config.yaml",
    },
    hooks: None,
    skills_dir: None,
    setup_note: Some(
        "Hermes has no per-project config: add the snippet above to ~/.hermes/config.yaml, then /reload-mcp.",
    ),
    verified: false,
};

/// pi: no MCP by design. Agents use `chitchat tool` from pi's bash tool, as the
/// installed skill explains; commands pi runs are its children, so identity works.
pub static PI: Harness = Harness {
    id: "pi",
    name: "pi",
    binaries: &["pi"],
    identity: Identity::Process,
    hook_pid_env: None,
    mcp_session_env: None,
    mcp: Mcp::None,
    hooks: None,
    skills_dir: Some(".agents/skills"),
    setup_note: Some(
        "pi has no MCP: its agents use `chitchat tool … --client pi` from bash (see the skill). Trust this project in pi so it loads project skills.",
    ),
    verified: false,
};

/// Every supported harness, in the order `init`, `doctor` and `clients` list them.
pub static ALL: &[&Harness] = &[
    &CLAUDE, &CODEX, &GEMINI, &COPILOT, &CURSOR, &QWEN, &DROID, &OPENCODE, &AMP, &KIRO, &CRUSH,
    &ZED, &HERMES, &PI,
];

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
        ALL.iter().copied().find(|h| {
            h.binaries
                .iter()
                .any(|b| name == *b || name.starts_with(&format!("{b}-")))
        })
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
    fn config_paths_stay_inside_the_workspace_and_hooks_are_complete() {
        for h in ALL {
            let mut paths = vec![];
            if let Some(hooks) = &h.hooks {
                paths.push(hooks.file);
                assert!(!hooks.events.is_empty(), "{}", h.id);
                assert!(
                    hooks.event_name(HookEvent::SessionStart).is_some(),
                    "{}",
                    h.id
                );
            }
            if let Mcp::Json { file, .. } | Mcp::TomlBlock { file } = &h.mcp {
                paths.push(file);
            }
            paths.extend(h.skills_dir);
            for p in paths {
                assert!(
                    !p.starts_with('/') && !p.starts_with('~') && !p.contains(".."),
                    "{}: {p}",
                    h.id
                );
            }
            // No harness other than Claude Code may write Claude Code's files.
            if h.id != "claude" {
                assert!(!paths_touch(h, ".claude/settings"), "{}", h.id);
                assert!(!paths_touch(h, ".mcp.json"), "{}", h.id);
            }
        }
    }

    fn paths_touch(h: &Harness, prefix: &str) -> bool {
        let hook = h.hooks.as_ref().is_some_and(|x| x.file.starts_with(prefix));
        let mcp = matches!(&h.mcp, Mcp::Json { file, .. } if file.starts_with(prefix));
        hook || mcp
    }

    #[test]
    fn parse_is_case_insensitive_and_lists_ids_on_error() {
        assert_eq!(parse("Claude").unwrap().id, "claude");
        let err = parse("nope").unwrap_err();
        assert!(err.contains("claude") && err.contains("pi"), "{err}");
    }

    #[test]
    fn process_names_map_to_harnesses_exactly() {
        assert_eq!(
            Harness::from_process_name("claude").map(|h| h.id),
            Some("claude")
        );
        assert_eq!(
            Harness::from_process_name("cursor-agent").map(|h| h.id),
            Some("cursor")
        );
        // "pi" must not match unrelated processes that merely contain it.
        assert_eq!(Harness::from_process_name("pip"), None);
        assert_eq!(Harness::from_process_name("node"), None);
    }
}
