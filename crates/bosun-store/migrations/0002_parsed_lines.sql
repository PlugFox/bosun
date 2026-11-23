-- The complete lines parsed of each source file, for the report's ingest health (#26): the
-- fraction of gap lines per parser version is a metric (#3, "Ingest contract").
--
-- NULL when unknown: a file parsed before this column existed keeps NULL until it is re-read
-- from zero (a restart or `bosun reingest`), because the lines before its offset were never
-- counted. Ingest sets it to the lines up to `parsed_offset`.
ALTER TABLE source_file ADD COLUMN parsed_lines INTEGER CHECK (parsed_lines >= 0);
