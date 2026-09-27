//! Working out which project an agent belongs to.
//!
//! Agents running in parallel usually sit in different git worktrees of the same
//! repository, so the project key is derived from the `origin` remote when there
//! is one, and otherwise from the main worktree (via the git common dir). Both are
//! the same for every worktree. `CHITCHAT_PROJECT` overrides detection.

use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::Result;

pub const PROJECT_ENV: &str = "CHITCHAT_PROJECT";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Project {
    /// Stable identity shared by every worktree: "github.com/owner/repo" or "path:/abs/root".
    pub key: String,
    /// Short human name, e.g. "chitchat".
    pub name: String,
    /// Root of the worktree the agent is running in.
    pub root: PathBuf,
}

pub fn detect(cwd: &Path) -> Result<Project> {
    let cwd = cwd.canonicalize().unwrap_or_else(|_| cwd.to_path_buf());

    if let Some(key) = std::env::var(PROJECT_ENV)
        .ok()
        .filter(|k| !k.trim().is_empty())
    {
        let key = key.trim().to_string();
        return Ok(Project {
            name: last_segment(&key),
            key,
            root: cwd,
        });
    }

    let Some((root, common_dir)) = git_dirs(&cwd) else {
        return Ok(Project {
            key: format!("path:{}", cwd.display()),
            name: last_segment(&cwd.to_string_lossy()),
            root: cwd,
        });
    };

    let key = match git(&root, &["config", "--get", "remote.origin.url"]).as_deref() {
        Some(url) => normalize_remote(url),
        None => {
            // No remote: identify the repo by its main worktree, which owns the common dir.
            let main_root = common_dir
                .parent()
                .map_or_else(|| root.clone(), Path::to_path_buf);
            format!("path:{}", main_root.display())
        }
    };
    Ok(Project {
        name: last_segment(&key),
        key,
        root,
    })
}

/// Returns (worktree root, git common dir) if `cwd` is inside a git work tree.
fn git_dirs(cwd: &Path) -> Option<(PathBuf, PathBuf)> {
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
    let root = PathBuf::from(lines.next()?);
    let common_dir = PathBuf::from(lines.next()?);
    Some((root, common_dir))
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

fn last_segment(key: &str) -> String {
    key.trim_end_matches('/')
        .rsplit(['/', '\\', ':'])
        .find(|s| !s.is_empty())
        .unwrap_or(key)
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

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
    fn non_git_directory_is_keyed_by_path() {
        let dir = tempfile::tempdir().unwrap();
        let project = detect(dir.path()).unwrap();
        assert!(project.key.starts_with("path:"), "{}", project.key);
    }

    #[test]
    fn worktrees_share_a_key_without_a_remote() {
        let dir = tempfile::tempdir().unwrap();
        let main = dir.path().join("main");
        std::fs::create_dir(&main).unwrap();
        let run = |args: &[&str]| {
            let ok = Command::new("git")
                .arg("-C")
                .arg(&main)
                .args(args)
                .output()
                .unwrap()
                .status
                .success();
            assert!(ok, "git {args:?}");
        };
        run(&["init", "-q"]);
        run(&[
            "-c",
            "user.name=t",
            "-c",
            "user.email=t@t",
            "commit",
            "-q",
            "--allow-empty",
            "-m",
            "init",
        ]);
        let linked = dir.path().join("linked");
        run(&["worktree", "add", "-q", linked.to_str().unwrap()]);

        let a = detect(&main).unwrap();
        let b = detect(&linked).unwrap();
        assert_eq!(a.key, b.key);
        assert_ne!(a.root, b.root);
    }
}
