//! Landing modes (#8, sections "Landing modes" and "Squash"): the commit an item lands as,
//! built with plumbing so that no checkout moves.
//!
//! [`merge_commit`] returns the sha of the commit the target branch should point at, for one of
//! three methods:
//!
//! - [`Method::FastForward`]: the item head itself, which must descend from the target head.
//!   Its commits already carry the trailers, so nothing is written;
//! - [`Method::Squash`]: one commit on the target head with the item head's tree;
//! - [`Method::NoFastForward`]: a merge commit whose parents are the target head and the item
//!   head, with the tree git merges them to.
//!
//! Whatever the method, the commit's tree must equal the tree the verification ran on, or the
//! commit is refused with [`Error::TreeMismatch`]. A commit built here keeps the `Item:`, `Run:`
//! and `Session:` trailers of the item's commits: the ones its message lacks are added.
//!
//! The commit is created, not published. Moving the target branch is the caller's `update-ref`
//! with the target head as the old value, which is what keeps a target that moved in the
//! meantime from being overwritten. All git runs through [`Git::command`], hooks disabled.

use std::fmt;

use crate::Git;
use crate::message::trailers;

/// The trailers a landed commit keeps, as the trailer hook writes them (#7, D6).
const TRAILER_KEYS: [&str; 3] = ["Item", "Run", "Session"];

/// The methods an item can land by.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum Method {
    /// `ff`: the target moves to the item head, which must descend from it.
    FastForward,
    /// `squash`: one commit on the target with the item's tree.
    Squash,
    /// `no-ff`: a merge commit with both heads as parents.
    NoFastForward,
}

impl Method {
    /// The method named `name` as the configuration spells it (`ff`, `squash` or `no-ff`).
    #[must_use]
    pub fn parse(name: &str) -> Option<Self> {
        match name {
            "ff" => Some(Self::FastForward),
            "squash" => Some(Self::Squash),
            "no-ff" => Some(Self::NoFastForward),
            _ => None,
        }
    }

    /// The name the configuration spells the method with.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::FastForward => "ff",
            Self::Squash => "squash",
            Self::NoFastForward => "no-ff",
        }
    }
}

/// What can go wrong building a landing commit.
#[derive(Debug)]
#[non_exhaustive]
pub enum Error {
    /// A git command failed (a merge that conflicts, for example).
    Git(crate::Error),
    /// A revision does not name a commit or a tree.
    Unknown {
        /// The revision as given.
        rev: String,
    },
    /// The item head does not descend from the target head, so a fast-forward cannot take the
    /// target there.
    NotDescendant {
        /// The target head.
        target: String,
        /// The item head.
        item: String,
    },
    /// The commit would have a tree other than the one the verification ran on.
    TreeMismatch {
        /// The verified tree.
        expected: String,
        /// The tree the commit would have.
        found: String,
    },
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Git(error) => write!(f, "{error}"),
            Self::Unknown { rev } => write!(f, "{rev} does not name a commit or a tree"),
            Self::NotDescendant { target, item } => write!(
                f,
                "{item} does not descend from {target}; the target is not moved"
            ),
            Self::TreeMismatch { expected, found } => write!(
                f,
                "the landing commit would have tree {found}, not the verified {expected}; it is refused"
            ),
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Git(error) => Some(error),
            _ => None,
        }
    }
}

impl From<crate::Error> for Error {
    fn from(error: crate::Error) -> Self {
        Self::Git(error)
    }
}

