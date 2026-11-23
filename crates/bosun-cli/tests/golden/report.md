# bosun report: personal

Range: 2000-01-03T00:00:00Z to 2000-01-09T00:00:00Z (end excluded). Scope: every repository.

Every number is exact: raw token counts from the transcripts, priced at the rates valid at each message's time. Eqv is input-equivalent tokens of claude-opus-5-5.

## Total

|  | sessions | messages | eqv | cost USD |
| --- | ---: | ---: | ---: | ---: |
| real total | 7 | 26 | 1101089 | 4.4044 |
| old method: the last usage block per session | 7 | 7 | 10924 | 0.0437 |

The real total is 100.8x the old method's number.

## By category

| category | tokens | eqv | share of eqv | cost USD |
| --- | ---: | ---: | ---: | ---: |
| input | 92 | 88 | 0.0% | 0.0004 |
| cache write 5m | 1630 | 1986 | 0.2% | 0.0079 |
| cache write 1h | 6130 | 10985 | 1.0% | 0.0439 |
| cache read | 288100 | 8505 | 0.8% | 0.0340 |
| output | 217080 | 1079525 | 98.0% | 4.3181 |

## By model

| model | messages | input | cache write 5m | cache write 1h | cache read | output | eqv | cost USD |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| &lt;synthetic&gt; | 1 | 0 | 0 | 0 | 0 | 0 | 0 | 0.0000 |
| claude-haiku-5-5 | 1 | 2 | 30 | 500 | 150000 | 600 | 2380 | 0.0095 |
| claude-opus-5-5 | 22 | 85 | 1570 | 5230 | 127100 | 215180 | 1094762 | 4.3791 |
| claude-sonnet-5-5 | 2 | 5 | 30 | 400 | 11000 | 1300 | 3946 | 0.0158 |
| total | 26 | 92 | 1630 | 6130 | 288100 | 217080 | 1101089 | 4.4044 |

## By session kind

| kind | sessions | messages | eqv | share of eqv | cost USD |
| --- | ---: | ---: | ---: | ---: | ---: |
| interactive | 7 | 26 | 1101089 | 100.0% | 4.4044 |
| run | 0 | 0 | 0 | 0.0% | 0.0000 |

## Eqv per commit

Eqv of the messages in a repository over its commits, both in the period; the previous period is 1999-12-28T00:00:00Z to 2000-01-03T00:00:00Z.

| repository | commits | eqv | eqv per commit | previous commits | previous eqv | previous eqv per commit | change |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| a | 3 | 839360 | 279787 | 2 | 513932 | 256966 | +8.9% |
| win-a | 1 | 253020 | 253020 | 0 | 0 |  |  |
| win-b | 0 | 4106 |  | 0 | 0 |  |  |
| (no repository) | 0 | 4603 |  | 0 | 0 |  |  |
| in repositories | 4 | 1096486 | 274121 | 2 | 513932 | 256966 | +6.7% |

Commit reader:

- a: scanned
- win-a: not scanned yet
- win-b: failed: not a git repository

## Context curve

Prompt size (input plus cache reads and writes) per context window; a compaction is a drop of it between consecutive messages. Windows: 8; compactions: 4.

| session | agent | messages | start | peak | compactions |
| --- | --- | ---: | ---: | ---: | ---: |
| 00000000-0000-4000-8000-007100000000 |  | 5 | 321 | 150532 | 2 |
| 00000000-0000-4000-8000-008200000000 |  | 2 | 9087 | 10098 | 0 |
| 00000000-0000-4000-8000-003100000000 |  | 2 | 4321 | 9876 | 0 |
| 00000000-0000-4000-8000-004100000000 |  | 5 | 4321 | 9876 | 1 |
| 00000000-0000-4000-8000-005100000000 |  | 2 | 4321 | 9876 | 0 |
| 00000000-0000-4000-8000-006100000000 |  | 2 | 4321 | 9876 | 0 |
| 00000000-0000-4000-8000-008100000000 |  | 6 | 4321 | 8065 | 1 |
| 00000000-0000-4000-8000-003100000000 | a0000000003100001 | 2 | 5032 | 6403 | 0 |

## Ingest health

Over the whole store, whatever the range.

