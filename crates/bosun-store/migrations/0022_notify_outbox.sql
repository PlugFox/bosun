-- The notification outbox of the identity (#9, "Delivery through the outbox"): one row per
-- notice and channel, written in the transaction of the event it tells of (a transition into
-- Needs you, a repository paused by the breaker, a drained Ready column), and sent by the
-- daemon's notification drainer.
--
-- A notice is fanned out at queue time to every live channel whose `events` has it; the
-- `(channel_id, key)` pair is unique, so a replayed event queues nothing more. A row holds
-- what to say, never a message: `facts` is a JSON object of the numbers and words the text
-- needs (the item machine's reason, the attempt, the breaker's figures), built by the daemon,
-- never text an agent wrote. The message is rendered at send time.
--
-- A row is pending until it is sent (`sent_ms`) or dropped (`dropped_ms`: too old, refused
-- for good by the service, or its channel removed). `tries`, `last_error` and `retry_at_ms`
-- are the backoff of failed sends.
CREATE TABLE notify_outbox (
    id INTEGER PRIMARY KEY,
    channel_id INTEGER NOT NULL REFERENCES notify_channel (id),
    key TEXT NOT NULL,
    event TEXT NOT NULL,
    repo TEXT NOT NULL,
    item TEXT,
    facts TEXT NOT NULL CHECK (json_type(facts) = 'object'),
    created_ms INTEGER NOT NULL,
    tries INTEGER NOT NULL DEFAULT 0 CHECK (tries >= 0),
    last_error TEXT,
    retry_at_ms INTEGER,
    sent_ms INTEGER,
    dropped_ms INTEGER,
    UNIQUE (channel_id, key)
) STRICT;

CREATE INDEX notify_outbox_pending ON notify_outbox (channel_id, created_ms)
    WHERE sent_ms IS NULL AND dropped_ms IS NULL;
