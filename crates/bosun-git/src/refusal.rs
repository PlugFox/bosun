//! Why an item goes to Needs you before its verdict runs (#7 "Verdict", #59): a new ignored
//! file, a change to git's own configuration, or a protected path its commits touched.
//!
//! These are not failed attempts, unlike a [`Reason`](crate::Reason): retrying cannot help, and
//! nothing is undone (a new ignored file is never deleted; it may be the user's build output).
//!
//! A refusal for new ignored files is one line however many files there are: they are folded to
//! their places (a build directory, or a top-level directory) with counts, at most 10 places and
//! [`LINE_CAP`] characters, and the line says when a build ran below the repository root (the
//! usual cause of thousands of them: a relative target directory, or a build from a crate).

use std::collections::BTreeMap;
use std::fmt::{self, Write as _};
use std::path::PathBuf;

/// One condition that sends the item to Needs you without running the verdict.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum Refusal {
    /// Ignored files appeared during the session outside the declared build directories.
    NewIgnored {
        /// The new ignored paths, repository-relative, `/`-separated, sorted: every one, the
        /// guard's evidence (the text folds them).
        paths: Vec<String>,
        /// The names treated as build directories when the paths are folded, sorted, lower
        /// case: the last component of each build directory, and [`KNOWN_BUILD_NAMES`].
        build_names: Vec<String>,
    },
    /// A file of git's own configuration (a hook, `.git/config`, `config.worktree`, an
    /// `include.path` target, `info/exclude`, `info/attributes`) was created, changed or
    /// deleted since the snapshot at claim.
    ConfigChanged {
        /// The file.
        path: PathBuf,
        /// The SHA-256 of its content at claim; `None` when it did not exist.
        before: Option<String>,
        /// The SHA-256 of its content now; `None` when it no longer exists.
        after: Option<String>,
    },
    /// The item's commits touched protected paths that the snapshot does not allow.
    Protected {
        /// The touched protected paths, repository-relative, `/`-separated, sorted.
        paths: Vec<String>,
        /// The diff stat of those paths over the item's commits, for the Needs you comment.
        stat: String,
    },
}

impl fmt::Display for Refusal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let opt = |o: &Option<String>| o.clone().unwrap_or_else(|| "(none)".to_owned());
        match self {
            // Mutant (`mutant_git_ignored_unfolded`): every path listed, as before the fold.
            Self::NewIgnored { paths, .. } if cfg!(mutant_git_ignored_unfolded) => write!(
                f,
                "new ignored files outside the build directories: {}",
                paths.join(", ")
            ),
            Self::NewIgnored { paths, build_names } => f.write_str(&fold(paths, build_names)),
            Self::ConfigChanged {
                path,
                before,
                after,
            } => write!(
                f,
                "git configuration changed: {} ({} -> {})",
                path.display(),
                opt(before),
                opt(after)
            ),
            Self::Protected { paths, stat } => write!(
                f,
                "the commits touch protected paths: {}\n{}",
                paths.join(", "),
                stat.trim_end()
            ),
        }
    }
}

/// The names of build directories wherever they are, besides the configured ones.
pub const KNOWN_BUILD_NAMES: &[&str] = &["target"];

/// The longest text of a [`Refusal::NewIgnored`] (when its first place fits): the report's cut
/// of a reason, under the next prompt's.
const LINE_CAP: usize = 300;

/// The most places a [`Refusal::NewIgnored`] names.
const MAX_PLACES: usize = 10;

/// The hint when a build place lies below the repository root.
const BELOW_ROOT: &str = "a build ran below the repository root (a relative target directory, \
                          or a build started from a crate directory)";

/// The build names of `build_dirs` (the last component of each, lower case) and
/// [`KNOWN_BUILD_NAMES`], sorted, without repeats.
pub(crate) fn build_names<'a>(build_dirs: impl IntoIterator<Item = &'a String>) -> Vec<String> {
    let mut names: Vec<String> = build_dirs
        .into_iter()
        .filter_map(|dir| {
            dir.trim_start_matches("./")
                .trim_end_matches('/')
                .rsplit('/')
                .next()
                .filter(|name| !name.is_empty())
                .map(str::to_ascii_lowercase)
        })
        .chain(KNOWN_BUILD_NAMES.iter().map(|n| (*n).to_owned()))
        .collect();
    names.sort();
    names.dedup();
    names
}

