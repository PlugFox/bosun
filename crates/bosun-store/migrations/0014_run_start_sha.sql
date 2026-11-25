-- HEAD when a session started (#8): the start of its commit range, recorded before the spawn,
-- so a daemon that re-attaches the session after a restart (E3, E22) counts only the session's
-- own commits. NULL for a run written before this column, and when HEAD could not be read.
ALTER TABLE run ADD COLUMN start_sha TEXT;