| parser version | files | files with a line count | lines | gaps | gaps per line |
| --- | ---: | ---: | ---: | ---: | ---: |
| 3 | 17 | 17 | 131 | 3 | 2.3% |
| total | 17 | 17 | 131 | 3 | 2.3% |

### Agreement with the provider's own totals

Sessions with the provider's totals (the last `cost-state` record of the main transcript) and a message in the range, main transcript plus sub-agents, over the messages inside the provider's window (from its `startTime` to the last record before the line): 1; within 5% on the total: 1; transcripts larger by more than 5%: 0. Not compared, with no message inside the window: 0.

| all sessions | input | cache write | cache read | output | total |
| --- | ---: | ---: | ---: | ---: | ---: |
| transcripts | 12 | 1620 | 24000 | 51310 | 76942 |
| provider | 12 | 1620 | 24000 | 51310 | 76942 |
| difference | +0.0% | +0.0% | +0.0% | +0.0% | +0.0% |

Signed difference, transcripts less provider: +0 tokens. The provider's excess is the known undercount of the transcript method, calls its transcripts never hold: 0 on models no transcript holds, +0 on the other models.

| session | windows | messages | transcripts | provider | difference | relative | absent models | other models | within |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | --- |
| 00000000-0000-4000-8000-003100000000 | 2 | 4 | 76942 | 76942 | +0 | +0.0% | 0 | +0 | yes |

## Items

No item has a transition in the range.

### Retried items

None.

### Flaky verdicts

None.

### Refused protected-path edits

None.

### Time parked

None.

### Commits of the runs

Each commit is linked to its session and item by the session's commit range: exact. Commits of the range made outside the runs: 4.

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
| Read | 0 | 3 | 16 | 3 | 21 | 1 |
| Agent | 0 | 1 | 6 | 0 | 12 | 0 |
| Bash | 1 | 1 | 9 | 0 | 11 | 0 |
| Grep | 0 | 1 | 3 | 3 | 4 | 0 |
| Edit | 0 | 1 | 7 | 0 | 0 | 0 |
| Write | 0 | 1 | 4 | 0 | 0 | 0 |
| all tools | 1 | 8 | 45 | 6 | 48 | 1 |

### Cost by file (estimate)

| repository | path | reads | searches | writes | execs | unknown | sessions | calls | est. tokens | carry turns | est. eqv | of it carry |
| --- | --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| a | src/lib.rs | 3 | 0 | 1 | 0 | 0 | 2 | 4 | 23 | 3 | 21 | 1 |
| a | src | 0 | 1 | 0 | 0 | 0 | 1 | 1 | 3 | 3 | 4 | 0 |
| a | src/new.rs | 0 | 0 | 1 | 0 | 0 | 1 | 1 | 4 | 0 | 0 | 0 |

### Repeated reads (estimate)

Within a session: 1 reads, 6 est. eqv. Across sessions: 1 reads, 8 est. eqv.

| repository | path | reads | sessions | repeats within | est. eqv within | repeats across | est. eqv across |
| --- | --- | ---: | ---: | ---: | ---: | ---: | ---: |
| a | src/lib.rs | 3 | 2 | 1 | 6 | 1 | 8 |

### Cost of finding (estimate)

Reads and searches before a session's first write: 2 sessions, 4 calls, 25 est. eqv. Sessions that wrote nothing: 0, 0 calls, 0 est. eqv.

| session | first write | calls | est. tokens | carry turns | est. eqv | of it carry |
| --- | --- | ---: | ---: | ---: | ---: | ---: |
| 00000000-0000-4000-8000-008100000000 | 2000-01-08T00:00:12Z | 3 | 13 | 6 | 17 | 1 |
| 00000000-0000-4000-8000-008200000000 | 2000-01-08T01:00:04Z | 1 | 6 | 0 | 8 | 0 |

### Items: inferred sessions (estimate)

Interactive sessions attributed to an item by inference: an interactive session is inferred for an item when one of its windows contains a commit that no run's commit range holds and whose Item: trailer names the item, by the time-and-repository link of #3 (the window's span, from its first to its last message, holds the commit's time, and its messages are in the commit's repository); the whole session counts, main transcript and sub-agents, so the figure is an upper bound. Their tokens are exact; the attribution is not, and these numbers are never added to an item's runs.

None.

