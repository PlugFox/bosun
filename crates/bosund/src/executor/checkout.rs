//! The guard on the user's checkout in worktree mode (#8, E4 as amended by E21 and E23): the
//! item runs in a worktree off the daemon's clone, so nothing it does should reach the user's
//! checkout. The checkout is captured at claim ([`UserCheckout::capture`]: HEAD, its branch,
//! every ref, the stash and `git status --porcelain`) and compared before the verdict
//! ([`compare_checkout`]).
//!
//! The guard is lenient (E21): the user may be working in that checkout meanwhile. It compares
//! HEAD, `refs/heads/*` and `refs/tags/*` only (E23): `refs/remotes/*` (a `fetch --prune`, a
//! fetch after a force-push), `refs/stash`, `refs/notes/*` and every other namespace are not
//! compared. The filter is in the comparison, so a capture stored with every ref by an earlier
//! daemon is read the same way.
//!
//! - Needs you (`guard_checkout`): HEAD switched to another branch or detached; an existing
//!   branch or tag deleted, or moved to a commit that does not descend from its captured value.
//! - Tolerated: a branch or tag moved forward (the user's own commits), a new one, any change
//!   to a ref outside `refs/heads/` and `refs/tags/`.
//! - A warning, not Needs you: changed status lines (the user editing files).
//!
//! The real boundary is the OS-user layout of #2; this guard is hygiene.

use std::collections::BTreeSet;
use std::path::Path;

use bosun_git::{Base, Git};

/// The stash ref, which the guard does not compare.
const STASH: &str = "refs/stash";

/// Whether the guard compares `reference` (E23): the branches and the tags only. A capture an
/// earlier daemon stored with every ref is read the same way.
fn compared(reference: &str) -> bool {
    if cfg!(mutant_loop_guard_compares_remotes) {
        // Mutant (`mutant_loop_guard_compares_remotes`): every ref but `refs/stash` is
        // compared, as in E21 before E23.
        return reference != STASH;
    }
    reference.starts_with("refs/heads/") || reference.starts_with("refs/tags/")
}

/// The user's checkout as captured: its base and its status lines.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct UserCheckout {
    /// HEAD, its branch, every ref and the stash entries.
    pub(crate) base: Base,
    /// The lines of `git status --porcelain --untracked-files=all`.
    pub(crate) status: Vec<String>,
}

impl UserCheckout {
    /// Captures the checkout at `user`.
    ///
    /// # Errors
    ///
    /// A git command fails.
    pub(crate) fn capture(user: &Path) -> Result<Self, bosun_git::Error> {
        let git = Git::new(user);
        let base = Base::capture(&git)?;
        let status = git
            .run(&["status", "--porcelain", "--untracked-files=all"], None)?
            .lines()
            .filter(|l| !l.trim().is_empty())
            .map(str::to_owned)
            .collect();
        Ok(Self { base, status })
    }

    /// As the worktree row stores it (`worktree.user_base_json`): the base the way the
    /// snapshot row stores one, and the status lines.
    pub(crate) fn to_json(&self) -> String {
        serde_json::json!({
            "head": self.base.head,
            "branch": self.base.branch,
            "refs": self.base.refs,
            "stash": self.base.stash,
            "status": self.status,
        })
        .to_string()
    }

    /// Reads [`UserCheckout::to_json`] back.
    ///
    /// # Errors
    ///
    /// It is not what [`UserCheckout::to_json`] writes.
    pub(crate) fn from_json(text: &str) -> Result<Self, String> {
        let value: serde_json::Value =
            serde_json::from_str(text).map_err(|e| format!("the user's checkout: {e}"))?;
        Ok(Self {
            base: Base {
                head: field(&value, "head")?,
                branch: field(&value, "branch")?,
                refs: field(&value, "refs")?,
                stash: field(&value, "stash")?,
            },
            status: field(&value, "status")?,
        })
    }
}

/// Field `name` of the stored capture `value`.
fn field<T: serde::de::DeserializeOwned>(
    value: &serde_json::Value,
    name: &str,
) -> Result<T, String> {
    serde_json::from_value(value[name].clone())
        .map_err(|e| format!("the user's checkout, `{name}`: {e}"))
}

/// What [`compare_checkout`] found.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct CheckoutDiff {
    /// What sends the item to Needs you (`guard_checkout`), one line each.
    pub(crate) refusals: Vec<String>,
    /// What is only reported: status lines that appeared or went.
    pub(crate) warnings: Vec<String>,
}

