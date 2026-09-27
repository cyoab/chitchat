//! End-to-end checks against the built `chitchat` binary: real MCP servers over
//! stdio and real hook invocations, with two fake agents whose "client process"
//! is a `sleep` we control.

use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Output, Stdio};
use std::sync::mpsc::{self, Receiver};
use std::time::Duration;

use serde_json::{Value, json};

struct Env {
    home: tempfile::TempDir,
    repo: PathBuf,
}

impl Env {
    fn new() -> Self {
        let home = tempfile::tempdir().unwrap();
        let repo = home.path().join("repo");
        std::fs::create_dir_all(repo.join("src")).unwrap();
        Env { home, repo }
    }

    /// A `chitchat` command isolated from the real `~/.chitchat` and from any agent
    /// session the tests happen to run inside.
    fn cmd(&self, client_pid: Option<u32>) -> Command {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_chitchat"));
        cmd.current_dir(&self.repo)
            .env("CHITCHAT_HOME", self.home.path().join("data"))
            .env("CHITCHAT_PROJECT", "example.com/team/demo")
            .env_remove("CHITCHAT_CLIENT_PID")
            .env_remove("CLAUDECODE")
            .env_remove("CLAUDE_PID")
            .env_remove("CLAUDE_CODE_SESSION_ID")
            .env_remove("CLAUDE_PROJECT_DIR");
        if let Some(pid) = client_pid {
            cmd.env("CHITCHAT_CLIENT_PID", pid.to_string());
        }
        cmd
    }

    fn hook(&self, client_pid: u32, event: &str, client: &str, payload: Value) -> Option<Value> {
        let mut cmd = self.cmd(Some(client_pid));
        cmd.args(["hook", event, "--client", client]);
        let payload = json!({ "cwd": self.repo })
            .as_object()
            .unwrap()
            .clone()
            .into_iter()
            .chain(payload.as_object().cloned().unwrap_or_default())
            .collect::<serde_json::Map<_, _>>();
        let out = run_with_stdin(cmd, &Value::Object(payload).to_string());
        assert!(out.status.success(), "{event}: {out:?}");
        let stdout = String::from_utf8(out.stdout).unwrap();
        (!stdout.trim().is_empty()).then(|| serde_json::from_str(&stdout).unwrap())
    }

    fn cli(&self, args: &[&str]) -> String {
        let out = self.cmd(None).args(args).output().unwrap();
        assert!(out.status.success(), "{args:?}: {out:?}");
        String::from_utf8(out.stdout).unwrap()
    }
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

/// A fake client process: agents count as online while it runs.
struct FakeClient(Child);

impl FakeClient {
    fn start() -> Self {
        FakeClient(Command::new("sleep").arg("120").spawn().unwrap())
    }
    fn pid(&self) -> u32 {
        self.0.id()
    }
}

impl Drop for FakeClient {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// A running `chitchat mcp` we talk JSON-RPC to.
struct Mcp {
    child: Child,
    stdin: Option<ChildStdin>,
    lines: Receiver<String>,
    next_id: i64,
    session: Option<String>,
}

impl Mcp {
    fn start(env: &Env, client: &str, client_pid: u32, session: Option<&str>) -> Self {
        let mut child = env
            .cmd(Some(client_pid))
            .args(["mcp", "--client", client])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        let stdout = child.stdout.take().unwrap();
        let (tx, lines) = mpsc::channel();
        std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines() {
                if tx.send(line.unwrap()).is_err() {
                    break;
                }
            }
        });
        let mut mcp = Mcp {
            stdin: child.stdin.take(),
            child,
            lines,
            next_id: 1,
            session: session.map(str::to_string),
        };
        // Codex CLI still initializes with protocol 2025-06-18.
        let init = mcp.request(
            "initialize",
            json!({
                "protocolVersion": "2025-06-18",
                "capabilities": {},
                "clientInfo": { "name": "test-client", "version": "0" }
            }),
        );
        assert_eq!(init["serverInfo"]["name"], "chitchat", "{init}");
        assert_eq!(init["protocolVersion"], "2025-06-18", "{init}");
        mcp.send(json!({ "jsonrpc": "2.0", "method": "notifications/initialized" }));
        mcp
    }

    fn send(&mut self, msg: Value) {
        writeln!(self.stdin.as_mut().unwrap(), "{msg}").unwrap();
    }

