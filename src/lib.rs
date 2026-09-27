//! chitchat: shared memory and a project group chat for Claude Code and Codex
//! agents. Every agent runs its own `chitchat mcp` (stdio) process and hooks run
//! `chitchat hook`; all of them share one SQLite database. See
//! `docs/research/prior-art.md` for the design.

pub mod agents;
pub mod backup;
pub mod chat;
pub mod claims;
pub mod cli;
pub mod clients;
pub mod db;
pub mod digest;
pub mod doctor;
pub mod format;
pub mod hook;
pub mod human;
pub mod mcp;
pub mod memory;
pub mod paths;
pub mod procs;
pub mod project;
pub mod session;
pub mod workspace;
