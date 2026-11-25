//! Landing in worktree mode (#8 "Landing"; E2, E6, E14, E19, E20): the one keyed
//! [`EffectKind::Land`](bosun_core::EffectKind::Land) of an item whose repository has
//! `repo.<name>.worktree`, in the modes `local` (`merge` with `ff`, no push, onto the branch the
//! item was claimed from) and `merge`. Without `repo.<name>.worktree` landing is the loop's own
//! (nothing to do) and nothing here runs.
//!
//! The task ([`land`]) reports one event, and every step is redone from the `landing` row and
//! from git itself after a crash:
//!
//! 1. A passing verdict run in worktree mode recorded the commit it ran on and its tree
//!    (`Write::Landing`, [`super::verdict`]): the only evidence that a commit was verified.
//! 2. The target is fetched from the user's checkout into the daemon's clone, fast-forward only
//!    ([`super::workspace::Workspace::sync_target`]); a refused fetch (the user rewrote the
//!    target) is [`Event::LandingRefused`]. The base the item's preconditions compare refs with,
//!    and the capture of the user's checkout, follow the target ([`Done::Followed`]). A target
//!    that holds the commit the attempt's row says it landed, at it or past it (the user
//!    committed on it after a fast-forward the daemon crashed behind), is already landed
//!    ([`already_landed`]): it is not rebased onto again. The row is the attempt's own and its
//!    head's (a new attempt or a new verified head clears what landed, `bosun-store`), so an
//!    old landed commit never matches.
//! 3. A rebase left stopped in the item's worktree (git finishing or stopping after the daemon
//!    died during step 3, or a session) is aborted first and reported ([`abort_stale_rebase`];
//!    the preconditions do the same before the verdict): HEAD would be a partial pick of the
//!    item's commits. An abort that fails is [`Event::LandingRefused`]. Then a target that is
//!    not an ancestor of the worktree's head is rebased onto; a conflict is stored as the
//!    item's `conflict` note and reported as [`Event::Conflict`].
//! 4. A head the row does not name as verified, rebased now or before a crash, is
//!    [`Event::Rebased`]: the machine runs the verdict on it, and a pass emits `Land` again. A
//!    verdict that passed before the rebase is never a verdict on the target.
//! 5. The commit to land is [`bosun_git::merge::merge_commit`] of the verified tree; for
//!    `squash` and `no-ff` its message is the card's title and the attempt's summary, with the
//!    `Item:`, `Run:` and `Session:` trailers of the attempt's last session.
//!
//! The rebase of step 3 and the commit of step 5 carry the repository identity's author (E26,
//! #2), read from its git configuration file, never the daemon process's global git identity;
//! an identity whose author cannot be read refuses the landing.
//! 6. The clone offers it under [`git::landing_ref`]; the user's repository fetches it (objects
//!    only, E14) and [`bosun_git::land::fast_forward`] moves the target there (E2): by ref when
//!    no checkout has it, by `merge --ff-only` in a clean checkout. A dirty or busy checkout
//!    defers it: reported once, retried every [`super::Options::poll`] while the item stays in
//!    `Landing` and the repository claims nothing. A target the user committed on meanwhile
//!    starts again from step 2, at most [`RESTARTS`] times.
//!
//! [`Done::Followed`]: super::Done::Followed

use std::sync::Arc;

use bosun_config::{Config, LandingMode, MergeMethod, Repo};
use bosun_core::{Event, ItemId};
use bosun_git::land::{self, FastForward, Rebase};
use bosun_git::merge::{self, Method};
use bosun_git::{Git, Ident};
use bosun_store::{LandingRow, Write};
use tokio::sync::watch;

use super::workspace::Workspace;
use super::{Claimed, ItemWorktree, Shared, blocking, git, identity, lookup, now_ms, verdict};

/// The kind of the item note a conflicting rebase leaves (`Write::ItemNote`).
pub(crate) const NOTE_CONFLICT: &str = "conflict";

/// How many times a target the user committed on during the fast-forward starts the landing
/// again before it is refused.
pub(crate) const RESTARTS: u32 = 3;

/// How many times in a row one attempt may report the same head [`Event::Rebased`] before the
/// landing is refused: a passing verdict that cannot record the head it verified would
/// otherwise run the verdict again without end.
pub(crate) const REBASED_LIMIT: u32 = 3;

