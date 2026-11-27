-- What a tool call needs for the detectors of #210 beyond its sizes: a shell command's
-- fingerprint and redacted text (a command run verbatim again and again), the times of a call
-- and its result (wall time per tool), and the media part of a result (media bytes apart from
-- text bytes). Claude's parser version 4 writes them (#210).
--
-- `command_hash` is the lowercase hex SHA-256 (64 digits) of a shell command's text,
-- whitespace-normalised and scrubbed, so that two spellings of one command share it.
-- `command_text` is that same text cut to at most 200 bytes (bytes, not characters) at a char
-- boundary, after scrubbing. `call_ts_ms` is the timestamp of the call's record and
-- `result_ts_ms` that of its result's own record, milliseconds since the Unix epoch, UTC.
-- `result_media_bytes` is the part of `result_bytes` that is media (base64 image data);
-- `result_bytes` keeps counting text and media together. Byte counts and times are exact.
--
-- What text may be stored, exactly: for a `Bash` or `PowerShell` call of a message whose cwd
-- lies in a repository root (or an alias of one), its `command` input, whitespace-normalised,
-- scrubbed (`bosun_core::publish::scrub`), at most 200 bytes. Such a text may name a path
-- outside the repository; that is accepted. Nothing of any other tool's input (no
-- description, no file content, no prompt), no byte of any result's content, and nothing at
-- all (text or hash) of a call of a message attributed to no repository.
--
-- NULL when unknown: every column is NULL for a row written by Claude's parser version 3 or
-- less until `bosun reingest`, and for another provider's call that does not give it.
-- `result_ts_ms` and `result_media_bytes` are NULL until the call's result is seen. A message
-- attributed to a repository only after it was stored (a root added later) keeps NULL in
-- `command_hash` and `command_text` until `bosun reingest`.
ALTER TABLE tool_call ADD COLUMN command_hash TEXT
    CHECK (command_hash IS NULL OR length(command_hash) = 64);
ALTER TABLE tool_call ADD COLUMN command_text TEXT
    CHECK (command_text IS NULL OR length(CAST(command_text AS BLOB)) <= 200);
ALTER TABLE tool_call ADD COLUMN call_ts_ms INTEGER;
ALTER TABLE tool_call ADD COLUMN result_ts_ms INTEGER;
ALTER TABLE tool_call ADD COLUMN result_media_bytes INTEGER CHECK (result_media_bytes >= 0);
