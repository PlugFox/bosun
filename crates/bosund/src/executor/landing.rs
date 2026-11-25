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
//!    and the capture of the user's checkout, follow the target ([`Done::Followed`]).
//! 3. A target that is not an ancestor of the worktree's head is rebased onto; a conflict is
//!    stored as the item's `conflict` note and reported as [`Event::Conflict`].
//! 4. A head the row does not name as verified, rebased now or before a crash, is
//!    [`Event::Rebased`]: the machine runs the verdict on it, and a pass emits `Land` again. A
//!    verdict that passed before the rebase is never a verdict on the target.
//! 5. The commit to land is [`bosun_git::merge::merge_commit`] of the verified tree; for
//!    `squash` and `no-ff` its message is the card's title and the attempt's summary, with the
//!    `Item:`, `Run:` and `Session:` trailers of the attempt's last session.
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
use bosun_git::Git;
use bosun_git::land::{self, FastForward, Rebase};
use bosun_git::merge::{self, Method};
use bosun_store::{LandingRow, Write};
use tokio::sync::watch;

use super::workspace::Workspace;
use super::{Claimed, ItemWorktree, Shared, blocking, git, lookup, now_ms, verdict};

/// The kind of the item note a conflicting rebase leaves (`Write::ItemNote`).
pub(crate) const NOTE_CONFLICT: &str = "conflict";

/// How many times a target the user committed on during the fast-forward starts the landing
/// again before it is refused.
pub(crate) const RESTARTS: u32 = 3;

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
        kill,
    };
    let event = task.run(config).await;
    Landed {
        event,
        followed: task.followed.then_some(task.claimed),
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
        let Some((repo, _)) = lookup(config, &self.shared.options.repo) else {
            return Some(self.refused("the repository is no longer configured".to_owned()));
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
            match self.pass(repo, &wt, method).await {
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
    async fn pass(&mut self, repo: &Repo, wt: &ItemWorktree, method: Method) -> Pass {
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
        // A crash after the fast-forward: the target is at what this attempt landed.
        if let Some(row) = &row
            && row.landed_sha.as_deref() == Some(t.as_str())
        {
            return Pass::Done(
                self.delivered(wt, &target, row.delivered_ms.is_none())
                    .await,
            );
        }

        // 3. The head of the item's worktree, rebased onto the target unless it descends.
        let dir = self.claimed.workdir.clone();
        let at = t.clone();
        let rebased = blocking(move || -> Result<Result<String, Vec<String>>, String> {
            let git = Git::new(&dir);
            let head = git::head(&git).map_err(|e| e.to_string())?;
            let descends = git
                .test(&["merge-base", "--is-ancestor", &at, &head])
                .map_err(|e| e.to_string())?;
            if descends {
                return Ok(Ok(head));
            }
            match land::rebase_onto(&git, &at).map_err(|e| e.to_string())? {
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
            None => return Pass::Done(Some(Event::Rebased { attempt })),
        };

        // 5. The commit to land.
        let (message, trailers) = match self.message(method).await {
            Ok(made) => made,
            Err(e) => return self.refuse(e),
        };
        let clone = wt.workspace.clone_path();
        let (at, head) = (t.clone(), h.clone());
        let made = blocking(move || {
            merge::merge_commit(
                &Git::new(clone),
                method,
                &at,
                &head,
                &verified_tree,
                &message,
                &trailers,
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
            Git::new(&user).run(
                &["fetch", "--quiet", "--no-tags", "--", &from, &offered],
                None,
            )?;
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
                        () = verdict::killed(&mut self.kill) => return Pass::Done(None),
                    }
                }
                Err(land::Error::NotDescendant { head, .. }) => {
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
            files.join(", ")
        ));
        let note = Write::ItemNote {
            item: item.clone(),
            kind: NOTE_CONFLICT.to_owned(),
            body: serde_json::json!({"target": target, "target_head": t, "files": files})
                .to_string(),
            at_ms: now_ms(),
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
