//! `chitchat doctor`: shows where data lives and what chitchat detects here.

use anyhow::Result;

use crate::clients;
use crate::hook::HookEvent;
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

    println!("chitchat  {}", env!("CARGO_PKG_VERSION"));
    println!("binary    {}", clients::binary_path()?.display());
    println!("database  {}", db_path.display());
    println!("sqlite    {sqlite} (journal: {journal})");
    println!(
        "schema    v{schema} (binary knows v{})",
        crate::db::SCHEMA_VERSION
    );
    let backups = crate::backup::list()?;
    match backups.first() {
        Some(latest) => println!(
            "backups   {} in {} (latest {})",
            backups.len(),
            crate::backup::dir()?.display(),
            latest
                .path
                .file_name()
                .unwrap_or_default()
                .to_string_lossy()
        ),
        None => println!("backups   none yet (`chitchat backup`)"),
    }
    match (session.vendor, session.session_id) {
        (Some(vendor), Some(id)) => println!("session   {} {id}", vendor.as_str()),
        (Some(vendor), None) => println!("session   {} (no session id)", vendor.as_str()),
        _ => println!("session   none detected (not running inside an agent)"),
    }

    let Some(project) = project else {
        println!("workspace none here: run `chitchat init` to set one up");
        return Ok(());
    };
    println!("workspace {} [{}]", project.name, project.key);
    println!("root      {}", project.root.display());
    for client in [Client::Claude, Client::Codex] {
        let s = clients::status(&project.root, client);
        let label = if client == Client::Claude {
            "claude"
        } else {
            "codex "
        };
        let installed = if clients::available(client) {
            ""
        } else {
            " (CLI not on PATH)"
        };
        println!(
            "{label}    MCP server {}; hooks {}/{}{installed}",
            if s.mcp {
                "configured"
            } else {
                "not configured"
            },
            s.hooks,
            HookEvent::ALL.len()
        );
    }
    Ok(())
}
