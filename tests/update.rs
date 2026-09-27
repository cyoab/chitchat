//! Exercise release installation against local fixtures. The updater only ever
//! runs a copy of the test binary, and curl is replaced by a fixture transport.
#![cfg(unix)]

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

struct Fixture {
    temp: tempfile::TempDir,
    binary: PathBuf,
    archive: PathBuf,
    asset: String,
}

fn script(path: &Path, contents: &str) {
    fs::write(path, contents).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
}

impl Fixture {
    fn new() -> Self {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        let bin = root.join("bin");
        fs::create_dir(&bin).unwrap();
        let binary = bin.join("chitchat");
        fs::copy(env!("CARGO_BIN_EXE_chitchat"), &binary).unwrap();
        let os = if cfg!(target_os = "macos") {
            "apple-darwin"
        } else {
            "unknown-linux-musl"
        };
        let asset = format!("chitchat-{}-{os}.tar.gz", std::env::consts::ARCH);
        let archive = root.join(&asset);
        let fake_bin = root.join("transport");
        fs::create_dir(&fake_bin).unwrap();
        script(
            &fake_bin.join("curl"),
            r##"#!/bin/sh
set -eu
printf 'call\n' >> "$UPDATE_FIXTURE/calls"
dest=
last=
while [ "$#" -gt 0 ]; do
  case "$1" in
    --output) shift; dest="$1" ;;
  esac
  last="$1"
  shift
done
case "$last" in
  */latest) if [ "${UPDATE_DELAY:-0}" = 1 ]; then sleep 2; fi; printf 'https://github.com/cyoab/chitchat/releases/tag/v0.2.0' ;;
  *.sha256) cp "$UPDATE_FIXTURE/$UPDATE_ASSET.sha256" "$dest" ;;
  *.tar.gz) cp "$UPDATE_FIXTURE/$UPDATE_ASSET" "$dest" ;;
  *) exit 22 ;;
esac
"##,
        );
        let fixture = Self {
            temp,
            binary,
            archive,
            asset,
        };
        fixture.package("0.2.0", false);
        fixture
    }

    fn package(&self, version: &str, extra_file: bool) {
        let payload = self.temp.path().join("payload");
        fs::create_dir_all(&payload).unwrap();
        script(
            &payload.join("chitchat"),
            &format!("#!/bin/sh\nprintf 'chitchat {version}\\n'\n"),
        );
        let mut tar = Command::new("tar");
        tar.arg("-czf")
            .arg(&self.archive)
            .arg("-C")
            .arg(&payload)
            .arg("chitchat");
        if extra_file {
            fs::write(payload.join("extra"), "unexpected").unwrap();
            tar.arg("extra");
        }
        assert!(tar.status().unwrap().success());
        let hash = Command::new("shasum")
            .args(["-a", "256"])
            .arg(&self.archive)
            .output()
            .unwrap();
        assert!(hash.status.success());
        let hash = String::from_utf8(hash.stdout).unwrap();
        fs::write(
            self.temp.path().join(format!("{}.sha256", self.asset)),
            format!(
                "{}  {}\n",
                hash.split_whitespace().next().unwrap(),
                self.asset
            ),
        )
        .unwrap();
    }

    fn cmd(&self) -> Command {
        let mut cmd = self.base_cmd();
        cmd.arg("update");
        cmd
    }

    fn base_cmd(&self) -> Command {
        let mut cmd = Command::new(&self.binary);
        cmd.current_dir(self.temp.path())
            .env("CHITCHAT_HOME", self.temp.path().join("data"))
            .env("CLAUDE_CONFIG_DIR", self.temp.path().join("claude"))
            .env("CODEX_HOME", self.temp.path().join("codex"))
            .env("GEMINI_CLI_HOME", self.temp.path().join("gemini"))
            .env("CHITCHAT_AUTO_UPDATE", "0")
            .env("UPDATE_FIXTURE", self.temp.path())
            .env("UPDATE_ASSET", &self.asset);
        let mut paths = vec![self.temp.path().join("transport")];
        paths.extend(std::env::split_paths(&std::env::var_os("PATH").unwrap()));
        cmd.env("PATH", std::env::join_paths(paths).unwrap());
        cmd
    }

    fn run(&self, args: &[&str]) -> Output {
        self.cmd().args(args).output().unwrap()
    }

    fn unchanged(&self, before: &[u8]) {
        assert_eq!(fs::read(&self.binary).unwrap(), before);
        assert_eq!(
            fs::read_dir(self.binary.parent().unwrap()).unwrap().count(),
            1,
            "staging files must be cleaned"
        );
    }
}

