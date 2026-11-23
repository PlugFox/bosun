-- The stage-0 tables of #3 ("Schema"). Forward-only: a released migration is never edited; a
-- change is a new file with the next version.
--
-- Conventions: timestamps are UTC integer milliseconds (`*_ms`); token counts and byte counts
-- are integers; prices are integer micro-dollars per million tokens, the unit of
-- `bosun_core::Rates`. Every derived number (cost, eqv) is computed at query time: no column
-- here holds one. SQL holds no merge logic; the writer applies `bosun_core::merge`.

CREATE TABLE repo (
    id INTEGER PRIMARY KEY,
    -- The canonical path key (`bosun_core::canonical_path`).
    path TEXT NOT NULL UNIQUE,
    remote TEXT,
    default_branch TEXT
) STRICT;

-- One per provider in this database (one database per identity, #2).
CREATE TABLE account (
    id INTEGER PRIMARY KEY,
    provider TEXT NOT NULL UNIQUE,
    label TEXT NOT NULL
) STRICT;

-- A model id as transcripts name it.
CREATE TABLE model (
    id INTEGER PRIMARY KEY,
    provider TEXT NOT NULL,
    name TEXT NOT NULL,
    UNIQUE (provider, name)
) STRICT;

-- The versioned price table. A row prices `model` (as transcripts name it, or without the date
-- suffix) over [valid_from_ms, valid_to_ms); a row with `prompt_threshold` applies only to a
-- message whose prompt size is strictly greater. Changing a price closes a row and adds one;
-- it never touches `message`.
CREATE TABLE price (
    id INTEGER PRIMARY KEY,
    -- The revision of the table that added the row; the seed is version 1.
    version INTEGER NOT NULL CHECK (version >= 1),
    model TEXT NOT NULL,
    valid_from_ms INTEGER NOT NULL,
    valid_to_ms INTEGER CHECK (valid_to_ms IS NULL OR valid_to_ms > valid_from_ms),
    prompt_threshold INTEGER CHECK (prompt_threshold IS NULL OR prompt_threshold >= 0),
    input INTEGER NOT NULL CHECK (input >= 0),
    cache_write_5m INTEGER NOT NULL CHECK (cache_write_5m >= 0),
    cache_write_1h INTEGER NOT NULL CHECK (cache_write_1h >= 0),
    cache_read INTEGER NOT NULL CHECK (cache_read >= 0),
    output INTEGER NOT NULL CHECK (output >= 0),
    -- Set on the rows of the model every eqv is measured against; one model only.
    is_reference INTEGER NOT NULL DEFAULT 0 CHECK (is_reference IN (0, 1))
) STRICT;

CREATE UNIQUE INDEX price_key ON price (model, ifnull(prompt_threshold, -1), valid_from_ms);

-- A transcript file, identified by its file id (not its path, so a rename is not a new file).
CREATE TABLE source_file (
    id INTEGER PRIMARY KEY,
    provider TEXT NOT NULL,
    -- The file id (`bosun_core::Origin::file`).
    file_key TEXT NOT NULL,
    path TEXT NOT NULL,
    size INTEGER NOT NULL DEFAULT 0 CHECK (size >= 0),
    parsed_offset INTEGER NOT NULL DEFAULT 0 CHECK (parsed_offset >= 0),
    parser_version INTEGER NOT NULL DEFAULT 0,
    -- Hash of the last parsed line, to tell a rewritten file from a grown one.
    last_line_hash TEXT,
    UNIQUE (provider, file_key)
) STRICT;

-- One row per transcript file, that is per context window.
CREATE TABLE session (
    id INTEGER PRIMARY KEY,
    provider TEXT NOT NULL,
    -- The provider's session id.
    session_key TEXT NOT NULL,
    -- The sub-agent id; NULL for the main context.
    agent_key TEXT,
    kind TEXT NOT NULL DEFAULT 'interactive' CHECK (kind IN ('run', 'interactive')),
    parent_id INTEGER REFERENCES session (id),
    root_id INTEGER REFERENCES session (id),
    agent_type TEXT,
    spawn_depth INTEGER,
    spawn_tool_use_id TEXT,
    first_cwd TEXT,
    started_ms INTEGER,
    ended_ms INTEGER
) STRICT;

CREATE UNIQUE INDEX session_key ON session (provider, session_key, ifnull(agent_key, ''));
CREATE INDEX session_parent ON session (parent_id);

-- One API message, assembled from its records by `bosun_core::merge`. Counted once: the key is
-- (provider, message id).
CREATE TABLE message (
    id INTEGER PRIMARY KEY,
    provider TEXT NOT NULL,
    message_key TEXT NOT NULL,
    -- The owning window and file: those of the earliest record (#3, "Dedup and assembly").
    session_id INTEGER NOT NULL REFERENCES session (id),
    source_file_id INTEGER NOT NULL REFERENCES source_file (id),
    ts_ms INTEGER NOT NULL,
    model_id INTEGER NOT NULL REFERENCES model (id),
    cwd TEXT NOT NULL,
    -- Attribution by cwd; NULL until a repository is matched.
    repo_id INTEGER REFERENCES repo (id),
    input_tokens INTEGER NOT NULL CHECK (input_tokens >= 0),
    cache_write_5m_tokens INTEGER NOT NULL CHECK (cache_write_5m_tokens >= 0),
    cache_write_1h_tokens INTEGER NOT NULL CHECK (cache_write_1h_tokens >= 0),
    cache_read_tokens INTEGER NOT NULL CHECK (cache_read_tokens >= 0),
    output_tokens INTEGER NOT NULL CHECK (output_tokens >= 0),
    -- The provider's usage object as it was, for fields that are not columns.
    usage_json TEXT NOT NULL,
    parser_version INTEGER NOT NULL,
    UNIQUE (provider, message_key)
) STRICT;

CREATE INDEX message_ts ON message (ts_ms);
CREATE INDEX message_session ON message (session_id);
CREATE INDEX message_repo ON message (repo_id);

CREATE TABLE tool_call (
    id INTEGER PRIMARY KEY,
    message_id INTEGER NOT NULL REFERENCES message (id),
    tool_use_key TEXT NOT NULL,
    name TEXT NOT NULL,
    -- A file path, or the head (first word) of a command.
    target TEXT,
    input_bytes INTEGER NOT NULL CHECK (input_bytes >= 0),
    result_bytes INTEGER NOT NULL CHECK (result_bytes >= 0),
    is_error INTEGER NOT NULL CHECK (is_error IN (0, 1)),
    UNIQUE (message_id, tool_use_key)
) STRICT;

CREATE TABLE file_touch (
    id INTEGER PRIMARY KEY,
    tool_call_id INTEGER NOT NULL REFERENCES tool_call (id),
    -- Repository-relative.
    path TEXT NOT NULL,
    kind TEXT NOT NULL CHECK (kind IN ('read', 'write', 'search')),
    UNIQUE (tool_call_id, path, kind)
) STRICT;

CREATE INDEX file_touch_path ON file_touch (path);

CREATE TABLE "commit" (
    id INTEGER PRIMARY KEY,
    repo_id INTEGER NOT NULL REFERENCES repo (id),
    sha TEXT NOT NULL,
    ts_ms INTEGER NOT NULL,
    author TEXT NOT NULL,
    lines_added INTEGER NOT NULL CHECK (lines_added >= 0),
    lines_removed INTEGER NOT NULL CHECK (lines_removed >= 0),
    -- A JSON object of trailer name to the list of its values.
    trailers_json TEXT NOT NULL DEFAULT '{}',
    UNIQUE (repo_id, sha)
) STRICT;

CREATE INDEX commit_ts ON "commit" (ts_ms);

-- A line that could not be read as expected. Metadata only, never the line itself.
CREATE TABLE ingest_gap (
    id INTEGER PRIMARY KEY,
    source_file_id INTEGER NOT NULL REFERENCES source_file (id),
    byte_offset INTEGER NOT NULL CHECK (byte_offset >= 0),
    length INTEGER NOT NULL CHECK (length >= 0),
    record_type TEXT,
    -- A JSON array of the line's top-level key names.
    top_level_keys_json TEXT NOT NULL,
    reason TEXT NOT NULL,
    parser_version INTEGER NOT NULL,
    UNIQUE (source_file_id, byte_offset)
) STRICT;

-- The provider's own per-model totals for a root session (Claude Code `cost-state`, the last
-- record wins). A cross-check, never a source of the bill.
CREATE TABLE session_check (
    id INTEGER PRIMARY KEY,
    root_session_id INTEGER NOT NULL REFERENCES session (id),
    model TEXT NOT NULL,
    input_tokens INTEGER NOT NULL CHECK (input_tokens >= 0),
    cache_write_tokens INTEGER NOT NULL CHECK (cache_write_tokens >= 0),
    cache_read_tokens INTEGER NOT NULL CHECK (cache_read_tokens >= 0),
    output_tokens INTEGER NOT NULL CHECK (output_tokens >= 0),
    -- The provider's own figure, as it reported it.
    cost_usd REAL NOT NULL,
    recorded_ms INTEGER NOT NULL,
    UNIQUE (root_session_id, model)
) STRICT;

-- Shown on trend charts. Nothing writes it in stage 0.
CREATE TABLE annotation (
    id INTEGER PRIMARY KEY,
    ts_ms INTEGER NOT NULL,
    repo_id INTEGER REFERENCES repo (id),
    text TEXT NOT NULL
) STRICT;
