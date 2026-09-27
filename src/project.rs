//! Workspaces: which chitchat project a directory belongs to.
//!
//! A workspace is a directory where `chitchat init` wrote `.chitchat/workspace.json`
//! (a random id and a name). Everything below it belongs to that workspace, and so
//! does every linked git worktree of the repository it lives in: agents working in
//! parallel worktrees share one chat and one memory. Directories outside any
//! workspace have no project, and chitchat stays out of the way there.
//! `CHITCHAT_PROJECT` names a project explicitly (tests, unusual setups).

use std::hash::{BuildHasher, Hasher};
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

pub const PROJECT_ENV: &str = "CHITCHAT_PROJECT";
pub const MARKER_DIR: &str = ".chitchat";
pub const MARKER_FILE: &str = "workspace.json";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Project {
    /// Stable identity: "ws:<id>" for workspaces.
    pub key: String,
    /// Short human name, e.g. "chitchat".
    pub name: String,
    /// The workspace directory as seen from the agent's worktree: file paths in
    /// claims are relative to it.
    pub root: PathBuf,
}

/// Contents of `.chitchat/workspace.json`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Marker {
    pub id: String,
    pub name: String,
}

impl Marker {
    pub fn new(name: &str) -> Self {
        Marker {
            id: new_id(),
            name: name.to_string(),
        }
    }

    pub fn key(&self) -> String {
        format!("ws:{}", self.id)
    }
}

/// The workspace containing `cwd`, if any.
pub fn detect(cwd: &Path) -> Result<Option<Project>> {
    let cwd = canonical(cwd);

    if let Some(key) = std::env::var(PROJECT_ENV)
        .ok()
        .filter(|k| !k.trim().is_empty())
    {
        let key = key.trim().to_string();
        let root = git_dirs(&cwd).map_or(cwd, |(top, _)| top);
        return Ok(Some(Project {
            name: last_segment(&key),
            key,
            root,
        }));
    }

    Ok(detect_workspace(&cwd))
}

/// The workspace containing `cwd` by its marker (ignoring `CHITCHAT_PROJECT`).
pub fn detect_workspace(cwd: &Path) -> Option<Project> {
    let cwd = canonical(cwd);
    if let Some((dir, marker)) = find_marker(&cwd) {
        return Some(Project {
            key: marker.key(),
            name: marker.name,
            root: dir,
        });
    }

    // A linked worktree doesn't have the (untracked) marker; its main worktree does.
    let (top, main) = linked_worktree(&cwd)?;
    let rel = cwd.strip_prefix(&top).unwrap_or(Path::new(""));
    let mut dir = main.join(rel);
    loop {
        if let Some(marker) = read_marker(&dir) {
            let sub = dir.strip_prefix(&main).unwrap_or(Path::new(""));
            return Some(Project {
                key: marker.key(),
                name: marker.name,
                root: top.join(sub),
            });
        }
        if dir == main || !dir.pop() {
            return None;
        }
    }
}

/// The nearest directory at or above `start` holding a workspace marker.
pub fn find_marker(start: &Path) -> Option<(PathBuf, Marker)> {
    start
        .ancestors()
        .find_map(|dir| read_marker(dir).map(|m| (dir.to_path_buf(), m)))
}

pub fn read_marker(dir: &Path) -> Option<Marker> {
    let text = std::fs::read_to_string(dir.join(MARKER_DIR).join(MARKER_FILE)).ok()?;
    serde_json::from_str(&text).ok()
}

pub fn write_marker(dir: &Path, marker: &Marker) -> Result<()> {
    let path = dir.join(MARKER_DIR).join(MARKER_FILE);
    std::fs::create_dir_all(dir.join(MARKER_DIR))?;
    let mut text = serde_json::to_string_pretty(marker)?;
    text.push('\n');
    std::fs::write(&path, text).with_context(|| format!("writing {}", path.display()))
}

/// 128 random bits as hex. std's hasher keys are randomly seeded per instance,
/// which is plenty for an identifier (not for secrets).
pub fn new_id() -> String {
    let mut out = String::new();
    for salt in [0u64, 1] {
        let mut h = std::collections::hash_map::RandomState::new().build_hasher();
        h.write_u64(salt);
        h.write_u128(
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |d| d.as_nanos()),
        );
        h.write_u32(std::process::id());
        out.push_str(&format!("{:016x}", h.finish()));
    }
    out
}

/// Keys that versions before workspaces used for the repository at `dir`, so
/// `chitchat init` can adopt data those versions recorded.
pub fn legacy_keys(dir: &Path) -> Vec<String> {
    let mut keys = Vec::new();
    let Some((top, common)) = git_dirs(dir) else {
        keys.push(format!("path:{}", dir.display()));
        return keys;
    };
    if let Some(url) = git(&top, &["config", "--get", "remote.origin.url"]) {
        keys.push(normalize_remote(&url));
    }
    let main = common.parent().map_or(top, Path::to_path_buf);
    keys.push(format!("path:{}", main.display()));
    keys
}

/// For a linked worktree: (its root, the main worktree's root).
pub fn linked_worktree(cwd: &Path) -> Option<(PathBuf, PathBuf)> {
    let (top, common) = git_dirs(cwd)?;
    let main = canonical(common.parent()?);
    (main != top).then_some((top, main))
}

