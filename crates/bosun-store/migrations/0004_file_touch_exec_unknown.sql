-- Shell commands as file touches (#76, #44 A).
--
-- A `Bash` or `PowerShell` call now touches the files its command names. Two kinds are new:
-- `exec`, a script run by an interpreter (`python x.py`), which is neither a read nor a write;
-- and `unknown`, a file named by a command whose effect on it is not known, which is a gap and
-- never a read. Only paths inside the message's repository are stored for them.
--
-- SQLite cannot change a CHECK constraint, so the table is rebuilt with its rows and index.
-- Calls stored before this migration have no shell touches until `bosun reingest` re-reads
-- them with parser version 3.
CREATE TABLE file_touch_new (
    id INTEGER PRIMARY KEY,
    tool_call_id INTEGER NOT NULL REFERENCES tool_call (id),
    -- Repository-relative; absolute outside every repository, for file tools only.
    path TEXT NOT NULL,
    kind TEXT NOT NULL CHECK (kind IN ('read', 'write', 'search', 'exec', 'unknown')),
    UNIQUE (tool_call_id, path, kind)
) STRICT;

INSERT INTO file_touch_new (id, tool_call_id, path, kind)
SELECT id, tool_call_id, path, kind
FROM file_touch;

DROP TABLE file_touch;

ALTER TABLE file_touch_new RENAME TO file_touch;

CREATE INDEX file_touch_path ON file_touch (path);
