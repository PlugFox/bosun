-- The tool calls refused during an attempt's sessions (#7: refusals per attempt): one row per
-- refused call, in the order the session's stream lists them (`seq` from 0 within the session
-- `spawn`). `source` says who refused it: `permission`, the agent CLI under the permission lists
-- (for Claude Code, the result's `permission_denials`); `hook`, a repository's PreToolUse hook
-- (reserved: not read yet). `tool` is the tool's name; `target` the call's subject (a command
-- line, a path, a URL; capped by the runner), NULL when the stream did not say; `reason` why
-- it was refused (Claude Code's `decision_reason_type`), NULL when the stream no longer said.
-- The targets name files and commands, so a public repository's item report only counts them;
-- `bosun report` shows them redacted. Written at the session's end; a replayed write adds
-- nothing.
CREATE TABLE attempt_denial (
    id INTEGER PRIMARY KEY,
    attempt_id INTEGER NOT NULL REFERENCES attempt (id),
    spawn INTEGER NOT NULL,
    seq INTEGER NOT NULL CHECK (seq >= 0),
    source TEXT NOT NULL CHECK (source IN ('permission', 'hook')),
    tool TEXT NOT NULL,
    target TEXT,
    reason TEXT,
    UNIQUE (attempt_id, spawn, seq)
) STRICT;
