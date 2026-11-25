//! The loop's git: the preconditions with the values the commits may carry, the commit range
//! of a session, and setting an item's work aside. Every command goes through `bosun-git`'s
//! hooks-disabled invoker (D3 of #7).

use std::collections::BTreeSet;
use std::path::Path;
use std::sync::LazyLock;

use bosun_core::ItemId;
use bosun_git::{Base, Error, Expected, Git, Reason};
use bosun_store::RunRow;
use regex::Regex;

/// A `Co-authored-by:` line of a commit message.
static CO_AUTHOR: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?im)^[ \t]*co-authored-by[ \t]*:[ \t]*(.*?)[ \t]*\r?$")
        .expect("the co-author pattern compiles")
});

/// The co-author the agent CLI itself adds to its commits (Claude Code: `Claude <model>
/// <noreply@anthropic.com>`); any other co-author is forged.
static PROVIDER_CO_AUTHOR: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^Claude(?: [^<>]*)? <noreply@anthropic\.com>$")
        .expect("the provider co-author pattern compiles")
});

/// The value of the `Run:` trailer of a session: its attempt and session numbers.
pub(crate) fn run_label(attempt: u32, spawn: u32) -> String {
    format!("a{attempt}s{spawn}")
}

/// The commit HEAD resolves to.
pub(crate) fn head(git: &Git) -> Result<String, Error> {
    Ok(git
        .run(&["rev-parse", "--verify", "--quiet", "HEAD^{commit}"], None)?
        .trim()
        .to_owned())
}

/// The commits of `base..head`.
pub(crate) fn range(git: &Git, base: &str, head: &str) -> Result<Vec<String>, Error> {
    let out = git.run(&["rev-list", &format!("{base}..{head}")], None)?;
    Ok(out.lines().map(str::to_owned).collect())
}

/// The most paths of an `uncommitted` note.
pub(crate) const UNCOMMITTED_CAP: usize = 50;

/// What [`preconditions`] found.
#[derive(Debug, Default)]
pub(crate) struct Checked {
    /// Why the attempt fails its preconditions; empty when they hold.
    pub(crate) reasons: Vec<Reason>,
    /// Worktree mode: the paths left uncommitted or untracked, at most [`UNCOMMITTED_CAP`].
    /// The verdict runs in a checkout of the commit, so they are not verified, and not an
    /// error either.
    pub(crate) uncommitted: Vec<String>,
}

/// The commit `head` and `target` (a ref or a commit) have in common: `git merge-base`.
pub(crate) fn merge_base(git: &Git, head: &str, target: &str) -> Result<String, Error> {
    Ok(git
        .run(&["merge-base", "--end-of-options", head, target], None)?
        .trim()
        .to_owned())
}

/// Worktree mode (#8): `base` with the start of the item's own commits as its head: the merge
/// base of the worktree's HEAD and the clone's `refs/heads/<target>` now. Landing rebases the
/// item onto the target, so the commits that came in from the target (a person's, with
/// whatever messages they have) are never judged as the agent's. The refs and the stash are
/// `base`'s own, as captured.
pub(crate) fn item_base(git: &Git, base: &Base, target: &str) -> Result<Base, Error> {
    let head = head(git)?;
    let start = merge_base(git, &head, &format!("refs/heads/{target}"))?;
    Ok(Base {
        head: start,
        ..base.clone()
    })
}

/// The verdict preconditions of the item claimed at `base`. The trailers a commit may carry
/// are the item's own: its id, its runs' labels and its sessions' ids, plus the co-author line
/// the provider writes. With `target` (worktree mode, E19: the branch the item lands on) the
/// clean tree is not one of them: what is not committed is listed in [`Checked::uncommitted`]
/// instead, and the item's commits start at [`item_base`], while the claim's own head must
/// still be an ancestor of HEAD.
pub(crate) fn preconditions(
    repo: &Path,
    base: &Base,
    item: &ItemId,
    runs: &[RunRow],
    target: Option<&str>,
) -> Result<Checked, Error> {
    let git = Git::new(repo);
    let claim = base;
    let own;
    let base = match target {
        Some(target) => {
            own = item_base(&git, base, target)?;
            &own
        }
        None => base,
    };
    let mut expected = Expected {
        item: BTreeSet::from([item.to_string()]),
        run: runs.iter().map(|r| run_label(r.attempt, r.spawn)).collect(),
        session: runs.iter().map(|r| r.session_key.clone()).collect(),
        co_authored_by: BTreeSet::new(),
    };
    if let Ok(head) = head(&git) {
        let log = git.run(
            &["log", "--format=%B", &format!("{}..{head}", base.head)],
            None,
        )?;
        expected.co_authored_by = CO_AUTHOR
            .captures_iter(&log)
            .map(|c| c[1].to_owned())
            .filter(|v| PROVIDER_CO_AUTHOR.is_match(v))
            .collect();
    }
    if target.is_none() {
        return Ok(Checked {
            reasons: bosun_git::check(&git, base, &expected)?,
            uncommitted: Vec::new(),
        });
    }
    // History below the claim's head rewritten; else no commit of the item's own.
    let mut reasons = bosun_git::ancestry(&git, claim)?;
    if reasons.is_empty() {
        reasons = bosun_git::ancestry(&git, base)?;
    }
    reasons.extend(bosun_git::refs(&git, base)?);
    reasons.extend(bosun_git::messages(&git, base, &expected)?);
    let mut uncommitted: Vec<String> = Vec::new();
    for reason in bosun_git::clean_tree(&git)? {
        if let Reason::Dirty { entries } = reason {
            // A porcelain v1 entry is `XY path`.
            for path in entries.iter().map(|e| e.get(3..).unwrap_or(e)) {
                if uncommitted.len() < UNCOMMITTED_CAP && !uncommitted.iter().any(|p| p == path) {
                    uncommitted.push(path.to_owned());
                }
            }
        }
    }
    Ok(Checked {
        reasons,
        uncommitted,
    })
}

