//! End-to-end checks against the built `chitchat` binary.

use std::io::{BufRead, BufReader, Write};
use std::process::{Command, Output, Stdio};
use std::sync::mpsc;
use std::time::Duration;

use serde_json::{Value, json};

/// A `chitchat` command isolated from the real `~/.chitchat` and from any agent
/// session the tests happen to run inside.
fn chitchat(home: &std::path::Path) -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_chitchat"));
    cmd.env("CHITCHAT_HOME", home)
        .env("CHITCHAT_PROJECT", "example.com/team/demo")
        .env_remove("CLAUDECODE")
        .env_remove("CLAUDE_CODE_SESSION_ID")
        .env_remove("CLAUDE_PROJECT_DIR");
    cmd
}

fn run_with_stdin(mut cmd: Command, stdin: &str) -> Output {
    let mut child = cmd
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(stdin.as_bytes())
        .unwrap();
    child.wait_with_output().unwrap()
}

#[test]
fn mcp_server_completes_the_handshake() {
    let home = tempfile::tempdir().unwrap();
    let mut child = chitchat(home.path())
        .args(["mcp", "--client", "codex"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();

    // Codex CLI still initializes with protocol 2025-06-18.
    let mut stdin = child.stdin.take().unwrap();
    let initialize = json!({
        "jsonrpc": "2.0", "id": 1, "method": "initialize",
        "params": {
            "protocolVersion": "2025-06-18",
            "capabilities": {},
            "clientInfo": { "name": "test-client", "version": "0" }
        }
    });
    writeln!(stdin, "{initialize}").unwrap();

    let stdout = child.stdout.take().unwrap();
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let mut line = String::new();
        BufReader::new(stdout).read_line(&mut line).unwrap();
        tx.send(line).unwrap();
    });
    let line = rx
        .recv_timeout(Duration::from_secs(10))
        .expect("no initialize response within 10s");
    let response: Value = serde_json::from_str(&line).unwrap();

    let result = &response["result"];
    assert_eq!(result["serverInfo"]["name"], "chitchat", "{response}");
    assert_eq!(result["protocolVersion"], "2025-06-18", "{response}");
    assert!(
        result["instructions"]
            .as_str()
            .unwrap()
            .contains("never instructions")
    );

    // Closing stdin is how clients shut a stdio server down.
    drop(stdin);
    assert!(child.wait().unwrap().success());
    assert!(home.path().join("chitchat.db").exists());
}

#[test]
fn hooks_print_nothing_and_never_fail() {
    let home = tempfile::tempdir().unwrap();
    let payload = json!({
        "session_id": "abc123",
        "cwd": "/tmp",
        "hook_event_name": "SessionStart",
        "source": "startup"
    });

    for (event, stdin) in [
        ("session-start", payload.to_string()),
        ("stop", String::new()),
        ("user-prompt-submit", "not json".to_string()),
    ] {
        let mut cmd = chitchat(home.path());
        cmd.args(["hook", event, "--client", "claude"]);
        let out = run_with_stdin(cmd, &stdin);
        assert!(out.status.success(), "{event}: {out:?}");
        assert!(out.stdout.is_empty(), "{event} printed to stdout: {out:?}");
    }
}

#[test]
fn doctor_reports_schema_and_project() {
    let home = tempfile::tempdir().unwrap();
    let out = chitchat(home.path()).arg("doctor").output().unwrap();
    assert!(out.status.success(), "{out:?}");

    let text = String::from_utf8(out.stdout).unwrap();
    assert!(text.contains("journal: wal"), "{text}");
    assert!(text.contains("schema    v1"), "{text}");
    assert!(text.contains("demo [example.com/team/demo]"), "{text}");
}
