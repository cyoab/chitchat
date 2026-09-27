use clap::{Parser, Subcommand};

use crate::hook::HookEvent;
use crate::session::Client;

/// Shared memory and a project group chat for Claude Code and Codex agents.
#[derive(Debug, Parser)]
#[command(name = "chitchat", version, about)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Command,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Run the MCP server over stdio (what each agent's MCP config launches).
    Mcp {
        /// The client launching this server. Detected automatically for Claude Code.
        #[arg(long, value_enum)]
        client: Option<Client>,
    },
    /// Handle a Claude Code or Codex hook event (reads the JSON payload on stdin).
    Hook {
        #[arg(value_enum)]
        event: HookEvent,
        #[arg(long, value_enum)]
        client: Client,
    },
    /// Post a message to the project chat as yourself.
    Post {
        message: String,
        /// Send directly to one agent (e.g. "@codex-1") instead of the room.
        #[arg(long)]
        to: Option<String>,
        #[arg(long, default_value = "general")]
        room: String,
    },
    /// Follow the project chat in this terminal.
    Tail {
        #[arg(long, default_value = "general")]
        room: String,
    },
    /// Configure Claude Code or Codex (MCP server + hooks) to use chitchat.
    Install {
        #[arg(value_enum)]
        client: Client,
    },
    /// Show where data lives and what chitchat detects in this directory.
    Doctor,
}