/// Removes what is untracked and not ignored, and every uncommitted change: the checkout was
/// clean at `base`, so what is left is a verdict's or a session's.
pub(crate) fn reset_to(git: &Git, commit: &str) -> Result<(), Error> {
    git.run(&["reset", "-q", "--hard", commit], None)?;
    git.run(&["clean", "-q", "-f", "-d"], None)?;
    Ok(())
}

/// After a verdict on the base commit: the checkout as it was at claim.
pub(crate) fn restore_if_dirty(repo: &Path, base: &Base) -> Result<(), Error> {
    let git = Git::new(repo);
    if !bosun_git::clean_tree(&git)?.is_empty() {
        reset_to(&git, &base.head)?;
    }
    Ok(())
}

/// The name of the ref an item's work is set aside under.
pub(crate) fn aside_ref(item: &ItemId) -> String {
    format!("refs/bosun/items/{}", slug(item))
}

/// The name of the ref the daemon's clone offers an item's landed commit under, for the
/// user's repository to fetch (#8, E14).
pub(crate) fn landing_ref(item: &ItemId) -> String {
    format!("refs/bosun/landing/{}", slug(item))
}

/// `item` as a ref name component: every character but ASCII letters and digits is `-`.
fn slug(item: &ItemId) -> String {
    item.0
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect()
}

/// An item that did not end Done: its work (commits, and uncommitted changes as one more
/// commit) is kept under [`aside_ref`], and the checkout goes back to `base` on the claim
/// branch, clean.
pub(crate) fn set_aside(repo: &Path, base: &Base, item: &ItemId) -> Result<(), Error> {
    let git = Git::new(repo);
    let head = head(&git)?;
    let dirty = !bosun_git::clean_tree(&git)?.is_empty();
    let branch = git.optional(&["symbolic-ref", "--quiet", "HEAD"])?;
    if head == base.head && !dirty && branch == base.branch {
        return Ok(());
    }
    let tip = if dirty {
        snapshot_commit(&git, &head, item)?
    } else {
        head
    };
    if tip != base.head {
        git.run(&["update-ref", &aside_ref(item), &tip], None)?;
    }
    match &base.branch {
        Some(branch) => {
            let short = branch.strip_prefix("refs/heads/").unwrap_or(branch);
            git.run(&["checkout", "-q", "-f", short], None)?;
        }
        None => {
            git.run(&["checkout", "-q", "-f", "--detach", &base.head], None)?;
        }
    }
    reset_to(&git, &base.head)
}

/// Keeps the work of the checkout at `dir` under [`aside_ref`] of `item`: its HEAD, with
/// uncommitted changes (untracked files too) as one more commit. In a linked worktree the ref
/// is written to the repository the worktree hangs off (refs are shared). Nothing in the
/// checkout changes. Returns the commit kept.
pub(crate) fn keep_work(dir: &Path, item: &ItemId) -> Result<String, Error> {
    let git = Git::new(dir);
    let head = head(&git)?;
    let tip = if bosun_git::clean_tree(&git)?.is_empty() {
        head
    } else {
        snapshot_commit(&git, &head, item)?
    };
    git.run(&["update-ref", &aside_ref(item), &tip], None)?;
    Ok(tip)
}

/// A commit on top of `head` holding the working tree as it is (untracked files too, ignored
/// ones not), made through a temporary index so the checkout's own index is untouched.
fn snapshot_commit(git: &Git, head: &str, item: &ItemId) -> Result<String, Error> {
    let scratch =
        tempfile::tempdir().map_err(|e| git.unexpected(format!("a temporary index: {e}")))?;
    let index = scratch.path().join("index");
    let run = |args: &[&str]| -> Result<String, Error> {
        let mut command = git.command();
        command
            .env("GIT_INDEX_FILE", &index)
            .args(["-c", "user.name=bosun", "-c", "user.email=bosun@localhost"])
            .args(args);
        let out = command
            .output()
            .map_err(|e| git.unexpected(format!("git {}: {e}", args.join(" "))))?;
        if out.status.success() {
            Ok(String::from_utf8_lossy(&out.stdout).trim().to_owned())
        } else {
            Err(git.unexpected(format!(
                "git {}: {}",
                args.join(" "),
                String::from_utf8_lossy(&out.stderr).trim()
            )))
        }
    };
    run(&["read-tree", head])?;
    run(&["add", "-A"])?;
    let tree = run(&["write-tree"])?;
    let message = format!("bosun: uncommitted work of {item}, set aside");
    run(&["commit-tree", &tree, "-p", head, "-m", &message])
}
