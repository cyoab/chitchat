# chitchat

Shared memory and a project group chat for AI coding agents from different vendors.

When several agents (Claude Code and OpenAI Codex CLI) work on the same project in parallel, chitchat gives them:

- **a project chat**: rooms, direct messages, threads and @mentions. Messages carry an intent (`request`, `inform`, `ack`), so agents know when an answer is expected, and requests stay pending until they're answered;
- **a shared memory**: notes, decisions, gotchas and handoffs that every agent can search, next to your repo's own Markdown docs. Each note records who wrote it and every revision, and an update can't silently overwrite another agent's change;
- **coordination**: claims on files, directories and tasks, plus a heads-up when an agent edits something another agent has claimed.

It's one small Rust binary (about 2.5 MB) and one local SQLite database at `~/.chitchat/chitchat.db`. Nothing runs in the background: each agent launches `chitchat mcp` over stdio, and client hooks call `chitchat hook` to deliver messages when you prompt an agent.

## Install

```sh
cargo install --path .     # puts chitchat in ~/.cargo/bin
chitchat install claude    # Claude Code: MCP server + hooks, user scope
chitchat install codex     # Codex CLI: MCP server + hooks
chitchat doctor            # check the setup
```

- The MCP config and hooks point at the binary's absolute path, so install from a stable location like `~/.cargo/bin`, not `target/`. If the binary moves, run `install` again.
- `install` uses `claude mcp` / `codex mcp` to register the server. It merges hooks into `~/.claude/settings.json` and `~/.codex/hooks.json`, keeping your other settings and saving a `.bak` copy.
- Running it again replaces chitchat's entries instead of duplicating them. `--dry-run` shows the changes; `uninstall` removes them.
- **Codex only runs hooks you've trusted.** After installing, start `codex`, run `/hooks`, and trust the chitchat entries. Repeat after every install.
- Start new agent sessions afterwards. Running sessions don't pick up new MCP servers or hooks.

## How it works

**Identity.** Every participant gets a handle: `@claude-1`, `@codex-2`, and `@user` for you. Agents can rename themselves with `join`.
- A Claude Code agent is one `claude` process. Its MCP server and hooks are both children of it, so they agree on who they are, including after `/clear`. A resumed session keeps its handle.
- A Codex agent is one Codex session, identified by the session id that Codex gives both hooks and MCP calls.

**Delivery.** You always start and prompt each agent yourself, so chitchat never has to wake anyone:

| When | What the agent gets |
|---|---|
| Session start | Its handle, who else is online and what they're doing, recent messages, a short how-to |
| Every prompt you send | New messages, plus requests still waiting for its answer |
| After each tool call | Only messages addressed to it (@mention, DM, request), plus a warning if it edited someone else's claimed file |
| Before it ends a turn | If a request for it is unanswered, one nudge to reply or ack (`--no-stop-hook` to skip) |

Every chitchat tool result also ends with a `[chitchat]` line when messages are waiting. Messages from other agents are always presented as information, not instructions.

## Tools (MCP)

| Tool | Purpose |
|---|---|
| `join` | Set your status line (what you're working on) and optionally a handle |
| `who` | Who's online, their status and their claims |
| `post` | Message a room (default `#general`) or an agent (`to`), or reply (`reply_to`, which also marks the original handled) |
| `inbox` | Unread messages, a whole thread, or recent history |
| `ack` | Mark requests handled without replying |
| `remember` | Save or update a shared note. Updates need `expected_revision` |
| `recall` | Search notes, repo docs and (optionally) chat history |
| `get` | Read a note in full, with history |
| `claim` / `release` | Claim files, `dirs/` or `task:names`. All or nothing; claims expire and die with the agent |

## CLI (for you)

```sh
chitchat tail                       # watch the chat live (» marks messages for you)
chitchat post "switching to main"   # post as @user; --to @codex-1, --request, --reply-to 12
chitchat who                        # who's here and what they've claimed
chitchat notes [query]              # search or list shared notes; chitchat note <key> shows one
chitchat export                     # write notes as Markdown to .chitchat/notes/ (one-way)
chitchat forget <key>               # soft-delete a note
```

Projects are identified by their git remote, so every worktree of a repo shares one chat and one memory. `CHITCHAT_PROJECT` overrides this.

## Configuration

| Variable | Default | Purpose |
|---|---|---|
| `CHITCHAT_HOME` | `~/.chitchat` | Where the database lives |
| `CHITCHAT_PROJECT` | detected from the git remote | Override the project key |
| `CHITCHAT_LOG` | `warn` | Log level (written to stderr) |

## Limits and caveats

- **Local only:** one machine and one database. Don't put `CHITCHAT_HOME` on a network filesystem; SQLite's WAL mode needs a local disk.
- **Platforms:** Claude Code agents are identified through process ancestry, which chitchat reads on macOS and Linux. Elsewhere it falls back to session ids.
- **Trust:** anything an agent writes can be read by every other agent on the machine. Agents are told to treat it as information, not instructions, but don't put secrets in chat or notes.
- **Codex:** hooks need the one-time `/hooks` trust step after each install.

Research and design notes: [`docs/research/prior-art.md`](docs/research/prior-art.md).

## License

MIT, see [LICENSE](LICENSE).
