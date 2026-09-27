//! Who is on the other end of a `chitchat mcp` or `chitchat hook` process.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

/// A client chitchat integrates with (what `--client` accepts).
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum Client {
    Claude,
    Codex,
}

impl From<Client> for Vendor {
    fn from(client: Client) -> Self {
        match client {
            Client::Claude => Vendor::Claude,
            Client::Codex => Vendor::Codex,
        }
    }
}

/// Who a participant is. Stored in `agents.vendor`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Vendor {
    Claude,
    Codex,
    Human,
}

impl Vendor {
    pub fn as_str(self) -> &'static str {
        match self {
            Vendor::Claude => "claude",
            Vendor::Codex => "codex",
            Vendor::Human => "human",
        }
    }
}

/// What the environment tells us about the calling agent session.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SessionHint {
    pub vendor: Option<Vendor>,
    pub session_id: Option<String>,
    pub project_dir: Option<PathBuf>,
}

impl SessionHint {
    /// Claude Code passes its session id and project dir to the stdio servers it
    /// spawns. Codex passes nothing comparable to MCP servers, so Codex sessions are
    /// identified by `--client codex` in their config plus per-call metadata.
    pub fn from_env() -> Self {
        let var = |name: &str| std::env::var(name).ok().filter(|v| !v.is_empty());
        let claude_session = var("CLAUDE_CODE_SESSION_ID");
        let is_claude = claude_session.is_some() || var("CLAUDECODE").as_deref() == Some("1");
        SessionHint {
            vendor: is_claude.then_some(Vendor::Claude),
            session_id: claude_session,
            project_dir: var("CLAUDE_PROJECT_DIR").map(PathBuf::from),
        }
    }
}

/// Best-effort vendor from the client process name ("claude", "codex", ...).
pub fn vendor_from_process(name: &str) -> Option<Vendor> {
    let name = name.to_ascii_lowercase();
    if name.contains("claude") {
        Some(Vendor::Claude)
    } else if name.contains("codex") {
        Some(Vendor::Codex)
    } else {
        None
    }
}