/// Builds the commit `method` lands an item head as, on the target head `target_head`, and
/// returns its sha.
///
/// `verified_tree` is the tree the verification ran on; the commit must have exactly that tree.
/// `message` is the commit message for `squash` and `no-ff`; the trailers of the item's commits
/// are added to it where it lacks them. `ff` ignores `message`.
///
/// # Errors
///
/// A revision names nothing, the item does not descend from the target for `ff`, the commit
/// would have another tree than `verified_tree`, or a git command fails (a merge that
/// conflicts, for example).
pub fn merge_commit(
    git: &Git,
    method: Method,
    target_head: &str,
    item_head: &str,
    verified_tree: &str,
    message: &str,
) -> Result<String, Error> {
    let target = resolve(git, target_head, "commit")?;
    let item = resolve(git, item_head, "commit")?;
    let expected = resolve(git, verified_tree, "tree")?;
    match method {
        Method::FastForward => {
            if !git.test(&["merge-base", "--is-ancestor", &target, &item])? {
                return Err(Error::NotDescendant { target, item });
            }
            check_tree(&item, &expected, git)?;
            Ok(item)
        }
        Method::Squash => {
            let tree = resolve(git, &item, "tree")?;
            // Mutant (`mutant_git_squash_skip_tree_check`): the squash is written without
            // comparing its tree with the verified one, so a tree that changed since the
            // verification lands unverified.
            if !cfg!(mutant_git_squash_skip_tree_check) {
                ensure_tree(&tree, &expected)?;
            }
            let message = with_trailers(git, &target, &item, message)?;
            commit(git, &[&target], &tree, &message)
        }
        Method::NoFastForward => {
            let tree = merged_tree(git, &target, &item)?;
            ensure_tree(&tree, &expected)?;
            let message = with_trailers(git, &target, &item, message)?;
            commit(git, &[&target, &item], &tree, &message)
        }
    }
}

/// The sha of `rev` as a commit (`kind` `commit`) or as a tree (`kind` `tree`).
fn resolve(git: &Git, rev: &str, kind: &str) -> Result<String, Error> {
    let spec = format!("{rev}^{{{kind}}}");
    git.optional(&["rev-parse", "--verify", "--quiet", &spec])?
        .ok_or_else(|| Error::Unknown {
            rev: rev.to_owned(),
        })
}

/// The tree `item` would have if it landed as the tip of `target`'s history: the tree `item`
/// itself has for a squash, git's merge of the two for a merge commit.
fn merged_tree(git: &Git, target: &str, item: &str) -> Result<String, Error> {
    let out = git.run(&["merge-tree", "--write-tree", target, item], None)?;
    let tree = out.lines().next().unwrap_or_default().trim();
    if tree.is_empty() {
        return Err(git
            .unexpected(format!(
                "merge-tree printed no tree for {target} and {item}"
            ))
            .into());
    }
    Ok(tree.to_owned())
}

/// Refuses a commit whose tree is not `expected`.
fn ensure_tree(found: &str, expected: &str) -> Result<(), Error> {
    if found == expected {
        Ok(())
    } else {
        Err(Error::TreeMismatch {
            expected: expected.to_owned(),
            found: found.to_owned(),
        })
    }
}

/// Refuses `commit` (an existing commit, for `ff`) unless its tree is `expected`.
fn check_tree(commit: &str, expected: &str, git: &Git) -> Result<(), Error> {
    let tree = git.run(
        &["rev-parse", "--verify", &format!("{commit}^{{tree}}")],
        None,
    )?;
    ensure_tree(tree.trim(), expected)
}

