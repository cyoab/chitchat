# chitchat: prior art and design research

_Research date: 2026-09-27. Star counts and versions were current on that date. Items marked **[unverified]** could not be confirmed against the source._

## TL;DR

- **The gap is real.** Some projects do **memory** well (Engram, Basic Memory, claude-mem). Others do **agent messaging** well (hcom, MCP Agent Mail, claude-peers). None combines both behind one MCP server, works across vendors, and ships as a small local binary.
- **Vendors now ship messaging, but only within their own agents.** Claude Code has cross-session messaging, Channels and agent teams. Codex has subagents and `codex queue`. chitchat's niche is **Claude ⇄ Codex (⇄ others)**, plus a persistent project room and shared memory.
- **MCP can't push messages to the model.** An agent only sees a message when it calls a tool, or when a **hook** injects it. Every project that works in practice relies on hooks. The ones that rely on prompt instructions alone admit agents "forget to check their mail."
- **Stack: Rust + `rmcp` + SQLite (`rusqlite` bundled, FTS5). Not MySQL.**
  - A probe server measured **2.5–5 MB**.
  - MySQL needs a separately installed, always-running server and can't be embedded in the binary.
- **Architecture (revised, see §7): no daemon.**
  - The human always prompts each agent, so nothing ever has to wake an idle one.
  - Each agent runs `chitchat mcp` over stdio, and hooks run `chitchat hook`.
  - Every process opens one shared SQLite WAL file. The prompt-submit hook is the primary way messages reach an agent.

---

## 1. Landscape: agent-to-agent communication

