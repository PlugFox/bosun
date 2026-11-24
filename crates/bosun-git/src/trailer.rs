//! The `Item:`, `Run:` and `Session:` trailers (D6 of #7, #59): a hook injected into the
//! agent's git adds them, and every commit is verified against the attempt's values. Nothing is
//! ever amended.
//!
//! # The injection
//!
//! [`Hooks::install`] writes a hooks directory the daemon owns, outside every repository. Only
//! the agent's git is pointed at it, through command-scope configuration in the environment
//! (`GIT_CONFIG_COUNT`, `GIT_CONFIG_KEY_<n>`, `GIT_CONFIG_VALUE_<n>`: `core.hooksPath`), which
//! git reads after the system, global, local and worktree files, so it overrides a
//! repository-local `core.hooksPath` (confirmed with Git for Windows by
//! `tests::the_injected_hooks_path_overrides_a_repository_local_one`). Nothing is installed in
//! `.git/hooks`, so the user's own commits never see it.
//!
//! - `prepare-commit-msg` adds the three trailers from [`ITEM_VAR`], [`RUN_VAR`] and
//!   [`SESSION_VAR`], only when [`MARKER_VAR`] is set and the commit is made in the item's own
//!   checkout (`git rev-parse --show-toplevel` equals [`CHECKOUT_VAR`]; unset or empty matches
//!   nothing), with `git interpret-trailers --if-exists replace`, so `--amend` leaves one of each.
//!   Git runs it under `--no-verify` too. A commit the agent makes in another repository (a test's
//!   throwaway one, say) inherits the environment but gets no trailer (#134).
//! - Every hook, `prepare-commit-msg` included, then runs the repository's own hook of the same
//!   name, from the `core.hooksPath` the repository's configuration sets (any scope but the
//!   command scope the injection itself is in), or from `.git/hooks`: the user's hooks keep
//!   working in agent sessions.
//!
//! The entries go after the ones the identity's environment already sets (its
//! `credential.helper` reset is entry 0): [`Hooks::git_config`] is data, and `bosun-identity`'s
//! `Env::add_git_config` appends it and raises the count.
//!
//! # Verification
//!
//! An agent can bypass the hook (`-c core.hooksPath=...`, `commit-tree`). A trailer with a
//! value other than the attempt's is forged and fails the attempt ([`messages`](crate::messages)
//! with the [`Expected`] values); a missing trailer is only a line in the item report
//! ([`missing`]), since the database still links the commit by its range.

use std::io;
use std::path::{Path, PathBuf};

use crate::message::{self, Expected};
use crate::{Base, Error, Git};

/// The variable that switches the trailers on. Without it the injected hooks only chain.
pub const MARKER_VAR: &str = "BOSUN_TRAILERS";
/// The variable holding the `Item:` value.
pub const ITEM_VAR: &str = "BOSUN_ITEM";
/// The variable holding the `Run:` value.
pub const RUN_VAR: &str = "BOSUN_RUN";
/// The variable holding the `Session:` value.
pub const SESSION_VAR: &str = "BOSUN_SESSION";
/// The variable holding the item's checkout as `git rev-parse --show-toplevel` spells it
/// ([`Hooks::checkout`]): only a commit there gets the trailers.
pub const CHECKOUT_VAR: &str = "BOSUN_CHECKOUT";

/// Every hook name git runs from `core.hooksPath`; each one is installed, so each chains.
pub const HOOK_NAMES: &[&str] = &[
    "applypatch-msg",
    "pre-applypatch",
    "post-applypatch",
    "pre-commit",
    "pre-merge-commit",
    "prepare-commit-msg",
    "commit-msg",
    "post-commit",
    "pre-rebase",
    "post-checkout",
    "post-merge",
    "pre-push",
    "pre-receive",
    "update",
    "proc-receive",
    "post-receive",
    "post-update",
    "reference-transaction",
    "push-to-checkout",
    "pre-auto-gc",
    "post-rewrite",
    "sendemail-validate",
    "fsmonitor-watchman",
    "p4-changelist",
    "p4-prepare-changelist",
    "p4-post-changelist",
    "p4-pre-submit",
    "post-index-change",
];

/// The values an agent session's commits carry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Trailers {
    /// `Item:`, the item's reference (such as `owner/repo#59`).
    pub item: String,
    /// `Run:`, the run's id.
    pub run: String,
    /// `Session:`, the agent session's id.
    pub session: String,
}

/// The injected hooks directory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Hooks {
    dir: PathBuf,
}

