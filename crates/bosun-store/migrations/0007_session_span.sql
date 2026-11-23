-- The span of each context window (#95): `session.started_ms` and `ended_ms` are the first and
-- last time of the messages the window owns, kept by the writer on every message it merges.
-- Ingest never wrote them before, so the inferred commit link (#23) linked nothing; the backfill
-- gives a stored database its spans without a reingest.

-- The writer reads a window's first and last message on every merge: one index seek each.
DROP INDEX message_session;
CREATE INDEX message_session ON message (session_id, ts_ms);

UPDATE session SET
    started_ms = (SELECT min(ts_ms) FROM message WHERE session_id = session.id),
    ended_ms = (SELECT max(ts_ms) FROM message WHERE session_id = session.id)
WHERE EXISTS (SELECT 1 FROM message WHERE session_id = session.id);
