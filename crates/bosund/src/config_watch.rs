//! Watching the configuration file (#4, #55, #62): an edit is read again and, when valid,
//! replaces the configuration the loops see; an invalid edit keeps the previous one in force
//! and is reported.
//!
//! The file is compared by content every [`INTERVAL`] (a poll, not a file-system watch: an
//! editor's save is often a rename, and a poll survives that on every platform).
//!
//! # What waits for the next daemon start (#186)
//!
//! A loop reads its repository's configuration live from the channel (`executor::lookup`), so
//! a repository's policy, model, verdict, worktree, landing, sweep and the rest (and
//! `[budget]`) take effect at the next item or the next check. Some of it is built once at
//! start instead, from the configuration as read then, and never again:
//!
//! - `state::open` builds one store and one ingest per identity (`bosun_ingest::plans`: its
//!   `database`, its `claude_config_dir`, the `path` of each of its repositories), the list of
//!   each identity's repositories (their `identity`), their commit reader (their `path`), their
//!   board (`executor::board::Board::of`: the repository's `queue`, its `path`, whose `origin`
//!   names the GitHub repository, and a clone of its whole identity, which the board's `gh`
//!   reads through `executor::identity::bundle`: `github_login`, `gh_config_dir`,
//!   `git_config`, `ssh_key`, `claude_config_dir`, `codex_home`, `github_host` and
//!   `remote_allowlist`; and `author_allowlist` in the Projects v2 binding), and each
//!   identity's `author_allowlist` and resource names.
//! - `state::start_loops` starts one loop per repository with a queue, as configured then.
//! - `leases::Leases::new` reads `[resource.<name>]`.
//!
//! So [`restart_only`] names, against the configuration the daemon started with: a repository
//! added or removed; a repository's `identity`, `queue` or `path` changed (the loop reads
//! `path` live, but its ingest, commit reader and board keep the old one, a mixed state that
//! only a start ends); an identity added or removed, or any of its fields changed (each is
//! read by one of the above); a resource added, removed or changed. Nothing else gives a line.
//! Each line is logged once, after the reload line ([`Pending`]); an edit that puts a field
//! back clears its line, and a line that comes back is logged again.
//!
//! A repository removed from the configuration keeps its loop until the next start, but the
//! loop claims nothing new (its `lookup` fails), and its item in flight goes to Needs you
//! (`config_changed`: the repository's hashes are gone, which counts as a change).

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use bosun_config::{Config, DiskProbe, Identity};
use tokio::sync::watch;
use tokio::task::JoinHandle;

/// How often the file is read.
pub(crate) const INTERVAL: Duration = Duration::from_secs(1);

/// The end of every line of [`restart_only`].
const NEXT_START: &str = "takes effect at the next daemon start";

/// What the watcher reports: the last reload's error, while it is the file's current content,
/// and the lines the last reload logged.
#[derive(Debug, Clone, Default)]
pub(crate) struct Reported(Arc<Mutex<Seen>>);

#[derive(Debug, Default)]
struct Seen {
    error: Option<String>,
    logged: Vec<String>,
}

