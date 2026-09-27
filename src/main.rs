use std::process::ExitCode;

use anyhow::Result;
use clap::Parser;
use tracing::level_filters::LevelFilter;

use chitchat::cli::{Cli, Command};
use chitchat::{human, workspace};

fn main() -> ExitCode {
    // Rust ignores SIGPIPE, so printing to a closed pipe (`chitchat tail | head`)
    // would panic. Exit quietly instead, like other command-line tools.
    #[cfg(unix)]
    // SAFETY: restores the default disposition before any threads are started.
    unsafe {
        libc::signal(libc::SIGPIPE, libc::SIG_DFL);
    }
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
        Command::Update {
            check,
            version,
            auto,
        } => chitchat::update::run(check, version.as_deref(), auto),
        Command::Import { from, dry_run } => chitchat::import::run(from.0, dry_run),
        Command::Mcp { client } => chitchat::mcp::run(client),
        Command::Tool {
            name,
            args,
            client,
            list,
        } => chitchat::mcp::run_tool(client, name.as_deref(), args.as_deref(), list),
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
        Command::Init {
            path,
            clients,
            name,
            no_import,
            no_stop_hook,
        } => workspace::init(
            path.as_deref(),
            &workspace::InitOptions {
                clients,
                import: !no_import,
                stop_hook: !no_stop_hook,
                name,
            },
        ),
        Command::Deinit { path, clients } => workspace::deinit(path.as_deref(), &clients),
        Command::Workspaces => workspace::list(),
        Command::Clients => workspace::list_clients(),
        Command::Backup { out, list } => human::backup(out.as_deref(), list),
        Command::Restore { backup } => human::restore(&backup),
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
