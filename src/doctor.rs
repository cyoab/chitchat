//! `chitchat doctor`: shows where data lives and what chitchat detects here.

use anyhow::Result;

use crate::clients;
use crate::harness;
use crate::session::SessionHint;

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
    if let Some(update) = crate::update::status() {
        println!("updates   {update}");
    }
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
    match (session.harness, session.session_id) {
        (Some(h), Some(id)) => println!("session   {} {id}", h.id),
        (Some(h), None) => println!("session   {} (no session id)", h.id),
        _ => println!("session   none detected (not running inside an agent)"),
    }

    let Some(project) = project else {
        println!("workspace none here: run `chitchat init` to set one up");
        return Ok(());
    };
    println!("workspace {} [{}]", project.name, project.key);
    println!("root      {}", project.root.display());
    for h in harness::ALL {
        let s = clients::status(&project.root, h);
        if !h.available() && !s.configured() {
            continue;
        }
        let installed = if h.available() { "" } else { " (not on PATH)" };
        let hooks = if s.hook_events == 0 {
            "no hook support".to_string()
        } else {
            format!("hooks {}/{}", s.hooks, s.hook_events)
        };
        let skill = match s.skill {
            Some(true) => "; skill installed",
            Some(false) => "; skill missing",
            None => "",
        };
        println!(
            "{:<9} MCP server {}; {hooks}{skill}{installed}",
            h.id,
            if s.mcp {
                "configured"
            } else {
                "not configured"
            },
        );
    }
    Ok(())
}
