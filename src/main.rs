use std::process::ExitCode;

use anyhow::Result;
use clap::Parser;
use tracing::level_filters::LevelFilter;

use chitchat::cli::{Cli, Command};
use chitchat::{human, install};

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
        Command::Post {
            message,
            to,
            room,
            request,
            reply_to,
        } => human::post(&message, to, room, request, reply_to),
        Command::Tail {
            room,
            lines,
            no_follow,
        } => human::tail(room.as_deref(), lines, !no_follow),
        Command::Who => human::who(),
        Command::Notes {
            query,
            kind,
            messages,
            limit,
        } => human::notes(&query.join(" "), kind.as_deref(), messages, limit),
        Command::Note { key, history } => human::note(&key, history),
        Command::Forget { key } => human::forget(&key),
        Command::Export { dir } => human::export(dir),
        Command::Install {
            client,
            dry_run,
            no_stop_hook,
        } => install::install(client, dry_run, !no_stop_hook),
        Command::Uninstall { client, dry_run } => install::uninstall(client, dry_run),
        Command::Doctor => chitchat::doctor::run(),
    }
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
