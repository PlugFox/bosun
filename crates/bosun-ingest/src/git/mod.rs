//! The commit reader (#3, "Linking cost to commits"; #23): commits of a configured repository,
//! read through the `git` command line.
//!
//! [`scan`] reads the commits reachable from the repository's branches (local and
//! remote-tracking) that the previous scan had not reached, and the repository's origin URL and
//! default branch. It writes nothing, to the repository or anywhere else: the caller stores the
//! commits ([`bosun_core::Commit`]) and then keeps the returned [`Cursor`] for the next scan.
//! [`refresh`] does both for the daemon: it scans from the commits already stored and stores
//! what it found.
//!
//! # Why the command line
//!
//! `git` is the one implementation of the repository format that every configured repository is
//! already used with, so its view of refs, worktrees, packs, alternates, shallow clones and
//! repository extensions is the user's view, and it costs no dependency. The reader runs a
//! handful of short read-only commands per scan; a library would save the process spawns and
//! nothing that matters at this rate.
//!
//! # Safety
//!
//! Every command is read-only, and read commands run no hooks. As defence in depth each one goes
//! through the daemon's one invoker, [`bosun_git::Git`] (hooks and fsmonitor off, no terminal
//! prompt, no optional locks, so `git` does not refresh the index of a repository it only
//! reads), and the log is read with `--no-ext-diff` and `--no-textconv` (configuration could
//! otherwise name a program for both).
//!
//! # Incremental reads
//!
//! A [`Cursor`] holds the commits whose ancestry has already been read: the branch tips of the
//! last scan, or, after a restart, the commits already stored ([`Cursor::from_shas`]). A scan
//! reads the commits reachable from the current tips and from none of the cursor's commits.
//! When history is rewritten (an amend, a rebase, a force push), the rewritten commits are new
//! objects and are read; the commits they replaced are left to the store, which keeps them. A
//! cursor commit that no longer exists (rewritten, then pruned by `git gc`, which by default
//! waits two weeks) is dropped from the cursor, so the history only it covered is read again;
//! storing a commit again changes nothing, so the cost is one re-read. A cursor seeded from
//! every stored commit loses nothing that way.

mod refresh;
#[cfg(test)]
mod tests;

pub use bosun_git::Error;
pub use refresh::{RefreshError, refresh};

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use bosun_core::Commit;
use bosun_git::Git;

/// The refs whose commits are read: local and remote-tracking branches.
const REFS: [&str; 2] = ["refs/heads", "refs/remotes"];

/// Separates commits in the log output.
const RECORD: char = '\u{1e}';
/// Separates a trailer from the next one.
const TRAILER: char = '\u{1f}';
/// Separates a trailer's name from its value.
const KEY_VALUE: char = '\u{1d}';

/// The commits whose ancestry has been read. See the module documentation.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Cursor {
    shas: BTreeSet<String>,
}

impl Cursor {
    /// A cursor that has read nothing: the next scan reads every commit.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// A cursor at the given commits, such as every commit already stored for the repository.
    #[must_use]
    pub fn from_shas<I, S>(shas: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        Self {
            shas: shas.into_iter().map(Into::into).collect(),
        }
    }

    /// The commits of the cursor.
    #[must_use]
    pub fn shas(&self) -> &BTreeSet<String> {
        &self.shas
    }
}

/// What one scan of a repository found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Scan {
    /// The URL of `origin`, without any user name or password it carries; `None` when the
    /// repository has no `origin`.
    pub remote: Option<String>,
    /// The default branch: the one `origin/HEAD` names, otherwise a local `main` or `master`;
    /// `None` when there is neither.
    pub default_branch: Option<String>,
    /// The commits not read before, newest first.
    pub commits: Vec<Commit>,
    /// The cursor to scan from next time, once `commits` are stored.
    pub cursor: Cursor,
}

/// Reads the commits of the repository at `repo` that `cursor` has not reached, with its origin
/// URL and default branch.
///
/// # Errors
///
/// `git` cannot be run, `repo` is not a repository, or a command fails or prints something
/// unexpected. A cursor commit that no longer exists is not an error.
pub fn scan(repo: &Path, cursor: &Cursor) -> Result<Scan, Error> {
    let remote = optional(repo, &["config", "--get", "remote.origin.url"])?.map(|url| scrub(&url));
    let default_branch = default_branch(repo)?;

    // The tips first, then the log from exactly those tips: a commit made in between is read
    // by the next scan, never skipped.
    let mut args = vec!["for-each-ref", "--format=%(objectname) %(objecttype)"];
    args.extend(REFS);
    let mut tips = BTreeSet::new();
    for line in run(repo, &args, None)?.lines() {
        match line.split_once(' ') {
            Some((sha, "commit")) => {
                tips.insert(sha.to_owned());
            }
            // A ref to a tag or tree object has no history to read.
            Some(_) => {}
            None => return Err(output(repo, format!("a ref line {line:?}"))),
        }
    }

    let commits = if tips.is_empty() {
        Vec::new()
    } else {
        let mut revisions = String::new();
        for tip in &tips {
            revisions.push_str(tip);
            revisions.push('\n');
        }
        for seen in existing(repo, &cursor.shas)? {
            revisions.push('^');
            revisions.push_str(seen);
            revisions.push('\n');
        }
        // `%x1e`, `%x1f` and `%x1d` are `RECORD`, `TRAILER` and `KEY_VALUE`.
        let format = "--format=%x1e%H%x00%ct%x00%an <%ae>%x00\
                      %(trailers:only,unfold,separator=%x1f,key_value_separator=%x1d)%x00";
        let log = run(
            repo,
            &[
                "log",
                "--stdin",
                "--no-color",
                "--no-mailmap",
                "--encoding=UTF-8",
                "--numstat",
                "--find-renames",
                "--no-ext-diff",
                "--no-textconv",
                format,
            ],
            Some(&revisions),
        )?;
        parse_log(repo, &log)?
    };

    Ok(Scan {
        remote,
        default_branch,
        commits,
        cursor: Cursor { shas: tips },
    })
}

