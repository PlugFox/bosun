# Leaving an in-repository loop

Before bosun, an unattended run lived inside the repository it worked on: a supervisor script
started agent sessions one after another, a Markdown queue file listed the items, and a Stop
hook refused the end of the agent's turn until the current item was closed. This page is for a
repository that runs a supervisor script and a Stop hook and moves to bosun (stage 1, #7). It
says what to delete and what replaces each part.

The short version: the loop moves out of the repository and out of the agent. bosun claims a
card, starts a fresh headless session, lets it exit normally, runs the card's verdict itself,
and goes on to the next card. Nothing inside the agent enforces the loop, so nothing of the old
loop may stay behind in the repository.

## Before you start

- Let a run in progress finish, or stop it, before you change anything. Do not mix the two
  loops on one checkout: bosun works in the repository's main checkout in stage 1, one item at a
  time.
- Keep the old run logs if you want them; bosun does not read them. It ingests the agent's own
  transcripts (stage 0, #6), including the sessions of past runs that the agent still keeps, and
  bills them correctly per turn.

## What to delete

| Part of the old loop | Delete | Replaced by |
| --- | --- | --- |
| The Stop hook | the `Stop` entry under `"hooks"` in `.claude/settings.json` and `.claude/settings.local.json`, and the script it runs | the daemon's loop: a session that exits is classified, and the daemon runs the verdict (#7) |
| Hook state | whatever the hook kept between turns (the "current item" file, its counters) | the item's state in the identity's database and on the card |
| The supervisor script | the script that starts sessions, retries, and decides when an item is done | `bosund`, installed once per user (`bosun install`), with the repository in its configuration |
| The run lock | the lock file or script that kept two runs apart | the claim marker on the board and one item per repository at a time |
| The queue file | the Markdown list frozen when a run starts | the board: a GitHub Projects v2 project, or the local-file board (one TOML file you edit while the daemon runs) |
| The bill and the run report | the scripts that summed tokens and wrote the run's report | `bosun tokens` and `bosun report`, from the ingested transcripts |
| The freeze list | the list of files the agent must not change, and the check that enforced it | the repository's permission policy (below) |

**The Stop hook must go.** A hook that refuses the end of a turn keeps every headless session
bosun starts in that repository from exiting when its work is done: the hook knows nothing of
bosun's item and sends the agent back to its own, until a budget cap (if one is configured)
kills the session and the item ends in Needs you. `bosun doctor` warns when a configured repository still has one: its
`stop_hook` check reads both settings files of each repository and names every file with a
`hooks.Stop` entry. Other hooks are not touched by this check; review them yourself, since they
run inside every session bosun starts.

## The queue file becomes cards

Each item of the old queue file becomes one card in the Ready column. The prose that told the
agent what to do is the card's body. The command that decided whether the item was done becomes
the card's verdict, in the fenced `bosun` block of the body (the item contract, #5):

````markdown
What to do, for the agent.

```bosun
verdict = "cargo test -p frob"
```
````

The verdict runs in the daemon, through `sh` (the `sh` of the Git installation on Windows),
after the session exits; exit status 0 means done. The agent never decides it. `bosun lint`
checks a card before you move it to Ready.

Only you move cards to Ready. Until then they can be edited, added and reordered freely, and
they stay editable during a run; that is the main difference from a queue file frozen at start.

## The freeze list becomes the policy

The freeze list was the set of files an unattended agent must not change: test harnesses, gate
scripts, lint rules, the loop's own files. In bosun it is the repository's permission policy in
the configuration file (`bosun-config`, #4):

```toml
[repo.app]
path = 'C:\src\app'
identity = 'personal'
build_dirs = ["target"]
queue = { kind = "projects", owner = "me", number = 1 }
landing = { mode = "local" }

[repo.app.policy]
protected = ["tests/harness", "tools/gate.sh", "clippy.toml"]
push = false
```

- `policy.protected`: the freeze list. An entry with no `/` matches that name at any depth; an
  entry with a `/` matches that path and everything under it; there are no wildcards. Paths are
  relative to the repository and `/`-separated. `.gitignore` is always protected. If an item's
  commits touch a protected path, the verdict is not run and the item goes to Needs you with the
  diff stat.
- `allow-protected` in a card's `bosun` block lets that one item change listed protected paths.
  It is part of the card's snapshot at claim, so only you can grant it.
- `build_dirs`: where new ignored files are expected. A new ignored file anywhere else sends the
  item to Needs you, so an agent cannot hide the file that makes the verdict pass behind a new
  `.gitignore` line. Nothing is deleted.
- `policy.push`: `false` by default; the agent's environment has no GitHub write credentials
  (#2).
- `landing.mode`: `local`, the only mode in stage 1. The item's commits stay on the branch of
  the main checkout.

The policy and the repository's configuration are hashed at claim; a change of either during a
session sends the item to Needs you.

## Checklist

1. Finish or stop the old run.
2. Delete the Stop hook entry and its script, the supervisor scripts, the lock and the hook's
   state files. Commit the deletion.
3. Add the repository to the configuration with its queue, `build_dirs` and policy; move the
   freeze list into `policy.protected`.
4. Turn each queue-file item into a card with a `bosun` block, run `bosun lint` on it, and move it
   to Ready.
5. Run `bosun doctor`: `stop_hook` is `ok` for the repository, and no check fails.