/// `message` with each of the `Item:`, `Run:` and `Session:` trailers that it lacks added, with
/// the value the item's commits carry (the newest commit's value wins). Trailers the message
/// already has are kept as they are.
fn with_trailers(git: &Git, target: &str, item: &str, message: &str) -> Result<String, Error> {
    let mut message = message.to_owned();
    // Mutant (`mutant_git_merge_drop_trailers`): the message is written as given, without the
    // trailers of the item's commits, so the landed commit loses its attribution.
    if cfg!(mutant_git_merge_drop_trailers) {
        return Ok(message);
    }
    let range = format!("{target}..{item}");
    // Oldest first, so that a later commit's value replaces an earlier one.
    let log = git.run(&["log", "--reverse", "--format=%B%x00", &range], None)?;
    let mut found: Vec<(&str, String)> = Vec::new();
    for body in log.split('\0') {
        for (key, value) in trailers(body) {
            let Some(&key) = TRAILER_KEYS.iter().find(|k| k.eq_ignore_ascii_case(&key)) else {
                continue;
            };
            match found.iter_mut().find(|(k, _)| *k == key) {
                Some(slot) => slot.1 = value,
                None => found.push((key, value)),
            }
        }
    }
    let present: Vec<String> = trailers(&message)
        .into_iter()
        .map(|(key, _)| key.to_ascii_lowercase())
        .collect();
    let mut command = vec!["interpret-trailers".to_owned()];
    for (key, value) in &found {
        if !present.contains(&key.to_ascii_lowercase()) {
            command.push("--trailer".to_owned());
            command.push(format!("{key}: {value}"));
        }
    }
    if command.len() == 1 {
        return Ok(message);
    }
    if !message.ends_with('\n') {
        message.push('\n');
    }
    let command: Vec<&str> = command.iter().map(String::as_str).collect();
    git.run(&command, Some(&message)).map_err(Error::from)
}

/// Writes a commit with `tree`, the given `parents` and `message`, and returns its sha.
fn commit(git: &Git, parents: &[&str], tree: &str, message: &str) -> Result<String, Error> {
    let mut args = vec!["commit-tree", tree];
    for parent in parents {
        args.push("-p");
        args.push(*parent);
    }
    args.extend(["-F", "-"]);
    let mut message = message.to_owned();
    if !message.ends_with('\n') {
        message.push('\n');
    }
    Ok(git.run(&args, Some(&message))?.trim().to_owned())
}

#[cfg(test)]
mod tests {
    use std::fs;

    use tempfile::TempDir;

    use super::*;

    /// A repository on `main` with one commit. Its own configuration carries the identity and
    /// turns signing off, so every commit the tests make needs nothing else.
    fn repository() -> TempDir {
        let dir = tempfile::tempdir().unwrap();
        let git = Git::new(dir.path());
        git.run(&["init", "--quiet", "--initial-branch=main"], None)
            .unwrap();
        for (key, value) in [
            ("user.name", "Test"),
            ("user.email", "test@example.invalid"),
            ("commit.gpgSign", "false"),
        ] {
            git.run(&["config", key, value], None).unwrap();
        }
        write_commit(&git, "a.txt", "base\n", "Initial commit");
        dir
    }

    /// Writes `file` with `content`, commits everything with `message`, and returns the sha.
    fn write_commit(git: &Git, file: &str, content: &str, message: &str) -> String {
        fs::write(git.repo().join(file), content).unwrap();
        git.run(&["add", "--all"], None).unwrap();
        git.run(&["commit", "--quiet", "-m", message], None)
            .unwrap();
        rev(git, "HEAD")
    }

    /// The sha `rev` names.
    fn rev(git: &Git, rev: &str) -> String {
        git.run(&["rev-parse", "--verify", rev], None)
            .unwrap()
            .trim()
            .to_owned()
    }

    /// The tree `rev` has.
    fn tree(git: &Git, rev_name: &str) -> String {
        rev(git, &format!("{rev_name}^{{tree}}"))
    }

    /// The message of commit `rev`.
    fn message(git: &Git, rev: &str) -> String {
        git.run(&["log", "-1", "--format=%B", rev], None).unwrap()
    }

    /// The three trailers an item's commits carry in these tests.
    const TRAILERS: &str = "Item: owner/repo#8\nRun: r1\nSession: s1\n";

    /// Asserts that `text` has each of the three trailers as a line of its own.
    fn assert_trailers(text: &str) {
        for line in TRAILERS.lines() {
            assert!(
                text.lines().any(|l| l == line),
                "{line:?} is missing from {text:?}"
            );
        }
    }

