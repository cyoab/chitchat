//! Where chitchat keeps its data.
//!
//! Everything lives under one directory, `~/.chitchat` by default, so that every
//! agent on the machine shares the same database. `CHITCHAT_HOME` overrides it,
//! which tests and throwaway experiments rely on.

use std::path::PathBuf;

use anyhow::{Context, Result};

pub const HOME_ENV: &str = "CHITCHAT_HOME";

pub fn home() -> Result<PathBuf> {
    if let Some(dir) = std::env::var_os(HOME_ENV).filter(|v| !v.is_empty()) {
        return Ok(PathBuf::from(dir));
    }
    let home =
        dirs::home_dir().context("could not determine the home directory; set CHITCHAT_HOME")?;
    Ok(home.join(".chitchat"))
}

pub fn db_path() -> Result<PathBuf> {
    Ok(home()?.join("chitchat.db"))
}