/// Every other worktree of the repository whose main worktree is `main`.
pub fn linked_worktrees(main: &Path) -> Vec<PathBuf> {
    let Some(out) = git(main, &["worktree", "list", "--porcelain"]) else {
        return Vec::new();
    };
    out.lines()
        .filter_map(|l| l.strip_prefix("worktree "))
        .map(|p| canonical(Path::new(p)))
        .filter(|p| p != main && p.exists())
        .collect()
}

/// Returns (worktree root, git common dir) if `cwd` is inside a git work tree.
pub fn git_dirs(cwd: &Path) -> Option<(PathBuf, PathBuf)> {
    let out = git(
        cwd,
        &[
            "rev-parse",
            "--path-format=absolute",
            "--show-toplevel",
            "--git-common-dir",
        ],
    )?;
    let mut lines = out.lines();
    let root = canonical(Path::new(lines.next()?));
    let common_dir = canonical(Path::new(lines.next()?));
    Some((root, common_dir))
}

pub fn canonical(path: &Path) -> PathBuf {
    path.canonicalize().unwrap_or_else(|_| path.to_path_buf())
}

fn git(dir: &Path, args: &[&str]) -> Option<String> {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let text = String::from_utf8(out.stdout).ok()?.trim().to_string();
    (!text.is_empty()).then_some(text)
}

/// Normalizes a git remote URL to "host/path", so that HTTPS, SSH and scp-style
/// URLs for the same repository agree. Credentials and ports are dropped, which
/// also keeps tokens embedded in remote URLs out of the database.
pub fn normalize_remote(url: &str) -> String {
    let url = url.trim();
    let (host, path) = match url.split_once("://") {
        Some((_scheme, rest)) => rest.split_once('/').unwrap_or((rest, "")),
        // scp-like syntax: [user@]host:path
        None => match url.split_once(':') {
            Some((host, path)) if !host.contains('/') => (host, path),
            _ => {
                return url
                    .trim_end_matches('/')
                    .trim_end_matches(".git")
                    .to_string();
            }
        },
    };
    let host = host.rsplit_once('@').map_or(host, |(_, h)| h);
    let host = host.split_once(':').map_or(host, |(h, _port)| h);
    let path = path.trim_matches('/');
    let path = path.strip_suffix(".git").unwrap_or(path);
    format!("{}/{}", host.to_ascii_lowercase(), path)
}

pub fn last_segment(key: &str) -> String {
    key.trim_end_matches('/')
        .rsplit(['/', '\\', ':'])
        .find(|s| !s.is_empty())
        .unwrap_or(key)
        .to_string()
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    pub fn git_in(dir: &Path, args: &[&str]) {
        let ok = Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(["-c", "user.name=t", "-c", "user.email=t@t"])
            .args(args)
            .output()
            .unwrap()
            .status
            .success();
        assert!(ok, "git {args:?}");
    }

    #[test]
    fn remote_forms_normalize_to_the_same_key() {
        for url in [
            "https://github.com/cyoab/chitchat.git",
            "https://github.com/cyoab/chitchat",
            "https://github.com/cyoab/chitchat/",
            "https://user:ghp_secret@github.com/cyoab/chitchat.git",
            "git@github.com:cyoab/chitchat.git",
            "ssh://git@github.com/cyoab/chitchat",
            "ssh://git@GitHub.com:22/cyoab/chitchat.git",
        ] {
            assert_eq!(normalize_remote(url), "github.com/cyoab/chitchat", "{url}");
        }
    }

    #[test]
    fn name_is_the_last_path_segment() {
        assert_eq!(last_segment("github.com/cyoab/chitchat"), "chitchat");
        assert_eq!(last_segment("path:/Users/me/code/app"), "app");
    }

    #[test]
    fn ids_are_unique() {
        let (a, b) = (new_id(), new_id());
        assert_eq!(a.len(), 32);
        assert_ne!(a, b);
    }

    #[test]
    fn directories_outside_a_workspace_have_no_project() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(detect(dir.path()).unwrap(), None);
    }

    #[test]
    fn subdirectories_and_linked_worktrees_share_the_workspace() {
        let dir = tempfile::tempdir().unwrap();
        let main = canonical(dir.path()).join("main");
        std::fs::create_dir_all(main.join("app/src")).unwrap();
        git_in(&main, &["init", "-q"]);
        git_in(&main, &["commit", "-q", "--allow-empty", "-m", "init"]);
        // The workspace is a subdirectory of the repo ("by directory").
        let marker = Marker::new("app");
        write_marker(&main.join("app"), &marker).unwrap();
        std::fs::write(main.join("app/src/.keep"), "").unwrap();
        git_in(&main, &["add", "app/src/.keep"]);
        git_in(&main, &["commit", "-q", "-m", "src"]);
        let linked = canonical(dir.path()).join("linked");
        git_in(&main, &["worktree", "add", "-q", linked.to_str().unwrap()]);

        let from_sub = detect(&main.join("app/src")).unwrap().unwrap();
        assert_eq!(from_sub.key, marker.key());
        assert_eq!(from_sub.root, main.join("app"));

        let from_linked = detect(&linked.join("app/src")).unwrap().unwrap();
        assert_eq!(from_linked.key, marker.key());
        assert_eq!(from_linked.root, linked.join("app"));

        // The repo root itself is outside the workspace.
        assert_eq!(detect(&main).unwrap(), None);
        assert_eq!(linked_worktrees(&main), [linked]);
    }
}
