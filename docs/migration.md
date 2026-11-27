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
  loops on one checkout: for a repository without `worktree` (stage 1), bosun works in the
  repository's main checkout, one item at a time. With `worktree` it works in a clone of its own
  ([Worktree mode](#worktree-mode)).
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
model = "opus"                # required with a queue: the sessions' default model
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
- `model`: the model a session asks for when its card names none (a card's `model` overrides
  it). It is required with a `queue`: unattended sessions do not read your own Claude Code
  settings, so without it a session would run on whatever the CLI defaults to that day.
- `build_dirs`: where new ignored files are expected. A new ignored file anywhere else sends the
  item to Needs you, so an agent cannot hide the file that makes the verdict pass behind a new
  `.gitignore` line. Nothing is deleted.
- `policy.push`: `false` by default; the agent's environment has no GitHub write credentials
  (#2).
- `landing.mode`: `local`, the only mode for a repository without `worktree` (stage 1). The
  item's commits stay on the branch of the main checkout. `merge` and `pr` need `worktree`
  ([Worktree mode](#worktree-mode)).

The policy and the repository's configuration are hashed at claim; a change of either during a
session sends the item to Needs you.

## Worktree mode

Once a repository runs under bosun as above, it can move on from the main checkout: with
`repo.<name>.worktree` (#8) the items no longer work in your checkout but in worktrees of a
clone the daemon owns, so you can keep working in the checkout while an item runs. Without the
key nothing below applies and the repository behaves as in stage 1.

```toml
[repo.app]
path = 'C:\src\app'
identity = 'personal'
build_dirs = ["target"]
queue = { kind = "projects", owner = "me", number = 1 }
model = "opus"
landing = { mode = "local" }  # or "merge", or "pr"
worktree = { root = 'C:\wt\app', setup = "cargo fetch", disk_floor = "20GB", shared_build = { dir = 'C:\wt\app-target', resource = "target" } }

[repo.app.policy]
protected = ["tests/harness", "tools/gate.sh"]

[resource.target]
kind = "build-dir"
capacity = 1                  # exclusive
scope = "machine"             # or "identity", "repository"
```

### The clone and the worktrees

- `worktree.root` is where the item worktrees go. It is required and lies outside the
  repository's `path` (neither the path nor under it).
- The daemon makes a bare clone at `<root>/.clone` whose origin is your checkout
  (`C:\wt\app\.clone`, cloned from `C:\src\app`). Worktrees share refs and git configuration
  with their repository, so they hang off this clone, never off your checkout. The clone owns
  its objects (never `--shared`, never `--reference`); anything else found at its path is an
  error and is left as it is.
- A bare clone copies none of your checkout's local git configuration: not its filters, hooks,
  `core.autocrlf`, LFS or submodule settings. The daemon writes only fixed values into the
  clone's own configuration: the line-ending policy is pinned to git's defaults
  (`core.autocrlf=false`, `core.eol=native`; the repository's `.gitattributes` still decides),
  `core.longpaths=true` on Windows, and the Git LFS filter when `git lfs` is installed. What
  you set globally or for the system applies to the clone as to any repository, except the
  line endings. A repository that needs more prepares its worktrees with `setup`.
- At claim the target branch is fetched from your checkout into the clone, fast-forward only: a
  target you rewrote is an error, not an overwrite.
- `setup` is a shell command run in each new worktree, the way a verdict runs; if it fails,
  the item goes to Needs you with no session started and no worktree left behind. `teardown`
  runs in a worktree before it is removed. `disk_floor` is the free space below which no new
  worktree is made (sizes in `B`, `KB`, `MB`, `GB`, `TB` or `KiB`, `MiB`, `GiB`, `TiB`).

### `shared_build` and resources

- `worktree.shared_build` is the one build directory the repository's worktrees share. Its
  `dir` is absolute and neither holds nor lies under the repository's `path` or `worktree.root`
  (`cargo clean` deletes it whole); its `resource` names a `[resource.<name>]` with
  `capacity = 1`. Every process the daemon starts in a worktree (setup, teardown, sessions,
  verdicts) gets the directory as `env` (default `CARGO_TARGET_DIR`), under an exclusive lease
  of the resource held from the session through the landing's re-verdict.
- It is the default for Rust: per-worktree targets cost disk (12-15 GB each). Leaving out
  `shared_build` gives each worktree its own build directory, the opt-in of a repository that
  raises concurrency.
- `[resource.<name>]` declares something items lease: `kind` (required, free text such as
  `build-dir`, `gpu`, `device`), `capacity` (holders at once, default 1, exclusive) and
  `scope`, what it is shared within: `machine` (the default), `identity` or `repository`.
  Resources are read at daemon start; an edit takes effect at the next start.
- A card names the resources it needs in its `bosun` block (`needs = ["device"]`) and holds
  them from before its first session until it leaves the loop. Only a card of a repository
  with `worktree` may need one; without it `bosun lint` refuses the card, as in stage 1.

### `bosun with`

Interactive work that needs the same resource takes the daemon's lease instead of a lock file
of its own:

```sh
bosun with target -- cargo build
bosun with --repo app device -- ./flash.sh
```

`bosun with [--repo <name>] <resource>... -- <command> [args...]` waits until the whole set is
granted (saying so on stderr), runs the command with your terminal, releases the lease and
exits with the command's exit code. A daemon that cannot be reached or refuses is an error and
the command is not run; Ctrl-C while waiting withdraws the request. A daemon restart is
survived. With several identities configured, `--identity` (or `--repo`) says whose lease it
is.

### Landing

`landing.mode` says how a verified item's commits reach the target (`landing.target`, default
the branch the item was claimed from):

- `local` (the default): `merge` with the method `ff` and no push.
- `merge`: `merge.method` is `ff` (default), `squash` or `no-ff`; `merge.push` stays `false`
  (`true` is not available yet).
- `pr`: the verified head is pushed to the branch `bosun/<slug>` of `origin`, under the
  identity's credentials, and a pull request into the target is opened (`pr.draft`, default
  true; `pr.reviewers`, `pr.labels`). The item is based on and rebased onto the target as it is
  on the forge, never your local branch. Nothing is merged: your target and your checkout never
  move.

In `local` and `merge` the daemon's clone offers the verified commit, your repository fetches
it (objects only), and the target is fast-forwarded there: by ref when no checkout has it
checked out, by `merge --ff-only` in a clean checkout. A dirty or busy checkout of the target
defers the landing: it is reported once and retried while the item stays in Landing, and the
repository claims nothing meanwhile. A target you committed on in the meantime is rebased onto
and verified again before it lands.

### The protected list

`policy.protected`, `.gitignore` (always protected) and a card's `allow-protected` apply in
worktree mode as before: an item whose commits touch a protected path goes to Needs you
without a verdict.

### What your checkout sees

- No ref of the daemon's: the item refs under `refs/bosun/` exist only in the clone.
- At landing, the objects of the landed commit are fetched into your repository (and the Git
  LFS objects it points to); no `FETCH_HEAD` is written, and no gc or maintenance is started
  there.
- The target moves only by a clean fast-forward, and in `pr` mode not at all.
- Your own commits and edits in the checkout do not send an item to Needs you (edits are
  reported); switching the checkout's branch while an item runs does, before its verdict.

### The automatic pause

For a repository without `worktree`, an interactive session in its checkout pauses it
(`daemon.auto_pause`, default on) until the session has been idle for `daemon.auto_pause_idle`.
In worktree mode the items and your interactive work no longer share a tree, so the automatic
pause is off by default. Set `repo.<name>.auto_pause = true` to keep it (or `false` to turn it
off for one repository without `worktree`). The key is not part of the repository's
configuration hash: changing it does not disturb an item in flight.

## Checklist

1. Finish or stop the old run.
2. Delete the Stop hook entry and its script, the supervisor scripts, the lock and the hook's
   state files. Commit the deletion.
3. Add the repository to the configuration with its queue, `model`, `build_dirs` and policy;
   move the freeze list into `policy.protected`.
4. Turn each queue-file item into a card with a `bosun` block, run `bosun lint` on it, and move it
   to Ready.
5. Run `bosun doctor`: `stop_hook` is `ok` for the repository, and no check fails.
6. Optionally, once items land as you expect, move the repository to worktree mode: add
   `worktree` (a `root` outside the repository, `setup`, a `shared_build` with its
   `[resource.<name>]`), choose `landing.mode`, and set `repo.<name>.auto_pause = true` if you
   still want your interactive sessions to pause it ([Worktree mode](#worktree-mode)).
