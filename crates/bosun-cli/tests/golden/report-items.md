# bosun report: personal

Range: 2000-01-10T00:00:00Z to 2000-01-11T00:00:00Z (end excluded). Scope: every repository.

Every number is exact: raw token counts from the transcripts, priced at the rates valid at each message's time. Eqv is input-equivalent tokens of claude-opus-5-5.

## Total

|  | sessions | messages | eqv | cost USD |
| --- | ---: | ---: | ---: | ---: |
| real total | 4 | 14 | 4556 | 0.0182 |
| old method: the last usage block per session | 4 | 4 | 1792 | 0.0072 |

The real total is 2.5x the old method's number.

## By category

| category | tokens | eqv | share of eqv | cost USD |
| --- | ---: | ---: | ---: | ---: |
| input | 163 | 136 | 3.0% | 0.0005 |
| cache write 5m | 1630 | 1700 | 37.3% | 0.0068 |
| cache write 1h | 0 | 0 | 0.0% | 0.0000 |
| cache read | 16300 | 680 | 14.9% | 0.0027 |
| output | 489 | 2040 | 44.8% | 0.0082 |

## By model

| model | messages | input | cache write 5m | cache write 1h | cache read | output | eqv | cost USD |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| claude-opus-5-5 | 9 | 109 | 1090 | 0 | 10900 | 327 | 3652 | 0.0146 |
| claude-sonnet-5-5 | 5 | 54 | 540 | 0 | 5400 | 162 | 904 | 0.0036 |
| total | 14 | 163 | 1630 | 0 | 16300 | 489 | 4556 | 0.0182 |

## By session kind

| kind | sessions | messages | eqv | share of eqv | cost USD |
| --- | ---: | ---: | ---: | ---: | ---: |
| interactive | 1 | 2 | 1273 | 27.9% | 0.0051 |
| run | 3 | 12 | 3283 | 72.1% | 0.0131 |

## Eqv per commit

Eqv of the messages in a repository over its commits, both in the period; the previous period is 2000-01-09T00:00:00Z to 2000-01-10T00:00:00Z.

| repository | commits | eqv | eqv per commit | previous commits | previous eqv | previous eqv per commit | change |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| a | 5 | 4556 | 911 | 0 | 0 |  |  |
| in repositories | 5 | 4556 | 911 | 0 | 0 |  |  |

## Context curve

Prompt size (input plus cache reads and writes) per context window; a compaction is a drop of it between consecutive messages. Windows: 5; compactions: 0.

| session | agent | messages | start | peak | compactions |
| --- | --- | ---: | ---: | ---: | ---: |
| 00000000-0000-4000-8000-072000000009 |  | 2 | 2109 | 2109 | 0 |
| 00000000-0000-4000-8000-072000000003 |  | 2 | 1887 | 1887 | 0 |
| 00000000-0000-4000-8000-072000000002 |  | 3 | 1443 | 1443 | 0 |
| 00000000-0000-4000-8000-072000000001 |  | 5 | 555 | 1221 | 0 |
| 00000000-0000-4000-8000-072000000001 | a0000000072000001 | 2 | 777 | 888 | 0 |

## Ingest health

Over the whole store, whatever the range.

| parser version | files | files with a line count | lines | gaps | gaps per line |
| --- | ---: | ---: | ---: | ---: | ---: |
| 3 | 5 | 5 | 14 | 0 | 0.0% |
| total | 5 | 5 | 14 | 0 | 0.0% |

### Agreement with the provider's own totals

Sessions with the provider's totals (the last `cost-state` record of the main transcript) and a message in the range, main transcript plus sub-agents, over the messages inside the provider's window (from its `startTime` to the last record before the line): 0; within 5% on the total: 0; transcripts larger by more than 5%: 0. Not compared, with no message inside the window: 0.

## Items

Runs: the messages of the sessions bosun spawned for each item, sub-agents included, over the item's whole history. Exact; sessions inferred for an item are under the estimates and are not in these numbers.

| item | repository | state | attempts | failed | sessions | wall clock | messages | runs eqv | runs cost USD | parked |
| --- | --- | --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | --- |
| local:demo#1 | a | done (flaky) | 2 | 1 | 2 | 0:35:19 | 10 | 2144 | 0.0086 | limit 0:15:00 (1x) |
| local:demo#2 | a | needs_you (guard_protected_path) | 1 | 0 | 1 | 0:00:00 | 2 | 1139 | 0.0046 |  |

### Attempts