    fn request(&mut self, method: &str, params: Value) -> Value {
        let id = self.next_id;
        self.next_id += 1;
        self.send(json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params }));
        loop {
            let line = self
                .lines
                .recv_timeout(Duration::from_secs(10))
                .expect("no response within 10s");
            let msg: Value = serde_json::from_str(&line).unwrap();
            if msg["id"] == id {
                return msg["result"].clone();
            }
        }
    }

    /// Calls a tool (as Codex would, with `_meta.sessionId` when set) and returns
    /// its text, panicking on tool errors.
    fn call(&mut self, tool: &str, args: Value) -> String {
        let mut params = json!({ "name": tool, "arguments": args });
        if let Some(session) = &self.session {
            params["_meta"] = json!({ "sessionId": session, "threadId": session });
        }
        let result = self.request("tools/call", params);
        let text = result["content"][0]["text"]
            .as_str()
            .unwrap_or_default()
            .to_string();
        assert_ne!(result["isError"], true, "{tool} failed: {text}");
        text
    }
}

impl Drop for Mcp {
    fn drop(&mut self) {
        drop(self.stdin.take()); // closing stdin is how clients stop stdio servers
        let _ = self.child.wait();
    }
}

fn context(output: &Option<Value>) -> String {
    output
        .as_ref()
        .and_then(|v| v["hookSpecificOutput"]["additionalContext"].as_str())
        .unwrap_or_default()
        .to_string()
}