/// One place of new ignored files.
struct Place {
    /// What is shown: `<dir>/`, or the path when the place has one file.
    shown: String,
    /// Its files.
    count: usize,
    /// Whether it is a build directory below the repository root.
    below_root: bool,
}

/// The text of a [`Refusal::NewIgnored`]: `paths` folded to their places (see the module doc).
///
/// A path with a component other than its last equal to a build name is in the place made of
/// its components up to the first such one; any other path is in the place of its first
/// component, shown as the longest common directory of its paths.
fn fold(paths: &[String], build_names: &[String]) -> String {
    // Keyed by (whether a build place, its directory or first component).
    let mut grouped: BTreeMap<(bool, String), (Vec<&str>, bool)> = BTreeMap::new();
    for path in paths {
        let parts: Vec<&str> = path.split('/').collect();
        let build = parts[..parts.len() - 1]
            .iter()
            .position(|c| build_names.iter().any(|n| n.eq_ignore_ascii_case(c)));
        let (key, below_root) = match build {
            Some(i) => ((true, format!("{}/", parts[..=i].join("/"))), i > 0),
            None => ((false, parts[0].to_owned()), false),
        };
        let place = grouped.entry(key).or_default();
        place.0.push(path);
        place.1 |= below_root;
    }
    let mut places: Vec<Place> = grouped
        .into_iter()
        .map(|((build, dir), (files, below_root))| {
            let shown = if files.len() == 1 {
                files[0].to_owned()
            } else if build {
                dir
            } else {
                common_dir(&files)
            };
            Place {
                shown,
                count: files.len(),
                below_root,
            }
        })
        .collect();
    places.sort_by(|a, b| b.count.cmp(&a.count).then_with(|| a.shown.cmp(&b.shown)));

    let mut text = format!(
        "new ignored files outside the build directories ({}): ",
        files(paths.len())
    );
    // Mutant (`mutant_git_ignored_no_build_hint`): the hint is never written.
    if places.iter().any(|p| p.below_root) && !cfg!(mutant_git_ignored_no_build_hint) {
        text.push_str(BELOW_ROOT);
        text.push_str("; ");
    }
    let rest = |shown: usize| {
        if shown == places.len() {
            return String::new();
        }
        let more: usize = places[shown..].iter().map(|p| p.count).sum();
        let left = places.len() - shown;
        format!(
            " and {more} more {} in {left} more {}",
            if more == 1 { "file" } else { "files" },
            if left == 1 { "place" } else { "places" }
        )
    };
    let mut shown = 0;
    for (i, place) in places.iter().enumerate().take(MAX_PLACES) {
        let mut next = text.clone();
        if i > 0 {
            next.push_str(", ");
        }
        if place.count == 1 {
            next.push_str(&place.shown);
        } else {
            let _ = write!(next, "{} ({})", place.shown, files(place.count));
        }
        if i > 0 && next.chars().count() + rest(i + 1).chars().count() > LINE_CAP {
            break;
        }
        text = next;
        shown = i + 1;
    }
    text.push_str(&rest(shown));
    text
}

/// `<n> files`, or `1 file`.
fn files(n: usize) -> String {
    if n == 1 {
        "1 file".to_owned()
    } else {
        format!("{n} files")
    }
}

/// The longest common directory of `paths` (all of one first component), ending in `/`.
fn common_dir(paths: &[&str]) -> String {
    let dirs: Vec<Vec<&str>> = paths
        .iter()
        .map(|p| {
            let mut parts: Vec<&str> = p.split('/').collect();
            parts.pop();
            parts
        })
        .collect();
    let mut common: &[&str] = &dirs[0];
    for dir in &dirs[1..] {
        let same = common.iter().zip(dir).take_while(|(a, b)| a == b).count();
        common = &common[..same];
    }
    format!("{}/", common.join("/"))
}

