//! Release updates. Network and replacement work happen only in `update`, never
//! on the MCP startup path. All updater output goes to stderr.

use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::{Context, Result, bail};
use semver::Version;
use serde::{Deserialize, Serialize};

const RELEASES: &str = "https://github.com/cyoab/chitchat/releases";
const DAY_MS: i64 = 24 * 60 * 60 * 1000;

#[derive(Debug, Default, Serialize, Deserialize)]
struct State {
    checked_at: i64,
    latest: Option<String>,
    outcome: String,
}

fn state(home: &Path) -> Option<State> {
    serde_json::from_slice(&std::fs::read(home.join("update.json")).ok()?).ok()
}

fn due(home: &Path, now: i64) -> bool {
    state(home)
        .is_none_or(|s| s.checked_at <= 0 || now < s.checked_at || now - s.checked_at >= DAY_MS)
}

fn write_state(home: &Path, state: &State) -> Result<()> {
    let temp = home.join("update.json.tmp");
    std::fs::write(&temp, serde_json::to_vec(state)?)?;
    std::fs::rename(temp, home.join("update.json"))?;
    Ok(())
}

/// A human-readable status for `doctor`, without triggering a check.
pub fn status() -> Option<String> {
    let home = crate::paths::home().ok()?;
    let enabled =
        !cfg!(debug_assertions) && std::env::var("CHITCHAT_AUTO_UPDATE").as_deref() != Ok("0");
    Some(match state(&home) {
        Some(s) => format!(
            "auto {}; checked {}: {}",
            if enabled { "on" } else { "off" },
            crate::format::ago(s.checked_at),
            s.outcome
        ),
        None => format!("auto {}; never checked", if enabled { "on" } else { "off" }),
    })
}

/// Best effort and nonblocking. Debug builds never update themselves; integration
/// tests also explicitly set CHITCHAT_AUTO_UPDATE=0.
pub fn spawn_auto_check() {
    if cfg!(debug_assertions) || std::env::var("CHITCHAT_AUTO_UPDATE").as_deref() == Ok("0") {
        return;
    }
    if let Err(e) = spawn() {
        tracing::debug!("could not start update check: {e:#}");
    }
}