/// The run of [`Event::Rebased`] reports of one head in one attempt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Streak {
    pub(crate) attempt: u32,
    pub(crate) head: String,
    pub(crate) count: u32,
}

impl Streak {
    /// Whether the streak has reached [`REBASED_LIMIT`]: the landing is refused.
    pub(crate) fn refused(&self) -> bool {
        // Mutant (`mutant_loop_rebased_unbounded`): the same head is rebased and verified
        // again without end.
        !cfg!(mutant_loop_rebased_unbounded) && self.count >= REBASED_LIMIT
    }
}

/// The streak after the landing of `attempt` reported `head` [`Event::Rebased`] again,
/// following `prev`: one more for the same head in the same attempt, a new one otherwise.
pub(crate) fn rebased_again(prev: Option<&Streak>, attempt: u32, head: &str) -> Streak {
    match prev {
        Some(p) if p.attempt == attempt && p.head == head => Streak {
            count: p.count + 1,
            ..p.clone()
        },
        _ => Streak {
            attempt,
            head: head.to_owned(),
            count: 1,
        },
    }
}

/// Which landing this is.
#[derive(Debug)]
pub(crate) struct Spec {
    pub(crate) item: ItemId,
    pub(crate) attempt: u32,
    /// The card's title: the subject of a squash or merge commit.
    pub(crate) title: String,
}

/// What the task reports.
#[derive(Debug, Default)]
pub(crate) struct Landed {
    /// The machine's event; `None` when the task was killed.
    pub(crate) event: Option<Event>,
    /// The claim with its base and its capture of the user's checkout moved along with the
    /// target, when landing moved it.
    pub(crate) followed: Option<Claimed>,
    /// The head reported [`Event::Rebased`], when that is the event.
    pub(crate) rebased: Option<String>,
}

/// Runs the landing of `spec` (see the module documentation).
pub(crate) async fn land(
    shared: &Arc<Shared>,
    config: &Config,
    claimed: &Claimed,
    spec: &Spec,
    kill: watch::Receiver<bool>,
) -> Landed {
    let mut task = Task {
        shared,
        spec,
        claimed: claimed.clone(),
        followed: false,
        rebased: None,
        kill,
    };
    let event = task.run(config).await;
    Landed {
        event,
        followed: task.followed.then_some(task.claimed),
        rebased: task.rebased,
    }
}

/// One landing in progress.
struct Task<'a> {
    shared: &'a Arc<Shared>,
    spec: &'a Spec,
    /// The claim, its base and user capture updated as the target moves.
    claimed: Claimed,
    /// [`Task::claimed`] differs from the claim the task was given.
    followed: bool,
    /// The head of an [`Event::Rebased`] report.
    rebased: Option<String>,
    kill: watch::Receiver<bool>,
}

/// How one pass from step 2 ended.
enum Pass {
    /// The task reports this (or nothing, killed).
    Done(Option<Event>),
    /// The user committed on the target meanwhile: from step 2 again.
    Again,
}

