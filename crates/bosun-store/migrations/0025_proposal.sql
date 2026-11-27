-- Heat's proposals (#11, part D, "Heat: conclusions and Backlog proposals"): one row per
-- finding of a detector that is checked, deduplicated and followed up.
--
-- A row is the snapshot of the finding, never recomputed: the metric of its subject over a
-- window (its value, the detector's threshold, the target, the sample), the detector's version
-- and, in `params_json`, the rest of what the numbers were computed with (`bytes_per_token`
-- for an estimate, the highest `parser_version` of the window, the window's model and effort).
-- Integers are in the metric's unit; shares in ppm. `evidence_json` is capped at 16 KiB and
-- already redacted. A row without a metric, a value, a target and a window is refused: a card
-- without them is not created.
--
-- The lifecycle is `bosun_core::proposal`: `open -> (published) -> landed -> measured`,
-- `open | published -> dismissed`, `open -> expired`. `dedup_key` is detector, provider, subject
-- kind and repository-relative subject (`bosun_core::proposal::dedup_key`); one repository has
-- at most one active (`open`, `published` or `landed`) proposal per key. The publication
-- columns (`card_key`, `issue_number`, `published_ms`) are reserved for the publishing card.
CREATE TABLE proposal (
    id INTEGER PRIMARY KEY,
    proposal_key TEXT NOT NULL UNIQUE CHECK (proposal_key <> ''),
    dedup_key TEXT NOT NULL CHECK (dedup_key <> ''),
    repo_id INTEGER NOT NULL REFERENCES repo (id),
    provider TEXT NOT NULL CHECK (provider <> ''),
    detector TEXT NOT NULL CHECK (detector <> ''),
    detector_version INTEGER NOT NULL CHECK (detector_version >= 1),
    remedy TEXT NOT NULL CHECK (remedy <> ''),
    subject_kind TEXT NOT NULL
        CHECK (subject_kind IN ('file', 'dir', 'test', 'tool_target', 'command', 'repo')),
    subject TEXT NOT NULL CHECK (subject <> ''),
    -- The metric.
    metric TEXT NOT NULL CHECK (metric <> ''),
    unit TEXT NOT NULL CHECK (unit <> ''),
    exactness TEXT NOT NULL CHECK (exactness IN ('exact', 'estimate')),
    baseline_value INTEGER NOT NULL,
    threshold INTEGER NOT NULL,
    target_value INTEGER NOT NULL,
    window_from_ms INTEGER NOT NULL,
    window_to_ms INTEGER NOT NULL,
    sample INTEGER NOT NULL CHECK (sample > 0),
    -- The guard: a metric that must not get worse.
    guard_metric TEXT,
    guard_baseline INTEGER,
    guard_after INTEGER,
    -- The estimate and the rest of the snapshot.
    impact_eqv_estimate INTEGER,
    params_json TEXT NOT NULL DEFAULT '{}',
    evidence_json TEXT NOT NULL DEFAULT '[]' CHECK (length(evidence_json) <= 16384),
    -- The life.
    state TEXT NOT NULL
        CHECK (state IN ('open', 'published', 'landed', 'measured', 'dismissed', 'expired')),
    created_ms INTEGER NOT NULL,
    last_seen_ms INTEGER NOT NULL,
    seen_count INTEGER NOT NULL CHECK (seen_count >= 1),
    -- The publication (reserved).
    card_key TEXT,
    issue_number INTEGER,
    published_ms INTEGER,
    -- The landing.
    item_id INTEGER REFERENCES item (id),
    landed_sha TEXT,
    landed_ms INTEGER,
    landed_how TEXT CHECK (landed_how IN ('item', 'marker', 'manual')),
    -- The follow-up.
    followup_due_ms INTEGER,
    after_value INTEGER,
    after_sample INTEGER,
    measured_ms INTEGER,
    outcome TEXT CHECK (outcome IN ('reached', 'moved', 'not_moved', 'insufficient')),
    -- The dismissal.
    dismissed_ms INTEGER,
    dismiss_reason TEXT,
    CHECK (window_to_ms > window_from_ms),
    CHECK ((guard_metric IS NULL) = (guard_baseline IS NULL)),
    CHECK (state NOT IN ('landed', 'measured') OR (landed_ms IS NOT NULL AND landed_how IS NOT NULL)),
    CHECK ((state = 'measured') = (outcome IS NOT NULL AND measured_ms IS NOT NULL)),
    CHECK ((state = 'dismissed') = (dismissed_ms IS NOT NULL))
) STRICT;

CREATE UNIQUE INDEX proposal_active ON proposal (repo_id, dedup_key)
    WHERE state IN ('open', 'published', 'landed');
CREATE INDEX proposal_repo ON proposal (repo_id, state);