fn spawn() -> Result<()> {
    let home = crate::paths::home()?;
    if !due(&home, crate::db::now_ms()) {
        return Ok(());
    }
    let mut cmd = Command::new(std::env::current_exe()?);
    cmd.args(["update", "--auto"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        // Only an async-signal-safe syscall is used after fork.
        unsafe {
            cmd.pre_exec(|| {
                if libc::setsid() == -1 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
    }
    let mut child = cmd.spawn().context("starting updater")?;
    // Reap the child while MCP is alive; the child can also outlive this process.
    std::thread::spawn(move || {
        let _ = child.wait();
    });
    Ok(())
}

pub fn run(check: bool, requested: Option<&str>, auto: bool) -> Result<()> {
    if auto && std::env::var("CHITCHAT_AUTO_UPDATE").as_deref() == Ok("0") {
        return Ok(());
    }
    // Reject malformed explicit versions before doing I/O.
    if let Some(tag) = requested {
        parse_version(tag)?;
    }
    let home = crate::paths::home()?;
    let mut builder = std::fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    builder.create(&home)?;
    let lock = OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(home.join("update.lock"))?;
    match lock.try_lock() {
        Ok(()) => {}
        Err(std::fs::TryLockError::WouldBlock) if auto => return Ok(()),
        Err(std::fs::TryLockError::WouldBlock) => {
            bail!("another update is in progress; try again shortly")
        }
        Err(e) => return Err(e.into()),
    }
    let now = crate::db::now_ms();
    if auto && !due(&home, now) {
        return Ok(());
    }
    let mut progress = State {
        checked_at: now,
        latest: None,
        outcome: "check started".into(),
    };
    // Failed checks are throttled too, including a killed download process.
    write_state(&home, &progress)?;
    let result = update(check, requested, &mut progress);
    if let Err(e) = &result {
        progress.outcome = format!("failed: {e:#}");
    }
    write_state(&home, &progress)?;
    result
}

fn update(check: bool, requested: Option<&str>, progress: &mut State) -> Result<()> {
    let tag = match requested {
        Some(v) => {
            if v.starts_with('v') {
                v.to_string()
            } else {
                format!("v{v}")
            }
        }
        None => latest_tag()?,
    };
    let version = parse_version(&tag)?;
    progress.latest = Some(tag.clone());
    let executable = std::env::current_exe()?.canonicalize()?;
    // A different updater may have replaced the binary while this process was
    // starting. Compare against the installed binary under the update lock.
    let current = installed_version(&executable)?;
    if requested.is_none() && version.cmp_precedence(&current).is_le() || version == current {
        progress.outcome = format!("up to date (installed {current}, latest {version})");
    } else if check {
        progress.outcome = format!("available {version} (installed {current})");
    } else {
        let asset = format!("chitchat-{}.tar.gz", target()?);
        let stage = Stage::new(
            executable
                .parent()
                .context("binary has no parent directory")?,
        )?;
        let archive = stage.0.join(&asset);
        let checksum = stage.0.join(format!("{asset}.sha256"));
        let base = format!("{RELEASES}/download/{tag}");
        download(&format!("{base}/{asset}"), &archive)?;
        download(&format!("{base}/{asset}.sha256"), &checksum)?;
        install_archive(&archive, &checksum, &asset, &executable, &stage.0, &version)?;
        progress.outcome = format!(
            "installed {version} (was {current}); running sessions keep their current version until restarted"
        );
    }
    eprintln!("chitchat: {}", progress.outcome);
    Ok(())
}

fn parse_version(tag: &str) -> Result<Version> {
    // Version parsing also excludes URL separators, whitespace and shell syntax.
    Version::parse(tag.strip_prefix('v').unwrap_or(tag))
        .context("expected a semantic version such as v0.2.0")
}

fn latest_tag() -> Result<String> {
    let output = curl()
        .args([
            "--output",
            "/dev/null",
            "--write-out",
            "%{url_effective}",
            &format!("{RELEASES}/latest"),
        ])
        .output()
        .context("starting curl (required for updates)")?;
    let redirect = output_text(output, "looking up the latest release")?;
    let tag = redirect
        .trim()
        .strip_prefix(&format!("{RELEASES}/tag/"))
        .context("GitHub did not redirect to a release tag")?;
    parse_version(tag)?;
    Ok(tag.to_string())
}

fn curl() -> Command {
    let mut cmd = Command::new("curl");
    cmd.args([
        "--fail",
        "--silent",
        "--show-error",
        "--location",
        "--proto",
        "=https",
        "--proto-redir",
        "=https",
        "--connect-timeout",
        "10",
        "--max-time",
        "120",
        "--retry",
        "2",
        "--retry-max-time",
        "180",
    ]);
    cmd.stdin(Stdio::null());
    cmd
}

fn download(url: &str, destination: &Path) -> Result<()> {
    let output = curl()
        .arg("--output")
        .arg(destination)
        .arg(url)
        .output()
        .context("starting curl (required for updates)")?;
    output_text(output, "downloading release")?;
    Ok(())
}

fn output_text(output: std::process::Output, action: &str) -> Result<String> {
    if !output.status.success() {
        bail!(
            "{action}: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    String::from_utf8(output.stdout).with_context(|| format!("{action}: non-UTF-8 output"))
}

fn target() -> Result<String> {
    let os = match std::env::consts::OS {
        "macos" => "apple-darwin",
        "linux" => "unknown-linux-musl",
        os => bail!("no release binary for {os}; build from source"),
    };
    let arch = std::env::consts::ARCH;
    if !matches!(arch, "aarch64" | "x86_64") {
        bail!("no release binary for {arch}; build from source");
    }
    Ok(format!("{arch}-{os}"))
}

fn installed_version(binary: &Path) -> Result<Version> {
    let text = output_text(
        Command::new(binary)
            .arg("--version")
            .stdin(Stdio::null())
            .output()
            .context("reading binary version")?,
        "reading binary version",
    )?;
    parse_version(
        text.trim()
            .strip_prefix("chitchat ")
            .context("downloaded binary did not identify itself as chitchat")?,
    )
}

fn verify_checksum(archive: &Path, checksum: &Path, asset: &str) -> Result<()> {
    let text = std::fs::read_to_string(checksum)?;
    let fields: Vec<_> = text.split_whitespace().collect();
    if fields.len() != 2
        || fields[1].trim_start_matches('*') != asset
        || fields[0].len() != 64
        || !fields[0].bytes().all(|b| b.is_ascii_hexdigit())
    {
        bail!("invalid SHA-256 file for {asset}");
    }
    let output = match Command::new("sha256sum").arg(archive).output() {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Command::new("shasum")
            .args(["-a", "256"])
            .arg(archive)
            .output()
            .context("need sha256sum or shasum to verify updates")?,
        result => result.context("running sha256sum")?,
    };
    let actual = output_text(output, "hashing download")?;
    if !actual
        .split_whitespace()
        .next()
        .is_some_and(|h| h.eq_ignore_ascii_case(fields[0]))
    {
        bail!("SHA-256 mismatch; installed binary was not changed");
    }
    Ok(())
}

fn install_archive(
    archive: &Path,
    checksum: &Path,
    asset: &str,
    executable: &Path,
    stage: &Path,
    expected: &Version,
) -> Result<()> {
    verify_checksum(archive, checksum, asset)?;
    let listing = output_text(
        Command::new("tar")
            .arg("-tzf")
            .arg(archive)
            .output()
            .context("starting tar")?,
        "reading archive",
    )?;
    if listing.lines().collect::<Vec<_>>() != ["chitchat"] {
        bail!("release archive must contain only chitchat");
    }
    let candidate = stage.join("chitchat");
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&candidate)?;
    // Extract only to an already-open file: archive paths/symlinks are never
    // materialized on disk. A malicious archive cannot write outside staging.
    let output = Command::new("tar")
        .arg("-xOzf")
        .arg(archive)
        .arg("chitchat")
        .stdout(Stdio::from(file.try_clone()?))
        .stderr(Stdio::piped())
        .output()?;
    output_text(output, "extracting binary")?;
    file.flush()?;
    if file.metadata()?.len() == 0 {
        bail!("release binary is empty");
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        file.set_permissions(std::fs::Permissions::from_mode(0o755))?;
    }
    file.sync_all()?;
    drop(file); // Linux cannot execute a binary with an open writable handle.
    if installed_version(&candidate)? != *expected {
        bail!("release binary version does not match the requested version");
    }
    std::fs::rename(&candidate, executable)
        .context("replacing installed binary (check directory permissions)")?;
    File::open(executable.parent().context("binary has no parent")?)?.sync_all()?;
    Ok(())
}

/// Private staging on the destination filesystem makes the final rename atomic.
struct Stage(PathBuf);
impl Stage {
    fn new(parent: &Path) -> Result<Self> {
        for attempt in 0..100 {
            let dir = parent.join(format!(
                ".chitchat-update-{}-{}-{attempt}",
                std::process::id(),
                crate::db::now_ms()
            ));
            let mut builder = std::fs::DirBuilder::new();
            #[cfg(unix)]
            {
                use std::os::unix::fs::DirBuilderExt;
                builder.mode(0o700);
            }
            match builder.create(&dir) {
                Ok(()) => return Ok(Self(dir)),
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(e) => {
                    return Err(e).context("creating update staging directory next to binary");
                }
            }
        }
        bail!("could not create update staging directory")
    }
}
impl Drop for Stage {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn release_precedence_uses_semver_and_ignores_build_metadata() {
        for (older, newer) in [
            ("v0.2.0", "v0.10.0"),
            ("1.0.0-rc.2", "1.0.0-rc.10"),
            ("1.0.0-rc.10", "1.0.0"),
        ] {
            assert!(
                parse_version(older)
                    .unwrap()
                    .cmp_precedence(&parse_version(newer).unwrap())
                    .is_lt()
            );
        }
        assert!(
            parse_version("1.0.0+one")
                .unwrap()
                .cmp_precedence(&parse_version("1.0.0+two").unwrap())
                .is_eq()
        );
        for invalid in ["latest", "v1.2", "01.2.3", "1.2.3/../../x"] {
            assert!(parse_version(invalid).is_err());
        }
    }

    #[test]
    fn checks_recover_from_missing_corrupt_or_future_stamps() {
        let dir = tempfile::tempdir().unwrap();
        let now = DAY_MS * 3;
        assert!(due(dir.path(), now));
        std::fs::write(dir.path().join("update.json"), "interrupted").unwrap();
        assert!(due(dir.path(), now));
        write_state(
            dir.path(),
            &State {
                checked_at: now,
                ..Default::default()
            },
        )
        .unwrap();
        assert!(!due(dir.path(), now + DAY_MS - 1));
        assert!(due(dir.path(), now + DAY_MS));
        assert!(due(dir.path(), now - 1));
    }
}