#[test]
fn claude_and_codex_agents_talk_claim_and_remember() {
    let env = Env::new();
    let claude_proc = FakeClient::start();
    let codex_proc = FakeClient::start();
    let a = claude_proc.pid();
    let b = codex_proc.pid();

    // Claude's session starts: it is briefed and registered as @claude-1.
    let start = env.hook(
        a,
        "session-start",
        "claude",
        json!({ "session_id": "c-1", "source": "startup" }),
    );
    let briefing = context(&start);
    assert!(briefing.contains("You are @claude-1"), "{briefing}");
    assert_eq!(
        start.unwrap()["hookSpecificOutput"]["hookEventName"],
        "SessionStart"
    );

    // Codex (identified by session id) joins and asks Claude something.
    let mut codex = Mcp::start(&env, "codex", b, Some("x-1"));
    let tools = codex.request("tools/list", json!({}));
    let names: Vec<&str> = tools["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["name"].as_str().unwrap())
        .collect();
    assert_eq!(names.len(), 10, "{names:?}");
    let joined = codex.call("join", json!({ "status": "wiring the installer" }));
    assert!(
        joined.contains("You are @codex-1") && joined.contains("@claude-1"),
        "{joined}"
    );
    let posted = codex.call(
        "post",
        json!({ "body": "@claude-1 can you check src/db.rs uses BEGIN IMMEDIATE?", "intent": "request" }),
    );
    assert!(
        posted.starts_with("Posted #1 to #general for @claude-1"),
        "{posted}"
    );

    // Codex claims the file; Claude editing it gets a heads-up mid-turn, plus the
    // urgent message (it @mentions Claude).
    let claimed = codex.call(
        "claim",
        json!({ "resources": ["src/db.rs"], "reason": "adding retries" }),
    );
    assert!(claimed.starts_with("Claimed file:src/db.rs"), "{claimed}");
    let edit = env.hook(
        a,
        "post-tool-use",
        "claude",
        json!({ "session_id": "c-1", "tool_name": "Edit",
                "tool_input": { "file_path": env.repo.join("src/db.rs") } }),
    );
    let mid = context(&edit);
    assert!(mid.contains("claimed") && mid.contains("@codex-1"), "{mid}");
    assert!(
        mid.contains("#1") && mid.contains("BEGIN IMMEDIATE"),
        "{mid}"
    );

    // Claude tries to end its turn: the Stop hook blocks once to get an answer.
    let stop = env.hook(
        a,
        "stop",
        "claude",
        json!({ "session_id": "c-1", "stop_hook_active": false }),
    );
    let stop = stop.expect("stop should block");
    assert_eq!(stop["decision"], "block");
    assert!(stop["reason"].as_str().unwrap().contains("#1"));
    let again = env.hook(
        a,
        "stop",
        "claude",
        json!({ "session_id": "c-1", "stop_hook_active": false }),
    );
    assert!(again.is_none(), "nudges once per request: {again:?}");

    // Claude answers through its own MCP server (same client process, so same agent).
    let mut claude = Mcp::start(&env, "claude", a, None);
    let reply = claude.call(
        "post",
        json!({ "body": "Yes, every write path uses it.", "reply_to": 1 }),
    );
    assert!(
        reply.starts_with("Posted #2 to #general for @codex-1"),
        "{reply}"
    );
    let saved = claude.call(
        "remember",
        json!({ "kind": "decision", "title": "Writes use BEGIN IMMEDIATE", "body": "All read-then-write paths use TransactionBehavior::Immediate." }),
    );
    assert!(
        saved.contains("decision/writes-use-begin-immediate"),
        "{saved}"
    );

    // Codex's next prompt delivers the reply; its recall finds Claude's note.
    let prompt = env.hook(
        b,
        "user-prompt-submit",
        "codex",
        json!({ "session_id": "x-1", "prompt": "continue" }),
    );
    let digest = context(&prompt);
    assert!(
        digest.contains("#2") && digest.contains("every write path"),
        "{digest}"
    );
    let found = codex.call("recall", json!({ "query": "immediate transactions" }));
    assert!(
        found.contains("decision/writes-use-begin-immediate") && found.contains("@claude-1"),
        "{found}"
    );

    // Updating someone else's note needs the revision you read.
    let blind = codex.request(
        "tools/call",
        json!({ "name": "remember", "_meta": { "sessionId": "x-1" }, "arguments": {
            "key": "decision/writes-use-begin-immediate", "kind": "decision",
            "title": "Writes use BEGIN IMMEDIATE", "body": "changed" } }),
    );
    assert_eq!(blind["isError"], true, "{blind}");

    // Nothing new: hooks stay silent.
    assert!(
        env.hook(
            b,
            "user-prompt-submit",
            "codex",
            json!({ "session_id": "x-1" })
        )
        .is_none()
    );

    // The human sees everything in the terminal and can post too.
    let tail = env.cli(&["tail", "--no-follow"]);
    assert!(tail.contains("#1") && tail.contains("#2"), "{tail}");
    let human = env.cli(&["post", "--to", "@codex-1", "thanks both"]);
    assert!(human.starts_with("Posted #3 to @codex-1"), "{human}");
    let who = env.cli(&["who"]);
    assert!(
        who.contains("@codex-1 (codex) · \"wiring the installer\""),
        "{who}"
    );
    assert!(who.contains("claims: file:src/db.rs"), "{who}");
}

#[test]
fn hooks_never_fail_the_agent() {
    let env = Env::new();
    let client = FakeClient::start();
    for (event, stdin) in [
        ("user-prompt-submit", "not json"),
        ("stop", ""),
        ("post-tool-use", "{}"),
    ] {
        let mut cmd = env.cmd(Some(client.pid()));
        cmd.args(["hook", event, "--client", "codex"]);
        let out = run_with_stdin(cmd, stdin);
        assert!(out.status.success(), "{event}: {out:?}");
        assert!(out.stdout.is_empty(), "{event} printed: {out:?}");
    }
}

#[test]
fn export_writes_markdown_notes() {
    let env = Env::new();
    let client = FakeClient::start();
    let mut claude = Mcp::start(&env, "claude", client.pid(), None);
    claude.call(
        "remember",
        json!({ "kind": "gotcha", "title": "WAL needs a local disk", "body": "Not NFS." }),
    );
    let out_dir = env.home.path().join("notes");
    let out = env.cli(&["export", "--dir", out_dir.to_str().unwrap()]);
    assert!(out.contains("1 written"), "{out}");
    let note = std::fs::read_to_string(out_dir.join("gotcha/wal-needs-a-local-disk.md")).unwrap();
    assert!(
        note.contains("author: \"@claude-1\"") && note.contains("# WAL needs a local disk"),
        "{note}"
    );
    assert!(Path::new(&out_dir.join("INDEX.md")).exists());
}

#[test]
fn doctor_reports_schema_and_project() {
    let env = Env::new();
    let text = env.cli(&["doctor"]);
    assert!(text.contains("journal: wal"), "{text}");
    assert!(text.contains("schema    v1"), "{text}");
    assert!(text.contains("demo [example.com/team/demo]"), "{text}");
}