/// The commits of `shas` that still exist in the repository. `git log` refuses a missing
/// commit even when it is only excluded (`--ignore-missing` covers names, not objects), and a
/// rewrite followed by `git gc` leaves the cursor holding exactly such commits.
fn existing<'a>(repo: &Path, shas: &'a BTreeSet<String>) -> Result<Vec<&'a str>, Error> {
    if shas.is_empty() {
        return Ok(Vec::new());
    }
    let mut input = String::new();
    for sha in shas {
        input.push_str(sha);
        input.push('\n');
    }
    let found = run(
        repo,
        &["cat-file", "--batch-check=%(objectname) %(objecttype)"],
        Some(&input),
    )?;
    let commits: BTreeSet<&str> = found
        .lines()
        .filter_map(|line| line.strip_suffix(" commit"))
        .collect();
    Ok(shas
        .iter()
        .map(String::as_str)
        .filter(|sha| commits.contains(sha))
        .collect())
}

/// `origin/HEAD`'s branch, else a local `main` or `master`.
fn default_branch(repo: &Path) -> Result<Option<String>, Error> {
    if let Some(target) = optional(
        repo,
        &["symbolic-ref", "--quiet", "refs/remotes/origin/HEAD"],
    )? && let Some(branch) = target.strip_prefix("refs/remotes/origin/")
    {
        return Ok(Some(branch.to_owned()));
    }
    for branch in ["main", "master"] {
        let reference = format!("refs/heads/{branch}");
        if optional(repo, &["rev-parse", "--verify", "--quiet", &reference])?.is_some() {
            return Ok(Some(branch.to_owned()));
        }
    }
    Ok(None)
}

/// Drops the user name and password from `scheme://user:password@host/...`; any other URL is
/// kept as it is.
fn scrub(url: &str) -> String {
    let Some((scheme, rest)) = url.split_once("://") else {
        return url.to_owned();
    };
    let authority_end = rest.find('/').unwrap_or(rest.len());
    match rest[..authority_end].rfind('@') {
        Some(at) => format!("{scheme}://{}", &rest[at + 1..]),
        None => url.to_owned(),
    }
}

fn output(repo: &Path, what: String) -> Error {
    Git::new(repo).unexpected(what)
}

/// Runs `git args` in `repo` through the invoker, feeding it `stdin`; returns its standard
/// output.
fn run(repo: &Path, args: &[&str], stdin: Option<&str>) -> Result<String, Error> {
    Git::new(repo).run(args, stdin)
}

/// Runs a query that exits 1 with no output when there is nothing to report: its trimmed
/// output, or `None`.
fn optional(repo: &Path, args: &[&str]) -> Result<Option<String>, Error> {
    Git::new(repo).optional(args)
}

/// Parses the log printed with the format of [`scan`]: per commit, a record separator, the
/// header fields separated by NUL, then the `--numstat` lines.
fn parse_log(repo: &Path, log: &str) -> Result<Vec<Commit>, Error> {
    let mut commits = Vec::new();
    for record in log.split(RECORD).filter(|r| !r.trim().is_empty()) {
        let fields: Vec<&str> = record.splitn(5, '\0').collect();
        let [sha, ts, author, trailers, numstat] = fields[..] else {
            return Err(output(
                repo,
                format!("a log record with {} fields", fields.len()),
            ));
        };
        let seconds: i64 = ts
            .parse()
            .map_err(|_| output(repo, format!("a commit time {ts:?}")))?;
        let (lines_added, lines_removed) = count_lines(numstat);
        commits.push(Commit {
            sha: sha.to_owned(),
            ts_ms: seconds.saturating_mul(1000),
            author: author.to_owned(),
            lines_added,
            lines_removed,
            trailers: parse_trailers(trailers),
        });
    }
    Ok(commits)
}

/// Sums the `added<TAB>removed<TAB>path` lines; a binary file (`-`) counts no lines.
fn count_lines(numstat: &str) -> (u64, u64) {
    let mut added = 0u64;
    let mut removed = 0u64;
    for line in numstat.lines() {
        let mut columns = line.split('\t');
        let (Some(a), Some(r), Some(_)) = (columns.next(), columns.next(), columns.next()) else {
            continue;
        };
        added += a.parse::<u64>().unwrap_or(0);
        removed += r.parse::<u64>().unwrap_or(0);
    }
    (added, removed)
}

fn parse_trailers(text: &str) -> BTreeMap<String, Vec<String>> {
    let mut trailers: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for trailer in text.split(TRAILER) {
        let trailer = trailer.trim_matches('\n');
        if let Some((key, value)) = trailer.split_once(KEY_VALUE) {
            trailers
                .entry(key.trim().to_owned())
                .or_default()
                .push(value.trim().to_owned());
        }
    }
    trailers
}