impl Task<'_> {
    async fn run(&mut self, config: &Config) -> Option<Event> {
        let Some((repo, identity)) = lookup(config, &self.shared.options.repo) else {
            return Some(self.refused("the repository is no longer configured".to_owned()));
        };
        // The author and committer of the commits the landing makes (E26).
        let ident = match identity::bundle(&repo.identity, identity, &repo.path) {
            Ok(bundle) => git::ident(&bundle.author),
            Err(e) => {
                return Some(self.refused(format!("the repository identity's author: {e}")));
            }
        };
        let Some(wt) = self.claimed.worktree.clone() else {
            return Some(self.refused("the item has no worktree".to_owned()));
        };
        let method = match method(repo) {
            Ok(method) => method,
            Err(e) => return Some(self.refused(e)),
        };
        let mut restarts = 0;
        loop {
            match self.pass(repo, &wt, method, &ident).await {
                Pass::Done(event) => return event,
                Pass::Again if restarts < RESTARTS => restarts += 1,
                Pass::Again => {
                    return Some(self.refused(format!(
                        "the target {} kept moving while it was fast-forwarded ({} times)",
                        wt.target,
                        RESTARTS + 1
                    )));
                }
            }
        }
    }

    /// Steps 2 to 6, once.
    #[allow(clippy::too_many_lines)]
    async fn pass(
        &mut self,
        repo: &Repo,
        wt: &ItemWorktree,
        method: Method,
        ident: &Ident,
    ) -> Pass {
        let item = &self.spec.item;
        let attempt = self.spec.attempt;
        let target = wt.target.clone();

        // 2. The target's head now.
        let t = match self.sync(&wt.workspace, &target).await {
            Ok(t) => t,
            Err(e) => return self.refuse(e),
        };
        let row = match self.shared.deps.reader.landing(item).await {
            Ok(row) => row.filter(|r| r.attempt == attempt),
            Err(e) => return self.refuse(format!("its landing row: {e}")),
        };
        // A crash after the fast-forward: the target holds what this attempt landed (the user
        // may have committed on it since).
        if let Some(row) = &row {
            let (clone, landed, at) = (
                Git::new(wt.workspace.clone_path()),
                row.landed_sha.clone(),
                t.clone(),
            );
            match blocking(move || already_landed(&clone, landed.as_deref(), &at)).await {
                Ok(true) => {
                    return Pass::Done(
                        self.delivered(wt, &target, row.delivered_ms.is_none())
                            .await,
                    );
                }
                Ok(false) => {}
                Err(e) => return self.refuse(format!("whether it already landed: {e}")),
            }
        }

        // 3. The head of the item's worktree, rebased onto the target unless it descends. A
        // rebase left stopped there (a daemon that died during it, a session) holds a partial
        // pick: it is aborted first, so the head is the item's branch again.
        if let Err(e) = abort_stale_rebase(self.shared, item, self.claimed.workdir.clone()).await {
            return self.refuse(format!("the rebase left in the item's worktree: {e}"));
        }
        let dir = self.claimed.workdir.clone();
        let (at, committer) = (t.clone(), ident.clone());
        let rebased = blocking(move || -> Result<Result<String, Vec<String>>, String> {
            let git = Git::new(&dir);
            let head = git::head(&git).map_err(|e| e.to_string())?;
            let descends = git
                .test(&["merge-base", "--is-ancestor", &at, &head])
                .map_err(|e| e.to_string())?;
            if descends {
                return Ok(Ok(head));
            }
            match land::rebase_onto(&git, &at, &committer).map_err(|e| e.to_string())? {
                Rebase::Rebased { head } => Ok(Ok(head)),
                Rebase::Conflict { files } => Ok(Err(files)),
            }
        })
        .await;
        let h = match rebased {
            Ok(Ok(h)) => h,
            Ok(Err(files)) => return Pass::Done(Some(self.conflict(&target, &t, files).await)),
            Err(e) => {
                return self.refuse(format!("the rebase onto {target} at {t}: {e}"));
            }
        };

        // 4. Only a head a passing verdict run recorded lands.
        let verified = row
            .as_ref()
            .filter(|r| r.head_sha.as_deref() == Some(h.as_str()))
            .and_then(|r| r.verified_tree.clone());
        let verified_tree = match verified {
            Some(tree) => tree,
            // Mutant (`mutant_loop_accept_pre_rebase_verdict`): the rebased head lands on the
            // strength of the verdict that passed before the rebase.
            None if cfg!(mutant_loop_accept_pre_rebase_verdict) => match self.tree(&h).await {
                Ok(tree) => tree,
                Err(e) => return self.refuse(e),
            },
            None => {
                self.rebased = Some(h);
                return Pass::Done(Some(Event::Rebased { attempt }));
            }
        };

        // 5. The commit to land.
        let (message, trailers) = match self.message(method).await {
            Ok(made) => made,
            Err(e) => return self.refuse(e),
        };
        let clone = wt.workspace.clone_path();
        let (at, head, ident) = (t.clone(), h.clone(), ident.clone());
        let made = blocking(move || {
            merge::merge_commit(
                &Git::new(clone),
                method,
                &at,
                &head,
                &verified_tree,
                &message,
                &trailers,
                &ident,
            )
        })
        .await;
        let landed = match made {
            Ok(landed) => landed,
            Err(e) => return self.refuse(format!("the commit to land: {e}")),
        };
        if let Err(e) = self.row(&target, Some(landed.clone()), None).await {
            return self.refuse(e);
        }

        // 6. Delivered to the user's repository (E14), moved by E2.
        let (clone, user, offered, commit) = (
            wt.workspace.clone_path(),
            repo.path.clone(),
            git::landing_ref(item),
            landed.clone(),
        );
        let fetched = blocking(move || -> Result<(), bosun_git::Error> {
            Git::new(&clone).run(&["update-ref", &offered, &commit], None)?;
            let from = clone.to_string_lossy();
            // Objects only (E14): no `FETCH_HEAD`, no maintenance or gc in the user's `.git`;
            // the configuration as well as the flags, for a git that ignores the flag.
            let mut args = vec!["-c", "gc.auto=0", "-c", "maintenance.auto=false", "fetch"];
            args.extend(["--quiet", "--no-tags"]);
            args.extend(["--no-write-fetch-head", "--no-auto-maintenance"]);
            if cfg!(mutant_loop_landing_fetch_writes_fetch_head) {
                // Mutant (`mutant_loop_landing_fetch_writes_fetch_head`): the fetch writes the
                // user's `FETCH_HEAD` and may start maintenance there.
                args = vec!["fetch", "--quiet", "--no-tags"];
            }
            args.extend(["--", &from, &offered]);
            Git::new(&user).run(&args, None)?;
            Ok(())
        })
        .await;
        if let Err(e) = fetched {
            self.drop_offer(wt).await;
            return self.refuse(format!(
                "the landed commit cannot be fetched into the user's repository: {e}"
            ));
        }
        let mut reported = false;
        loop {
            let (user, branch, to, from) =
                (repo.path.clone(), target.clone(), landed.clone(), t.clone());
            let moved = blocking(move || fast_forward(&user, &branch, &to, &from)).await;
            match moved {
                Ok(FastForward::Moved { .. }) => {
                    return Pass::Done(self.delivered(wt, &target, true).await);
                }
                // Mutant (`mutant_loop_land_while_deferred`): a deferred fast-forward is taken
                // for a landing.
                Ok(FastForward::Deferred { .. }) if cfg!(mutant_loop_land_while_deferred) => {
                    self.drop_offer(wt).await;
                    return Pass::Done(Some(Event::Landed));
                }
                Ok(FastForward::Deferred { reason }) => {
                    if !reported {
                        reported = true;
                        self.shared.trouble(format!(
                            "{item}: the fast-forward of `{target}` is deferred: {reason}; the \
                             repository claims nothing until it moves"
                        ));
                    }
                    let poll = self.shared.options.poll;
                    tokio::select! {
                        () = tokio::time::sleep(poll) => {}
                        () = verdict::killed(&mut self.kill) => {
                            // Mutant (`mutant_loop_keep_landing_ref`): the offer outlives a
                            // landing killed while deferred.
                            if !cfg!(mutant_loop_keep_landing_ref) {
                                self.drop_offer(wt).await;
                            }
                            return Pass::Done(None);
                        }
                    }
                }
                Err(land::Error::NotDescendant { head, .. } | land::Error::Moved { head, .. }) => {
                    crate::log::log!(
                        "bosund: repository {}: {item}: {target} moved to {head} during the \
                         landing; it starts again",
                        self.shared.options.repo
                    );
                    self.drop_offer(wt).await;
                    return Pass::Again;
                }
                Err(e) => {
                    self.drop_offer(wt).await;
                    return self.refuse(format!("the fast-forward of `{target}` failed: {e}"));
                }
            }
        }
    }

    /// Step 2: the target fetched into the clone, and the claim following it.
    async fn sync(&mut self, workspace: &Workspace, target: &str) -> Result<String, String> {
        let (ws, branch) = (workspace.clone(), target.to_owned());
        let t = blocking(move || ws.sync_target(&branch))
            .await
            .map_err(|e| {
                format!(
                    "the target {target} cannot be fetched into the daemon's clone (was it \
                     rewritten?): {e}"
                )
            })?;
        self.follow(target, &t).await;
        Ok(t)
    }

    /// The daemon moved `refs/heads/<target>` in its clone to `t`, and the user's repository
    /// has it there: the base the preconditions compare refs with and the capture of the
    /// user's checkout take it in, in memory and in the store (the snapshot's base refs, the
    /// worktree's capture), so neither the next attempt nor a restarted daemon takes the move
    /// for tampering.
    async fn follow(&mut self, target: &str, t: &str) {
        let name = format!("refs/heads/{target}");
        let item = self.spec.item.clone();
        let mut writes = Vec::new();
        if self.claimed.base.refs.get(&name).map(String::as_str) != Some(t) {
            self.claimed.base.refs.insert(name.clone(), t.to_owned());
            writes.push(Write::SnapshotRefs {
                item: item.clone(),
                refs_json: serde_json::json!(self.claimed.base.refs).to_string(),
            });
        }
        if let Some(wt) = self.claimed.worktree.as_mut()
            && wt.user.base.refs.get(&name).map(String::as_str) != Some(t)
        {
            wt.user.base.refs.insert(name.clone(), t.to_owned());
            if wt.user.base.branch.as_deref() == Some(name.as_str()) {
                t.clone_into(&mut wt.user.base.head);
            }
            writes.push(Write::WorktreeUserBase {
                path: wt.worktree.path.to_string_lossy().into_owned(),
                user_base_json: wt.user.to_json(),
            });
        }
        if writes.is_empty() {
            return;
        }
        self.followed = true;
        if let Err(e) = self.shared.deps.writer.write(writes).await {
            self.shared
                .trouble(format!("{item}: the moved target cannot be recorded: {e}"));
        }
    }

    /// The fast-forward moved (now, or before a crash): the row says when, the clone's target
    /// follows, the offer goes.
    async fn delivered(&mut self, wt: &ItemWorktree, target: &str, stamp: bool) -> Option<Event> {
        if stamp && let Err(e) = self.row(target, None, Some(now_ms())).await {
            self.shared.trouble(format!("{}: {e}", self.spec.item));
        }
        if let Err(e) = self.sync(&wt.workspace, target).await {
            // Landed all the same: the next claim fetches the target again.
            self.shared.trouble(format!("{}: {e}", self.spec.item));
        }
        self.drop_offer(wt).await;
        Some(Event::Landed)
    }

    /// Step 3's conflict: stored as the item's note and reported.
    async fn conflict(&self, target: &str, t: &str, files: Vec<String>) -> Event {
        let item = &self.spec.item;
        self.shared.trouble(format!(
            "{item}: the rebase onto {target} at {t} conflicts in {}",
            listed(&files)
        ));
        // `at_ms` in the body too: the session started in answer to it reads it (`prompt_of`).
        let at_ms = now_ms();
        let note = Write::ItemNote {
            item: item.clone(),
            kind: NOTE_CONFLICT.to_owned(),
            body: serde_json::json!({
                "target": target, "target_head": t, "files": files, "at_ms": at_ms,
            })
            .to_string(),
            at_ms,
        };
        if let Err(e) = self.shared.deps.writer.write(vec![note]).await {
            self.shared
                .trouble(format!("the conflict of {item} cannot be stored: {e}"));
        }
        Event::Conflict {
            attempt: self.spec.attempt,
        }
    }

    /// The tree of commit `h`.
    async fn tree(&self, h: &str) -> Result<String, String> {
        let (dir, at) = (self.claimed.workdir.clone(), format!("{h}^{{tree}}"));
        blocking(move || {
            Git::new(dir)
                .run(&["rev-parse", "--verify", "--end-of-options", &at], None)
                .map(|t| t.trim().to_owned())
        })
        .await
        .map_err(|e| format!("the tree of {h}: {e}"))
    }

    /// The message of a squash or merge commit and the trailers: the card's title and the
    /// attempt's summary (empty when the agent gave none), the item and its attempt's last
    /// session. A fast-forward uses neither.
    async fn message(&self, method: Method) -> Result<(String, bosun_git::Trailers), String> {
        let item = &self.spec.item;
        let attempt = self.spec.attempt;
        let runs = self
            .shared
            .deps
            .reader
            .runs(item)
            .await
            .map_err(|e| format!("the runs of {item}: {e}"))?;
        let last = runs
            .iter()
            .filter(|r| r.attempt == attempt)
            .max_by_key(|r| r.spawn);
        let trailers = bosun_git::Trailers {
            item: item.to_string(),
            run: last.map_or_else(String::new, |r| git::run_label(r.attempt, r.spawn)),
            session: last.map_or_else(String::new, |r| r.session_key.clone()),
        };
        if method == Method::Ff {
            return Ok((String::new(), trailers));
        }
        if last.is_none() {
            return Err(format!(
                "attempt {attempt} of {item} has no session for the trailers"
            ));
        }
        let summary = self
            .shared
            .deps
            .reader
            .summary(item, attempt)
            .await
            .map_err(|e| format!("the summary of {item}: {e}"))?
            .map_or_else(String::new, |call| call.body);
        let message = bosun_core::publish::pr::squash_message(
            &self.spec.title,
            &summary,
            &bosun_core::publish::pr::Trailers {
                item: trailers.item.clone(),
                run: trailers.run.clone(),
                session: trailers.session.clone(),
            },
        );
        Ok((message, trailers))
    }

    /// Updates the landing row of the attempt.
    async fn row(
        &self,
        target: &str,
        landed: Option<String>,
        delivered: Option<i64>,
    ) -> Result<(), String> {
        let row = LandingRow {
            item: self.spec.item.clone(),
            attempt: self.spec.attempt,
            target: target.to_owned(),
            head_sha: None,
            verified_tree: None,
            landed_sha: landed,
            delivered_ms: delivered,
            pr_number: None,
            pr_url: None,
            pr_state: None,
            updated_ms: now_ms(),
        };
        self.shared
            .deps
            .writer
            .write(vec![Write::Landing(row)])
            .await
            .map_err(|e| format!("the landing row: {e}"))
    }

    /// Removes the clone's offer of the landed commit, if any.
    async fn drop_offer(&self, wt: &ItemWorktree) {
        let (clone, offered) = (wt.workspace.clone_path(), git::landing_ref(&self.spec.item));
        let dropped =
            blocking(move || Git::new(clone).run(&["update-ref", "-d", &offered], None)).await;
        if let Err(e) = dropped {
            self.shared.trouble(format!(
                "{}: the landing ref cannot be removed: {e}",
                self.spec.item
            ));
        }
    }

    /// Reports why the landing is refused: [`Event::LandingRefused`].
    fn refused(&self, why: impl Into<String>) -> Event {
        let why: String = why.into();
        self.shared
            .trouble(format!("{}: the landing is refused: {why}", self.spec.item));
        Event::LandingRefused
    }

    /// [`Task::refused`], as the end of a pass.
    fn refuse(&self, why: impl Into<String>) -> Pass {
        Pass::Done(Some(self.refused(why)))
    }
}

