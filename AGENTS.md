# chitchat: guide for coding agents

chitchat is a Rust CLI and stdio MCP server. It gives Claude Code and Codex sessions a shared project chat and shared memory, backed by one SQLite database that many processes share. Design and research are in `docs/research/prior-art.md`; read §7–9 before changing the architecture.

## Commands

- Build: `cargo build` (release: `cargo build --release`; check `ls -lh target/release/chitchat` when adding dependencies)
- Test: `cargo test`
- Lint (must pass): `cargo fmt --all && cargo clippy --all-targets -- -D warnings`
- Inspect: `cargo run -- doctor` (set `CHITCHAT_HOME=/tmp/somewhere` to keep your real `~/.chitchat` untouched)

## Layout

| Path | What it is |
|---|---|
| `src/main.rs` | Process entry: logging and subcommand dispatch |
| `src/cli.rs` | clap definitions for every subcommand |
| `src/mcp.rs` | `chitchat mcp`, the stdio MCP server (rmcp) |
| `src/hook.rs` | `chitchat hook <event>`, the Claude Code and Codex hook handler |
| `src/db.rs` | Connection setup (WAL, busy timeout, setup lock) and the migrator |
| `src/project.rs` | Project identity from the git remote or main worktree |
| `src/session.rs` | Client and vendor types; session detection from the environment |
| `src/paths.rs` | Data directory resolution (`CHITCHAT_HOME`) |
| `migrations/` | SQL migrations, applied in order |
| `tests/cli.rs` | End-to-end tests against the built binary |

## Rules

- **stdout is protocol.** In `chitchat mcp` it carries JSON-RPC. In `chitchat hook` it is injected into the agent's context. Never `println!` on those paths; log with `tracing`, which goes to stderr.
- **Hooks never fail the agent.** `chitchat hook` always exits 0 and prints nothing unless it has something for the agent.
- **Many processes share the database.**
  - Keep transactions short.
  - Use `TransactionBehavior::Immediate` for any read-then-write.
  - Always open connections through `db::open`.
- **Migrations are append-only.**
  - Never edit a released file in `migrations/`. Add a new file and append it to `MIGRATIONS` in `src/db.rs`.
  - Tables are `STRICT`, and timestamps are unix milliseconds.
- **Keep MCP tool results small** (under about 8k tokens) and paginate. Codex truncates at about 10k tokens.
- **Keep the binary small.** Justify every new dependency and check its size impact in a release build.
