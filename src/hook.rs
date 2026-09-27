//! `chitchat hook <event>`: the handler Claude Code and Codex hooks invoke.
//!
//! The client writes a JSON payload on stdin. Whatever this command prints on
//! stdout is injected into the agent's context, so it prints nothing unless it has
//! something for the agent; diagnostics go to stderr via `tracing`. A hook must
//! never block the agent, so every failure is logged and swallowed (exit 0).

use std::io::Read;

use anyhow::{Context, Result};
use serde::Deserialize;

use crate::session::Client;

#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum HookEvent {
    /// Session started or resumed: register the agent, show who else is here.
    SessionStart,
    /// The user sent a prompt: the main point where unread messages are delivered.
    UserPromptSubmit,
    /// A tool call finished: surface urgent items (@mentions, requests) mid-turn.
    PostToolUse,
    /// The agent is about to end its turn: nudge it to answer pending requests.
    Stop,
}

/// Fields common to Claude Code and Codex hook payloads. Unknown fields are ignored.
#[derive(Debug, Default, Deserialize)]
pub struct HookPayload {
    pub session_id: Option<String>,
    pub cwd: Option<String>,
    pub hook_event_name: Option<String>,
    /// Claude Code sets this on Stop when a Stop hook already forced a continuation.
    #[serde(default)]
    pub stop_hook_active: bool,
}

pub fn run(event: HookEvent, client: Client) {
    if let Err(err) = handle(event, client) {
        tracing::warn!(?event, ?client, "hook failed: {err:#}");
    }
}

fn handle(event: HookEvent, client: Client) -> Result<()> {
    let mut input = String::new();
    std::io::stdin()
        .read_to_string(&mut input)
        .context("reading hook payload")?;
    let payload: HookPayload = if input.trim().is_empty() {
        HookPayload::default()
    } else {
        serde_json::from_str(&input).context("parsing hook payload")?
    };
    tracing::debug!(?event, ?client, ?payload, "hook received");

    // Delivery (identity, unread digest, pending requests) lands in milestone 1.
    // Until then every event is a deliberate no-op, which is always safe.
    Ok(())
}