impl Reported {
    /// The error of the file as it is now, if its last edit was invalid.
    #[cfg_attr(
        not(test),
        expect(dead_code, reason = "the status methods (#70) report it")
    )]
    pub(crate) fn get(&self) -> Option<String> {
        self.lock().error.clone()
    }

    /// The lines the last reload logged, in order.
    #[cfg_attr(
        not(test),
        expect(dead_code, reason = "the tests read what was logged")
    )]
    pub(crate) fn logged(&self) -> Vec<String> {
        self.lock().logged.clone()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Seen> {
        self.0.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn set(&self, error: Option<String>, logged: Vec<String>) {
        *self.lock() = Seen { error, logged };
    }
}

/// One read of the file's new `text` over `previous`: the configuration now in force, and the
/// error when the edit was invalid (then it is `previous`).
pub(crate) fn reload(
    previous: &Config,
    text: &str,
    probe: &dyn DiskProbe,
) -> (Config, Option<String>) {
    let reload = bosun_config::reload(previous.clone(), text, probe);
    (reload.config, reload.error.map(|e| e.to_string()))
}

/// Every difference between the configuration the daemon started with (`start`) and `now`
/// that takes effect only at the next daemon start (see the module docs): repositories by
/// name, then identities by name, then resources by name. Each line ends in
/// "takes effect at the next daemon start".
pub(crate) fn restart_only(start: &Config, now: &Config) -> Vec<String> {
    let mut lines = Vec::new();
    for (name, before, after) in pairs(&start.repos, &now.repos) {
        match (before, after) {
            (None, Some(_)) => {
                lines.push(format!("repository {name} is added: its loop {NEXT_START}"));
            }
            (Some(_), None) => lines.push(format!(
                "repository {name} is removed: its loop claims nothing new and its item in \
                 flight goes to Needs you; the rest {NEXT_START}"
            )),
            (Some(before), Some(after)) => {
                // Mutant (`mutant_reload_misses_queue_change`): the queue is not compared.
                let queue = before.queue != after.queue && !cfg!(mutant_reload_misses_queue_change);
                for (field, changed) in [
                    ("identity", before.identity != after.identity),
                    ("queue", queue),
                    ("path", before.path != after.path),
                ] {
                    if changed {
                        lines.push(format!(
                            "repository {name}: its {field} changed: {NEXT_START}"
                        ));
                    }
                }
            }
            (None, None) => {}
        }
    }
    for (name, before, after) in pairs(&start.identities, &now.identities) {
        match (before, after) {
            (None, Some(_)) => lines.push(format!(
                "identity {name} is added: its store, ingest and loops take effect at the \
                 next daemon start"
            )),
            (Some(_), None) => lines.push(format!(
                "identity {name} is removed: it is served until its removal {NEXT_START}"
            )),
            (Some(before), Some(after)) => {
                for field in identity_changes(before, after) {
                    lines.push(format!(
                        "identity {name}: its {field} changed: {NEXT_START}"
                    ));
                }
            }
            (None, None) => {}
        }
    }
    for (name, before, after) in pairs(&start.resources, &now.resources) {
        match (before, after) {
            (None, Some(_)) => lines.push(format!("resource {name} is added: {NEXT_START}")),
            (Some(_), None) => lines.push(format!("resource {name} is removed: {NEXT_START}")),
            (Some(before), Some(after)) => {
                // Its kind, capacity or scope.
                if before != after {
                    lines.push(format!("resource {name} changed: {NEXT_START}"));
                }
            }
            (None, None) => {}
        }
    }
    lines
}

/// The names of either map, in order, with each one's value in `start` and in `now`.
fn pairs<'a, V>(
    start: &'a BTreeMap<String, V>,
    now: &'a BTreeMap<String, V>,
) -> Vec<(&'a str, Option<&'a V>, Option<&'a V>)> {
    let names: BTreeSet<&String> = start.keys().chain(now.keys()).collect();
    names
        .into_iter()
        .map(|name| (name.as_str(), start.get(name), now.get(name)))
        .collect()
}

/// The keys of an identity that differ, in the order of the file's documentation.
fn identity_changes(before: &Identity, after: &Identity) -> Vec<&'static str> {
    [
        (
            "claude_config_dir",
            before.claude_config_dir != after.claude_config_dir,
        ),
        ("database", before.database != after.database),
        ("github_host", before.github_host != after.github_host),
        ("github_login", before.github_login != after.github_login),
        ("gh_config_dir", before.gh_config_dir != after.gh_config_dir),
        ("git_config", before.git_config != after.git_config),
        ("ssh_key", before.ssh_key != after.ssh_key),
        ("codex_home", before.codex_home != after.codex_home),
        (
            "remote_allowlist",
            before.remote_allowlist != after.remote_allowlist,
        ),
        (
            "author_allowlist",
            before.author_allowlist != after.author_allowlist,
        ),
    ]
    .into_iter()
    .filter_map(|(field, changed)| changed.then_some(field))
    .collect()
}

/// The lines of [`restart_only`] the previous valid reload gave: a reload logs only the lines
/// not among them.
#[derive(Debug, Default)]
pub(crate) struct Pending(BTreeSet<String>);

impl Pending {
    /// The lines of `now` not given by the previous reload, in `now`'s order; `now` becomes
    /// the previous reload's.
    pub(crate) fn update(&mut self, now: &[String]) -> Vec<String> {
        let new = now
            .iter()
            // Mutant (`mutant_reload_relogs_pending`): every line is new at every reload.
            .filter(|line| cfg!(mutant_reload_relogs_pending) || !self.0.contains(*line))
            .cloned()
            .collect();
        self.0 = now.iter().cloned().collect();
        new
    }
}

