//! Landing modes (#8 "Landing modes", "Squash"): the commit a verified item lands as.
//!
//! [`merge_commit`] builds it with plumbing (`git commit-tree`), never by checking anything out,
//! and moves no ref:
//!
//! - [`Method::Ff`]: the item's head itself;
//! - [`Method::Squash`]: one commit on the target's head with the item head's tree;
//! - [`Method::NoFf`]: a merge commit whose parents are the target's head and the item's head.
//!
//! The item was verified on its branch rebased onto the target, so its head must descend from
//! the target's head; otherwise the target moved since the verification and the commit is
//! refused ([`Error::StaleTarget`]). The commit's tree must equal the verified tree, else it is
//! refused ([`Error::TreeMismatch`]) before any object is written.
//!
//! A squash or a merge commit is new, so its message gets the `Item:`, `Run:` and `Session:`
//! trailers (one of each, replacing any the message had); a fast-forward lands the item's own
//! commits, which already carry them.
//!
//! The new commits carry the repository identity given as an [`Ident`] (E26 of #8), never the
//! process's global git identity. They are never signed, whatever the repository's `commit.gpgSign` says (decided
//! on #126): an unattended landing must not depend on a signing agent or a pinentry prompt.
//!
//! Every git command goes through [`Git::command`], so no repository hook runs.

use std::fmt;
use std::str::FromStr;

use crate::{Git, Ident, Trailers};

/// How an item lands on its target.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Method {
    /// `ff`: the target moves to the item's head.
    Ff,
    /// `squash`: one commit on the target's head with the item head's tree.
    Squash,
    /// `no-ff`: a merge commit of the target's head and the item's head.
    NoFf,
}

impl Method {
    /// Every method, in the order of the documentation.
    pub const ALL: [Self; 3] = [Self::Ff, Self::Squash, Self::NoFf];

    /// The configuration spelling: `ff`, `squash` or `no-ff`.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Ff => "ff",
            Self::Squash => "squash",
            Self::NoFf => "no-ff",
        }
    }
}

impl fmt::Display for Method {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for Method {
    type Err = UnknownMethod;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::ALL
            .into_iter()
            .find(|m| m.as_str() == s)
            .ok_or_else(|| UnknownMethod(s.to_owned()))
    }
}

/// A method name other than `ff`, `squash` and `no-ff`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnknownMethod(pub String);

impl fmt::Display for UnknownMethod {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "unknown landing method {:?} (ff, squash or no-ff)",
            self.0
        )
    }
}

impl std::error::Error for UnknownMethod {}

