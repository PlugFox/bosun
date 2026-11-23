-- The tables of the loop (#7, #57): items, their snapshots, attempts, the sessions bosun runs
-- for them, verdict runs, the transition log, the outbox of board writes, and the agent's calls.
--
-- State names are those of `bosun_core::State::as_str` and `bosun_core::AttemptState::as_str`.
-- The rows here let the daemon rebuild every item exactly (`bosun_core::Item`), so crash
-- recovery reads its state from the database, never from the board. Timestamps are UTC integer
-- milliseconds; durations are measured on a monotonic clock and stored as such.

-- One start of the daemon. Its number orders recoveries: an item is recovered once per boot.
CREATE TABLE boot (
    number INTEGER PRIMARY KEY CHECK (number >= 1),
    started_ms INTEGER NOT NULL,
    pid INTEGER NOT NULL CHECK (pid >= 0)
) STRICT;

-- One task, backed by one card. The state columns hold `bosun_core::Item`: the state and its
-- detail, the current (or last) attempt and the counters.
CREATE TABLE item (
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
        'landing_refused')),
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

CREATE INDEX item_repo ON item (repo_id);
CREATE INDEX item_state ON item (state);

-- What the item was at claim (#5): the body, the parsed contract block and the comments that
-- will be inlined, their hash, and the hashes and base the claim captured. The verdict always
-- runs from here, never from the live issue.
CREATE TABLE snapshot (
    id INTEGER PRIMARY KEY,
    item_id INTEGER NOT NULL REFERENCES item (id),
    -- The idempotence key of the claim that took it: a replay stores nothing new.
    snapshot_key TEXT NOT NULL UNIQUE,
    taken_ms INTEGER NOT NULL,
    hash TEXT NOT NULL,
    body TEXT NOT NULL,
    contract_json TEXT NOT NULL,
    comments_json TEXT NOT NULL DEFAULT '[]',
    config_hash TEXT,
    policy_hash TEXT,
    base_sha TEXT,
    branch TEXT
) STRICT;

CREATE INDEX snapshot_item ON snapshot (item_id);

-- One try at an item (`bosun_core::Attempt`), with the child process of its current session
-- (pid and start time, #50) while one runs.
CREATE TABLE attempt (
    id INTEGER PRIMARY KEY,
    item_id INTEGER NOT NULL REFERENCES item (id),
    number INTEGER NOT NULL CHECK (number >= 1),
    snapshot_id INTEGER REFERENCES snapshot (id),
    state TEXT NOT NULL CHECK (state IN (
        'spawned', 'exited', 'killed', 'limited', 'verdict_pending', 'verdict_running', 'pass',
        'fail', 'flaky', 'timeout')),
    -- The current session's and verdict run's numbers (item-wide); run 0 before the first.
    spawn INTEGER NOT NULL CHECK (spawn >= 1),
    run INTEGER NOT NULL DEFAULT 0 CHECK (run >= 0),
    -- The first, non-passing, verdict run's result while its re-run is pending.
    first_run TEXT CHECK (first_run IN ('pass', 'fail', 'timeout')),
    crash_retries INTEGER NOT NULL DEFAULT 0 CHECK (crash_retries >= 0),
    -- HEAD when the attempt's session last ended: the end of its commit range.
    head_sha TEXT,
    pid INTEGER CHECK (pid >= 0),
    pid_started_ms INTEGER,
    started_ms INTEGER NOT NULL,
    -- Set when the attempt reaches pass, fail, flaky or timeout.
    ended_ms INTEGER,
    CHECK ((pid IS NULL) = (pid_started_ms IS NULL)),
    UNIQUE (item_id, number)
) STRICT;

-- One session bosun spawned for an attempt, keyed by the provider session id it pre-assigned
-- (`--session-id`). A resume keeps the id, so the key is not unique. A `session` row with this
-- provider and key is `kind = 'run'`, whichever of the two rows is written first.
CREATE TABLE run (
    id INTEGER PRIMARY KEY,
    attempt_id INTEGER NOT NULL REFERENCES attempt (id),
    -- The session's number (`bosun_core::Attempt::spawn`), unique within the item.
    spawn INTEGER NOT NULL CHECK (spawn >= 1),
    mode TEXT NOT NULL CHECK (mode IN ('fresh', 'resume', 'reset')),
    provider TEXT NOT NULL,
    session_key TEXT NOT NULL,
    started_ms INTEGER NOT NULL,
    ended_ms INTEGER,
    -- The runner's classification of the exit, and a limit's reset time if one was parsed.
    exit TEXT CHECK (exit IN ('ok', 'limit', 'auth', 'crash')),
    limit_reset_ms INTEGER,
    CHECK (exit = 'limit' OR limit_reset_ms IS NULL),
    UNIQUE (attempt_id, spawn)
) STRICT;

CREATE INDEX run_session ON run (provider, session_key);

-- One run of an attempt's verdict, with its child process while it runs.
CREATE TABLE verdict_run (
    id INTEGER PRIMARY KEY,
    attempt_id INTEGER NOT NULL REFERENCES attempt (id),
    -- The run's number (`bosun_core::Attempt::run`), unique within the item.
    run INTEGER NOT NULL CHECK (run >= 1),
    command TEXT NOT NULL,
    pid INTEGER CHECK (pid >= 0),
    pid_started_ms INTEGER,
    started_ms INTEGER NOT NULL,
    ended_ms INTEGER,
    result TEXT CHECK (result IN ('pass', 'fail', 'timeout')),
    exit_code INTEGER,
    duration_ms INTEGER CHECK (duration_ms >= 0),
    -- The verdict line (#44 B) and the bounded output tail.
    verdict_line TEXT,
    tail TEXT,
    CHECK ((pid IS NULL) = (pid_started_ms IS NULL)),
    UNIQUE (attempt_id, run)
) STRICT;

-- One arrow of the item machine, written before any of its effects (#1, invariant 7). The key
-- is `<item>/t<seq>`: a replayed transition is a no-op.
CREATE TABLE transition (
    id INTEGER PRIMARY KEY,
    transition_key TEXT NOT NULL UNIQUE,
    item_id INTEGER NOT NULL REFERENCES item (id),
    seq INTEGER NOT NULL CHECK (seq >= 1),
    attempt INTEGER NOT NULL CHECK (attempt >= 0),
    from_state TEXT NOT NULL,
    to_state TEXT NOT NULL,
    -- The park reason, outcome or needs-you reason of `to_state`, as in `item`.
    to_detail TEXT,
    attempt_from TEXT,
    attempt_to TEXT,
    cause TEXT NOT NULL,
    -- The effects in order: a JSON array of {"key", "kind"}.
    effects_json TEXT NOT NULL DEFAULT '[]',
    ts_ms INTEGER NOT NULL
) STRICT;

CREATE INDEX transition_item ON transition (item_id, seq);

-- Board writes, replayed in order until acknowledged (#5). No rendered body is stored: a
-- comment or the sticky report is rendered from the database at send time. Pending rows of one
-- `coalesce_key` (the sticky report) are one row; `revision` counts the writes folded into it,
-- and an acknowledgement of an older revision leaves the row pending.
CREATE TABLE outbox (
    id INTEGER PRIMARY KEY,
    item_id INTEGER NOT NULL REFERENCES item (id),
    -- The effect's key (`<item>/a<attempt>/t<seq>/e<index>`), also the comment's hidden key.
    effect_key TEXT NOT NULL UNIQUE,
    op TEXT NOT NULL CHECK (op IN ('project', 'comment', 'upsert_comment')),
    column_name TEXT CHECK (column_name IN ('in_progress', 'verify', 'done', 'needs_you')),
    reason TEXT,
    coalesce_key TEXT,
    revision INTEGER NOT NULL DEFAULT 1 CHECK (revision >= 1),
    created_ms INTEGER NOT NULL,
    tries INTEGER NOT NULL DEFAULT 0 CHECK (tries >= 0),
    last_error TEXT,
    retry_at_ms INTEGER,
    acked_ms INTEGER,
    CHECK ((op = 'project') = (column_name IS NOT NULL)),
    CHECK ((op = 'comment') = (reason IS NOT NULL))
) STRICT;

CREATE UNIQUE INDEX outbox_pending_coalesce ON outbox (coalesce_key)
    WHERE acked_ms IS NULL AND coalesce_key IS NOT NULL;
CREATE INDEX outbox_pending ON outbox (acked_ms, id);

-- A comment the daemon created on the board, by its key, so a replay edits it rather than
-- posting another, and the prompt builder can leave it out by node id (#2).
CREATE TABLE board_comment (
    id INTEGER PRIMARY KEY,
    item_id INTEGER NOT NULL REFERENCES item (id),
    comment_key TEXT NOT NULL,
    node_id TEXT NOT NULL,
    created_ms INTEGER NOT NULL,
    UNIQUE (item_id, comment_key)
) STRICT;

-- A call of the agent through bosun-mcp (#7): a summary, a question or a note, bound to its
-- attempt and to HEAD at the time of the call. Not a transition: a blocking question records
-- its transition separately. Every call is a row, a refused one (over its rate cap) too.
CREATE TABLE agent_call (
    id INTEGER PRIMARY KEY,
    attempt_id INTEGER NOT NULL REFERENCES attempt (id),
    kind TEXT NOT NULL CHECK (kind IN ('summary', 'question', 'note')),
    body TEXT NOT NULL,
    -- A summary's decisions and follow-ups: JSON arrays of strings.
    decisions_json TEXT,
    followups_json TEXT,
    -- A question blocks the item.
    blocking INTEGER CHECK (blocking IN (0, 1)),
    head_sha TEXT,
    refused INTEGER NOT NULL DEFAULT 0 CHECK (refused IN (0, 1)),
    ts_ms INTEGER NOT NULL,
    CHECK ((kind = 'question') = (blocking IS NOT NULL))
) STRICT;

CREATE INDEX agent_call_attempt ON agent_call (attempt_id, kind);

-- A test that failed on a verdict's first run and passed on its re-run (#44 D).
CREATE TABLE flaky_test (
    id INTEGER PRIMARY KEY,
    repo_id INTEGER NOT NULL REFERENCES repo (id),
    test_name TEXT NOT NULL,
    item_id INTEGER NOT NULL REFERENCES item (id),
    attempt_id INTEGER NOT NULL REFERENCES attempt (id),
    ts_ms INTEGER NOT NULL,
    UNIQUE (attempt_id, test_name)
) STRICT;

CREATE INDEX flaky_test_name ON flaky_test (repo_id, test_name);