/// Compares the user's checkout `after` with `before` (claim) by E21. `descends(old, new)`
/// answers whether commit `new` descends from `old` (`git merge-base --is-ancestor old new`).
pub(crate) fn compare_checkout(
    before: &UserCheckout,
    after: &UserCheckout,
    descends: impl Fn(&str, &str) -> bool,
) -> CheckoutDiff {
    // Mutant (`mutant_loop_strict_checkout_guard`): any difference is a refusal, as in E4
    // before E21.
    let strict = cfg!(mutant_loop_strict_checkout_guard);
    let descends = |old: &str, new: &str| !strict && descends(old, new);
    let mut diff = CheckoutDiff::default();
    let name = |branch: &Option<String>| {
        branch.as_deref().map_or_else(
            || "a detached HEAD".to_owned(),
            |b| b.strip_prefix("refs/heads/").unwrap_or(b).to_owned(),
        )
    };
    match (&before.base.branch, &after.base.branch) {
        (Some(was), Some(is)) if was == is => {}
        (None, None) => {
            if before.base.head != after.base.head && !descends(&before.base.head, &after.base.head)
            {
                diff.refusals.push(format!(
                    "the detached HEAD moved from {} to {}, which does not descend from it",
                    before.base.head, after.base.head
                ));
            }
        }
        (was, is) => {
            diff.refusals
                .push(format!("HEAD switched from {} to {}", name(was), name(is)));
        }
    }
    for (reference, old) in &before.base.refs {
        if !compared(reference) {
            continue;
        }
        match after.base.refs.get(reference) {
            None => diff.refusals.push(format!("{reference} was deleted")),
            Some(new) if new == old => {}
            Some(new) if descends(old, new) => {}
            Some(new) => diff.refusals.push(format!(
                "{reference} moved from {old} to {new}, which does not descend from it"
            )),
        }
    }
    if strict {
        for reference in after.base.refs.keys() {
            if compared(reference) && !before.base.refs.contains_key(reference) {
                diff.refusals.push(format!("{reference} was created"));
            }
        }
    }
    let was: BTreeSet<&String> = before.status.iter().collect();
    let is: BTreeSet<&String> = after.status.iter().collect();
    for line in is.difference(&was) {
        diff.warnings.push(format!("new status line `{line}`"));
    }
    for line in was.difference(&is) {
        diff.warnings.push(format!("status line `{line}` went"));
    }
    if strict {
        diff.refusals.append(&mut diff.warnings);
    }
    diff
}