| item | attempt | state | started | wall clock | sessions | models | refused calls | messages | input | cache write 5m | cache write 1h | cache read | output | eqv | cost USD |
| --- | ---: | --- | --- | ---: | --- | --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| local:demo#1 | 1 | verdict_running | 2000-01-10T00:00:01Z | 0:18:39 | 1 fresh 00000000-0000-4000-8000-072000000001 (limit); 2 resume 00000000-0000-4000-8000-072000000001 (ok) | claude-opus-5-5, claude-sonnet-5-5 | 0 | 7 | 52 | 520 | 0 | 5200 | 156 | 1491 | 0.0060 |
| local:demo#1 | 2 | flaky | 2000-01-10T00:18:40Z | 0:16:40 | 3 fresh 00000000-0000-4000-8000-072000000002 (ok) | claude-sonnet-5-5 | 1 | 3 | 39 | 390 | 0 | 3900 | 117 | 653 | 0.0026 |
| local:demo#2 | 1 | verdict_pending | 2000-01-10T00:50:01Z |  | 1 fresh 00000000-0000-4000-8000-072000000003 (ok) | claude-opus-5-5 | 0 | 2 | 34 | 340 | 0 | 3400 | 102 | 1139 | 0.0046 |

### Retried items

| item | attempts | failed |
| --- | ---: | ---: |
| local:demo#1 | 2 | 1 |

### Flaky verdicts

| item | attempt | failing tests |
| --- | ---: | --- |
| local:demo#1 | 2 | tests::shared_a, tests::shared_b |

### Refused protected-path edits

| item | attempt | at |
| --- | ---: | --- |
| local:demo#2 | 1 | 2000-01-10T00:51:41Z |

### Time parked

| reason | parks | time | still parked |
| --- | ---: | ---: | --- |
| limit | 1 | 0:15:00 | no |

### Commits of the runs

Each commit is linked to its session and item by the session's commit range: exact. Commits of the range made outside the runs: 1.

| commit | repository | at | item | attempt | session | Item: | Run: | Session: |
| --- | --- | --- | --- | ---: | --- | --- | --- | --- |
| 010101010101 | a | 2000-01-10T00:00:50Z | local:demo#1 | 1 | 00000000-0000-4000-8000-072000000001 | yes | yes | yes |
| 020202020202 | a | 2000-01-10T00:17:30Z | local:demo#1 | 1 | 00000000-0000-4000-8000-072000000001 | yes | yes | yes |
| 030303030303 | a | 2000-01-10T00:34:10Z | local:demo#1 | 2 | 00000000-0000-4000-8000-072000000002 | yes | yes | yes |
| 040404040404 | a | 2000-01-10T00:50:50Z | local:demo#2 | 1 | 00000000-0000-4000-8000-072000000003 | yes | no | no |

Trailers missing:

- 040404040404 (a, local:demo#2): Run, Session

---

## Estimates

Every number below is an estimate and is never added to the exact figures above. Each prices tool results (bytes_per_token = 4): tokens = ceil(result bytes / bytes_per_token); a result is carried by the messages after its call up to the first compaction after it; cost = tokens * cache-write rate of the first carrying message (1 h if it wrote more 1 h than 5 min tokens, else 5 min) + sum over the later carrying messages of tokens * their cache-read rate.

Known gaps:

- Tokens are bytes divided by one global ratio, the same for every tool, language and model.
- Images are estimated from their base64 bytes, which says little about their real token count.
- A call's input is part of the calling message's output and is not counted here; only results are.
- A result is carried only within the range: messages of the window before or after it are not seen, nor what a compaction keeps of it in its summary.

### Cost by tool (estimate)

| tool | errors | calls | est. tokens | carry turns | est. eqv | of it carry |
| --- | ---: | ---: | ---: | ---: | ---: | ---: |
| all tools | 0 | 0 | 0 | 0 | 0 | 0 |

### Cost by file (estimate)

No file was touched.

### Repeated reads (estimate)

Within a session: 0 reads, 0 est. eqv. Across sessions: 0 reads, 0 est. eqv.

### Cost of finding (estimate)

Reads and searches before a session's first write: 0 sessions, 0 calls, 0 est. eqv. Sessions that wrote nothing: 0, 0 calls, 0 est. eqv.

### Items: inferred sessions (estimate)

Interactive sessions attributed to an item by inference: an interactive session is inferred for an item when its messages of the range in a repository span the time of a commit there that no run's commit range holds and whose Item: trailer names the item (the time-and-repository link of #3); the whole session counts, main transcript and sub-agents, so the figure is an upper bound. Their tokens are exact; the attribution is not, and these numbers are never added to an item's runs.

| item | repository | session | evidence | messages | eqv | cost USD |
| --- | --- | --- | --- | ---: | ---: | ---: |
| local:demo#1 | a | 00000000-0000-4000-8000-072000000009 | commit 050505050505 names it | 2 | 1273 | 0.0051 |

