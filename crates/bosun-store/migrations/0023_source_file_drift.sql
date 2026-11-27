-- The drift counters of each source file: what tells the user that a provider's transcript
-- format moved under the parser (#6, #3 "Ingest contract").
--
-- `unknown_lines` counts the lines of record types the parser does not know (skipped).
-- `usage_lines` counts the lines of the provider's usage-bearing type (Claude: `assistant`),
-- whether they billed or not, and `unbilled_lines` those of them that billed nothing (a gap).
-- They count lines, not API messages: a message split over several lines counts once per line.
--
-- NULL when unknown: a file parsed before these columns existed keeps NULL until it is re-read
-- from zero (a restart or `bosun reingest`), because the lines before its offset were never
-- counted. Ingest sets them to the counts up to `parsed_offset`.
ALTER TABLE source_file ADD COLUMN unknown_lines INTEGER CHECK (unknown_lines >= 0);
ALTER TABLE source_file ADD COLUMN usage_lines INTEGER CHECK (usage_lines >= 0);
ALTER TABLE source_file ADD COLUMN unbilled_lines INTEGER CHECK (unbilled_lines >= 0);
