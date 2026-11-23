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
| `fork-tie` | original `…0101` in project `-fixture-repo-b` (`M1`, `M2`, cwd `repo-b`); fork `…0102` in `-fixture-repo-a` (`derived-from` via `fork`) repeats `M1` with the **same timestamp** and cwd `repo-a`, then adds `M3` | a timestamp tie goes to the smaller session id although the fork's file sorts first by path; equal usage records are told apart by the larger cwd, so `M1` counts in `repo-b` (#6, a tie found on a real history) |
| `subagent-cost-state` | main session (`M1` spawns an `Agent`, `M2`) and one sub-agent on Sonnet (2 messages, the first split in two records); two `cost-state` records at the end, the second the cumulative total of main plus sub-agent per model, the first smaller | the sub-agent is its own context window and adds to the total; `cost-state` is a cross-check, not a source |
| `cwd-change` | Windows paths; one session whose `cwd` moves `repo-a` → `repo-a\src` → `repo-b` → `repo-ab` → `repo-b\docs` | attribution is per message by prefix on whole components: `repo-ab` is under no root |
| `partial-last-line` | `M1`, `M2`, then a complete `M3` record with no trailing newline | the last line is still being written and is not counted |
| `skipped-records` | an unknown record type carrying a usage block; an `assistant` record whose `input_tokens` is a string and that repeats `M1`'s id with a larger output; an `assistant` record whose `message` is a string; a torn non-JSON line; known non-usage types | only well-formed `assistant` records count |
| `cache-and-models` | usage without `cache_creation` (all writes 5 min); a 1 h write; a `<synthetic>` message (zeros, uuid id); Haiku 5.5 with a 150 532-token prompt, over the 100k threshold; both kinds of writes | categories are taken field by field; the threshold matters to pricing, not to the sums |
| `resumed-cost-state` | case 11, day 11. Main session: `M1` on Opus, a `system` record and a `cost-state` of the first process (`startTime` 00:00); then the session resumes in a new process at 01:00 (`startTime` 01:00): `M2` spawns an `Agent`, a sub-agent on Sonnet (`M3`, `M4`) ends with a `cost-state` line of its own, the main transcript adds `M5`, a `system` record at 01:00:09 and the last `cost-state`; after it, at 02:00, a stale message with `M3`'s counts on Opus. The last `cost-state` names Opus as `claude-opus-5-5[1m]` with 500 cache reads more than `M2` + `M5`, Sonnet as `M3` + `M4`, and Haiku (input 100, output 50, no cost), which no transcript holds; it has no `totalCostUSD` | the sums count every message (4 main, 2 sub-agent); the provider's window is 01:00:00 to 01:00:09, the last timestamp before the line, so the check compares `M2`, `M5`, `M3`, `M4`: transcripts 29 755, provider 30 405, a difference of −650 (−2.1%), of which 150 is the absent Haiku and 500 the Opus remainder. The sub-agent's `cost-state` is not the session's check (#46) |
| `report` | for the report (#26, #27): searches and reads before the first write, the same file read twice in one session and again in a second session, a tool result with `is_error`, a compaction (`compact_boundary`, prompt size 8065 → 706) | plain sums; the case exists for the report's estimates |
| `shell-touches` | case 12, day 12, the twelve-runs fixture (#76, #44 A). One session of 13 messages (input 1, 5 min 10, read 1000, output 1 to 13), each a `Bash` call: twelve commands `sed -n '<range>p' src/lib.rs && python tool/check.py -- cargo test`, then `rustfmt --check src/lib.rs > /fixture/elsewhere/fmt.log 2>&1; cat /fixture/elsewhere/notes.md` | plain sums (13 messages); the case exists for the shell classifier: twelve execs of `tool/check.py` and no read of it, twelve reads of `src/lib.rs`, one unknown touch of it by `rustfmt`, and nothing of the files outside the repository |

## `stub/`: a synthetic second provider

Not a real provider: a format invented to prove that the event model needs no new field for a
provider other than Claude (#6, #20). `bosun-ingest`'s `stub` module documents it.

```
stub/<case>/
  sessions/<session>.jsonl   one event per line: `start`, `turn` (one API message), `result`
  repos.json                 as for `claude/`
  expected.json              as for `claude/`, written by hand
```

Ids, timestamps and paths follow the templates above; the case number of `basic` is 91, its
day is 9.

| Case | What it holds | Why the sums are what they are |
| --- | --- | --- |
| `basic` | a `start`; a turn in `repo-a` that opens a file; a turn in `repo-b`, under no root, that runs a failing command; their results; an unknown `telemetry` event | 2 messages, one per repository entry; the unknown event is skipped |
