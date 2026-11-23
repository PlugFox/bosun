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

## Ingest health

Over the whole store, whatever the range.

| parser version | files | files with a line count | lines | gaps | gaps per line |
| --- | ---: | ---: | ---: | ---: | ---: |
| 1 | 12 | 12 | 79 | 3 | 3.8% |
| total | 12 | 12 | 79 | 3 | 3.8% |

### Agreement with the provider's own totals

Sessions with the provider's totals (the last `cost-state` record) and a message in the range, main transcript plus sub-agents, over all time: 1; within 5% on the total: 1.

| all sessions | input | cache write | cache read | output | total |
| --- | ---: | ---: | ---: | ---: | ---: |
| transcripts | 12 | 1620 | 24000 | 51310 | 76942 |
| provider | 12 | 1620 | 24000 | 51310 | 76942 |
| difference | +0.0% | +0.0% | +0.0% | +0.0% | +0.0% |

| session | windows | messages | transcripts | provider | difference | within |
| --- | ---: | ---: | ---: | ---: | ---: | --- |
| 00000000-0000-4000-8000-003100000000 | 2 | 4 | 76942 | 76942 | +0.0% | yes |