/// Whether the repository-relative `entry` (a build directory or an `allow-protected` entry,
/// `/`-separated) covers `path`: the path itself or anything under it. ASCII case is ignored,
/// as in the permission policy: on Windows and macOS the two spellings are one file.
pub(crate) fn covers(entry: &str, path: &str) -> bool {
    let entry = entry
        .trim_start_matches("./")
        .trim_end_matches('/')
        .to_ascii_lowercase();
    let path = path
        .trim_start_matches("./")
        .trim_end_matches('/')
        .to_ascii_lowercase();
    !entry.is_empty()
        && (path == entry
            || path
                .strip_prefix(&entry)
                .is_some_and(|rest| rest.starts_with('/')))
}

#[cfg(test)]
mod tests {
    use super::{Refusal, build_names, covers};

    /// The text of a new-ignored refusal of `paths` with the build name `target`.
    fn text(paths: Vec<String>) -> String {
        Refusal::NewIgnored {
            paths,
            build_names: vec!["target".to_owned()],
        }
        .to_string()
    }

    /// Run 6: thousands of files of a build started from a crate directory are one short line
    /// with the places, their counts and the hint; 40 places are cut to 10 and counted; a build
    /// place at the root has no hint. `mutant_git_ignored_unfolded` lists every path;
    /// `mutant_git_ignored_no_build_hint` drops the hint.
    #[test]
    fn thousands_of_new_ignored_files_fold_to_their_places_with_a_hint() {
        let mut paths: Vec<String> = (0..50)
            .flat_map(|i| (0..100).map(move |j| format!("crates/demo/target/mutants/m{i}/f{j}.o")))
            .collect();
        paths.extend(["logs/a.log", "logs/b.log", "notes.tmp"].map(str::to_owned));
        paths.sort();
        let folded = text(paths);
        assert!(folded.chars().count() <= 300, "{folded}");
        assert!(
            folded.starts_with(
                "new ignored files outside the build directories (5003 files): \
                 a build ran below the repository root"
            ),
            "{folded}"
        );
        for part in [
            "crates/demo/target/ (5000 files)",
            "logs/ (2 files)",
            "notes.tmp",
        ] {
            assert!(folded.contains(part), "{part}: {folded}");
        }
        assert!(!folded.contains("f1.o"), "{folded}");
        assert!(!folded.contains('\n'), "{folded}");

        let many = text((0..40).map(|i| format!("dir{i:02}/x.log")).collect());
        assert!(many.chars().count() <= 300, "{many}");
        assert!(many.matches(", ").count() < 10, "{many}");
        assert!(many.ends_with(" more places"), "{many}");
        assert!(!many.contains("below the repository root"), "{many}");

        let root = text(vec![
            "target/debug/x.o".to_owned(),
            "target/debug/y.o".to_owned(),
        ]);
        assert_eq!(
            root,
            "new ignored files outside the build directories (2 files): target/ (2 files)"
        );

        assert_eq!(
            text(vec!["hidden.log".to_owned()]),
            "new ignored files outside the build directories (1 file): hidden.log"
        );
        let deep = text(vec![
            "logs/2026/a.log".to_owned(),
            "logs/2026/b.log".to_owned(),
        ]);
        assert!(deep.ends_with(": logs/2026/ (2 files)"), "{deep}");
    }

    #[test]
    fn build_names_are_the_last_components_and_target() {
        let dirs = ["./out/Build/".to_owned(), "target".to_owned()];
        assert_eq!(build_names(&dirs), ["build", "target"]);
        assert_eq!(build_names(&[]), ["target"]);
    }

    #[test]
    fn entries_cover_the_path_and_what_is_under_it() {
        assert!(covers("target", "target"));
        assert!(covers("target/", "target/debug/x.o"));
        assert!(covers("./Target", "target/x"));
        assert!(!covers("target", "target2/x"));
        assert!(!covers("target", "sub/target/x"));
        assert!(!covers("", "x"));
        assert!(covers("tests/h.rs", "tests/h.rs"));
        assert!(!covers("tests/h.rs", "tests/h.rs.bak"));
    }
}