#[test]
#[cfg(not(debug_assertions))]
fn release_mcp_starts_promptly_and_updater_outlives_it_without_protocol_output() {
    use std::io::{BufRead, BufReader, Write};
    use std::process::Stdio;
    use std::time::{Duration, Instant};

    let f = Fixture::new();
    let mut server = f
        .base_cmd()
        .args(["mcp", "--client", "codex"])
        .env("CHITCHAT_PROJECT", "update-fixture")
        .env("CHITCHAT_AUTO_UPDATE", "1")
        .env("CHITCHAT_AUTO_BACKUP", "0")
        .env("UPDATE_DELAY", "1")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let stdout = server.stdout.take().unwrap();
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        for line in BufReader::new(stdout).lines() {
            if tx.send(line.unwrap()).is_err() {
                break;
            }
        }
    });
    writeln!(
        server.stdin.as_mut().unwrap(),
        "{}",
        serde_json::json!({
            "jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {
                "protocolVersion": "2025-06-18", "capabilities": {},
                "clientInfo": { "name": "update-test", "version": "0" }
            }
        })
    )
    .unwrap();
    // Fixture curl sleeps for two seconds. The MCP handshake must complete first.
    let response = rx.recv_timeout(Duration::from_millis(1500));
    server.kill().unwrap();
    server.wait().unwrap();
    let response: serde_json::Value = serde_json::from_str(&response.unwrap()).unwrap();
    assert_eq!(response["result"]["serverInfo"]["name"], "chitchat");
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let state = fs::read(f.temp.path().join("data/update.json")).unwrap_or_default();
        if String::from_utf8_lossy(&state).contains("installed 0.2.0") {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "detached updater did not finish: {}",
            String::from_utf8_lossy(&state)
        );
        std::thread::sleep(Duration::from_millis(50));
    }
    assert!(
        rx.try_recv().is_err(),
        "updater output must not use MCP stdout"
    );
}

#[test]
fn check_then_install_uses_verified_release_without_stdout() {
    let f = Fixture::new();
    let before = fs::read(&f.binary).unwrap();
    let check = f.run(&["--check"]);
    assert!(
        check.status.success(),
        "{}",
        String::from_utf8_lossy(&check.stderr)
    );
    assert!(check.stdout.is_empty());
    assert!(String::from_utf8_lossy(&check.stderr).contains("available 0.2.0"));
    f.unchanged(&before);
    let installed = f.run(&["--version", "v0.2.0"]);
    assert!(
        installed.status.success(),
        "{}",
        String::from_utf8_lossy(&installed.stderr)
    );
    assert!(installed.stdout.is_empty());
    assert_eq!(
        fs::read(&f.binary).unwrap(),
        fs::read(f.temp.path().join("payload/chitchat")).unwrap()
    );
    let state: serde_json::Value =
        serde_json::from_slice(&fs::read(f.temp.path().join("data/update.json")).unwrap()).unwrap();
    assert!(
        state["outcome"]
            .as_str()
            .unwrap()
            .contains("installed 0.2.0")
    );
}

#[test]
fn bad_checksum_archive_or_version_never_replaces_installed_binary() {
    for failure in ["checksum", "archive", "version"] {
        let f = Fixture::new();
        let before = fs::read(&f.binary).unwrap();
        match failure {
            "checksum" => fs::write(
                f.temp.path().join(format!("{}.sha256", f.asset)),
                format!("{}  {}\n", "0".repeat(64), f.asset),
            )
            .unwrap(),
            "archive" => f.package("0.2.0", true),
            "version" => f.package("0.3.0", false),
            _ => unreachable!(),
        }
        let out = f.run(&["--version", "v0.2.0"]);
        assert!(!out.status.success(), "{failure}");
        assert!(out.stdout.is_empty());
        f.unchanged(&before);
    }
}

#[test]
fn automatic_checks_are_throttled_even_after_failure_and_respect_opt_out() {
    let f = Fixture::new();
    assert!(f.run(&["--auto"]).status.success());
    assert!(!f.temp.path().join("calls").exists());
    fs::write(
        f.temp.path().join(format!("{}.sha256", f.asset)),
        "bad checksum",
    )
    .unwrap();
    let first = f
        .cmd()
        .arg("--auto")
        .env("CHITCHAT_AUTO_UPDATE", "1")
        .output()
        .unwrap();
    assert!(!first.status.success());
    let calls = fs::read(f.temp.path().join("calls")).unwrap();
    let second = f
        .cmd()
        .arg("--auto")
        .env("CHITCHAT_AUTO_UPDATE", "1")
        .output()
        .unwrap();
    assert!(second.status.success());
    assert_eq!(calls, fs::read(f.temp.path().join("calls")).unwrap());
}

#[test]
fn update_lock_prevents_concurrent_installation_and_invalid_tags_do_no_io() {
    let f = Fixture::new();
    let invalid = f.run(&["--version", "../../bad"]);
    assert!(!invalid.status.success());
    assert!(!f.temp.path().join("calls").exists());
    let data = f.temp.path().join("data");
    fs::create_dir_all(&data).unwrap();
    let lock = fs::File::create(data.join("update.lock")).unwrap();
    lock.lock().unwrap();
    let out = f.run(&["--check"]);
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("another update"));
    assert!(!f.temp.path().join("calls").exists());
}
