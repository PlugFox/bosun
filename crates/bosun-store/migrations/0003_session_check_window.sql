-- The provider's window of a session check, and costs the provider did not give (#46).
--
-- Claude Code's `cost-state` totals are not the session's over all time: its counter starts
-- again when a session is resumed in a new process (`startTime`), and records written after
-- the line are not in it. The check is compared with the messages inside the window only.

-- One row per root session that has a check, written with its `session_check` rows.
CREATE TABLE session_check_window (
    root_session_id INTEGER PRIMARY KEY REFERENCES session (id),
    -- When the provider's counter started (`startTime`); NULL when it did not say.
    start_ms INTEGER,
    -- The timestamp of the last timestamped record, of any type, before the check's line in
    -- the root transcript; NULL when none was seen.
    end_ms INTEGER,
    -- The provider's own total cost (`totalCostUSD`); NULL when it did not give one.
    total_cost_usd REAL
) STRICT;

-- A session check stored before this migration has no row in `session_check_window`: its
-- window is unbounded on both sides, the old behaviour, until `bosun reingest` re-reads it.

-- A model's cost the provider did not give stays missing (NULL), not 0. SQLite cannot drop a
-- NOT NULL constraint, so the table is rebuilt. Costs stored before this migration as 0 for a
-- missing value cannot be told apart from a real 0 and are kept.
CREATE TABLE session_check_new (
    id INTEGER PRIMARY KEY,
    root_session_id INTEGER NOT NULL REFERENCES session (id),
    model TEXT NOT NULL,
    input_tokens INTEGER NOT NULL CHECK (input_tokens >= 0),
    cache_write_tokens INTEGER NOT NULL CHECK (cache_write_tokens >= 0),
    cache_read_tokens INTEGER NOT NULL CHECK (cache_read_tokens >= 0),
    output_tokens INTEGER NOT NULL CHECK (output_tokens >= 0),
    -- The provider's own figure, as it reported it; NULL when it gave none.
    cost_usd REAL,
    recorded_ms INTEGER NOT NULL,
    UNIQUE (root_session_id, model)
) STRICT;

INSERT INTO session_check_new
    (id, root_session_id, model, input_tokens, cache_write_tokens, cache_read_tokens,
     output_tokens, cost_usd, recorded_ms)
SELECT id, root_session_id, model, input_tokens, cache_write_tokens, cache_read_tokens,
       output_tokens, cost_usd, recorded_ms
FROM session_check;

DROP TABLE session_check;

ALTER TABLE session_check_new RENAME TO session_check;

-- The last timestamp seen in each source file, carried from one read to the next: a read can
-- begin right at a `cost-state` line, whose window ends at the timestamp before it. NULL for a
-- file read before this column existed, until it is re-read from zero.
ALTER TABLE source_file ADD COLUMN last_ts_ms INTEGER;
