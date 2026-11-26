-- A repository's identity survives a move (#132): a configured repository is found by its
-- configuration name (`repo.<name>`), and its path is an attribute that may change. A moved
-- repository keeps its row, so every row that points at it (messages, commits, items) follows
-- without being rewritten; the paths it was configured at before are its aliases, so a working
-- directory under one still attributes to it.
--
-- `name` is NULL for a root nothing configured by name (one registered by path only).
ALTER TABLE repo ADD COLUMN name TEXT;
CREATE UNIQUE INDEX repo_name ON repo (name) WHERE name IS NOT NULL;

-- A canonical path a repository was configured at before.
CREATE TABLE repo_alias (
    path TEXT PRIMARY KEY,
    repo_id INTEGER NOT NULL REFERENCES repo (id)
) STRICT;

-- The link of existing rows. An item id is `<repo name>#<card>`, so each row with items is
-- named after the prefix of its items' keys. When one name's items sit on more than one row
-- (a move before this migration), the row of the most recently updated item gets it: the unique
-- index allows one. Rows without items stay unnamed.
UPDATE repo SET name = (
    SELECT substr(i.item_key, 1, instr(i.item_key, '#') - 1)
    FROM item i
    WHERE i.repo_id = repo.id AND instr(i.item_key, '#') > 1
    ORDER BY i.updated_ms DESC, i.id DESC
    LIMIT 1
)
WHERE repo.id = (
    -- This row is the one of its (most recent) name: no other row has a later item of it.
    SELECT w.repo_id FROM item w
    WHERE instr(w.item_key, '#') > 1
      AND substr(w.item_key, 1, instr(w.item_key, '#') - 1) = (
          SELECT substr(i.item_key, 1, instr(i.item_key, '#') - 1)
          FROM item i
          WHERE i.repo_id = repo.id AND instr(i.item_key, '#') > 1
          ORDER BY i.updated_ms DESC, i.id DESC
          LIMIT 1)
    ORDER BY w.updated_ms DESC, w.id DESC
    LIMIT 1
);
