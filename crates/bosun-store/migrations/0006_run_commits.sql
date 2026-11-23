-- The commit range of each run session (#72): HEAD when the session started and when it ended,
-- and the commits of `base..head` as git listed them when it ended. The report links a commit
-- to its session and item through these rows: exactly, never by time.

ALTER TABLE run ADD COLUMN base_sha TEXT;
ALTER TABLE run ADD COLUMN head_sha TEXT;

CREATE TABLE run_commit (
    id INTEGER PRIMARY KEY,
    run_id INTEGER NOT NULL REFERENCES run (id),
    repo_id INTEGER NOT NULL REFERENCES repo (id),
    sha TEXT NOT NULL,
    -- One session made a commit: the first range that lists it keeps it.
    UNIQUE (repo_id, sha)
) STRICT;

CREATE INDEX run_commit_run ON run_commit (run_id);
