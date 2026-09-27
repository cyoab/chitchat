-- chitchat schema v1.
--
-- Released migrations are immutable: change the schema by adding a new file and
-- appending it to MIGRATIONS in src/db.rs. All timestamps are unix milliseconds.

-- A project is a repository, identified so that every worktree of the same repo
-- maps to the same row.
CREATE TABLE projects (
    id         INTEGER PRIMARY KEY,
    key        TEXT NOT NULL UNIQUE,  -- "github.com/owner/repo", or "path:/abs/main/worktree"
    name       TEXT NOT NULL,
    root       TEXT,                  -- most recently seen worktree root
    created_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL
) STRICT;

-- One row per participant in a project: an agent session, or the human.
CREATE TABLE agents (
    id           INTEGER PRIMARY KEY,
    project_id   INTEGER NOT NULL REFERENCES projects (id),
    handle       TEXT NOT NULL,       -- "claude-1", "codex-2", "you"
    vendor       TEXT NOT NULL CHECK (vendor IN ('claude', 'codex', 'human')),
    session_id   TEXT,                -- the client's own session / thread id
    cwd          TEXT,
    status       TEXT,                -- short "working on ..." line shown to other agents
    created_at   INTEGER NOT NULL,
    last_seen_at INTEGER NOT NULL,
    UNIQUE (project_id, handle)
) STRICT;

CREATE UNIQUE INDEX agents_by_session
    ON agents (project_id, vendor, session_id)
    WHERE session_id IS NOT NULL;

-- A message goes either to a room or directly to one agent.
CREATE TABLE messages (
    id           INTEGER PRIMARY KEY,
    project_id   INTEGER NOT NULL REFERENCES projects (id),
    sender_id    INTEGER NOT NULL REFERENCES agents (id),
    room         TEXT,                               -- NULL for a direct message
    recipient_id INTEGER REFERENCES agents (id),     -- set only for a direct message
    thread_id    INTEGER REFERENCES messages (id),   -- root message of the thread
    intent       TEXT NOT NULL CHECK (intent IN ('request', 'inform', 'ack')),
    body         TEXT NOT NULL,
    created_at   INTEGER NOT NULL,
    CHECK ((room IS NULL) != (recipient_id IS NULL))
) STRICT;

CREATE INDEX messages_by_room ON messages (project_id, room, id);
CREATE INDEX messages_by_thread ON messages (thread_id) WHERE thread_id IS NOT NULL;

-- Fan-out on write: one row per (message, recipient), created when the message is
-- posted. Unread, unanswered-request and ack queries all read this table.
CREATE TABLE receipts (
    message_id   INTEGER NOT NULL REFERENCES messages (id) ON DELETE CASCADE,
    agent_id     INTEGER NOT NULL REFERENCES agents (id),
    mentioned    INTEGER NOT NULL DEFAULT 0 CHECK (mentioned IN (0, 1)),  -- @mention or DM
    delivered_at INTEGER,             -- first shown to the agent (hook or inbox)
    acked_at     INTEGER,             -- marked handled by the agent
    PRIMARY KEY (message_id, agent_id)
) STRICT, WITHOUT ROWID;

CREATE INDEX receipts_open ON receipts (agent_id, message_id) WHERE acked_at IS NULL;

-- Shared memory. The DB is the source of truth; Markdown export is derived from it.
CREATE TABLE notes (
    id            INTEGER PRIMARY KEY,
    project_id    INTEGER REFERENCES projects (id),  -- NULL = global, shared by all projects
    key           TEXT NOT NULL,                     -- stable topic key, e.g. "decision/storage"
    kind          TEXT NOT NULL,                     -- decision | fact | gotcha | doc | handoff ...
    title         TEXT NOT NULL,
    body          TEXT NOT NULL,
    tags          TEXT NOT NULL DEFAULT '',          -- space-separated
    revision      INTEGER NOT NULL DEFAULT 1,
    author_id     INTEGER REFERENCES agents (id),
    superseded_by INTEGER REFERENCES notes (id),
    created_at    INTEGER NOT NULL,
    updated_at    INTEGER NOT NULL,
    deleted_at    INTEGER                            -- soft delete
) STRICT;

