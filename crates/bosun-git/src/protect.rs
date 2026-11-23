//! Protected paths (#7 "Protected paths", #59): if the item's commits touch a path the
//! repository's permission policy protects, the verdict is not run and the item goes to Needs
//! you with the diff stat, unless the item's snapshot lists the path in `allow-protected`.

use std::collections::BTreeSet;

use crate::refusal::{Refusal, covers};
use crate::{Base, Error, Git};

/// The protected paths the item's commits (`base..HEAD`) touched and `allowed` does not list:
/// one [`Refusal::Protected`] with the paths and their diff stat, or nothing.
///
/// - `protects` says whether a repository-relative, `/`-separated path is protected: the
///   repository's permission policy (`bosun_config::Policy::protects`), which always protects
///   `.gitignore`.
/// - `allowed` is the item snapshot's `allow-protected` list: repository-relative entries,
///   each covering the path it names and everything under it. Only the user can grant it,
///   since it is part of the snapshot.
///
/// Every path any commit since the base touched counts, not only the net diff: a change made
/// and reverted is still an edit of a path the item was not allowed to edit. With
/// `--no-renames` a rename touches both of its paths.
///
/// # Errors
///
/// A git command fails.
pub fn protected(
    git: &Git,
    base: &Base,
    protects: impl Fn(&str) -> bool,
    allowed: &[String],
) -> Result<Vec<Refusal>, Error> {
    if cfg!(mutant_git_skip_protected_paths) {
        return Ok(Vec::new());
    }
    let range = format!("{}..HEAD", base.head);
    let log = git.run(
        &[
            "log",
            "-z",
            "--format=",
            "--name-only",
            "--no-renames",
            "--diff-merges=first-parent",
            &range,
            "--",
        ],
        None,
    )?;
    let paths: BTreeSet<String> = log
        .split('\0')
        .map(|p| p.trim_start_matches('\n'))
        .filter(|p| !p.is_empty())
        .filter(|p| protects(p) && !allowed.iter().any(|entry| covers(entry, p)))
        .map(str::to_owned)
        .collect();
    if paths.is_empty() {
        return Ok(Vec::new());
    }
    let pathspecs: Vec<String> = paths.iter().map(|p| format!(":(literal){p}")).collect();
    let mut args = vec![
        "diff",
        "--stat=200",
        "--no-color",
        "--no-renames",
        &base.head,
        "HEAD",
        "--",
    ];
    args.extend(pathspecs.iter().map(String::as_str));
    let mut stat = git.run(&args, None)?;
    if stat.trim().is_empty() {
        // Changed and changed back: the net diff is empty, so show the commits that did it.
        let mut args = vec![
            "log",
            "--stat=200",
            "--no-color",
            "--no-renames",
            "--format=%h %s",
            &range,
            "--",
        ];
        args.extend(pathspecs.iter().map(String::as_str));
        stat = git.run(&args, None)?;
    }
    Ok(vec![Refusal::Protected {
        paths: paths.into_iter().collect(),
        stat,
    }])
}