impl Hooks {
    /// Writes the hooks into `dir` (created if missing, every script overwritten), a directory
    /// the daemon owns outside every repository.
    ///
    /// # Errors
    ///
    /// The directory or a script cannot be written.
    pub fn install(dir: impl Into<PathBuf>) -> io::Result<Self> {
        let dir = dir.into();
        std::fs::create_dir_all(&dir)?;
        for name in HOOK_NAMES {
            let path = dir.join(name);
            std::fs::write(&path, script(name))?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt as _;
                std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755))?;
            }
        }
        Ok(Self { dir })
    }

    /// The directory.
    #[must_use]
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// The command-scope configuration entries to append to the agent's
    /// `GIT_CONFIG_KEY_<n>`/`GIT_CONFIG_VALUE_<n>` list: `core.hooksPath` = the directory, with
    /// `/` separators (git's own spelling on Windows too).
    #[must_use]
    pub fn git_config(&self) -> Vec<(String, String)> {
        vec![(
            "core.hooksPath".to_owned(),
            self.dir.to_string_lossy().replace('\\', "/"),
        )]
    }

    /// The item's checkout as the hook compares it: `git rev-parse --show-toplevel` in `git`'s
    /// repository, so both sides have git's own spelling (`D:/...` on Windows).
    ///
    /// # Errors
    ///
    /// The git command fails or prints nothing.
    pub fn checkout(git: &Git) -> Result<String, Error> {
        let top = git.run(&["rev-parse", "--show-toplevel"], None)?;
        let top = top.trim_end_matches(['\r', '\n']);
        if top.is_empty() {
            return Err(git.unexpected("an empty top level".to_owned()));
        }
        Ok(top.to_owned())
    }

    /// The variables the hook reads: the marker, the three values and the item's `checkout`
    /// (as [`Hooks::checkout`] returns it).
    #[must_use]
    pub fn env(trailers: &Trailers, checkout: &str) -> Vec<(String, String)> {
        vec![
            (MARKER_VAR.to_owned(), "1".to_owned()),
            (CHECKOUT_VAR.to_owned(), checkout.to_owned()),
            (ITEM_VAR.to_owned(), trailers.item.clone()),
            (RUN_VAR.to_owned(), trailers.run.clone()),
            (SESSION_VAR.to_owned(), trailers.session.clone()),
        ]
    }
}

/// The script of hook `name`: POSIX `sh`, which Git for Windows runs hooks with too.
fn script(name: &str) -> String {
    let marker = if cfg!(mutant_git_trailer_all_commits) {
        // Mutant: the trailers go on every commit through these hooks, marker or not.
        "true".to_owned()
    } else {
        format!("[ -n \"${{{MARKER_VAR}:-}}\" ]")
    };
    let checkout = if cfg!(mutant_git_trailers_everywhere) {
        // Mutant (`mutant_git_trailers_everywhere`): every repository is the item's checkout.
        "true".to_owned()
    } else {
        format!(
            "[ -n \"${{{CHECKOUT_VAR}:-}}\" ] && \
             [ \"$(git rev-parse --show-toplevel 2>/dev/null)\" = \"${CHECKOUT_VAR}\" ]"
        )
    };
    let trailers = if name == "prepare-commit-msg" {
        format!(
            r#"
# The trailers (D6 of #7): only in an agent session and in the item's own checkout (#134),
# replacing a previous value so that `--amend` keeps one of each.
add() {{
    [ -n "$2" ] || return 0
    git -c trailer.separators=: interpret-trailers --in-place --where end \
        --if-exists replace --if-missing add --trailer "$1: $2" "$msg" || exit 1
}}
if {marker} && [ -n "${{1:-}}" ] && {checkout}; then
    msg=$1
    add Item "${{{ITEM_VAR}:-}}"
    add Run "${{{RUN_VAR}:-}}"
    add Session "${{{SESSION_VAR}:-}}"
fi
"#
        )
    } else {
        String::new()
    };
    format!(
        r#"#!/bin/sh
# Written by bosun (#59) into a directory outside every repository and reached only by an agent
# session's git, through GIT_CONFIG_COUNT (core.hooksPath). Do not edit: it is rewritten.
hook={name}
{trailers}
# Chain to the repository's own hook of this name: the core.hooksPath its configuration sets
# (the command scope is this injection), or the hooks directory of the repository.
own=$(git config --show-scope --get-all core.hooksPath 2>/dev/null |
    awk -F '\t' '$1 != "command" {{ v = substr($0, length($1) + 2) }} END {{ print v }}')
case $own in
    '') own=$(git rev-parse --git-common-dir)/hooks ;;
    '~/'*) own=$HOME/${{own#'~/'}} ;;
esac
here=$(cd "$(dirname "$0")" && pwd -P)
there=$(cd "$own" 2>/dev/null && pwd -P)
if [ -n "$there" ] && [ "$here" != "$there" ] && [ -f "$own/$hook" ] && [ -x "$own/$hook" ]; then
    exec "$own/$hook" "$@"
fi
exit 0
"#
    )
}

/// One commit since the base that lacks some of the trailers, for the item report.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Missing {
    /// The commit.
    pub sha: String,
    /// The trailers it lacks: `Item`, `Run` and `Session`, in that order.
    pub keys: Vec<&'static str>,
}

/// The commits since the base (`base..HEAD`) that do not carry an `Item:`, `Run:` and
/// `Session:` trailer with an expected value (`trailer_missing` in the report). A trailer with
/// an unexpected value is forged, which [`messages`](crate::messages) reports.
///
/// # Errors
///
/// A git command fails or prints something unexpected.
pub fn missing(git: &Git, base: &Base, expected: &Expected) -> Result<Vec<Missing>, Error> {
    let mut found = Vec::new();
    for (sha, body) in crate::precondition::log(git, base)? {
        let present = message::trailers(&body);
        let keys: Vec<&'static str> = [
            ("Item", &expected.item),
            ("Run", &expected.run),
            ("Session", &expected.session),
        ]
        .into_iter()
        .filter(|(key, values)| {
            !present
                .iter()
                .any(|(k, v)| k.eq_ignore_ascii_case(key) && values.contains(v))
        })
        .map(|(key, _)| key)
        .collect();
        if !keys.is_empty() {
            found.push(Missing { sha, keys });
        }
    }
    Ok(found)
}
