# chitchat: guide for coding agents

chitchat is a Rust CLI and stdio MCP server. It gives Claude Code and Codex sessions a shared project chat and shared memory, backed by one SQLite database that many processes share. Design and research are in `docs/research/prior-art.md`; read §7–9 before changing the architecture.

## Commands

- Build: `cargo build` (release: `cargo build --release`; check `ls -lh target/release/chitchat` when adding dependencies)
- Test: `cargo test`
- Lint (must pass): `cargo fmt --all && cargo clippy --all-targets -- -D warnings`
- Inspect: `cargo run -- doctor` (set `CHITCHAT_HOME=/tmp/somewhere` to keep your real `~/.chitchat` untouched)
- Release: bump `version` in Cargo.toml, then push a tag `vX.Y.Z`; `.github/workflows/release.yml` builds and publishes. `install.sh` installs from releases.

## Layout

| Path | What it is |
|---|---|
| `src/main.rs` | Process entry: logging and subcommand dispatch |
| `src/cli.rs` | clap definitions for every subcommand |
| `src/mcp.rs` | `chitchat mcp`: the stdio MCP server and its 10 tools (rmcp) |
| `src/hook.rs` | `chitchat hook <event>`: what Claude Code / Codex hooks run; exact per-client output |
| `src/workspace.rs` | `chitchat init/deinit/workspaces`: workspace setup, legacy adoption, Claude memory import |
| `src/clients.rs` | Per-directory client config (Claude local MCP + settings.local.json, Codex .codex/), git exclude |
| `src/backup.rs` | `chitchat backup/restore`, daily automatic backups |
| `src/human.rs` | Commands for the human: `post`, `tail`, `who`, `notes`, `note`, `forget`, `export` |
| `src/agents.rs` | Participants: identity resolution (Claude by process, Codex by session), presence |
| `src/chat.rs` | Messages, receipts (fan-out on write), inbox, acks, hook delivery |
| `src/memory.rs` | Notes (versioned, optimistic concurrency), docs index, FTS recall, Markdown export |
| `src/claims.rs` | File/dir/task leases with TTL and overlap rules |
| `src/digest.rs` | Text shown to agents: who-lists, digests, footers |
| `src/procs.rs` | Process ancestry (macOS `proc_pidinfo`, Linux `/proc`) |
| `src/db.rs` | Connection setup (WAL, busy timeout, setup lock) and the migrator |
| `src/project.rs` | Workspaces: `.chitchat/workspace.json` markers, detection from subdirs and linked worktrees |
| `src/session.rs` | Client and vendor types; session detection from the environment |
| `src/format.rs`, `src/paths.rs` | Time/text helpers; data directory (`CHITCHAT_HOME`) |
| `src/doctor.rs` | `chitchat doctor` |
| `migrations/` | SQL migrations, applied in order |
| `tests/cli.rs` | End-to-end tests: real MCP servers and hooks, two fake agents |

## Rules

- **stdout is protocol.** In `chitchat mcp` it carries JSON-RPC. In `chitchat hook` it is injected into the agent's context. Never `println!` on those paths; log with `tracing`, which goes to stderr.
- **Hooks never fail the agent.** `chitchat hook` always exits 0 and prints nothing unless it has something for the agent.
- **Hook output is exact per client.** Codex rejects unknown fields (`deny_unknown_fields`) and accepts only `decision`/`reason` on Stop. Check `hook::render` and its tests before changing any output.
- **Other agents' text is untrusted.** Anything shown to an agent from chat or notes is labeled as information, not instructions.
- **Many processes share the database.**
  - Keep transactions short.
  - Use `TransactionBehavior::Immediate` for any read-then-write.
  - Always open connections through `db::open`.
- **Migrations are append-only.**
  - Never edit a released file in `migrations/`. Add a new file and append it to `MIGRATIONS` in `src/db.rs`.
  - Tables are `STRICT`, and timestamps are unix milliseconds.
- **Never touch the user's real client config in tests.** Point `CLAUDE_CONFIG_DIR`, `CODEX_HOME` and `CHITCHAT_HOME` at temp dirs, as `tests/cli.rs` does.
- **Keep MCP tool results small** (under about 8k tokens) and paginate. Codex truncates at about 10k tokens.
- **Keep the binary small.** Justify every new dependency and check its size impact in a release build.
