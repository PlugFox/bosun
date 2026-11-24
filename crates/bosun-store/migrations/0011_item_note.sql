-- What the daemon found about an item that its Needs-you comment shows and no other row holds
-- (#111): the lint findings of a bounce, the figure of a budget cap that tripped. Written with
-- the transition, so a comment still in the outbox when the daemon restarts is posted with
-- them. One note per item and kind; a later bounce or cap replaces it.
CREATE TABLE item_note (
    id INTEGER PRIMARY KEY,
    item_id INTEGER NOT NULL REFERENCES item (id),
    -- `lint` (a JSON array of findings) or `budget` (a JSON object: cap, used, limit).
    kind TEXT NOT NULL,
    body TEXT NOT NULL,
    ts_ms INTEGER NOT NULL,
    UNIQUE (item_id, kind)
) STRICT;