CREATE UNIQUE INDEX notes_by_key ON notes (coalesce(project_id, 0), key);

-- Every revision of every note, including the current one.
CREATE TABLE note_versions (
    note_id    INTEGER NOT NULL REFERENCES notes (id) ON DELETE CASCADE,
    revision   INTEGER NOT NULL,
    title      TEXT NOT NULL,
    body       TEXT NOT NULL,
    tags       TEXT NOT NULL,
    author_id  INTEGER REFERENCES agents (id),
    created_at INTEGER NOT NULL,
    PRIMARY KEY (note_id, revision)
) STRICT, WITHOUT ROWID;

CREATE TRIGGER notes_version_on_insert AFTER INSERT ON notes BEGIN
    INSERT INTO note_versions (note_id, revision, title, body, tags, author_id, created_at)
    VALUES (new.id, new.revision, new.title, new.body, new.tags, new.author_id, new.updated_at);
END;

-- Writers bump `revision` on content changes; soft deletes and supersedes don't.
CREATE TRIGGER notes_version_on_update AFTER UPDATE OF revision ON notes BEGIN
    INSERT INTO note_versions (note_id, revision, title, body, tags, author_id, created_at)
    VALUES (new.id, new.revision, new.title, new.body, new.tags, new.author_id, new.updated_at);
END;

-- Full-text search (BM25). External-content tables, kept in sync by triggers.
CREATE VIRTUAL TABLE notes_fts USING fts5 (
    title, body, tags,
    content = 'notes', content_rowid = 'id',
    tokenize = 'porter unicode61'
);

CREATE TRIGGER notes_fts_on_insert AFTER INSERT ON notes BEGIN
    INSERT INTO notes_fts (rowid, title, body, tags) VALUES (new.id, new.title, new.body, new.tags);
END;

CREATE TRIGGER notes_fts_on_delete AFTER DELETE ON notes BEGIN
    INSERT INTO notes_fts (notes_fts, rowid, title, body, tags)
    VALUES ('delete', old.id, old.title, old.body, old.tags);
END;

CREATE TRIGGER notes_fts_on_update AFTER UPDATE OF title, body, tags ON notes BEGIN
    INSERT INTO notes_fts (notes_fts, rowid, title, body, tags)
    VALUES ('delete', old.id, old.title, old.body, old.tags);
    INSERT INTO notes_fts (rowid, title, body, tags) VALUES (new.id, new.title, new.body, new.tags);
END;

CREATE VIRTUAL TABLE messages_fts USING fts5 (
    body,
    content = 'messages', content_rowid = 'id',
    tokenize = 'porter unicode61'
);

-- Messages are immutable, so there is no update trigger.
CREATE TRIGGER messages_fts_on_insert AFTER INSERT ON messages BEGIN
    INSERT INTO messages_fts (rowid, body) VALUES (new.id, new.body);
END;

CREATE TRIGGER messages_fts_on_delete AFTER DELETE ON messages BEGIN
    INSERT INTO messages_fts (messages_fts, rowid, body) VALUES ('delete', old.id, old.body);
END;

-- Claims on files or tasks. Exclusivity is enforced by writers inside a
-- BEGIN IMMEDIATE transaction, since expiry makes a unique index impossible.
CREATE TABLE leases (
    id          INTEGER PRIMARY KEY,
    project_id  INTEGER NOT NULL REFERENCES projects (id),
    resource    TEXT NOT NULL,        -- "file:src/db.rs", "task:auth-refactor"
    holder_id   INTEGER NOT NULL REFERENCES agents (id),
    exclusive   INTEGER NOT NULL DEFAULT 1 CHECK (exclusive IN (0, 1)),
    reason      TEXT,
    created_at  INTEGER NOT NULL,
    expires_at  INTEGER NOT NULL,
    released_at INTEGER
) STRICT;

CREATE INDEX leases_active ON leases (project_id, resource) WHERE released_at IS NULL;
