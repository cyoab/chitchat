use std::path::PathBuf;

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
    /// Post a message to the project chat as @user.
    Post {
        message: String,
        /// Send directly to one agent (e.g. "@codex-1") instead of the room.
        #[arg(long)]
        to: Option<String>,
        /// Room to post in (default "general").
        #[arg(long)]
        room: Option<String>,
        /// Ask for an answer: recipients are reminded until they reply.
        #[arg(long)]
        request: bool,
        /// Answer message #ID (goes to the same room or person).
        #[arg(long, value_name = "ID")]
        reply_to: Option<i64>,
    },
    /// Show the project chat and follow new messages (Ctrl-C to stop).
    Tail {
        /// Only this room.
        #[arg(long)]
        room: Option<String>,
        /// How many past messages to show first.
        #[arg(short = 'n', long, default_value_t = 20)]
        lines: usize,
        /// Print the history and exit instead of following.
        #[arg(long)]
        no_follow: bool,
    },
    /// Show who is in this project, what they're doing and what they've claimed.
    Who,
    /// Search shared notes and project docs (no query: list recent notes).
    Notes {
        query: Vec<String>,
        /// Only notes of this kind.
        #[arg(long)]
        kind: Option<String>,
        /// Also search chat history.
        #[arg(long)]
        messages: bool,
        #[arg(long, default_value_t = 20)]
        limit: usize,
    },
    /// Show one note in full.
    Note {
        key: String,
        /// Include previous revisions.
        #[arg(long)]
        history: bool,
    },
    /// Delete a note (soft: it stays in history).
    Forget { key: String },
    /// Write shared notes as Markdown (default: <repo>/.chitchat/notes).
    Export {
        #[arg(long)]
        dir: Option<PathBuf>,
    },
    /// Make this directory a chitchat workspace and set up Claude Code and Codex
    /// for it (and for its git worktrees). Safe to run again.
    Init {
        /// Workspace directory (default: current directory).
        path: Option<PathBuf>,
        /// Only set up this client (repeatable). Default: every installed client.
        #[arg(long = "client", value_enum)]
        clients: Vec<Client>,
        /// Workspace name (default: the directory name).
        #[arg(long)]
        name: Option<String>,
        /// Don't import Claude Code's existing memory files as shared notes.
        #[arg(long)]
        no_import: bool,
        /// Skip the Stop hook that reminds agents to answer pending requests.
        #[arg(long)]
        no_stop_hook: bool,
    },
    /// Turn chitchat off for this workspace (keeps its chat and notes).
    Deinit {
        path: Option<PathBuf>,
        #[arg(long = "client", value_enum)]
        clients: Vec<Client>,
    },
    /// List every chitchat workspace on this machine.
    Workspaces,
    /// Back up the chitchat database (default: ~/.chitchat/backups/).
    Backup {
        /// Write the backup here instead.
        #[arg(long)]
        out: Option<PathBuf>,
        /// List existing backups instead of making one.
        #[arg(long)]
        list: bool,
    },
    /// Restore the database from a backup file, or "latest". The current database
    /// is backed up first.
    Restore { backup: String },
    /// Show where data lives and what chitchat detects in this directory.
    Doctor,
}