/// How many conflicting file names a trouble message lists.
const LISTED: usize = 20;

/// The first [`LISTED`] of `files`, and how many more there are.
fn listed(files: &[String]) -> String {
    let text = files
        .iter()
        .take(LISTED)
        .map(String::as_str)
        .collect::<Vec<_>>()
        .join(", ");
    match files.len().checked_sub(LISTED) {
        Some(more) if more > 0 => format!("{text} and {more} more"),
        _ => text,
    }
}

/// How `repo` lands: `local` is `ff` (E6, E20); `merge` its method. `pr` is refused by the
/// configuration in this version.
fn method(repo: &Repo) -> Result<Method, String> {
    match repo.landing.mode {
        LandingMode::Local => Ok(Method::Ff),
        LandingMode::Merge => match repo.landing.merge.method {
            MergeMethod::Ff => Ok(Method::Ff),
            MergeMethod::Squash => Ok(Method::Squash),
            MergeMethod::NoFf => Ok(Method::NoFf),
            other => Err(format!("the merge method {other:?} is not supported")),
        },
        other => Err(format!("the landing mode {other:?} is not supported")),
    }
}

/// Aborts a rebase stopped in the item's worktree at `dir` (a daemon that died during the
/// landing's rebase, a session that left one), reported through [`Shared::trouble`] when there
/// was one. Its head is then the item's branch again, never a partial pick of it.
pub(super) async fn abort_stale_rebase(
    shared: &Shared,
    item: &ItemId,
    dir: std::path::PathBuf,
) -> Result<(), String> {
    let aborted = blocking(move || -> Result<bool, land::Error> {
        let git = Git::new(&dir);
        // Mutant (`mutant_loop_land_over_stale_rebase`): the stopped rebase is left, and its
        // partial pick is the head that is checked, verified and landed.
        if cfg!(mutant_loop_land_over_stale_rebase) || !land::rebase_in_progress(&git)? {
            return Ok(false);
        }
        git.run(&["rebase", "--abort"], None)?;
        Ok(true)
    })
    .await
    .map_err(|e| e.to_string())?;
    if aborted {
        shared.trouble(format!(
            "{item}: a rebase was left in progress in its worktree; it is aborted"
        ));
    }
    Ok(())
}