/// Why [`merge_commit`] refused or failed. Nothing was written when it refused.
#[derive(Debug)]
#[non_exhaustive]
pub enum Error {
    /// The commit's tree is not the verified tree.
    TreeMismatch {
        /// The tree that was verified.
        verified: String,
        /// The tree the commit would have.
        tree: String,
    },
    /// The item's head does not descend from the target's head: the verification was made on
    /// another target head.
    StaleTarget {
        /// The target's head.
        target: String,
        /// The item's head.
        item: String,
    },
    /// A git command failed.
    Git(crate::Error),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::TreeMismatch { verified, tree } => write!(
                f,
                "the commit's tree {tree} is not the verified tree {verified}: refused"
            ),
            Self::StaleTarget { target, item } => write!(
                f,
                "{item} does not descend from the target at {target}: \
                 not verified on this target head, refused"
            ),
            Self::Git(error) => error.fmt(f),
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

/// The commit `item_head` lands on `target_head` as with `method` (see the module
/// documentation); returns its sha. No ref moves and nothing is checked out.
///
/// `message` is the message of a squash or a merge commit, which gets `trailers`; a
/// fast-forward ignores both. A new commit has `ident` as its author and committer, whatever
/// the process's global git identity is (E26 of #8).
///
/// # Errors
///
/// [`Error::StaleTarget`] when `item_head` does not descend from `target_head`,
/// [`Error::TreeMismatch`] when the commit's tree would not be `verified_tree`, or git fails
/// (a name that is not a commit or a tree included).
#[allow(clippy::too_many_arguments)]
pub fn merge_commit(
    repo: &Git,
    method: Method,
    target_head: &str,
    item_head: &str,
    verified_tree: &str,
    message: &str,
    trailers: &Trailers,
    ident: &Ident,
) -> Result<String, Error> {
    let target = resolve(repo, target_head, "commit")?;
    let item = resolve(repo, item_head, "commit")?;
    let verified = resolve(repo, verified_tree, "tree")?;
    if !repo.test(&["merge-base", "--is-ancestor", &target, &item])? {
        return Err(Error::StaleTarget { target, item });
    }
    let tree = resolve(repo, &item, "tree")?;
    // Mutant (`mutant_git_squash_skip_tree_check`): any tree lands.
    if tree != verified && !cfg!(mutant_git_squash_skip_tree_check) {
        return Err(Error::TreeMismatch { verified, tree });
    }
    let parents: &[&str] = match method {
        Method::Ff => return Ok(item),
        Method::Squash => &[&target],
        Method::NoFf => &[&target, &item],
    };
    let message = with_trailers(repo, message, trailers)?;
    // Explicitly unsigned (decided on #126). `commit-tree` reads no `commit.gpgSign` today, so
    // the flag guards against a git that starts to.
    // Mutant (`mutant_git_merge_signed`): the commit is signed, as the repository asks.
    let sign = if cfg!(mutant_git_merge_signed) {
        "--gpg-sign"
    } else {
        "--no-gpg-sign"
    };
    let mut args = vec!["commit-tree", sign, &tree];
    for parent in parents {
        args.extend(["-p", parent]);
    }
    args.extend(["-F", "-"]);
    let mut command = repo.command();
    // Mutant (`mutant_loop_ambient_commit_identity`): the commit gets the process's global git
    // identity as its author and committer.
    if !cfg!(mutant_loop_ambient_commit_identity) {
        ident.author_and_committer(&mut command);
    }
    Ok(repo
        .run_in(command, &args, Some(&message))?
        .trim()
        .to_owned())
}

/// `message` with the `Item:`, `Run:` and `Session:` trailers, one of each.
fn with_trailers(repo: &Git, message: &str, trailers: &Trailers) -> Result<String, Error> {
    // Mutant (`mutant_git_merge_drop_trailers`): the message goes in as given.
    if cfg!(mutant_git_merge_drop_trailers) {
        return Ok(message.to_owned());
    }
    let item = format!("Item: {}", trailers.item);
    let run = format!("Run: {}", trailers.run);
    let session = format!("Session: {}", trailers.session);
    Ok(repo.run(
        &[
            "-c",
            "trailer.separators=:",
            "interpret-trailers",
            "--where",
            "end",
            "--if-exists",
            "replace",
            "--trailer",
            &item,
            "--trailer",
            &run,
            "--trailer",
            &session,
        ],
        Some(message),
    )?)
}

/// The full sha of the `kind` (`commit` or `tree`) `name` names.
fn resolve(repo: &Git, name: &str, kind: &str) -> Result<String, Error> {
    Ok(repo
        .run(
            &[
                "rev-parse",
                "--verify",
                "--end-of-options",
                &format!("{name}^{{{kind}}}"),
            ],
            None,
        )?
        .trim()
        .to_owned())
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};

    use tempfile::TempDir;

    use super::*;
    use crate::NO_HOOKS;

