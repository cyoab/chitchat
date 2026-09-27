//! What the environment says about the agent session on the other end of a
//! `chitchat mcp` process.

use std::path::PathBuf;

use crate::harness::{self, Harness};

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SessionHint {
    pub harness: Option<&'static Harness>,
    pub session_id: Option<String>,
    pub project_dir: Option<PathBuf>,
}

impl SessionHint {
    /// Some harnesses tell the stdio servers they spawn who they are: Claude Code
    /// sets `CLAUDECODE=1`, `CLAUDE_CODE_SESSION_ID` and `CLAUDE_PROJECT_DIR`.
    /// Others (Codex) pass nothing comparable and are identified by `--client` in
    /// their config plus per-call metadata.
    pub fn from_env() -> Self {
        let var = |name: &str| std::env::var(name).ok().filter(|v| !v.is_empty());
        for h in harness::ALL {
            if let Some(session) = h.mcp_session_env.and_then(var) {
                return SessionHint {
                    harness: Some(h),
                    session_id: Some(session),
                    project_dir: var("CLAUDE_PROJECT_DIR").map(PathBuf::from),
                };
            }
        }
        let is_claude = var("CLAUDECODE").as_deref() == Some("1");
        SessionHint {
            harness: is_claude.then_some(&harness::CLAUDE),
            session_id: None,
            project_dir: var("CLAUDE_PROJECT_DIR").map(PathBuf::from),
        }
    }
}
