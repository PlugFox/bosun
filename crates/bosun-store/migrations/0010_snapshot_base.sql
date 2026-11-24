-- The rest of the base captured at claim (#68): every ref and the stash entries, as JSON, next
-- to `base_sha` and `branch`. Crash recovery rebuilds the base from them, so the preconditions
-- of an attempt that outlived a daemon compare the repository with the same base as before.
-- Snapshots taken before this migration have neither; their items cannot be resumed.
ALTER TABLE snapshot ADD COLUMN base_refs TEXT;
ALTER TABLE snapshot ADD COLUMN base_stash TEXT;