    /// Runs `git args` in `dir` through the invoker, with no user or system configuration and
    /// no signing; it must succeed. Returns the trimmed output.
    fn git(dir: &Path, args: &[&str]) -> String {
        let out = Git::new(dir)
            .command()
            .args(["-c", "commit.gpgSign=false"])
            .args(["-c", "user.name=Test"])
            .args(["-c", "user.email=test@example.invalid"])
            .args(args)
            .env("GIT_CONFIG_GLOBAL", NO_HOOKS)
            .env("GIT_CONFIG_NOSYSTEM", "1")
            // Not the trailer hooks of an agent session these tests may run in.
            .env_remove("GIT_CONFIG_COUNT")
            .env_remove(crate::MARKER_VAR)
            .output()
            .expect("git runs");
        assert!(
            out.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).trim().to_owned()
    }

    /// Writes `file` and commits it with `message`. Returns the sha.
    fn commit(dir: &Path, file: &str, text: &str, message: &str) -> String {
        std::fs::write(dir.join(file), text).unwrap();
        git(dir, &["add", "--all"]);
        git(dir, &["commit", "--quiet", "-m", message]);
        git(dir, &["rev-parse", "HEAD"])
    }

    fn trailers() -> Trailers {
        Trailers {
            item: "owner/repo#126".to_owned(),
            run: "run-1".to_owned(),
            session: "session-1".to_owned(),
        }
    }

    /// The identity the daemon's commits carry: not the repository's own `Test`.
    fn ident() -> Ident {
        Ident {
            name: "Repository Identity".to_owned(),
            email: "identity@example.invalid".to_owned(),
        }
    }

    /// The trailers of [`trailers`] as the hook writes them.
    const TRAILERS: &str = "Item: owner/repo#126\nRun: run-1\nSession: session-1";

    /// A repository on `main` with one commit, and `item` branched from it with two commits
    /// carrying the trailers. Returns the directory, the repository, the target's head and
    /// the item's head. The code under test reads the user's configuration, so the repository
    /// names its own committer and turns signing off.
    fn setup() -> (TempDir, PathBuf, String, String) {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path().join("repo");
        std::fs::create_dir(&repo).unwrap();
        git(&repo, &["init", "--quiet", "--initial-branch=main"]);
        git(&repo, &["config", "user.name", "Test"]);
        git(&repo, &["config", "user.email", "test@example.invalid"]);
        git(&repo, &["config", "commit.gpgSign", "false"]);
        let target = commit(&repo, "a.txt", "first\n", "first");
        git(&repo, &["checkout", "--quiet", "-b", "item"]);
        commit(&repo, "b.txt", "b\n", &format!("add b\n\n{TRAILERS}"));
        let item = commit(&repo, "c.txt", "c\n", &format!("add c\n\n{TRAILERS}"));
        (dir, repo, target, item)
    }

    fn head(dir: &Path, rev: &str) -> String {
        git(dir, &["rev-parse", rev])
    }

    /// The `Item:`, `Run:` and `Session:` lines of `body`, in order.
    fn trailer_lines(body: &str) -> Vec<&str> {
        body.lines()
            .filter(|l| {
                ["Item: ", "Run: ", "Session: "]
                    .iter()
                    .any(|k| l.starts_with(k))
            })
            .collect()
    }

