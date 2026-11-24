-- no-transaction
-- Stage 2 (#8): the worktrees the daemon made, the session shims it started (with how far it
-- has read their output, E18), the history of waits for leased resources (E15: the live
-- leases are in the daemon's memory and its journal, not here) and where each landing stands.
-- And on `item`: how many conflicts its landing met, and two more reasons an item needs the
-- user, `setup` and `guard_checkout`.
--
-- SQLite cannot change a CHECK constraint, so `item` is rebuilt as migration 9 does it: outside
-- sqlx's transaction, foreign keys off, the rebuild in a transaction of its own, every id
-- copied, keys on again.
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
        'landing_refused', 'handshake', 'identity', 'setup', 'guard_checkout')),
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
    -- How many times the item's landing met a conflict (#8).
    conflicts INTEGER NOT NULL DEFAULT 0 CHECK (conflicts >= 0),
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

-- One git worktree the daemon made for an item (#8): the item's own, or a throwaway checkout a
-- verdict ran in. A path is never reused, so it is the key.
CREATE TABLE worktree (
    id INTEGER PRIMARY KEY,
    item_id INTEGER NOT NULL REFERENCES item (id),
    path TEXT NOT NULL UNIQUE,
    branch TEXT NOT NULL,
    base_sha TEXT NOT NULL,
    -- 'item' or 'verdict'.
    kind TEXT NOT NULL CHECK (kind IN ('item', 'verdict')),
    state TEXT NOT NULL DEFAULT 'created' CHECK (state IN ('created', 'removed', 'quarantined')),
    -- A quarantined worktree: the file that blocked removal, and the error.
    blocking TEXT,
    error TEXT,
    -- Its size on disk when it was last measured, in bytes.
    bytes INTEGER CHECK (bytes >= 0),
    -- The user's checkout as it was at claim (E4), as JSON; NULL for a verdict checkout.
    user_base_json TEXT,
    created_ms INTEGER NOT NULL,
    ended_ms INTEGER,
    CHECK ((state = 'created') = (ended_ms IS NULL))
) STRICT;
CREATE INDEX worktree_item ON worktree (item_id);

-- One session shim the daemon started (#8): where its record is, how far the daemon has read
-- its output files, and the digest of the agent credential its session was issued.
CREATE TABLE process (
    id INTEGER PRIMARY KEY,
    item_id INTEGER NOT NULL REFERENCES item (id),
    attempt INTEGER NOT NULL CHECK (attempt >= 1),
    spawn INTEGER NOT NULL CHECK (spawn >= 1),
    session_key TEXT NOT NULL,
    pid INTEGER NOT NULL CHECK (pid >= 0),
    pid_started_ms INTEGER NOT NULL,
    record_path TEXT NOT NULL UNIQUE,
    stdout_offset INTEGER NOT NULL DEFAULT 0 CHECK (stdout_offset >= 0),
    stderr_offset INTEGER NOT NULL DEFAULT 0 CHECK (stderr_offset >= 0),
    credential_digest BLOB,
    started_ms INTEGER NOT NULL,
    gone_ms INTEGER
) STRICT;
CREATE INDEX process_item ON process (item_id);

-- The history of waits for leased resources (#8, E15): one row per request and resource.
-- `item_id` is NULL for a `bosun with` command. `holder` is the holder's label.
CREATE TABLE lease_wait (
    id INTEGER PRIMARY KEY,
    item_id INTEGER REFERENCES item (id),
    holder TEXT NOT NULL,
    resource TEXT NOT NULL,
    filed_ms INTEGER NOT NULL,
    granted_ms INTEGER,
    released_ms INTEGER,
    UNIQUE (holder, resource, filed_ms)
) STRICT;

-- Where an item's landing stands (#8): every step of landing is redone from this row.
CREATE TABLE landing (
    id INTEGER PRIMARY KEY,
    item_id INTEGER NOT NULL UNIQUE REFERENCES item (id),
    attempt INTEGER NOT NULL CHECK (attempt >= 1),
    target TEXT NOT NULL,
    -- The item's head after the rebase, the tree its verdict passed on, the commit it landed as.
    head_sha TEXT,
    verified_tree TEXT,
    landed_sha TEXT,
    delivered_ms INTEGER,
    pr_number INTEGER CHECK (pr_number >= 1),
    pr_url TEXT,
    pr_state TEXT CHECK (pr_state IN ('open', 'merged', 'closed')),
    updated_ms INTEGER NOT NULL
) STRICT;

COMMIT;

PRAGMA foreign_keys = ON;