/// The lines one reload of `path` logs: the reload line (with `error` when the edit was
/// invalid, else the count of `pending` changes that wait for the next start), then each of
/// `new` (the pending lines not logged before).
pub(crate) fn report(
    path: &std::path::Path,
    error: Option<&str>,
    pending: usize,
    new: &[String],
) -> Vec<String> {
    let path = path.display();
    let first = match (error, pending) {
        (Some(e), _) => {
            format!("bosund: {path}: the edit is invalid, the previous configuration stays: {e}")
        }
        (None, 0) => format!("bosund: {path}: configuration reloaded"),
        (None, 1) => {
            format!("bosund: {path}: configuration reloaded; 1 change {NEXT_START}")
        }
        (None, n) => format!(
            "bosund: {path}: configuration reloaded; {n} changes take effect at the next \
             daemon start"
        ),
    };
    std::iter::once(first)
        .chain(new.iter().map(|line| format!("bosund: {path}: {line}")))
        .collect()
}

/// Watches `path`, publishing each valid edit on `tx`, until the task is aborted. Each valid
/// edit is compared with the configuration on `tx` now, the daemon's start configuration.
pub(crate) fn spawn(
    path: PathBuf,
    tx: watch::Sender<Arc<Config>>,
    probe: Arc<dyn DiskProbe + Send + Sync>,
    every: Duration,
    reported: Reported,
) -> JoinHandle<()> {
    // Read now, not in the task: an edit made right after this call is a change.
    let mut last = std::fs::read(&path).ok();
    let start = tx.borrow().clone();
    tokio::spawn(async move {
        let mut pending = Pending::default();
        let mut waiting = 0;
        loop {
            tokio::time::sleep(every).await;
            let Ok(now) = tokio::fs::read(&path).await else {
                // Mid-save, or removed: the configuration in force stays.
                continue;
            };
            if last.as_ref() == Some(&now) {
                continue;
            }
            last = Some(now.clone());
            let text = String::from_utf8_lossy(&now);
            let previous = tx.borrow().clone();
            let (config, error) = reload(&previous, &text, probe.as_ref());
            // An invalid edit leaves what is pending as it is.
            let new = if error.is_none() {
                let lines = restart_only(&start, &config);
                waiting = lines.len();
                pending.update(&lines)
            } else {
                Vec::new()
            };
            let logged = report(&path, error.as_deref(), waiting, &new);
            for line in &logged {
                crate::log::log!("{line}");
            }
            reported.set(error, logged);
            if config != *previous {
                tx.send_replace(Arc::new(config));
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A configuration of identity `test` and repository `demo` (its board at `board`, its
    /// checkout at `repo`), `protected` as its policy's list, then `more`.
    fn text_of(
        dir: &std::path::Path,
        repo: &str,
        board: &str,
        protected: &str,
        more: &str,
    ) -> String {
        let db = dir.join("db").join("bosun.db");
        format!(
            "[identity.test]\nclaude_config_dir = '{}'\ndatabase = '{}'\n\n\
             [repo.demo]\npath = '{}'\nidentity = 'test'\n\
             queue = {{ kind = 'local_file', file = '{}' }}\nmodel = 'opus'\n\n\
             [repo.demo.policy]\nprotected = [{protected}]\n\n{more}",
            dir.join("claude").display(),
            db.display(),
            dir.join(repo).display(),
            dir.join(board).display(),
        )
    }

    fn text(dir: &std::path::Path, protected: &str) -> String {
        text_of(dir, "repo", "board.toml", protected, "")
    }

    /// `[repo.beta]` of identity `test`, at `<dir>/beta`.
    fn beta(dir: &std::path::Path) -> String {
        format!(
            "[repo.beta]\npath = '{}'\nidentity = 'test'\n",
            dir.join("beta").display()
        )
    }

    fn scene() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        for sub in ["claude", "db", "repo", "beta", "moved"] {
            std::fs::create_dir_all(dir.path().join(sub)).unwrap();
        }
        dir
    }

    fn parse(text: &str) -> Config {
        bosun_config::parse(text, &bosun_config::SystemProbe::new()).unwrap()
    }

    #[tokio::test]
    async fn an_invalid_edit_keeps_the_previous_configuration_and_is_reported() {
        let dir = scene();
        let path = dir.path().join("config.toml");
        let probe: Arc<dyn DiskProbe + Send + Sync> = Arc::new(bosun_config::SystemProbe::new());
        std::fs::write(&path, text(dir.path(), "'a'")).unwrap();
        let first = bosun_config::load(&path, probe.as_ref()).unwrap();
        let (tx, mut rx) = watch::channel(Arc::new(first.clone()));
        let reported = Reported::default();
        let task = spawn(
            path.clone(),
            tx,
            probe,
            Duration::from_millis(20),
            reported.clone(),
        );

        std::fs::write(&path, "[identity.test\nbroken").unwrap();
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        while reported.get().is_none() {
            assert!(std::time::Instant::now() < deadline, "no error reported");
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert_eq!(**rx.borrow(), first, "the previous configuration stays");
        assert!(!rx.has_changed().unwrap());

        std::fs::write(&path, text(dir.path(), "'a', 'b'")).unwrap();
        tokio::time::timeout(Duration::from_secs(10), rx.changed())
            .await
            .expect("a valid edit is published")
            .unwrap();
        assert!(rx.borrow().repos["demo"].policy.protects("b"));
        assert_eq!(reported.get(), None, "the error clears with the fix");
        task.abort();
    }

    /// #186: what a reload cannot apply is named, one line per difference; a field the loops
    /// read live gives none.
    #[test]
    fn a_reload_names_each_change_that_waits_for_the_next_daemon_start() {
        let dir = scene();
        let d = dir.path();
        let start = parse(&text(d, "'a'"));
        let rig = "[resource.rig]\nkind = 'device'\n";
        let now = parse(&text_of(
            d,
            "moved",
            "other-board.toml",
            "'a'",
            &format!("{}\n{rig}", beta(d)),
        ));
        let lines = restart_only(&start, &now);
        assert_eq!(
            lines,
            [
                "repository beta is added: its loop takes effect at the next daemon start",
                "repository demo: its queue changed: takes effect at the next daemon start",
                "repository demo: its path changed: takes effect at the next daemon start",
                "resource rig is added: takes effect at the next daemon start",
            ],
        );
        for line in &lines {
            assert!(
                line.ends_with("takes effect at the next daemon start"),
                "{line}"
            );
        }

        let protected = parse(&text(d, "'a', 'b'"));
        assert_ne!(protected, start);
        assert_eq!(restart_only(&start, &protected), Vec::<String>::new());

        let without = parse(&format!(
            "[identity.test]\nclaude_config_dir = '{}'\ndatabase = '{}'\n",
            d.join("claude").display(),
            d.join("db").join("bosun.db").display(),
        ));
        assert_eq!(
            restart_only(&start, &without),
            [
                "repository demo is removed: its loop claims nothing new and its item in \
                 flight goes to Needs you; the rest takes effect at the next daemon start"
            ],
        );
    }

    /// #186: a pending change is logged at the reload that makes it, not at every reload
    /// after; one that went away and comes back is logged again.
    #[tokio::test]
    async fn a_change_that_waits_for_the_next_start_is_logged_once() {
        let line = |s: &str| s.to_owned();
        let (a, b) = (
            line("repository beta is added"),
            line("resource rig is added"),
        );
        let mut pending = Pending::default();
        assert_eq!(
            pending.update(&[a.clone(), b.clone()]),
            [a.clone(), b.clone()]
        );
        assert_eq!(
            pending.update(&[a.clone(), b.clone()]),
            Vec::<String>::new()
        );
        assert_eq!(
            pending.update(std::slice::from_ref(&b)),
            Vec::<String>::new()
        );
        assert_eq!(pending.update(&[a.clone(), b.clone()]), [a]);

        let dir = scene();
        let d = dir.path();
        let path = d.join("config.toml");
        let probe: Arc<dyn DiskProbe + Send + Sync> = Arc::new(bosun_config::SystemProbe::new());
        std::fs::write(&path, text(d, "'a'")).unwrap();
        let first = bosun_config::load(&path, probe.as_ref()).unwrap();
        let (tx, mut rx) = watch::channel(Arc::new(first));
        let reported = Reported::default();
        let task = spawn(
            path.clone(),
            tx,
            probe,
            Duration::from_millis(20),
            reported.clone(),
        );
        let shown = path.display().to_string();

        std::fs::write(&path, text_of(d, "repo", "board.toml", "'a'", &beta(d))).unwrap();
        tokio::time::timeout(Duration::from_secs(10), rx.changed())
            .await
            .expect("the edit adding beta is published")
            .unwrap();
        assert_eq!(
            reported.logged(),
            [
                format!(
                    "bosund: {shown}: configuration reloaded; 1 change takes effect at the \
                     next daemon start"
                ),
                format!(
                    "bosund: {shown}: repository beta is added: its loop takes effect at the \
                     next daemon start"
                ),
            ],
        );

        std::fs::write(
            &path,
            text_of(d, "repo", "board.toml", "'a', 'b'", &beta(d)),
        )
        .unwrap();
        tokio::time::timeout(Duration::from_secs(10), rx.changed())
            .await
            .expect("the policy edit is published")
            .unwrap();
        assert!(rx.borrow().repos["demo"].policy.protects("b"));
        assert_eq!(
            reported.logged(),
            [format!(
                "bosund: {shown}: configuration reloaded; 1 change takes effect at the next \
                 daemon start"
            )],
            "beta's line is not logged again",
        );
        task.abort();
    }
}