    #[test]
    fn each_method_ends_with_the_verified_tree_and_the_trailers() {
        let (_dir, repo, target, item) = setup();
        let verified = head(&repo, &format!("{item}^{{tree}}"));
        let refs = git(&repo, &["for-each-ref"]);
        let expected: Vec<&str> = TRAILERS.lines().collect();

        for method in Method::ALL {
            assert_eq!(method.as_str().parse::<Method>().unwrap(), method);
            let landed = merge_commit(
                &Git::new(&repo),
                method,
                &target,
                &item,
                &verified,
                "Land the item\n\nWhat it does.\n",
                &trailers(),
                &ident(),
            )
            .unwrap_or_else(|e| panic!("{method}: {e}"));
            assert_eq!(
                head(&repo, &format!("{landed}^{{tree}}")),
                verified,
                "{method}"
            );
            let parents: Vec<String> = git(&repo, &["rev-list", "--parents", "-n1", &landed])
                .split(' ')
                .skip(1)
                .map(str::to_owned)
                .collect();
            match method {
                Method::Ff => {
                    assert_eq!(landed, item);
                    for sha in git(&repo, &["rev-list", &format!("{target}..{landed}")]).lines() {
                        let body = git(&repo, &["log", "-1", "--format=%B", sha]);
                        assert_eq!(trailer_lines(&body), expected, "ff: {sha}");
                    }
                }
                Method::Squash => assert_eq!(parents, std::slice::from_ref(&target)),
                Method::NoFf => assert_eq!(parents, [target.clone(), item.clone()]),
            }
            if method != Method::Ff {
                let body = git(&repo, &["log", "-1", "--format=%B", &landed]);
                assert!(
                    body.starts_with("Land the item\n\nWhat it does.\n"),
                    "{body}"
                );
                assert_eq!(trailer_lines(&body), expected, "{method}: {body}");
                for format in ["%an <%ae>", "%cn <%ce>"] {
                    assert_eq!(
                        git(
                            &repo,
                            &["log", "-1", &format!("--format={format}"), &landed]
                        ),
                        "Repository Identity <identity@example.invalid>",
                        "{method}: {format}"
                    );
                }
            }
        }
        // No ref moved and nothing was checked out.
        assert_eq!(git(&repo, &["for-each-ref"]), refs);
        assert_eq!(git(&repo, &["status", "--porcelain"]), "");
    }

    #[test]
    fn a_squash_that_differs_from_the_verified_tree_is_refused() {
        let (_dir, repo, target, item) = setup();
        let verified = head(&repo, &format!("{item}^{{tree}}"));
        // A commit made after the verification.
        let later = commit(
            &repo,
            "d.txt",
            "unverified\n",
            &format!("add d\n\n{TRAILERS}"),
        );
        let tree = head(&repo, &format!("{later}^{{tree}}"));
        let objects = git(&repo, &["count-objects"]);

        let error = merge_commit(
            &Git::new(&repo),
            Method::Squash,
            &target,
            &later,
            &verified,
            "Land the item\n",
            &trailers(),
            &ident(),
        )
        .unwrap_err();
        let Error::TreeMismatch {
            verified: v,
            tree: t,
        } = &error
        else {
            panic!("a squash of an unverified tree was answered {error:?}");
        };
        assert_eq!((v, t), (&verified, &tree));
        assert!(error.to_string().contains("refused"), "{error}");
        assert_eq!(git(&repo, &["count-objects"]), objects, "nothing written");

        // The target the verification was not made on is refused too.
        git(&repo, &["checkout", "--quiet", "main"]);
        let moved = commit(&repo, "m.txt", "main\n", "main moved");
        let stale = merge_commit(
            &Git::new(&repo),
            Method::Squash,
            &moved,
            &item,
            &verified,
            "Land the item\n",
            &trailers(),
            &ident(),
        )
        .unwrap_err();
        assert!(matches!(stale, Error::StaleTarget { .. }), "{stale:?}");
    }

    #[test]
    fn a_repository_that_signs_its_commits_still_lands_unsigned() {
        let (_dir, repo, target, item) = setup();
        let verified = head(&repo, &format!("{item}^{{tree}}"));
        // Signing is on, and the program that would sign does not exist.
        git(&repo, &["config", "commit.gpgSign", "true"]);
        git(
            &repo,
            &["config", "gpg.program", "bosun-no-such-signing-program"],
        );

        for method in [Method::Squash, Method::NoFf] {
            let landed = merge_commit(
                &Git::new(&repo),
                method,
                &target,
                &item,
                &verified,
                "Land the item\n",
                &trailers(),
                &ident(),
            )
            .unwrap_or_else(|e| panic!("{method}: {e}"));
            let raw = git(&repo, &["cat-file", "commit", &landed]);
            assert!(!raw.contains("gpgsig"), "{method}: {raw}");
        }
    }
}
