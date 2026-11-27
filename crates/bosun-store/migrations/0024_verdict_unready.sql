-- no-transaction
-- A verdict exit code that means "not ready", not "red" (#204): the repository's
-- `verdict.unready_exit`. A verdict run that exits with it is `unready`, neither a pass nor a
-- failure; the item parks (`unready`) and the verdict runs again after a backoff, uncounted, up to
-- a cap, which sends the item to Needs you with `resource_unready`. So `item` gets the park
-- reason, the reason and the count of not-ready runs (`unready`), and `verdict_run` the result.
-- `attempt.first_run` is unchanged: a not-ready run is never a first run.
--
-- SQLite cannot change a CHECK constraint, so `item` and `verdict_run` are rebuilt as migration
-- 21 does it: outside sqlx's transaction, foreign keys off, the rebuild in a transaction of its
-- own, every id and every column copied, keys on again. Every other column and constraint of
-- `item` is as migration 21 left it, and of `verdict_run` as migration 5 made it (no later
-- migration touched either; no table references `verdict_run`).
--
-- Outside sqlx's transaction, the script commits before sqlx records it in `_sqlx_migrations`:
-- a daemon that dies in between leaves the rebuilt tables without the record, and the next open
-- runs this script again. It re-runs cleanly: the rebuild copies every row and column of the
-- tables it finds, and `unready` is still 0 that early (no daemon has written an item since).
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
    park_reason TEXT CHECK (park_reason IN ('limit', 'paused', 'killed', 'daily_cap', 'unready')),
    park_until_ms INTEGER,
    park_next TEXT CHECK (park_next IN ('new_attempt', 'resume', 'reset', 'verify')),
    -- How a done item got there.
    outcome TEXT CHECK (outcome IN ('pass', 'flaky', 'user_closed')),
    -- Why an item needs the user.
    reason TEXT CHECK (reason IN (
        'lint', 'cycle', 'question', 'auth', 'budget', 'park_cap', 'config_changed',
        'attempts_exhausted', 'guard_protected_path', 'guard_ignored_file', 'guard_git_config',
        'landing_refused', 'handshake', 'identity', 'setup', 'guard_checkout', 'stale',
        'census_drop', 'resource_unready')),
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
    -- How many verdict runs exited with the repository's not-ready code (#204) since the
    -- counters were last reset.
    unready INTEGER NOT NULL DEFAULT 0 CHECK (unready >= 0),
    CHECK ((state = 'parked') = (park_reason IS NOT NULL AND park_next IS NOT NULL)),
    CHECK (state = 'parked' OR park_until_ms IS NULL),
    CHECK ((state = 'done') = (outcome IS NOT NULL)),
    CHECK ((state = 'needs_you') = (reason IS NOT NULL))
) STRICT;

INSERT INTO item_new (
    id, item_key, repo_id, card_key, state, park_reason, park_until_ms, park_next, outcome,
    reason, attempt_number, snapshot_id, seq, attempts, spawns, runs, failed, uncounted, boot,
    created_ms, updated_ms, conflicts)
SELECT
    id, item_key, repo_id, card_key, state, park_reason, park_until_ms, park_next, outcome,
    reason, attempt_number, snapshot_id, seq, attempts, spawns, runs, failed, uncounted, boot,
    created_ms, updated_ms, conflicts
FROM item;

DROP TABLE item;

ALTER TABLE item_new RENAME TO item;

CREATE INDEX item_repo ON item (repo_id);
CREATE INDEX item_state ON item (state);

CREATE TABLE verdict_run_new (
    id INTEGER PRIMARY KEY,
    attempt_id INTEGER NOT NULL REFERENCES attempt (id),
    -- The run's number (`bosun_core::Attempt::run`), unique within the item.
    run INTEGER NOT NULL CHECK (run >= 1),
    command TEXT NOT NULL,
    pid INTEGER CHECK (pid >= 0),
    pid_started_ms INTEGER,
    started_ms INTEGER NOT NULL,
    ended_ms INTEGER,
    result TEXT CHECK (result IN ('pass', 'fail', 'timeout', 'unready')),
    exit_code INTEGER,
    duration_ms INTEGER CHECK (duration_ms >= 0),
    -- The verdict line (#44 B) and the bounded output tail.
    verdict_line TEXT,
    tail TEXT,
    CHECK ((pid IS NULL) = (pid_started_ms IS NULL)),
    UNIQUE (attempt_id, run)
) STRICT;

INSERT INTO verdict_run_new (
    id, attempt_id, run, command, pid, pid_started_ms, started_ms, ended_ms, result, exit_code,
    duration_ms, verdict_line, tail)
SELECT
    id, attempt_id, run, command, pid, pid_started_ms, started_ms, ended_ms, result, exit_code,
    duration_ms, verdict_line, tail
FROM verdict_run;

DROP TABLE verdict_run;

ALTER TABLE verdict_run_new RENAME TO verdict_run;

COMMIT;

PRAGMA foreign_keys = ON;
