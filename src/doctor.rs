//! `chitchat doctor`: shows where data lives and what chitchat detects here.

use anyhow::Result;

use crate::session::{Client, SessionHint};

pub fn run() -> Result<()> {
    let db_path = crate::paths::db_path()?;
    let conn = crate::db::open(&db_path)?;
    let sqlite: String = conn.query_row("SELECT sqlite_version()", [], |r| r.get(0))?;
    let journal: String = conn.pragma_query_value(None, "journal_mode", |r| r.get(0))?;
    let schema = crate::db::schema_version(&conn)?;

    let cwd = std::env::current_dir()?;
    let project = crate::project::detect(&cwd)?;
    let session = SessionHint::from_env();

    println!("chitchat {}", env!("CARGO_PKG_VERSION"));
    println!("database  {}", db_path.display());
    println!("sqlite    {sqlite} (journal: {journal})");
    println!(
        "schema    v{schema} (binary knows v{})",
        crate::db::SCHEMA_VERSION
    );
    println!("project   {} [{}]", project.name, project.key);
    println!("worktree  {}", project.root.display());
    match (session.vendor, session.session_id) {
        (Some(vendor), Some(id)) => println!("session   {} {id}", vendor.as_str()),
        (Some(vendor), None) => println!("session   {} (no session id)", vendor.as_str()),
        _ => println!("session   none detected (not running inside an agent)"),
    }
    for client in [Client::Claude, Client::Codex] {
        let label = if client == Client::Claude {
            "claude"
        } else {
            "codex "
        };
        match crate::install::status(client) {
            Ok(s) => {
                let mcp = if s.mcp_registered {
                    "registered"
                } else {
                    "not registered"
                };
                let total = crate::hook::HookEvent::ALL.len();
                println!(
                    "{label}    MCP server {mcp} ({}); hooks {}/{total} ({})",
                    s.mcp_config.display(),
                    s.hooks_installed,
                    s.hooks_file.display()
                );
            }
            Err(e) => println!("{label}    could not check: {e:#}"),
        }
    }
    println!("install   chitchat install claude | chitchat install codex");
    Ok(())
}