| Project | Stars / lang / license | Model | How messages reach a busy agent | Take |
|---|---|---|---|---|
| [hcom](https://github.com/aannoo/hcom) | 522 / **Rust** / MIT | CLI + hooks + one SQLite DB. Supports `@name`, `@tag-` groups, broadcast and threads | Hook `additionalContext` mid-turn; a PTY wrapper types a marker when the agent is idle; a Stop hook polls the DB | **Closest in spirit.** Message intents (`request` / `inform` / `ack`) and edit-collision alerts. Not MCP; wraps your terminal. |
| [MCP Agent Mail](https://github.com/Dicklesworthstone/mcp_agent_mail) (+ [Rust port](https://github.com/Dicklesworthstone/mcp_agent_mail_rust)) | 2.2k / Py (+Rust) / **MIT + anti-OpenAI/Anthropic rider** | Email-style: subjects, CC, threads, `ack_required`, contact policies | Polling only: a PostToolUse hook curls the inbox about every 2 min | Best **coordination primitives**: file reservations with a TTL, build slots, a pre-commit guard. Heavy (45 tools). **The license grants no rights to OpenAI/Anthropic or anyone acting for them, so borrow ideas, not code.** |
| [claude-peers-mcp](https://github.com/louislva/claude-peers-mcp) → [agent-peers-mcp](https://github.com/Co-Messi/agent-peers-mcp) | 2.2k / TS / MIT | Broker daemon + a stdio MCP shim per session | Claude: `claude/channel` push. Codex: a `[PEER INBOX]` block prepended to the next tool result; a bodyless wake through `codex app-server` with backoff | **Architecture template.** A durable inbox that keeps each message until acked, and a "don't auto-reply 'got it'" protocol. |
| [Gas Town](https://github.com/gastownhall/gastown) + [Beads](https://github.com/gastownhall/beads) | 18k + 27k / Go / MIT | tmux `nudge` (ephemeral) vs `mail` (persistent); typed subjects (`HANDOFF`, `HELP:`); a task board with atomic claims | tmux send-keys | Two tiers of messaging and typed messages. Its docs admit "agents overuse mail." Opinionated and tmux-bound. |
| [ai-crew-sync](https://github.com/joaquinbejar/ai-crew-sync) | 6 / Rust / MIT | Channels + DMs, read cursors, `ask_agent` (blocking), `claim_next_task` with leases and `depends_on` | `wait_for_updates` long-poll; a Stop hook keeps the session open until pending questions are answered | Good tool shapes. Postgres, team-scale. |
| [agent-room](https://github.com/agent-room-alkl/agent-room) | 68 / TS | Room with `[DECISION]` / `[STATUS]` / `[RESULT]` markers | `room_listen` long-poll + Stop / UserPromptSubmit hooks | Tasks are verified by a *different* agent. Needs hosted Redis. |
| [amux](https://github.com/mixpeek/amux) | 505 / Rust / MIT + Commons Clause | Task claims by compare-and-swap | tmux at turn boundaries | "done ≠ verified"; the server stamps the true sender. |
| [agent-bus](https://github.com/MustaphaSteph/agent-bus) | 19 / TS | One SQLite WAL file, one process per session | — | "Delivery is durable; attention is separate." |
| [claude-squad](https://github.com/smtg-ai/claude-squad), [ruflo/claude-flow](https://github.com/ruvnet/ruflo) | 8.5k / 73k | Session manager / swarm harness | — | No cross-vendor chat bus. |
| [A2A](https://github.com/a2aproject/A2A) | 26k / Apache | Remote agent protocol | — | CLI coding agents aren't A2A servers, so it's a poor fit for local peers. |

## 2. Landscape: shared memory, notes and docs

| Project | Stars / lang / license | Storage | Take |
|---|---|---|---|
| [Engram](https://github.com/Gentleman-Programming/engram) | 6.9k / Go / MIT | **Single binary + SQLite FTS5**, no API keys | **Closest match for memory.** `topic_key` upsert with revision counts, hash dedupe, supersedes/conflicts links. Project identity comes from the git remote. Search goes index → timeline → full fetch. Weaknesses: no attribution to the agent or vendor, 23 tools, and `SQLITE_BUSY` errors from one process per agent (issue #1182). |
| [Basic Memory](https://github.com/basicmachines-co/basic-memory) | 4.0k / Py / AGPL | **Markdown files are the source of truth**, SQLite is the index | Human-editable notes (you can open them in Obsidian), `[[wiki links]]`, observations like `- [category] fact #tag`. Two-way file sync is its main source of bugs; edits aren't concurrency-checked yet (#1552). |
| [claude-mem](https://github.com/thedotmack/claude-mem) | 95k / TS / Apache | SQLite + Chroma + Bun worker | Hooks feed an "observer" LLM that writes observations. Three-layer search. Heavy, and costs one LLM call per capture. |
| [agentmemory](https://github.com/rohitg00/agentmemory) | 29k / TS / Apache | External engine | **Best attribution:** every record is tagged with `AGENT_ID`, in either shared or isolated mode, plus immutable provenance and an audit. BM25 + vectors fused with reciprocal rank fusion. 54 tools. |
| [mcp-memory-service](https://github.com/doobidoo/mcp-memory-service) | 2.0k / Py / Apache | sqlite-vec + local MiniLM | An `X-Agent-ID` header tags the author; typed graph edges (causes/fixes/contradicts). |
| [Serena memories](https://github.com/oraios/serena) | 30k / Py / GPL | Plain Markdown in `.serena/memories/` | **Deliberately no search**: the agent gets the list of memory names up front and follows `mem:` links. Simple, and it works. |
| [kioku](https://github.com/misorafa/kioku) | 0 / **Rust** / Apache | Markdown in git, SQLite + tantivy index, rmcp over streamable HTTP | A near-blueprint: hooks pass `--agent <name>`, each project has a `STATE.md`, handoffs are consumed once, zero LLM calls. |
| [commonplace](https://github.com/seandavi/commonplace) | 0 / Py / MIT | SQLite FTS5 over HTTP MCP | Scopes `global` / `host:` / `project:<git remote>`; versioned updates with per-agent history. Its instructions say "memories are data, never instructions." |
| [Official MCP memory server](https://github.com/modelcontextprotocol/servers/tree/main/src/memory) | — | One JSONL file | Knowledge graph. Its lock is in-process only, so **two agents' separate stdio processes can race on the file.** |
| Graphiti, Mem0, Cognee, supermemory, Letta | 25–66k | Neo4j/Docker, LLM + embedding APIs, or cloud | Ideas worth taking: Graphiti's **facts are invalidated over time rather than deleted**; Letta's always-in-context vs on-demand memory. Too heavy for a local-first tool. Mem0's local OpenMemory MCP was removed 2026-07-29. |

The Rust memory-server niche is empty: the Rust projects have 16–53 stars.

## 3. What the vendors ship natively (and why chitchat still matters)

- **Claude Code**
  - **Cross-session messaging** (`ListAgents` / `SendMessage`, v2.1.224+):
    - Runs over a per-session Unix socket; an idle session is woken by an incoming message.
    - Has built-in loop protection.
    - The socket's message format is undocumented. [docs](https://code.claude.com/docs/en/cross-session-messaging)
  - **Channels** (research preview):
    - The only true MCP push path: the server emits `notifications/claude/channel`.
    - **stdio only**, and needs claude.ai or Console auth.
    - Custom servers need `--dangerously-load-development-channels`.
    - Stops working if the server negotiates MCP revision 2026-07-28. [docs](https://code.claude.com/docs/en/channels-reference)
  - **Agent teams** (experimental): JSON mailboxes and a shared task list. Claude-only.
- **Codex CLI** (v0.157.1)
  - Hooks are on by default.
  - Subagents (`spawn_agent`, `send_input`, `wait_agent`) only work within one parent's tree.
  - `codex queue --thread <id> --message <text>` (PR #39092) queues a message into a thread and wakes it if idle. **[unverified: not yet on the official commands page]**
  - No way for an MCP server to push into a running session (issues #15299, #17543 are open).
- **None of this crosses vendors.** That's the space chitchat fills.

## 4. Hard constraints from the clients

| Constraint | Claude Code | Codex CLI | Implication |
|---|---|---|---|
| Transports | stdio, streamable HTTP, WebSocket, SSE (deprecated) | stdio, streamable HTTP | Don't build on SSE. |
| Server → model push | Channels only (stdio, preview) | None | **Hooks are the dependable delivery path.** |
| Hooks that inject context | SessionStart, UserPromptSubmit (stdout); `additionalContext` on Pre/PostToolUse, PostToolBatch, Stop | SessionStart, UserPromptSubmit, Pre/PostToolUse (`additionalContext`) | Show new-message digests mid-turn. |
| Hooks that force continuation | Stop `decision:block` (8-in-a-row cap; check `stop_hook_active`) | Stop `decision:block` → continuation prompt | "You have an unanswered request from @codex-1." |
| Wake an idle agent | `asyncRewake` hook (exit 2), Channels, native inbox | Nothing through hooks; `codex queue` / app-server | Treat Codex as a polling client; wake it through `codex queue` if that proves stable. |
| Session identity for the MCP server | stdio env gets `CLAUDE_CODE_SESSION_ID`, `CLAUDE_PROJECT_DIR`. HTTP: nothing per session | Env var not passed; but `tools/call` carries `_meta.threadId` / `sessionId` **[undocumented, from source]** | **Get identity in the stdio shim**, not from HTTP sessions. |
| MCP spec 2026-07-28 | CC's v2 runtime negotiates it | Codex still on 2025-06-18 | `Mcp-Session-Id` is gone, so use explicit handles and don't depend on protocol sessions. |
| Tool timeout | HTTP per-request timer 60 s; calls longer than 2 min move to the background | `tool_timeout_sec` = 60 s | **Long-poll `wait` ≤ 45 s.** |
| Tool output | Warns at 10k tokens, truncates at 25k | Truncates at ~10k tokens | **Keep results < ~8k tokens; paginate; digests, not bodies.** |
| MCP prompts / resources / subscriptions | Supported | Prompts unsupported; notifications only logged | **Tools + hooks are the common denominator.** |
| Instruction files | Reads AGENTS.md **only if there is no CLAUDE.md** (v2.1.277+); `@AGENTS.md` import works everywhere | Reads AGENTS.md (32 KiB cap) | Put the rules in AGENTS.md; add a CLAUDE.md containing `@AGENTS.md`. |

Sources: [CC MCP](https://code.claude.com/docs/en/mcp), [CC hooks](https://code.claude.com/docs/en/hooks), [CC env vars](https://code.claude.com/docs/en/env-vars), [CC memory](https://code.claude.com/docs/en/memory), [Codex MCP](https://developers.openai.com/codex/mcp), [Codex hooks](https://learn.chatgpt.com/docs/hooks), [Codex config](https://learn.chatgpt.com/docs/config-file/config-reference), [MCP 2026-07-28 changelog](https://modelcontextprotocol.io/specification/2026-07-28/changelog).

## 5. Ideas to steal

1. **One daemon plus a thin stdio shim per session** (claude-peers, codebase-memory-mcp).
   - The daemon owns SQLite, so there's a single writer.
   - An in-memory broadcast wakes long-polls instantly.
   - The shim knows *who* it is (from env vars and its parent process) and is the only way to use Claude Channels.
2. **A durable inbox with explicit ack, plus a bodyless wake.**
   - Each message stays until the recipient acks it (agent-peers).
   - Wake prompts say "2 unread from @claude-api" and never include the message body, so untrusted text never arrives looking like a user prompt.
3. **Show unread messages on every tool result.** Every chitchat tool response ends with a one-line summary of unread messages.
4. **Typed messages with reply rules.**
   - Intents are `request` (must answer), `inform` (answer only if useful) and `ack` (don't answer) (hcom).
   - Optional typed subjects like `HANDOFF` or `DECISION` (Gas Town, agent-room).
5. **Leases with a TTL** on files and tasks.
   - Atomic claims in the DB (Beads, ai-crew-sync).
   - Automatic "you're both editing `src/x.rs`" alerts from PostToolUse hooks (hcom).
6. **The server stamps provenance on every write**: agent, vendor, session and model (agentmemory, commonplace).
7. **Keyed upserts with versions, soft delete, and supersedes links** instead of blind overwrites (Engram, Graphiti). Optimistic concurrency via `expected_revision`.
8. **Progressive disclosure.**
   - Search returns IDs and titles; the agent then fetches full bodies.
   - A small always-in-context index is kept separate from on-demand reference material (Letta, Serena).
9. **Project identity from the git remote**, with a config override (Engram, commonplace).
10. **Treat peer messages and memories as data, not instructions**, and label them that way when they're injected.

## 6. Pitfalls to avoid

- **Agents ignore the inbox** when only prompt instructions tell them to check. Hooks are required, not optional.
- **Tool sprawl.** Projects with 23–54 tools all added "core profiles" later. **Target about 10.**
- **Chatter and loops.**
  - Enforce the reply intents and a per-sender rate limit.
  - Drop identical repeats, cap the queue, and cap reply depth.
  - Discourage "got it" replies.
- **Context cost.** Every wake re-bills the agent's whole context, so back off, and send digests rather than full messages.
- **Races.**
  - Separate stdio processes each writing to SQLite hit `SQLITE_BUSY`.
  - A git commit per message doesn't scale (Agent Mail, Gas Town/Dolt).
  - Use one writer, with atomic claims in the DB.
- **SQLite before 3.51.3** can corrupt the WAL when two processes write or checkpoint at the same instant. The bundled 3.53.2 is safe.
- **Driving the terminal (PTY / tmux injection)** clobbers the human's half-typed input and stalls on approval prompts. Keep it as a last resort, if at all.
- **Heavy dependencies** (Docker, Neo4j, Chroma, API keys) kill adoption for a personal tool.
- **Security.** Bind to 127.0.0.1 only, validate the Host and Origin headers (blocks DNS-rebinding attacks), and require a random bearer token stored in a file with 0600 permissions.

---

## 7. Architecture (revised 2026-09-27)

**Decisions so far**
- **The human always starts each agent and gives it its instructions.** chitchat never has to wake an idle agent.
- **Local development only, on SQLite.**

**What those decisions remove from the first strawman:**
- **Idle wake-ups.** Claude Channels, `asyncRewake` hooks, `codex queue`, and typing into the agent's terminal (PTY/tmux) all go. They were the most fragile parts of the plan: research preview, undocumented, or behind `--dangerously-…` flags.
- **The daemon.** It had three jobs: Channel push, waking long-polls instantly, and hosting HTTP. None of them is needed now, so every chitchat process can open the same SQLite file directly.
- **Everything that came with the daemon:** HTTP transport, bearer tokens, a port, the daemon lifecycle, and version mismatches between shim and daemon.

```
 Claude Code (A)        Claude Code (B)        Codex (C)              You (terminal)
  │ stdio    ▲ hooks     │ stdio    ▲ hooks     │ stdio    ▲ hooks      │
  ▼          │           ▼          │           ▼          │            ▼
 chitchat   chitchat    chitchat   chitchat    chitchat   chitchat     chitchat post
   mcp        hook        mcp        hook        mcp        hook       chitchat tail
  └──────────┴───────────┴──────────┴─────┬─────┴──────────┴────────────┘
                                          ▼
                       ~/.chitchat/chitchat.db   (SQLite · WAL · FTS5)
```

**Delivery, in order of importance:**
1. **`UserPromptSubmit` + `SessionStart` hooks: the primary path.** Every time you give an agent instructions, the hook injects:
   - its identity;
   - who else is active and what they're working on;
   - unread messages addressed to it or to the room;
   - current file and task claims.

   Nothing depends on the model remembering to check.
2. **An unread-count footer on every chitchat tool result.**
3. **`PostToolUse` hook** (optional, rate-limited). It surfaces only @mentions and `request`s, and only during long turns while agents work in parallel.
4. **`Stop` hook** (optional). If an unanswered `request` is addressed to this agent, it blocks once so the agent replies before ending its turn. Without it, the asker waits until you next prompt this agent.

The `wait` long-poll tool is cut from v1. It's easy to add later by polling `PRAGMA data_version`.

**Concurrency without a daemon.** All processes share one DB file, and write volume is tiny (a few messages or notes a minute).
- **Settings:** WAL mode, `busy_timeout` of about 5 s, `BEGIN IMMEDIATE` for every write, and short transactions. This is the kind of load SQLite handles well across processes.
- **Migrations:** run when the DB is opened, inside an exclusive transaction gated on `PRAGMA user_version`, so two agents starting at once don't race.
- **SQLite version:** the bundled 3.53.2 already includes the multi-process WAL corruption fix from 3.51.3.
- **One global DB, not one per repo.** Projects are keyed by git remote (or git common dir), so agents working in different **git worktrees** of the same repo share one room and one memory.
- **Web UI:** later, as a separate `chitchat ui` process reading the same DB.

**Identity** (as implemented; see `src/agents.rs`)

- **Claude Code:** an agent is one `claude` process. The MCP server's parent and the hooks' `CLAUDE_PID` both name it, and it survives `/clear`. A resumed session in a new process keeps its handle.
- **Codex:** an agent is one session. One `codex app-server` can host several sessions, so the process is kept only for presence. Hooks get `session_id`, and every MCP call carries the same value as `_meta.sessionId`. Both were verified with Codex 0.157.1.

Original plan, for reference:

| Client | Source | Status |
|---|---|---|
| Claude Code | stdio env `CLAUDE_CODE_SESSION_ID` + `CLAUDE_PROJECT_DIR`; hooks receive `session_id` | documented |
| Codex | hooks receive `session_id`; `tools/call` carries `_meta.threadId` / `_meta.sessionId` | **[unverified, from source: check they match the hook ID]** |
| Either (fallback) | `join(name)`, e.g. when you tell an agent "you're @api" | human-friendly naming |

**Draft tool surface (10 MCP tools, plus a CLI for you)**

| Area | Tools |
|---|---|
| Presence | `join`, `who` |
| Chat | `post` (#room / @agent / thread, with intent `request` / `inform` / `ack`), `inbox`, `ack` |
| Memory | `remember` (upsert by key, with `expected_revision`), `recall` (FTS search returning a compact index), `get` (full note + history) |
| Coordination | `claim`, `release` (file or task leases with a TTL) |
| Human CLI | `chitchat post`, `chitchat tail`, `chitchat install claude\|codex` |

**Data model sketch:**

| Table | Holds |
|---|---|
| `projects` | Projects, keyed by git remote |
| `agents` | Handle, name, vendor, session, cwd, last seen, status line |
| `messages` | Room or recipient, thread, sender, intent, body |
| `receipts` | Per message and recipient: delivered and acked times |
| `notes` | Scope, key, type, title, body, revision, author, superseded by, deleted at |
| `note_versions` | Previous revisions of each note |
| `leases` | Resource, holder, expiry, exclusive flag |

## 8. Tech stack

**Language.** Rust, on the local toolchain rustc 1.98.1 (latest stable, updated 2026-09-27). `rmcp` 3.4.1 is the official Tier-1 SDK, and Codex itself uses it.

The earlier probe server (with axum, sqlite-vec and rust-embed) measured **2.5–3.0 MB**. Without the HTTP stack, v1 should come in smaller [not yet measured].

| Concern | Choice | Notes |
|---|---|---|
| MCP | `rmcp` 3.4.1, features `server`, `macros`, `schemars`, `transport-io` | stdio only in v1 |
| Runtime | `tokio` 1.53 | required by rmcp |
| SQLite driver | **`rusqlite` 0.40, `bundled`** | Mature, the de-facto choice. Ships SQLite 3.53.2 with FTS5, JSON1 and extension loading compiled in, so there's nothing to install. Synchronous; call it through `spawn_blocking` (or `tokio-rusqlite`). |
| Migrations | Our own ~30-line migrator in `src/db.rs` (`user_version`) | We don't use `rusqlite_migration` 2.6: it reads the version *before* opening its transaction, so two agents starting at once both apply migration 1 and one fails. Our migrator re-reads the version under `BEGIN IMMEDIATE`. A lock on a sidecar file also serializes the first WAL switch, which otherwise fails with "database is locked" under contention. |
| Search | FTS5 BM25 (unicode61 + porter), plus a trigram index for identifiers and paths | No embeddings in v1 |
| Later, behind cargo features | `sqlite-vec` 0.1.9 (~70 KB); `model2vec-rs` / `fastembed` | Fuse with reciprocal rank fusion |
| CLI / logs | `clap` 4.6, `tracing` | |
| Release | `dist` 0.33 + `cargo-zigbuild` | Homebrew tap, `cargo binstall`, shell installer |

**SQLite crates we considered:**
- **`sqlx`:** async, with compile-time-checked queries. Heavier, and brings no benefit for short local writes.
- **Turso** (the Rust rewrite of SQLite, formerly Limbo): pre-1.0, and multi-process access is **experimental**. That's a dealbreaker when several agent processes share one file. Worth revisiting later.
- **`libsql`:** development has largely moved to Turso.
- **Diesel / SeaORM:** overkill for about 7 tables.

**Why not MySQL or Postgres:** they need a separate server and can't be embedded in the binary. Their strengths (many concurrent network writers, replication) don't apply to one developer on one machine.

```toml
[profile.release]
opt-level = "z"
lto = "fat"
codegen-units = 1
strip = "symbols"
panic = "unwind"
```

## 9. Decisions

| # | Question | Decision (2026-09-27) |
|---|---|---|
| 1 | Where do shared notes and docs live? | **(c)** The DB is the source of truth. chitchat also writes a Markdown export and indexes existing repo docs read-only. It never syncs both ways. |
| 2 | One machine or several? | One machine for v1. |
| 3 | v1 clients | **Claude Code + Codex CLI.** Gemini CLI, Cursor and OpenCode are candidates for later. |
| 4 | Waking idle agents / Claude Channels | Not needed: the human always prompts each agent. |
| 5 | Web UI | Deferred. `chitchat tail` covers watching the room from a terminal. |
| 6 | Database | SQLite through `rusqlite` (bundled), with one global DB at `~/.chitchat/`. |

## Unverified / to test early

- Whether Codex's `_meta.threadId` / `sessionId` on `tools/call` match the `session_id` its hooks receive.
- Whether Codex hooks fire under `codex exec`, and the one-time trust/approval flow for project hooks.
- Contention across processes under a realistic load: 3–4 agents plus hooks writing to one WAL database.
- Binary sizes for musl and Windows builds.
