//! Landing in worktree mode (#8 "Landing"; E2, E6, E14, E19, E20): the one keyed
//! [`EffectKind::Land`](bosun_core::EffectKind::Land) of an item whose repository has
//! `repo.<name>.worktree`, in the modes `local` (`merge` with `ff`, no push, onto the branch the
//! item was claimed from), `merge` and `pr`. Without `repo.<name>.worktree` landing is the
//! loop's own (nothing to do) and nothing here runs.
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
//! In `pr` mode steps 1 to 4 are the same; nothing is merged and the user's target does not
//! move ([`Task::pull_request`]):
//!
//! 5. The verified head `H` is offered and fetched into the user's repository as in step 6 (the
//!    offer is dropped again on every path), and pushed from there by name,
//!    `git push origin <H>:refs/heads/bosun/<slug>` ([`git::pr_branch`]), under the repository
//!    identity's tool environment (#2) with the repository's hooks disabled: a replay is a
//!    no-op. The `bosun/` branches are the daemon's: a head rebased since an earlier push (a
//!    crash with a moved target, a card reworked after its pull request closed) is pushed over
//!    it, with a lease (`--force-with-lease=refs/heads/<branch>:<expected>`) on the commit the
//!    daemon last pushed there (the item's [`NOTE_PR_PUSH`] note), else on what the remote has
//!    now ([`push`]): a branch someone else pushed to since is a refusal, never overwritten.
//! 6. The forge ([`super::forge`]) is asked for the open pull request of that branch. None: one
//!    is opened into the target with the card's title, the configured draft, reviewers and
//!    labels, and [`pr::body`] of the attempt's summary, the daemon's facts and `Closes #N`. One:
//!    only the daemon's region of its body is rewritten ([`pr::update`]), never a second one
//!    opened. The row stores its number, URL and `open`, and the landing is [`Event::Landed`].
//!
//! A push or forge answer that refuses is [`Event::LandingRefused`] with the error as the
//! item's `landing` note; no answer keeps the item in `Landing`, holding no lease (E24), and
//! steps 5 and 6 are tried again every [`super::Options::poll`] until they work or the task is
//! killed.
//!
//! [`pr::body`]: bosun_core::publish::pr::body
//! [`pr::update`]: bosun_core::publish::pr::update
//! [`Done::Followed`]: super::Done::Followed

use std::sync::Arc;

use bosun_config::{Config, LandingMode, MergeMethod, Repo};
use bosun_core::publish::pr;
use bosun_core::{Event, ItemId, RunResult};
use bosun_git::land::{self, FastForward, Rebase};
use bosun_git::merge::{self, Method};
use bosun_git::{Git, Ident};
use bosun_identity::{Bundle, Env, inherited};
use bosun_store::{LandingRow, PrState, Write};
use tokio::sync::watch;

use super::forge::{self, At, ForgeError, NewPullRequest, PullRequest};
use super::workspace::Workspace;
use super::{
    Claimed, ItemWorktree, Shared, Signal, blocking, git, identity, lookup, now_ms, verdict,
};

/// The kind of the item note a conflicting rebase leaves (`Write::ItemNote`).
pub(crate) const NOTE_CONFLICT: &str = "conflict";

/// The kind of the item note a refused pull request landing leaves (`Write::ItemNote`).
pub(crate) const NOTE_LANDING: &str = "landing";

/// The kind of the item note that holds the last commit a `pr` landing pushed, and to which
/// branch (`Write::ItemNote`): the lease of the next push ([`push`]).
pub(crate) const NOTE_PR_PUSH: &str = "pr_push";

