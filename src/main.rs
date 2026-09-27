use std::process::ExitCode;

use anyhow::{Result, bail};
use clap::Parser;
use tracing::level_filters::LevelFilter;

use chitchat::cli::{Cli, Command};

fn main() -> ExitCode {
    init_logging();
    match run(Cli::parse()) {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("chitchat: {err:#}");
            ExitCode::FAILURE
        }
    }
}

fn run(cli: Cli) -> Result<()> {
    match cli.command {
        Command::Mcp { client } => chitchat::mcp::run(client),
        Command::Hook { event, client } => {
            chitchat::hook::run(event, client);
            Ok(())
        }
        Command::Doctor => chitchat::doctor::run(),
        Command::Post { .. } => not_yet("post"),
        Command::Tail { .. } => not_yet("tail"),
        Command::Install { .. } => not_yet("install"),
    }
}

fn not_yet(command: &str) -> Result<()> {
    bail!("`chitchat {command}` is not implemented yet")
}

/// Logs go to stderr: stdout is reserved for MCP JSON-RPC and hook output.
/// Verbosity comes from `CHITCHAT_LOG` (error, warn, info, debug, trace).
fn init_logging() {
    let level = std::env::var("CHITCHAT_LOG")
        .ok()
        .and_then(|v| v.parse::<LevelFilter>().ok())
        .unwrap_or(LevelFilter::WARN);
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_max_level(level)
        .init();
}
