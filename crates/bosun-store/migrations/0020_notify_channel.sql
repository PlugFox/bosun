-- The notification channels of the identity (#9, the decision on channels): one row per
-- channel, created, changed and removed through the `notify.channel.*` methods. A channel
-- belongs to the identity whose database holds it; there is no channel shared across
-- identities (#2).
--
-- No column holds a secret: `params` is a JSON object of the kind's non-secret parameters (a
-- Telegram chat id, a webhook URL), and `secret_entry` is the name of the OS credential-store
-- entry the secret lives in (`notify/<identity>/<name>`, under the service `bosun`), never the
-- secret. Reading this file or one of its backups reveals no token.
--
-- `events` is a JSON array of the event names the channel gets (`needs_you`, `repo_paused`,
-- `ready_drained`); `min_gap_ms` the least time between two of its messages; `last_sent_ms` when
-- the last one went out (the drainer writes it). A removed channel keeps its row with
-- `removed_ms`; a name is unique among the live rows only, so it can be added again.
--
-- Numbered 20 although 21 exists: 20 was left to this card, whose run came later.
CREATE TABLE notify_channel (
    id INTEGER PRIMARY KEY,
    name TEXT NOT NULL CHECK (
        length(name) BETWEEN 1 AND 32 AND name NOT GLOB '*[^a-z0-9-]*'),
    kind TEXT NOT NULL,
    params TEXT NOT NULL CHECK (json_type(params) = 'object'),
    events TEXT NOT NULL CHECK (json_type(events) = 'array'),
    min_gap_ms INTEGER NOT NULL CHECK (min_gap_ms >= 0),
    secret_entry TEXT NOT NULL,
    last_sent_ms INTEGER,
    created_ms INTEGER NOT NULL,
    changed_ms INTEGER NOT NULL,
    removed_ms INTEGER
) STRICT;

CREATE UNIQUE INDEX notify_channel_live_name ON notify_channel (name) WHERE removed_ms IS NULL;