/// Whether `landed`, the commit this attempt landed, is in the history of the target's head
/// `t` in the daemon's clone: equal to it, or an ancestor (the user committed on the target
/// after the fast-forward). No landed commit, or one the clone does not have, is not landed.
fn already_landed(clone: &Git, landed: Option<&str>, t: &str) -> Result<bool, bosun_git::Error> {
    let Some(landed) = landed else {
        return Ok(false);
    };
    // Mutant (`mutant_loop_landed_only_when_equal`): only a target exactly at the landed commit
    // counts, and one the user committed on is rebased onto again.
    if cfg!(mutant_loop_landed_only_when_equal) {
        return Ok(landed == t);
    }
    if landed == t {
        return Ok(true);
    }
    let commit = format!("{landed}^{{commit}}");
    if !clone.test(&[
        "rev-parse",
        "--verify",
        "--quiet",
        "--end-of-options",
        &commit,
    ])? {
        return Ok(false);
    }
    clone.test(&["merge-base", "--is-ancestor", "--end-of-options", landed, t])
}

/// Moves `branch` of the user's repository at `user` to `to` (E2), which was read at `from`.
fn fast_forward(
    user: &std::path::Path,
    branch: &str,
    to: &str,
    from: &str,
) -> Result<FastForward, land::Error> {
    let git = Git::new(user);
    if cfg!(mutant_loop_land_ref_only) {
        // Mutant (`mutant_loop_land_ref_only`): only the ref moves, whoever has it checked
        // out.
        let full = format!("refs/heads/{branch}");
        git.run(&["update-ref", &full, to, from], None)?;
        return Ok(FastForward::Moved {
            from: from.to_owned(),
            to: to.to_owned(),
        });
    }
    land::fast_forward(&git, branch, to)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Runs git in `dir` with an identity of its own, whatever the developer's configuration.
    fn git(dir: &std::path::Path, args: &[&str]) -> String {
        let out = std::process::Command::new("git")
            .arg("-C")
            .arg(dir)
            .args([
                "-c",
                "user.name=Synthetic",
                "-c",
                "user.email=synthetic@example.com",
            ])
            .args(["-c", "commit.gpgSign=false"])
            .args(args)
            .output()
            .unwrap();
        assert!(out.status.success(), "git {args:?}: {out:?}");
        String::from_utf8(out.stdout).unwrap().trim().to_owned()
    }

    fn commit(dir: &std::path::Path, file: &str) -> String {
        std::fs::write(dir.join(file), file).unwrap();
        git(dir, &["add", file]);
        git(dir, &["commit", "-q", "-m", file]);
        git(dir, &["rev-parse", "HEAD"])
    }

    /// #8: a passing verdict that cannot record its head would report the same head rebased
    /// forever; the third report of one head in one attempt refuses the landing.
    #[test]
    fn the_same_head_rebased_three_times_is_refused() {
        let first = rebased_again(None, 1, "h");
        assert!(!first.refused());
        let second = rebased_again(Some(&first), 1, "h");
        assert!(!second.refused());
        let third = rebased_again(Some(&second), 1, "h");
        assert_eq!(third.count, REBASED_LIMIT);
        assert!(third.refused());

        // A new head, or a new attempt, starts again.
        let other = rebased_again(Some(&second), 1, "g");
        assert_eq!(other.count, 1);
        assert!(!rebased_again(Some(&other), 1, "g").refused());
        let next = rebased_again(Some(&second), 2, "h");
        assert_eq!(next.count, 1);
        assert!(!rebased_again(Some(&next), 2, "h").refused());
    }

    #[test]
    fn a_long_list_of_conflicting_files_is_capped() {
        let files: Vec<String> = (0..25).map(|i| format!("f{i}")).collect();
        let text = listed(&files);
        assert!(text.starts_with("f0, f1, "), "{text}");
        assert!(text.ends_with("f19 and 5 more"), "{text}");
        assert_eq!(listed(&files[..2]), "f0, f1");
    }

    /// #8: a landing is done when the target holds the landed commit, at it or past it (the
    /// user committed on the target after the fast-forward), and only then.
    #[test]
    fn a_target_past_the_landed_commit_counts_as_landed() {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path();
        git(repo, &["init", "-q", "-b", "main"]);
        let base = commit(repo, "base.txt");
        let landed = commit(repo, "landed.txt");
        let past = commit(repo, "user.txt");
        git(repo, &["checkout", "-q", "-b", "other", &base]);
        let elsewhere = commit(repo, "other.txt");
        let clone = Git::new(repo);

        assert!(already_landed(&clone, Some(&landed), &landed).unwrap());
        assert!(already_landed(&clone, Some(&landed), &past).unwrap());
        assert!(!already_landed(&clone, Some(&elsewhere), &past).unwrap());
        assert!(!already_landed(&clone, None, &past).unwrap());
    }
}
