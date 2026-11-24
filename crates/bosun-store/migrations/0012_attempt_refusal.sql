-- Why the daemon refused an attempt before its verdict (#137): the failed preconditions
-- (`bosun_git::precondition::Reason`) or the guard refusals (`bosun_git::Refusal`), in the
-- daemon's words, as a JSON array of strings. NULL when nothing refused it. The item report,
-- `bosun item` and the next attempt's prompt show them.
ALTER TABLE attempt ADD COLUMN refusal TEXT;
