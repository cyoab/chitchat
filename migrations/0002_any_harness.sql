-- chitchat schema v2: agents can come from any supported harness, not just
-- Claude Code and Codex, so `vendor` holds a harness id ("claude", "codex",
-- "gemini", "cursor", ...) or "human" instead of a fixed list.
--
-- SQLite can't change a CHECK constraint in place, so the table is rebuilt.
-- The migrator runs this with foreign-key enforcement off and checks the
-- foreign keys before committing.

CREATE TABLE agents_v2 (
    id                INTEGER PRIMARY KEY,
    project_id        INTEGER NOT NULL REFERENCES projects (id),
    handle            TEXT NOT NULL,  -- "claude-1", "gemini-2", "user"
    vendor            TEXT NOT NULL CHECK (vendor GLOB '[a-z]*' AND length(vendor) <= 32),
    client_pid        INTEGER,        -- the harness process
    client_started_at INTEGER,        -- its start time, to survive pid reuse
    session_id        TEXT,           -- the harness's current session / thread id
    cwd               TEXT,
    status            TEXT,           -- short "working on ..." line shown to other agents
    created_at        INTEGER NOT NULL,
    last_seen_at      INTEGER NOT NULL,
    UNIQUE (project_id, handle)
) STRICT;

INSERT INTO agents_v2 SELECT id, project_id, handle, vendor, client_pid, client_started_at,
                             session_id, cwd, status, created_at, last_seen_at
                      FROM agents;
DROP TABLE agents;
ALTER TABLE agents_v2 RENAME TO agents;

CREATE INDEX agents_by_client
    ON agents (project_id, client_pid, client_started_at)
    WHERE client_pid IS NOT NULL;

CREATE INDEX agents_by_session ON agents (project_id, vendor, session_id)
    WHERE session_id IS NOT NULL;