/// The remote of the user's repository a `pr` landing pushes to: the one the identity checks
/// read (#2).
const REMOTE: &str = "origin";

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
    /// The attempt's verdict run whose pass emitted the `Land` this task answers; it keys the
    /// task's [`Event::Conflict`] to this landing pass.
    pub(crate) run: u32,
    /// The card's title: the subject of a squash or merge commit, the title of a pull request.
    pub(crate) title: String,
    /// The card's number: the issue a pull request closes.
    pub(crate) card: u64,
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
    kill: watch::Receiver<Signal>,
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
    kill: watch::Receiver<Signal>,
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
        // The author and committer of the commits the landing makes (E26), and the identity
        // its push and pull request are made as (#2).
        let bundle = match identity::bundle(&repo.identity, identity, &repo.path) {
            Ok(bundle) => bundle,
            Err(e) => {
                return Some(self.refused(format!("the repository identity's author: {e}")));
            }
        };
        let ident = git::ident(&bundle.author);
        let Some(wt) = self.claimed.worktree.clone() else {
            return Some(self.refused("the item has no worktree".to_owned()));
        };
        let mode = match mode(repo) {
            Ok(mode) => mode,
            Err(e) => return Some(self.refused(e)),
        };
        let mut restarts = 0;
        loop {
            match self.pass(repo, &wt, mode, &ident, &bundle).await {
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
        how: Mode,
        ident: &Ident,
        bundle: &Bundle,
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

        let method = match how {
            Mode::Merge(method) => method,
            Mode::Pr => {
                return Pass::Done(self.pull_request(repo, wt, bundle, &target, &h).await);
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
        if let Err(e) = self.row(&target, Some(landed.clone()), None, None).await {
            return self.refuse(e);
        }

        // 6. Delivered to the user's repository (E14), moved by E2.
        if let Err(e) = self.offer(wt, &repo.path, &landed).await {
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
                        // The user may take hours: the item waits holding nothing (E24). Its
                        // re-verdict after a restart asks again; a fast-forward needs none.
                        // Mutant (`mutant_loop_hold_leases_while_deferred`): it keeps them.
                        if !cfg!(mutant_loop_hold_leases_while_deferred) {
                            self.shared
                                .deps
                                .leases
                                .release(&crate::leases::Holder::Item(item.clone()))
                                .await;
                        }
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

    /// The clone offers `commit` under [`git::landing_ref`] and the user's repository at
    /// `user` fetches it, objects only (E14). The offer stays: the caller drops it.
    async fn offer(
        &self,
        wt: &ItemWorktree,
        user: &std::path::Path,
        commit: &str,
    ) -> Result<(), bosun_git::Error> {
        let (clone, user, offered, commit) = (
            wt.workspace.clone_path(),
            user.to_owned(),
            git::landing_ref(&self.spec.item),
            commit.to_owned(),
        );
        blocking(move || -> Result<(), bosun_git::Error> {
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
        .await
    }

    /// Steps 5 and 6 of `pr` (see the module documentation): the verified head `h`, based on
    /// `target`, pushed as the item's branch and proposed in one open pull request.
    async fn pull_request(
        &mut self,
        repo: &Repo,
        wt: &ItemWorktree,
        bundle: &Bundle,
        target: &str,
        h: &str,
    ) -> Option<Event> {
        let item = self.spec.item.clone();
        let branch = git::pr_branch(&item);
        let facts = match self.facts(h).await {
            Ok(facts) => facts,
            Err(e) => return Some(self.refused(e)),
        };
        let summary = match self
            .shared
            .deps
            .reader
            .summary(&item, self.spec.attempt)
            .await
        {
            Ok(call) => call.map_or_else(String::new, |call| call.body),
            Err(e) => return Some(self.refused(format!("the summary of {item}: {e}"))),
        };
        // The daemon's own git and `gh` for the repository's identity, never an agent's (#2).
        let at = At {
            dir: repo.path.clone(),
            env: Env::tool(bundle, &inherited()),
        };
        let new = NewPullRequest {
            head: branch.clone(),
            base: target.to_owned(),
            title: self.spec.title.clone(),
            body: String::new(),
            draft: repo.landing.pr.draft,
            reviewers: repo.landing.pr.reviewers.clone(),
            labels: repo.landing.pr.labels.clone(),
        };
        let mut waiting = false;
        let opened = loop {
            let error = match self.publish(repo, wt, &at, &new, h, &summary, &facts).await {
                Ok(opened) => break opened,
                Err(error) => error,
            };
            match error {
                // Mutant (`mutant_loop_pr_refusal_is_retried`): a refusal is waited out as if
                // the forge had not answered.
                ForgeError::Refused(why) if !cfg!(mutant_loop_pr_refusal_is_retried) => {
                    return Some(self.pr_refused(&why).await);
                }
                ForgeError::Refused(why) | ForgeError::Unreachable(why) => {
                    if !waiting {
                        waiting = true;
                        // The forge may be away for hours: the item waits holding nothing
                        // (E24); the push and the forge need no lease.
                        self.shared
                            .deps
                            .leases
                            .release(&crate::leases::Holder::Item(item.clone()))
                            .await;
                        self.shared.trouble(format!(
                            "{item}: the pull request of `{branch}` waits: {why}; it is tried \
                             again, and the repository claims nothing meanwhile"
                        ));
                    }
                    let poll = self.shared.options.poll;
                    tokio::select! {
                        () = tokio::time::sleep(poll) => {}
                        () = verdict::killed(&mut self.kill) => return None,
                    }
                }
            }
        };
        let row = self
            .row(target, Some(h.to_owned()), None, Some(&opened))
            .await;
        if let Err(e) = row {
            return Some(self.refused(e));
        }
        Some(Event::Landed)
    }

    /// One try of steps 5 and 6 of `pr`: `h` fetched into the user's repository and pushed as
    /// `new.head`, then the branch's open pull request looked up and its region rewritten, or
    /// one opened. The offer is dropped whatever happens.
    #[allow(clippy::too_many_arguments)]
    async fn publish(
        &self,
        repo: &Repo,
        wt: &ItemWorktree,
        at: &At,
        new: &NewPullRequest,
        h: &str,
        summary: &str,
        facts: &[(String, String)],
    ) -> Result<PullRequest, ForgeError> {
        let offered = self.offer(wt, &repo.path, h).await;
        self.drop_offer(wt).await;
        offered.map_err(|e| {
            ForgeError::Refused(format!(
                "the verified commit cannot be fetched into the user's repository: {e}"
            ))
        })?;
        let recorded = self.last_push(&new.head).await;
        let (env, user, branch, head) = (
            at.env.clone(),
            repo.path.clone(),
            new.head.clone(),
            h.to_owned(),
        );
        blocking(move || push(&user, &env, &branch, &head, recorded.as_deref())).await?;
        self.note_push(&new.head, h).await;

        let facts: Vec<(&str, &str)> = facts
            .iter()
            .map(|(k, v)| (k.as_str(), v.as_str()))
            .collect();
        let forge = self.shared.deps.forge.clone();
        let (looked_at, head) = (at.clone(), new.head.clone());
        // Mutant (`mutant_loop_pr_create_without_lookup`): the pull request is opened without
        // looking for the one a landing before a crash opened.
        let existing = if cfg!(mutant_loop_pr_create_without_lookup) {
            None
        } else {
            blocking(move || forge.open_pull_request(&looked_at, &head)).await?
        };
        let forge = self.shared.deps.forge.clone();
        let at = at.clone();
        if let Some(existing) = existing {
            // People edit descriptions: only the daemon's region is the daemon's.
            // Mutant (`mutant_loop_pr_body_overwritten`): the whole body is written afresh.
            let body = if cfg!(mutant_loop_pr_body_overwritten) {
                pr::body(summary, &facts, self.spec.card)
            } else {
                pr::update(&existing.body, &facts)
            };
            let number = existing.number;
            let sent = body.clone();
            blocking(move || forge.set_body(&at, number, &sent)).await?;
            return Ok(PullRequest { body, ..existing });
        }
        let new = NewPullRequest {
            body: pr::body(summary, &facts, self.spec.card),
            ..new.clone()
        };
        blocking(move || forge.create(&at, &new)).await
    }

    /// The commit the daemon last pushed to `branch` for this item ([`NOTE_PR_PUSH`]); `None`
    /// when none is recorded, or the note does not read.
    async fn last_push(&self, branch: &str) -> Option<String> {
        #[derive(serde::Deserialize)]
        struct Pushed {
            branch: String,
            sha: String,
        }
        let body = self
            .shared
            .deps
            .reader
            .item_note(&self.spec.item, NOTE_PR_PUSH)
            .await
            .ok()
            .flatten()?;
        serde_json::from_str::<Pushed>(&body)
            .ok()
            .filter(|p| p.branch == branch)
            .map(|p| p.sha)
    }

    /// Records `sha` as the commit the daemon last pushed to `branch` ([`NOTE_PR_PUSH`]).
    async fn note_push(&self, branch: &str, sha: &str) {
        let item = &self.spec.item;
        let note = Write::ItemNote {
            item: item.clone(),
            kind: NOTE_PR_PUSH.to_owned(),
            body: serde_json::json!({ "branch": branch, "sha": sha }).to_string(),
            at_ms: now_ms(),
        };
        if let Err(e) = self.shared.deps.writer.write(vec![note]).await {
            self.shared.trouble(format!(
                "the pushed commit of {item} cannot be recorded: {e}"
            ));
        }
    }

    /// The daemon's facts of a pull request: the item, its attempts, the result line of the
    /// verdict run that passed, and the commit proposed.
    async fn facts(&self, h: &str) -> Result<Vec<(String, String)>, String> {
        let item = &self.spec.item;
        let attempt = self.spec.attempt;
        let runs = self
            .shared
            .deps
            .reader
            .verdict_runs(item)
            .await
            .map_err(|e| format!("the verdict runs of {item}: {e}"))?;
        let verdict = runs
            .iter()
            .rev()
            .find(|r| r.attempt == attempt && r.result == Some(RunResult::Pass))
            .and_then(|r| r.verdict_line.clone())
            .filter(|l| !l.trim().is_empty())
            .unwrap_or_else(|| "passed".to_owned());
        Ok(vec![
            ("Item".to_owned(), item.to_string()),
            ("Attempts".to_owned(), attempt.to_string()),
            ("Verdict".to_owned(), verdict),
            ("Commit".to_owned(), h.to_owned()),
        ])
    }

    /// A push or pull request the service refused: the item's `landing` note and
    /// [`Event::LandingRefused`], not a failed attempt.
    async fn pr_refused(&self, why: &str) -> Event {
        let item = &self.spec.item;
        let at_ms = now_ms();
        let note = Write::ItemNote {
            item: item.clone(),
            kind: NOTE_LANDING.to_owned(),
            body: serde_json::json!({ "error": why, "at_ms": at_ms }).to_string(),
            at_ms,
        };
        if let Err(e) = self.shared.deps.writer.write(vec![note]).await {
            self.shared
                .trouble(format!("the landing note of {item} cannot be stored: {e}"));
        }
        self.refused(format!("the pull request: {why}"))
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
        if stamp && let Err(e) = self.row(target, None, Some(now_ms()), None).await {
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
            run: self.spec.run,
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

    /// Updates the landing row of the attempt; `opened`: the pull request it opened, `open`.
    async fn row(
        &self,
        target: &str,
        landed: Option<String>,
        delivered: Option<i64>,
        opened: Option<&PullRequest>,
    ) -> Result<(), String> {
        let row = LandingRow {
            item: self.spec.item.clone(),
            attempt: self.spec.attempt,
            target: target.to_owned(),
            head_sha: None,
            verified_tree: None,
            landed_sha: landed,
            delivered_ms: delivered,
            pr_number: opened.map(|p| p.number),
            pr_url: opened.map(|p| p.url.clone()),
            pr_state: opened.map(|_| PrState::Open),
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

/// How an item lands after its head is verified on the target.
#[derive(Debug, Clone, Copy)]
enum Mode {
    /// The commit made by this method moves the user's target.
    Merge(Method),
    /// The head is pushed and proposed in a pull request.
    Pr,
}

/// How `repo` lands: `local` is `ff` (E6, E20); `merge` its method; `pr` a pull request.
fn mode(repo: &Repo) -> Result<Mode, String> {
    match repo.landing.mode {
        LandingMode::Local => Ok(Mode::Merge(Method::Ff)),
        LandingMode::Merge => match repo.landing.merge.method {
            MergeMethod::Ff => Ok(Mode::Merge(Method::Ff)),
            MergeMethod::Squash => Ok(Mode::Merge(Method::Squash)),
            MergeMethod::NoFf => Ok(Mode::Merge(Method::NoFf)),
            other => Err(format!("the merge method {other:?} is not supported")),
        },
        LandingMode::Pr => Ok(Mode::Pr),
        other => Err(format!("the landing mode {other:?} is not supported")),
    }
}

/// Pushes `head` by name (`<head>:refs/heads/<branch>`) from the user's repository at `user` to
/// [`REMOTE`] under `env`, the repository identity's tool environment, with the repository's
/// hooks disabled ([`Git::command`]).
///
/// The branch is the daemon's (`bosun/<slug>`), so a head rebased since an earlier push goes
/// over it, with a lease: `--force-with-lease=refs/heads/<branch>:<expected>`, where
/// `expected` is `recorded`, the commit the daemon last pushed there, or when none is
/// recorded what the remote has now (nothing: the branch must not exist). A remote already at
/// `head` is not pushed to again: a replay is a no-op, even one whose push was not recorded. A
/// rejection (another push since: the lease does not hold) is [`ForgeError::Refused`], a
/// remote that does not answer [`ForgeError::Unreachable`].
fn push(
    user: &std::path::Path,
    env: &Env,
    branch: &str,
    head: &str,
    recorded: Option<&str>,
) -> Result<(), ForgeError> {
    let git = Git::new(user);
    let remote_ref = format!("refs/heads/{branch}");
    let url = push_url(&git, env)?;
    let now = remote_head(&git, env, &url, &remote_ref)?;
    if now.as_deref() == Some(head) {
        return Ok(());
    }
    let expected = recorded.map(str::to_owned).or(now).unwrap_or_default();
    let spec = format!("{head}:{remote_ref}");
    let mut command = remote_command(&git, env);
    command.args(["push", "--quiet", "--no-verify"]);
    // Mutant (`mutant_loop_pr_push_without_lease`): a plain push, which a head rebased since
    // the branch was pushed cannot make.
    if !cfg!(mutant_loop_pr_push_without_lease) {
        command.arg(format!("--force-with-lease={remote_ref}:{expected}"));
    }
    command.args([REMOTE, &spec]);
    let out = command
        .output()
        .map_err(|e| ForgeError::Refused(format!("cannot run git: {e}")))?;
    if out.status.success() {
        return Ok(());
    }
    let stderr = String::from_utf8_lossy(&out.stderr).trim().to_owned();
    Err(forge::classify(format!(
        "git push {REMOTE} {spec}: {stderr}"
    )))
}

/// The credential helpers of a git that talks to the forge for the identity: the list the
/// identity's git configuration may have filled is emptied, then `gh auth git-credential`, which
/// answers from the identity's `gh` configuration (`GH_CONFIG_DIR` of [`Env::tool`]). `Env::tool`
/// sets `GIT_CONFIG_NOSYSTEM=1`, so without it an HTTPS remote has no credential at all. No
/// token is ever on a command line, in a URL or in a log: `gh` hands it to git on a pipe. On
/// Windows git runs the helper through its own `sh`, which finds `gh` on the identity's `PATH`
/// as the daemon's own `gh` calls do.
pub(crate) const CREDENTIALS: [&str; 4] = [
    "-c",
    "credential.helper=",
    "-c",
    "credential.helper=!gh auth git-credential",
];

/// A [`Git::command`] for a git that talks to the forge: `env`, the identity's tool environment,
/// and the [`CREDENTIALS`].
pub(crate) fn remote_command(git: &Git, env: &Env) -> std::process::Command {
    let mut command = git.command();
    env.apply(&mut command);
    command.args(CREDENTIALS);
    command
}

/// The URL a push to [`REMOTE`] goes to, `pushInsteadOf` and `pushurl` applied under `env`:
/// what [`remote_head`] reads, so the lease is taken on the remote the push goes to.
fn push_url(git: &Git, env: &Env) -> Result<String, ForgeError> {
    let mut command = git.command();
    env.apply(&mut command);
    command.args(["remote", "get-url", "--push", REMOTE]);
    let out = command
        .output()
        .map_err(|e| ForgeError::Refused(format!("cannot run git: {e}")))?;
    let url = String::from_utf8_lossy(&out.stdout).trim().to_owned();
    if !out.status.success() || url.is_empty() {
        let stderr = String::from_utf8_lossy(&out.stderr).trim().to_owned();
        return Err(ForgeError::Refused(format!(
            "the push URL of `{REMOTE}`: {stderr}"
        )));
    }
    Ok(url)
}

/// The commit `remote_ref` names at `url` now (`git ls-remote`); `None` when it does not exist.
fn remote_head(
    git: &Git,
    env: &Env,
    url: &str,
    remote_ref: &str,
) -> Result<Option<String>, ForgeError> {
    let mut command = remote_command(git, env);
    command.args(["ls-remote", "--quiet", "--", url, remote_ref]);
    let out = command
        .output()
        .map_err(|e| ForgeError::Refused(format!("cannot run git: {e}")))?;
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr).trim().to_owned();
        return Err(forge::classify(format!(
            "git ls-remote {REMOTE} {remote_ref}: {stderr}"
        )));
    }
    Ok(String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(|line| line.split_once('\t'))
        .find(|(_, name)| name.trim() == remote_ref)
        .map(|(sha, _)| sha.trim().to_owned()))
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

    /// #2, #8: a git that talks to the forge empties the identity's credential helpers and asks
    /// the identity's `gh` instead; nothing that looks like a token is on its command line.
    #[test]
    fn a_remote_git_takes_its_credentials_from_the_identitys_gh() {
        let command = remote_command(&Git::new("."), &Env::default());
        let args: Vec<String> = command
            .get_args()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        let at = args
            .windows(4)
            .position(|w| w == CREDENTIALS)
            .unwrap_or_else(|| panic!("{args:?}"));
        assert!(
            args[..at].iter().all(|a| !a.starts_with("credential.")),
            "{args:?}"
        );
        assert!(
            !args
                .iter()
                .any(|a| a.contains("token") || a.contains("password")),
            "{args:?}"
        );
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
