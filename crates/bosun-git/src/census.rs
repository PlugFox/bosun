//! The census of a repository's tests (#7): protected paths catch an edit of a named harness,
//! not a deleted assertion in an ordinary test file, so named regexes are counted over the
//! files a glob names at the item's base and at the commit its verdict judged, and a drop of a
//! kept counter (tests, assertions) or a rise of a capped one (switched-off tests) is reported.
//!
//! Both counts read a commit's tree (`git grep <rev>`), never the working files: what lands is
//! the commit, and an uncommitted edit neither hides a drop nor makes one.

use std::collections::BTreeMap;

use crate::{Error, Git};

/// A counter that moved the wrong way between the base and the judged commit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Finding {
    /// The counter's name, as the census names it.
    pub counter: String,
    /// Its count at the item's base.
    pub base: u64,
    /// Its count at the judged commit.
    pub head: u64,
}

/// Per counter (`(name, regex)`), the lines of the files `glob` names in `rev`'s tree that
/// match the regex: `git grep -I --count -E -e <regex> <rev> -- ":(glob)<glob>"`, the counts of
/// the files summed. The regex is git's extended regex (POSIX ERE); `glob` is a
/// repository-relative `:(glob)` pathspec; binary files are skipped. A counter nothing matches
/// counts 0.
///
/// # Errors
///
/// A git command fails (an unknown revision, a regex git refuses).
pub fn count(
    git: &Git,
    rev: &str,
    glob: &str,
    counters: &[(&str, &str)],
) -> Result<BTreeMap<String, u64>, Error> {
    let pathspec = format!(":(glob){glob}");
    let mut counts = BTreeMap::new();
    for &(name, regex) in counters {
        let mut args = vec!["grep", "-I", "--count", "-E", "-e", regex];
        // Mutant (`mutant_git_census_reads_working_tree`): the working tree is grepped instead
        // of the commit's tree.
        if !cfg!(mutant_git_census_reads_working_tree) {
            args.push(rev);
        }
        args.extend(["--", &pathspec]);
        let out = git.optional(&args)?;
        let total = out.as_deref().unwrap_or_default().lines().try_fold(
            0u64,
            |total, line| -> Result<u64, Error> {
                // `<rev>:<path>:<n>`: the path may hold a `:`, the number never does.
                let n = line
                    .rsplit_once(':')
                    .and_then(|(_, n)| n.trim().parse::<u64>().ok())
                    .ok_or_else(|| Error::Output {
                        repo: git.repo().to_owned(),
                        what: format!("git grep --count printed {line:?}"),
                    })?;
                Ok(total + n)
            },
        )?;
        counts.insert(name.to_owned(), total);
    }
    Ok(counts)
}

/// The counters that moved the wrong way from `base` to `head`: a `keep` counter lower at
/// `head`, or a `cap` counter higher, unless `allowed` (the card's `allow-census`) names it. A
/// counter missing from a count is 0 there. Sorted by name within `keep`, then `cap`.
#[must_use]
pub fn compare(
    base: &BTreeMap<String, u64>,
    head: &BTreeMap<String, u64>,
    keep: &[&str],
    cap: &[&str],
    allowed: &[String],
) -> Vec<Finding> {
    let at = |counts: &BTreeMap<String, u64>, name: &str| counts.get(name).copied().unwrap_or(0);
    let mut findings = Vec::new();
    let mut check = |name: &str, wrong: fn(u64, u64) -> bool| {
        let (b, h) = (at(base, name), at(head, name));
        if wrong(b, h) && !allowed.iter().any(|a| a == name) {
            findings.push(Finding {
                counter: name.to_owned(),
                base: b,
                head: h,
            });
        }
    };
    for name in keep {
        check(name, |base, head| head < base);
    }
    // Mutant (`mutant_git_census_cap_unchecked`): a capped counter may rise.
    if !cfg!(mutant_git_census_cap_unchecked) {
        for name in cap {
            check(name, |base, head| head > base);
        }
    }
    findings
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::*;
    use crate::NO_HOOKS;

    /// Runs `git args` in `dir` with no user or system configuration; it must succeed.
    fn git(dir: &Path, args: &[&str]) -> String {
        let out = Git::new(dir)
            .command()
            .args(["-c", "commit.gpgSign=false"])
            .args(["-c", "user.name=Test"])
            .args(["-c", "user.email=test@example.invalid"])
            .args(args)
            .env("GIT_CONFIG_GLOBAL", NO_HOOKS)
            .env("GIT_CONFIG_NOSYSTEM", "1")
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

    #[test]
    fn counts_lines_of_the_glob_at_a_commit_not_in_the_working_tree() {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path();
        git(repo, &["init", "-q", "-b", "main"]);
        std::fs::create_dir_all(repo.join("tests")).unwrap();
        std::fs::create_dir_all(repo.join("src")).unwrap();
        std::fs::write(
            repo.join("tests/a.rs"),
            "fn one() { assert!(true); }\nfn two() { assert_eq!(1, 1); }\nfn three() {}\n",
        )
        .unwrap();
        std::fs::write(repo.join("src/b.rs"), "fn b() { assert!(true); }\n").unwrap();
        git(repo, &["add", "-A"]);
        git(repo, &["commit", "-q", "-m", "tests"]);
        // Emptied in the working tree only.
        std::fs::write(repo.join("tests/a.rs"), "").unwrap();

        let counts = count(
            &Git::new(repo),
            "HEAD",
            "tests/**",
            &[("assertions", "assert"), ("nothing", "no such line")],
        )
        .unwrap();
        assert_eq!(counts["assertions"], 2, "{counts:?}");
        assert_eq!(counts["nothing"], 0, "{counts:?}");
    }

    #[test]
    fn a_drop_of_a_kept_counter_or_a_rise_of_a_capped_one_is_a_finding_unless_allowed() {
        let counts = |pairs: &[(&str, u64)]| -> BTreeMap<String, u64> {
            pairs.iter().map(|&(k, v)| (k.to_owned(), v)).collect()
        };
        let keep = |base: u64, head: u64, allowed: &[String]| {
            compare(
                &counts(&[("assertions", base)]),
                &counts(&[("assertions", head)]),
                &["assertions"],
                &[],
                allowed,
            )
        };
        let cap = |base: u64, head: u64| {
            compare(
                &counts(&[("ignored", base)]),
                &counts(&[("ignored", head)]),
                &[],
                &["ignored"],
                &[],
            )
        };
        assert_eq!(
            keep(2, 1, &[]),
            vec![Finding {
                counter: "assertions".to_owned(),
                base: 2,
                head: 1,
            }]
        );
        assert_eq!(keep(2, 3, &[]), []);
        assert_eq!(keep(2, 1, &["assertions".to_owned()]), []);
        assert_eq!(
            cap(0, 1),
            vec![Finding {
                counter: "ignored".to_owned(),
                base: 0,
                head: 1,
            }]
        );
        assert_eq!(cap(1, 0), []);
    }
}
