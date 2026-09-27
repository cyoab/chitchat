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
        let mut cmd = self.plain_cmd(&self.repo, client_pid);
        cmd.env("CHITCHAT_PROJECT", "example.com/team/demo");
        cmd
    }

    /// Without a CHITCHAT_PROJECT override: projects come from workspace markers.
    /// Client config dirs point into the temp home, so Claude Code memory files
    /// can be staged there and nothing real is read.
    fn plain_cmd(&self, dir: &Path, client_pid: Option<u32>) -> Command {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_chitchat"));
        cmd.current_dir(dir)
            .env("CHITCHAT_HOME", self.home.path().join("data"))
            .env("CLAUDE_CONFIG_DIR", self.home.path().join("claude-config"))
            .env("CODEX_HOME", self.home.path().join("codex-home"))
            .env_remove("CHITCHAT_PROJECT")
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
        Mcp::spawn(env.cmd(Some(client_pid)), client, session)
    }

    fn spawn(mut cmd: Command, client: &str, session: Option<&str>) -> Self {
        let mut child = cmd
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

    /// Like `call`, but returns (text, is_error) instead of panicking on errors.
    fn try_call(&mut self, tool: &str, args: Value) -> (String, bool) {
        let mut params = json!({ "name": tool, "arguments": args });
        if let Some(session) = &self.session {
            params["_meta"] = json!({ "sessionId": session, "threadId": session });
        }
        let result = self.request("tools/call", params);
        let text = result["content"][0]["text"]
            .as_str()
            .unwrap_or_default()
            .to_string();
        (text, result["isError"] == true)
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

fn run_ok(mut cmd: Command) -> String {
    let out = cmd.output().unwrap();
    assert!(out.status.success(), "{cmd:?}: {out:?}");
    String::from_utf8(out.stdout).unwrap()
}

fn git(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(["-c", "user.name=t", "-c", "user.email=t@t"])
        .args(args)
        .output()
        .unwrap();
    assert!(out.status.success(), "git {args:?}: {out:?}");
    String::from_utf8(out.stdout).unwrap()
}

#[test]
fn init_sets_up_an_existing_repo_with_worktrees_and_claude_memory() {
    let env = Env::new();
    let repo = env.repo.canonicalize().unwrap();
    std::fs::write(
        repo.join("README.md"),
        "# Demo\n\nThe widget cache lives in var/cache.\n",
    )
    .unwrap();
    git(&repo, &["init", "-q"]);
    git(&repo, &["add", "."]);
    git(&repo, &["commit", "-q", "-m", "init"]);
    let wt = repo.parent().unwrap().join("repo-wt");
    git(&repo, &["worktree", "add", "-q", wt.to_str().unwrap()]);

    // Claude Code already kept a memory for this repo.
    let encoded: String = repo
        .to_string_lossy()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect();
    let memory = env
        .home
        .path()
        .join("claude-config/projects")
        .join(encoded)
        .join("memory");
    std::fs::create_dir_all(&memory).unwrap();
    std::fs::write(
        memory.join("deploy-freeze.md"),
        "---\nname: deploy-freeze\ndescription: No deploys on Fridays\nmetadata:\n  type: project\n---\n\nThe team freezes deploys every Friday.\n",
    )
    .unwrap();
    std::fs::write(memory.join("MEMORY.md"), "- index, not a note\n").unwrap();
    // The user already has their own (untracked) Codex settings here.
    std::fs::create_dir_all(repo.join(".codex")).unwrap();
    std::fs::write(repo.join(".codex/config.toml"), "model = \"gpt-6\"\n").unwrap();

    let init = || {
        let mut cmd = env.plain_cmd(&repo, None);
        cmd.args(["init", "--client", "codex"]);
        run_ok(cmd)
    };
    let first = init();
    assert!(
        first.contains("Initialized chitchat workspace \"repo\""),
        "{first}"
    );
    assert!(
        first.contains("Imported Claude Code memory: 1 new"),
        "{first}"
    );
    assert!(first.contains("Docs: 1 Markdown files indexed"), "{first}");
    assert!(repo.join(".chitchat/workspace.json").exists());
    for dir in [&repo, &wt] {
        let hooks = std::fs::read_to_string(dir.join(".codex/hooks.json")).unwrap();
        assert_eq!(hooks.matches("--client codex").count(), 4, "{hooks}");
        let config = std::fs::read_to_string(dir.join(".codex/config.toml")).unwrap();
        assert!(config.contains("[mcp_servers.chitchat]"), "{config}");
    }
    let merged = std::fs::read_to_string(repo.join(".codex/config.toml")).unwrap();
    assert!(
        merged.starts_with("model = \"gpt-6\"\n\n# >>> chitchat"),
        "{merged}"
    );
    // None of it shows up in git.
    assert_eq!(git(&repo, &["status", "--porcelain"]), "");
    assert_eq!(git(&wt, &["status", "--porcelain"]), "");

    // Running it again changes nothing.
    let again = init();
    assert!(again.contains("Refreshing"), "{again}");
    assert!(again.contains("0 new, 0 updated, 1 unchanged"), "{again}");
    assert!(again.contains("already set up"), "{again}");

    // Subdirectories and the linked worktree are the same workspace.
    std::fs::create_dir_all(repo.join("src/deep")).unwrap();
    let mut notes = env.plain_cmd(&repo.join("src/deep"), None);
    notes.args(["notes"]);
    let listed = run_ok(notes);
    assert!(listed.contains("imported/claude/deploy-freeze"), "{listed}");
    let mut found = env.plain_cmd(&wt, None);
    found.args(["notes", "widget", "cache"]);
    let found = run_ok(found);
    assert!(
        found.contains("README.md") && found.contains("Demo"),
        "{found}"
    );
    let mut list = env.plain_cmd(&repo, None);
    list.arg("workspaces");
    assert!(run_ok(list).contains(repo.to_str().unwrap()));

    // deinit removes the client config but keeps the data.
    let mut off = env.plain_cmd(&repo, None);
    off.args(["deinit", "--client", "codex"]);
    let off = run_ok(off);
    assert!(off.contains("chitchat is off"), "{off}");
    for dir in [&repo, &wt] {
        assert!(!dir.join(".codex/hooks.json").exists());
    }
    assert!(!wt.join(".codex/config.toml").exists());
    // The user's own Codex settings survive, and nothing is left behind in the repo.
    let user_config = std::fs::read_to_string(repo.join(".codex/config.toml")).unwrap();
    assert_eq!(user_config, "model = \"gpt-6\"\n");
    assert!(!repo.join(".codex/config.toml.bak").exists());
    assert_eq!(git(&repo, &["status", "--porcelain"]), "");
    let mut still = env.plain_cmd(&repo, None);
    still.arg("notes");
    assert!(run_ok(still).contains("imported/claude/deploy-freeze"));
}

#[test]
fn outside_a_workspace_chitchat_stays_out_of_the_way() {
    let env = Env::new();
    let client = FakeClient::start();
    let outside = env.home.path().join("elsewhere");
    std::fs::create_dir_all(&outside).unwrap();

    let mut hook = env.plain_cmd(&outside, Some(client.pid()));
    hook.args(["hook", "user-prompt-submit", "--client", "claude"]);
    let out = run_with_stdin(
        hook,
        &json!({ "cwd": outside, "session_id": "s" }).to_string(),
    );
    assert!(out.status.success() && out.stdout.is_empty(), "{out:?}");

    let mut mcp = Mcp::spawn(env.plain_cmd(&outside, Some(client.pid())), "claude", None);
    let (text, is_error) = mcp.try_call("who", json!({}));
    assert!(is_error && text.contains("chitchat init"), "{text}");

    let out = env.plain_cmd(&outside, None).arg("who").output().unwrap();
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("chitchat init"));
}

#[test]
fn backups_restore_earlier_state() {
    let env = Env::new();
    env.cli(&["post", "first"]);
    let made = env.cli(&["backup"]);
    assert!(made.starts_with("Backed up to "), "{made}");
    env.cli(&["post", "second"]);
    assert!(env.cli(&["backup", "--list"]).contains("chitchat-"));

    let restored = env.cli(&["restore", "latest"]);
    assert!(
        restored.contains("previous database was saved"),
        "{restored}"
    );
    let tail = env.cli(&["tail", "--no-follow"]);
    assert!(tail.contains("first") && !tail.contains("second"), "{tail}");

    // The pre-restore copy has the newer state, so the restore can be undone.
    let pre = std::fs::read_dir(env.home.path().join("data/backups"))
        .unwrap()
        .flatten()
        .map(|e| e.path())
        .find(|p| p.to_string_lossy().contains("pre-restore-"))
        .unwrap();
    env.cli(&["restore", pre.to_str().unwrap()]);
    assert!(env.cli(&["tail", "--no-follow"]).contains("second"));
}
