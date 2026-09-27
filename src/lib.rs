//! chitchat: shared memory and a project group chat for Claude Code and Codex
//! agents. Every agent runs its own `chitchat mcp` (stdio) process and hooks run
//! `chitchat hook`; all of them share one SQLite database. See
//! `docs/research/prior-art.md` for the design.

pub mod cli;
pub mod db;
pub mod doctor;
pub mod hook;
pub mod mcp;
pub mod paths;
pub mod project;
pub mod session;