    /// A feature branch from `main`'s first commit with one commit that carries the trailers,
    /// and `main` moved on by one commit when `main_moves` is set. Leaves `feature` checked
    /// out and returns the target head `main` ends at.
    fn feature(git: &Git, main_moves: bool) -> String {
        git.run(&["checkout", "--quiet", "-b", "feature"], None)
            .unwrap();
        write_commit(git, "b.txt", "feature\n", &format!("Feature\n\n{TRAILERS}"));
        git.run(&["checkout", "--quiet", "main"], None).unwrap();
        let target = if main_moves {
            write_commit(git, "c.txt", "main\n", "Main moves on")
        } else {
            rev(git, "main")
        };
        git.run(&["checkout", "--quiet", "feature"], None).unwrap();
        target
    }

    #[test]
    fn a_squash_that_differs_from_the_verified_tree_is_refused() {
        let repo = repository();
        let git = Git::new(repo.path());
        let target = feature(&git, true);
        let item = rev(&git, "feature");
        // The verification ran on the target's tree, which is not what the squash would carry.
        let verified = tree(&git, &target);

        let error = merge_commit(
            &git,
            Method::Squash,
            &target,
            &item,
            &verified,
            "Squash of feature\n",
        )
        .unwrap_err();

        assert!(
            matches!(error, Error::TreeMismatch { .. }),
            "a squash with another tree was not refused as a tree mismatch: {error}"
        );
    }

    #[test]
    fn each_method_ends_with_the_verified_tree_and_the_trailers() {
        let repo = repository();
        let git = Git::new(repo.path());

        // Fast-forward: `main` has not moved, so the item head is the landing commit.
        let target = feature(&git, false);
        let item = rev(&git, "feature");
        let landed = merge_commit(
            &git,
            Method::FastForward,
            &target,
            &item,
            &tree(&git, &item),
            "ignored\n",
        )
        .unwrap();
        assert_eq!(landed, item);
        assert_eq!(tree(&git, &landed), tree(&git, &item));
        assert_trailers(&message(&git, &landed));

        // Squash and merge commit: `main` has moved on.
        git.run(&["checkout", "--quiet", "main"], None).unwrap();
        let target = rev(&git, "main");
        git.run(&["checkout", "--quiet", "feature"], None).unwrap();

        let squashed = merge_commit(
            &git,
            Method::Squash,
            &target,
            &item,
            &tree(&git, &item),
            "Squash of feature\n",
        )
        .unwrap();
        assert_eq!(tree(&git, &squashed), tree(&git, &item));
        assert_eq!(rev(&git, &format!("{squashed}^")), target);
        assert_trailers(&message(&git, &squashed));

        let merged_tree = git
            .run(&["merge-tree", "--write-tree", &target, &item], None)
            .unwrap();
        let merged_tree = merged_tree.lines().next().unwrap().trim().to_owned();
        let merged = merge_commit(
            &git,
            Method::NoFastForward,
            &target,
            &item,
            &merged_tree,
            "Merge feature\n",
        )
        .unwrap();
        assert_eq!(tree(&git, &merged), merged_tree);
        assert_eq!(rev(&git, &format!("{merged}^1")), target);
        assert_eq!(rev(&git, &format!("{merged}^2")), item);
        assert_trailers(&message(&git, &merged));
    }

    #[test]
    fn a_fast_forward_to_a_head_that_does_not_descend_from_the_target_is_refused() {
        let repo = repository();
        let git = Git::new(repo.path());
        let target = feature(&git, true);
        let item = rev(&git, "feature");

        let error = merge_commit(
            &git,
            Method::FastForward,
            &target,
            &item,
            &tree(&git, &item),
            "",
        )
        .unwrap_err();

        assert!(
            matches!(error, Error::NotDescendant { .. }),
            "a fast-forward to a diverged head was not refused: {error}"
        );
    }

    #[test]
    fn the_method_names_are_the_configuration_spellings() {
        for method in [Method::FastForward, Method::Squash, Method::NoFastForward] {
            assert_eq!(Method::parse(method.name()), Some(method));
        }
        assert_eq!(Method::parse("rebase"), None);
    }
}
