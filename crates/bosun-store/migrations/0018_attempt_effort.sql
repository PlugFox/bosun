-- The effort level the attempt's sessions asked for (#7, `--effort <level>`): the card's
-- `effort`, else the repository's `repo.<name>.effort`. Every session of an attempt asks for the
-- same one: it comes from the snapshot and the hashed configuration. NULL: no effort was passed,
-- and the agent CLI used its own default. The item report shows it on the attempt's line.
ALTER TABLE attempt ADD COLUMN effort TEXT;
