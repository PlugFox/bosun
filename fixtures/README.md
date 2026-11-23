# Fixtures

Synthetic inputs for tests. The repository is public: nothing here comes from a real session,
machine or repository. `cargo xtask lint-fixtures` (#16) enforces the templates below on every
file except Markdown and runs in CI; anything that does not match fails.

## Templates

| What | Template | Example |
| --- | --- | --- |
| UUID-shaped ids (session ids, record `uuid`, `parentUuid`, `promptId`, `leafUuid`, the id of a `<synthetic>` message) | `00000000-0000-4000-8000-` and 12 digits | `00000000-0000-4000-8000-001100000000` |
| Message, request and tool-use ids | `msg_`, `req_`, `toolu_` or `srvtoolu_`, 16 zeros, 8 digits | `msg_000000000000000001100001` |
| Sub-agent ids | `a`, 8 zeros, 8 digits | `a0000000003100001` |
| Timestamps | January 2000, UTC, milliseconds | `2000-01-01T00:00:01.000Z` |
| Epoch milliseconds (`startTime`) | within January 2000 | `946857600000` |
| Paths (`cwd`, `file_path`, `path`, repository roots, any absolute path in text) | under `/fixture` or `C:\fixture` | `/fixture/repo-a/src/lib.rs` |
| Project directories | Claude Code's encoding of a synthetic cwd: `-fixture...` or `C--fixture...` | `-fixture-repo-a` |
| E-mail addresses | at `example.com`, `example.org`, `example.net` or `example.invalid` | |

Transcript files are named after template ids: `projects/<project>/<session uuid>.jsonl`, and
`projects/<project>/<session uuid>/subagents/agent-<agent id>.jsonl` with its `.meta.json`.
A field named like an id (`id`, `uuid`, `*Id`, `*_id`, `*Uuid`, `session`, `agent`) must hold a
template id even when its value does not look like a real one, and a 13-digit number (real
epoch milliseconds) or a `~/` path anywhere fails.

The digits of the ids in `claude/` are allocated by case so that they read back: a session id
ends in `0` + case number (2 digits) + session number + zeros; record uuids start their last
group with `1`, message and request ids end in case, session, `000`, message number; tool-use
ids in case, session, `900`, call number. Timestamps use the case number as the day.

## `claude/`: Claude Code transcripts

One directory per case, in the layout of #3 ("Claude Code transcript format"):

```
claude/<case>/
  projects/<project>/<session>.jsonl
  projects/<project>/<session>/subagents/agent-<agent>.jsonl
  projects/<project>/<session>/subagents/agent-<agent>.meta.json
  repos.json      {"roots": [...]}: the repository roots messages are attributed to
  expected.json   the sums, computed by hand from the design of the case
```

`expected.json` has the shape `recount` prints (documented in `crates/recount/src/shape.rs`):
per context window (a main transcript, or a sub-agent with its `agent` id), per repository
(`null` for a cwd under no root) and in total, the message count and the five categories of
#3: `input`, `cache_write_5m`, `cache_write_1h`, `cache_read`, `output`. It is the independent
oracle: it is written by hand from the case's design and never copied from a program's
output. `recount` over each case must equal it (`cargo test -p recount`).

Token counts are small and distinct so the sums can be checked by eye. Messages reused across
cases: `M1` = (input 1, 5 min 20, 1 h 300, read 4000, output 50000), `M2` = (6, 70, 800, 9000,
10), `M3` = (2, 30, 0, 5000, 600), `M4` = (3, 0, 400, 6000, 700), `M5` = (4, 50, 0, 7000, 80).

| Case | What it holds | Why the sums are what they are |
| --- | --- | --- |
| `split-message` | `M1` written as three records (thinking, text, tool_use; output 10000, 30000, 50000), then `M2` | `M1` counts once with its last, largest output: 2 messages, not 4 records |
| `fork-continue` | parent `…0022` (`M1`, `M2`, `continued-in`); continued session `…0021` (`derived-from` via `resume`) repeats `M1`, `M2` an hour later and adds `M3`; fork `…0023` (`derived-from` via `fork`) repeats `M1` two hours later and adds `M4` | a repeated id belongs to the file where it appears earliest, the parent, although the continued session sorts first; nothing doubles |
| `subagent-cost-state` | main session (`M1` spawns an `Agent`, `M2`) and one sub-agent on Sonnet (2 messages, the first split in two records); two `cost-state` records at the end, the second the cumulative total of main plus sub-agent per model, the first smaller | the sub-agent is its own context window and adds to the total; `cost-state` is a cross-check, not a source |
| `cwd-change` | Windows paths; one session whose `cwd` moves `repo-a` → `repo-a\src` → `repo-b` → `repo-ab` → `repo-b\docs` | attribution is per message by prefix on whole components: `repo-ab` is under no root |
| `partial-last-line` | `M1`, `M2`, then a complete `M3` record with no trailing newline | the last line is still being written and is not counted |
| `skipped-records` | an unknown record type carrying a usage block; an `assistant` record whose `input_tokens` is a string and that repeats `M1`'s id with a larger output; an `assistant` record whose `message` is a string; a torn non-JSON line; known non-usage types | only well-formed `assistant` records count |
| `cache-and-models` | usage without `cache_creation` (all writes 5 min); a 1 h write; a `<synthetic>` message (zeros, uuid id); Haiku 5.5 with a 150 532-token prompt, over the 100k threshold; both kinds of writes | categories are taken field by field; the threshold matters to pricing, not to the sums |
| `report` | for the report (#26, #27): searches and reads before the first write, the same file read twice in one session and again in a second session, a tool result with `is_error`, a compaction (`compact_boundary`, prompt size 8065 → 706) | plain sums; the case exists for the report's estimates |