/// Captures the user's checkout at `user` again and compares it with `before`.
///
/// # Errors
///
/// The checkout cannot be captured.
pub(crate) fn check(user: &Path, before: &UserCheckout) -> Result<CheckoutDiff, bosun_git::Error> {
    let after = UserCheckout::capture(user)?;
    let git = Git::new(user);
    Ok(compare_checkout(before, &after, |old, new| {
        // What cannot be answered (an object that is not a commit, say) is not a descendant.
        git.test(&["merge-base", "--is-ancestor", old, new])
            .unwrap_or(false)
    }))
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::*;

    /// A checkout on `branch` (`None`: detached) at `head`, with `refs` and `status`.
    fn checkout(
        branch: Option<&str>,
        head: &str,
        refs: &[(&str, &str)],
        status: &[&str],
    ) -> UserCheckout {
        UserCheckout {
            base: Base {
                head: head.to_owned(),
                branch: branch.map(|b| format!("refs/heads/{b}")),
                refs: refs
                    .iter()
                    .map(|(r, o)| ((*r).to_owned(), (*o).to_owned()))
                    .collect::<BTreeMap<_, _>>(),
                stash: Vec::new(),
            },
            status: status.iter().map(|s| (*s).to_owned()).collect(),
        }
    }

    /// History: `a` is the parent of `b`; `c` is on another line.
    fn descends(old: &str, new: &str) -> bool {
        old == new || (old == "a" && new == "b")
    }

    #[test]
    fn an_unchanged_checkout_differs_in_nothing() {
        let before = checkout(Some("main"), "a", &[("refs/heads/main", "a")], &["?? x"]);
        assert_eq!(
            compare_checkout(&before, &before.clone(), descends),
            CheckoutDiff::default()
        );
    }

    #[test]
    fn a_switched_or_detached_head_is_refused() {
        let before = checkout(Some("main"), "a", &[("refs/heads/main", "a")], &[]);
        let switched = checkout(
            Some("escaped"),
            "a",
            &[("refs/heads/main", "a"), ("refs/heads/escaped", "a")],
            &[],
        );
        let diff = compare_checkout(&before, &switched, descends);
        assert_eq!(diff.refusals, ["HEAD switched from main to escaped"]);
        let detached = checkout(None, "a", &[("refs/heads/main", "a")], &[]);
        let diff = compare_checkout(&before, &detached, descends);
        assert_eq!(
            diff.refusals,
            ["HEAD switched from main to a detached HEAD"]
        );
    }

    #[test]
    fn a_deleted_or_rewound_ref_is_refused() {
        let before = checkout(
            Some("main"),
            "b",
            &[("refs/heads/main", "b"), ("refs/heads/old", "a")],
            &[],
        );
        let after = checkout(Some("main"), "a", &[("refs/heads/main", "a")], &[]);
        let diff = compare_checkout(&before, &after, descends);
        assert_eq!(diff.refusals.len(), 2, "{diff:?}");
        assert!(diff.refusals[0].starts_with("refs/heads/main moved from b to a"));
        assert_eq!(diff.refusals[1], "refs/heads/old was deleted");
        let elsewhere = checkout(
            Some("main"),
            "c",
            &[("refs/heads/main", "c"), ("refs/heads/old", "a")],
            &[],
        );
        assert_eq!(
            compare_checkout(&before, &elsewhere, descends)
                .refusals
                .len(),
            1
        );
    }

    #[test]
    fn the_users_commits_new_refs_and_the_stash_are_tolerated() {
        let before = checkout(
            Some("main"),
            "a",
            &[("refs/heads/main", "a"), ("refs/stash", "s1")],
            &[],
        );
        let after = checkout(
            Some("main"),
            "b",
            &[
                ("refs/heads/main", "b"),
                ("refs/heads/topic", "c"),
                ("refs/stash", "s2"),
            ],
            &[],
        );
        assert_eq!(
            compare_checkout(&before, &after, descends),
            CheckoutDiff::default()
        );
        let dropped = checkout(Some("main"), "a", &[("refs/heads/main", "a")], &[]);
        assert_eq!(
            compare_checkout(&before, &dropped, descends),
            CheckoutDiff::default()
        );
    }

    #[test]
    fn only_heads_and_tags_are_compared() {
        let before = checkout(
            Some("main"),
            "a",
            &[
                ("refs/heads/main", "a"),
                ("refs/heads/feature", "a"),
                ("refs/tags/v1", "a"),
                ("refs/remotes/origin/x", "a"),
                ("refs/remotes/origin/y", "b"),
                ("refs/notes/commits", "a"),
                ("refs/stash", "s1"),
            ],
            &[],
        );
        // A pruned and a force-fetched remote-tracking ref, rewritten notes, another stash.
        let elsewhere = checkout(
            Some("main"),
            "a",
            &[
                ("refs/heads/main", "a"),
                ("refs/heads/feature", "a"),
                ("refs/tags/v1", "a"),
                ("refs/remotes/origin/y", "c"),
                ("refs/notes/commits", "c"),
                ("refs/stash", "s2"),
            ],
            &[],
        );
        assert_eq!(
            compare_checkout(&before, &elsewhere, descends),
            CheckoutDiff::default()
        );
        let with = |changes: &[(&str, Option<&str>)]| {
            let mut after = before.clone();
            for (reference, value) in changes {
                match value {
                    Some(v) => after
                        .base
                        .refs
                        .insert((*reference).to_owned(), (*v).to_owned()),
                    None => after.base.refs.remove(*reference),
                };
            }
            compare_checkout(&before, &after, descends).refusals
        };
        assert_eq!(
            with(&[("refs/heads/feature", None)]),
            ["refs/heads/feature was deleted"]
        );
        assert_eq!(
            with(&[("refs/tags/v1", None)]),
            ["refs/tags/v1 was deleted"]
        );
        assert_eq!(
            with(&[("refs/heads/feature", Some("c"))]),
            ["refs/heads/feature moved from a to c, which does not descend from it"]
        );
        assert_eq!(
            with(&[("refs/tags/v1", Some("c"))]),
            ["refs/tags/v1 moved from a to c, which does not descend from it"]
        );
        assert_eq!(with(&[("refs/stash", Some("s3"))]), Vec::<String>::new());
    }

    #[test]
    fn changed_status_lines_are_warnings_not_refusals() {
        let before = checkout(
            Some("main"),
            "a",
            &[("refs/heads/main", "a")],
            &[" M kept.txt", "?? gone.txt"],
        );
        let after = checkout(
            Some("main"),
            "a",
            &[("refs/heads/main", "a")],
            &[" M kept.txt", "?? notes.txt"],
        );
        let diff = compare_checkout(&before, &after, descends);
        assert_eq!(diff.refusals, Vec::<String>::new());
        assert_eq!(
            diff.warnings,
            [
                "new status line `?? notes.txt`",
                "status line `?? gone.txt` went"
            ]
        );
    }

    #[test]
    fn a_detached_head_that_moves_forward_is_tolerated_and_one_that_jumps_is_not() {
        let before = checkout(None, "a", &[], &[]);
        assert_eq!(
            compare_checkout(&before, &checkout(None, "b", &[], &[]), descends),
            CheckoutDiff::default()
        );
        assert_eq!(
            compare_checkout(&before, &checkout(None, "c", &[], &[]), descends)
                .refusals
                .len(),
            1
        );
    }

    #[test]
    fn the_capture_reads_back_from_its_json() {
        let before = checkout(
            Some("main"),
            "a",
            &[("refs/heads/main", "a"), ("refs/stash", "s")],
            &[" M x"],
        );
        assert_eq!(UserCheckout::from_json(&before.to_json()).unwrap(), before);
        let detached = checkout(None, "a", &[], &[]);
        assert_eq!(
            UserCheckout::from_json(&detached.to_json()).unwrap(),
            detached
        );
    }
}
