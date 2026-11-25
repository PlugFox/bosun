-- The `setup` or `teardown` command running in a worktree (#8, E3): its child, by pid and start
-- time, recorded while it runs, so a daemon that died meanwhile leaves something to reap. Both
-- NULL or both set.
ALTER TABLE worktree ADD COLUMN child_pid INTEGER CHECK (child_pid >= 0);
ALTER TABLE worktree ADD COLUMN child_started_ms INTEGER
    CHECK ((child_pid IS NULL) = (child_started_ms IS NULL));
