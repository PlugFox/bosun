-- no-transaction
-- A new reason an item needs the user (#64): `handshake`, a session whose MCP server
-- (`bosun-mcp`) did not say hello in time, again after one uncounted retry.
--
-- SQLite cannot change a CHECK constraint, so `item` is rebuilt with its rows and indexes. Six
-- tables reference it, and dropping it with foreign keys enforced would delete through them (a
-- deferred check does not help: the copy is a different table until it is renamed). So this
-- migration runs outside sqlx's transaction, the way SQLite documents a table rebuild: foreign
-- keys off, the rebuild in a transaction of its own, keys on again. Every id is copied, so
-- every reference holds afterwards.
PRAGMA foreign_keys = OFF;

BEGIN;

CREATE TABLE item_new (
    id INTEGER PRIMARY KEY,
    -- `bosun_core::ItemId`, unique across repositories.
    item_key TEXT NOT NULL UNIQUE,
    repo_id INTEGER NOT NULL REFERENCES repo (id),
    -- The queue backend's id of the card, if it has one (a Projects v2 item node id).
    card_key TEXT,
    state TEXT NOT NULL DEFAULT 'ready' CHECK (state IN (
        'ready', 'claimed', 'running', 'parked', 'verifying', 'landing', 'done', 'needs_you',
        'cancelled')),
    -- A parked item: why, until when and what it does on waking.
    park_reason TEXT CHECK (park_reason IN ('limit', 'paused', 'killed', 'daily_cap')),
    park_until_ms INTEGER,
    park_next TEXT CHECK (park_next IN ('new_attempt', 'resume', 'reset', 'verify')),
    -- How a done item got there.
    outcome TEXT CHECK (outcome IN ('pass', 'flaky', 'user_closed')),
    -- Why an item needs the user.
    reason TEXT CHECK (reason IN (
        'lint', 'cycle', 'question', 'auth', 'budget', 'park_cap', 'config_changed',
        'attempts_exhausted', 'guard_protected_path', 'guard_ignored_file', 'guard_git_config',
        'landing_refused', 'handshake')),
    -- The current (or last) attempt's number; NULL before the first.
    attempt_number INTEGER CHECK (attempt_number >= 1),
    -- The snapshot the next attempt runs from: the last one taken.
    snapshot_id INTEGER REFERENCES snapshot (id),
    seq INTEGER NOT NULL DEFAULT 0 CHECK (seq >= 0),
    attempts INTEGER NOT NULL DEFAULT 0 CHECK (attempts >= 0),
    spawns INTEGER NOT NULL DEFAULT 0 CHECK (spawns >= 0),
    runs INTEGER NOT NULL DEFAULT 0 CHECK (runs >= 0),
    failed INTEGER NOT NULL DEFAULT 0 CHECK (failed >= 0),
    uncounted INTEGER NOT NULL DEFAULT 0 CHECK (uncounted >= 0),
    boot INTEGER NOT NULL DEFAULT 0 CHECK (boot >= 0),
    created_ms INTEGER NOT NULL,
    updated_ms INTEGER NOT NULL,
    CHECK ((state = 'parked') = (park_reason IS NOT NULL AND park_next IS NOT NULL)),
    CHECK (state = 'parked' OR park_until_ms IS NULL),
    CHECK ((state = 'done') = (outcome IS NOT NULL)),
    CHECK ((state = 'needs_you') = (reason IS NOT NULL))
) STRICT;

INSERT INTO item_new (
    id, item_key, repo_id, card_key, state, park_reason, park_until_ms, park_next, outcome,
    reason, attempt_number, snapshot_id, seq, attempts, spawns, runs, failed, uncounted, boot,
    created_ms, updated_ms)
SELECT
    id, item_key, repo_id, card_key, state, park_reason, park_until_ms, park_next, outcome,
    reason, attempt_number, snapshot_id, seq, attempts, spawns, runs, failed, uncounted, boot,
    created_ms, updated_ms
FROM item;

DROP TABLE item;

ALTER TABLE item_new RENAME TO item;

CREATE INDEX item_repo ON item (repo_id);
CREATE INDEX item_state ON item (state);

COMMIT;

PRAGMA foreign_keys = ON;
