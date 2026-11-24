//! Control (#7, #67): pauses, the kill switch, the daily cap's hold and the operator token.
//!
//! [`Control`] is daemon-wide and shared by the API and every repository's loop. The API
//! changes it (`control.*`, under the operator token); each loop reads its hold at every step
//! ([`Control::hold`] into the machine's context and the scheduler's gates) and is woken by
//! every change ([`Control::subscribe`]), so a kill switch reaches the running child at once.
//! The loops write back what they observe: the automatic pause of a repository with an
//! interactive session in its checkout, and the daily cap of an identity.
//!
//! # The kill switch and board writes
//!
//! The kill switch kills every child, parks every item and stops every board write until it is
//! released. [`Control::board_writes_stopped`] is the flag every board writer checks before a
//! write: the claim procedure here, and the outbox drainer (#65), which must not send a row
//! while it is set.
//!
//! # The operator token
//!
//! An agent session runs as the same OS user and can open its own connection to the endpoint,
//! so the switches take a credential it does not have: a token from the OS's random source,
//! written at every start to the file `bosun_client::operator::path` names (outside every
//! working tree; the daemon refuses to start when it is inside one), and held here as its
//! SHA-256 digest only; a clean stop removes the file if it still holds this daemon's token
//! ([`Control::withdraw`]). The agent's environment never names the file (it is denied by name in
//! `bosun-identity`), and the agent CLI is told not to read or edit it ([`Control::deny_rules`]).

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};

use bosun_api::rpc::{ErrorObject, code};
use bosun_api::{ControlStatus, EventBody, KillResult, Method, RepoControl, method};
use bosun_core::{Gates, Hold};
use serde_json::Value;
use sha2::{Digest, Sha256};
use tokio::sync::watch;

use crate::api::call;
use crate::state::{Served, State};

/// The hold names of the API.
pub(crate) fn hold_name(hold: Hold) -> &'static str {
    match hold {
        Hold::Kill => "kill",
        Hold::Pause => "pause",
        Hold::DailyCap => "daily_cap",
    }
}

#[derive(Debug, Default)]
struct Flags {
    killed: bool,
    /// Identities paused by hand, every repository of each.
    paused: BTreeSet<String>,
    /// Repositories paused by hand.
    paused_repos: BTreeSet<String>,
    /// Repositories paused automatically, until when.
    auto: BTreeMap<String, i64>,
    /// When each repository was last resumed by hand: interactive activity before it no longer
    /// pauses it.
    resumed: BTreeMap<String, i64>,
    /// Identities whose daily cap holds their queue, until when.
    daily: BTreeMap<String, i64>,
}

#[derive(Debug)]
struct Inner {
    flags: Mutex<Flags>,
    /// The board writes are stopped (the kill switch).
    stopped: AtomicBool,
    /// Bumped at every change.
    changed: watch::Sender<u64>,
    /// The digest of the operator token.
    operator: [u8; 32],
    /// The token's file, for the agent CLI's deny rules.
    operator_file: Option<PathBuf>,
}

/// The control state; cloning shares it.
#[derive(Debug, Clone)]
pub(crate) struct Control {
    inner: Arc<Inner>,
}

impl Default for Control {
    /// Control with a token nobody was given: the switches refuse every caller.
    fn default() -> Self {
        Self::with_token(&fresh_token(), None)
    }
}

/// A new operator token: the prefix and 32 bytes of the OS's random source in hex.
pub(crate) fn fresh_token() -> String {
    let mut bytes = [0u8; 32];
    getrandom::fill(&mut bytes).expect("the OS's random source is available");
    let mut token = bosun_client::operator::PREFIX.to_owned();
    for b in bytes {
        let _ = write!(token, "{b:02x}");
    }
    token
}

fn digest(token: &str) -> [u8; 32] {
    Sha256::digest(token.as_bytes()).into()
}

impl Control {
    /// Control that admits `token`; `file` is where the token was written.
    pub(crate) fn with_token(token: &str, file: Option<PathBuf>) -> Self {
        Self {
            inner: Arc::new(Inner {
                flags: Mutex::new(Flags::default()),
                stopped: AtomicBool::new(false),
                changed: watch::Sender::new(0),
                operator: digest(token),
                operator_file: file,
            }),
        }
    }

    fn flags(&self) -> MutexGuard<'_, Flags> {
        self.inner
            .flags
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn bump(&self) {
        self.inner.changed.send_modify(|n| *n += 1);
    }

    /// Refuses a request whose operator token is not this daemon's.
    pub(crate) fn authorize(&self, token: &str) -> Result<(), ErrorObject> {
        if cfg!(mutant_control_no_operator_token) {
            // Mutant: any caller may throw the switches.
            return Ok(());
        }
        // Digests compared, so the comparison's time says nothing of the token.
        if digest(token) == self.inner.operator {
            Ok(())
        } else {
            Err(ErrorObject::new(
                code::UNAUTHORIZED,
                "the control switches take the operator token (bosun_client::operator)",
            ))
        }
    }

    /// Wakes on every change.
    pub(crate) fn subscribe(&self) -> watch::Receiver<u64> {
        self.inner.changed.subscribe()
    }

    /// Throws or releases the kill switch.
    pub(crate) fn set_killed(&self, killed: bool) {
        self.flags().killed = killed;
        self.inner.stopped.store(killed, Ordering::SeqCst);
        self.bump();
    }

    pub(crate) fn killed(&self) -> bool {
        self.flags().killed
    }

    /// Every board writer checks this before a write: while the kill switch is thrown nothing
    /// is written to any board. The outbox drainer (#65) must check it before each row.
    pub(crate) fn board_writes_stopped(&self) -> bool {
        if cfg!(mutant_control_kill_keeps_writing) {
            // Mutant: the kill switch leaves board writes going.
            return false;
        }
        self.inner.stopped.load(Ordering::SeqCst)
    }

    /// Pauses `repo`, or every repository of `identity`.
    pub(crate) fn pause(&self, identity: &str, repo: Option<&str>) {
        {
            let mut flags = self.flags();
            match repo {
                Some(repo) => flags.paused_repos.insert(repo.to_owned()),
                None => flags.paused.insert(identity.to_owned()),
            };
        }
        self.bump();
    }

    /// Lifts the pause by hand of `repo` (and its automatic pause, until the next interactive
    /// activity after `now_ms`), or of `identity`.
    pub(crate) fn resume(&self, identity: &str, repo: Option<&str>, now_ms: i64) {
        {
            let mut flags = self.flags();
            match repo {
                Some(repo) => {
                    flags.paused_repos.remove(repo);
                    flags.auto.remove(repo);
                    flags.resumed.insert(repo.to_owned(), now_ms);
                }
                None => {
                    flags.paused.remove(identity);
                }
            }
        }
        self.bump();
    }

    /// When `repo` was last resumed by hand.
    pub(crate) fn resumed_at(&self, repo: &str) -> Option<i64> {
        self.flags().resumed.get(repo).copied()
    }

    /// Sets or clears the automatic pause of `repo`; `true` when it changed.
    pub(crate) fn set_auto(&self, repo: &str, until_ms: Option<i64>) -> bool {
        let changed = {
            let mut flags = self.flags();
            let before = flags.auto.get(repo).copied();
            match until_ms {
                Some(until) => flags.auto.insert(repo.to_owned(), until),
                None => flags.auto.remove(repo),
            };
            before.is_some() != until_ms.is_some()
        };
        if changed {
            self.bump();
        }
        changed
    }

    /// Sets or clears the daily cap's hold of `identity`; `true` when it changed.
    pub(crate) fn set_daily(&self, identity: &str, until_ms: Option<i64>) -> bool {
        let changed = {
            let mut flags = self.flags();
            let before = match until_ms {
                Some(until) => flags.daily.insert(identity.to_owned(), until),
                None => flags.daily.remove(identity),
            };
            before != until_ms
        };
        if changed {
            self.bump();
        }
        changed
    }

    /// The scheduler's gates of `repo` of `identity` at `now_ms`.
    pub(crate) fn gates(&self, identity: &str, repo: &str, now_ms: i64) -> Gates {
        let flags = self.flags();
        let auto = flags.auto.get(repo).is_some_and(|&until| now_ms < until);
        let mut paused_repos = BTreeSet::new();
        if flags.paused_repos.contains(repo) || auto {
            paused_repos.insert(repo.to_owned());
        }
        Gates {
            killed: flags.killed,
            paused: flags.paused.contains(identity),
            paused_repos,
            daily_cap_until_ms: flags.daily.get(identity).copied(),
            busy_repos: BTreeSet::new(),
        }
    }

    /// The hold in force on `repo` of `identity` at `now_ms`.
    pub(crate) fn hold(&self, identity: &str, repo: &str, now_ms: i64) -> Option<Hold> {
        self.gates(identity, repo, now_ms).hold(repo, now_ms)
    }

    /// The control state of `identity` and its repositories (by name).
    pub(crate) fn status(&self, identity: &str, repos: &[String], now_ms: i64) -> ControlStatus {
        let rows = repos
            .iter()
            .map(|repo| {
                let hold = self.hold(identity, repo, now_ms);
                let flags = self.flags();
                RepoControl {
                    repo: repo.clone(),
                    paused: flags.paused_repos.contains(repo),
                    auto_paused_until_ms: flags.auto.get(repo).copied().filter(|&t| now_ms < t),
                    hold: hold.map(|h| hold_name(h).to_owned()),
                }
            })
            .collect();
        let flags = self.flags();
        ControlStatus {
            identity: identity.to_owned(),
            killed: flags.killed,
            paused: flags.paused.contains(identity),
            daily_cap_until_ms: flags.daily.get(identity).copied().filter(|&t| now_ms < t),
            repos: rows,
        }
    }

    /// The agent CLI's deny rules for the operator token's file: read and edit refused. A rule
    /// list is not a boundary (see the module documentation), but it keeps the file out of a
    /// session that is only wandering.
    pub(crate) fn deny_rules(&self) -> Vec<String> {
        let Some(file) = &self.inner.operator_file else {
            return Vec::new();
        };
        let rule_path = rule_path(file);
        ["Read", "Edit", "Write"]
            .iter()
            .map(|tool| format!("{tool}({rule_path})"))
            .collect()
    }

    /// Removes the operator token's file at a clean stop, when it still holds this daemon's
    /// token: a token another daemon has since written there is left alone. Best effort.
    pub(crate) fn withdraw(&self) {
        if cfg!(mutant_control_keep_operator_token) {
            // Mutant: the token is left behind.
            return;
        }
        let Some(file) = &self.inner.operator_file else {
            return;
        };
        let ours = std::fs::read_to_string(file)
            .is_ok_and(|text| digest(text.trim()) == self.inner.operator);
        if ours && let Err(e) = std::fs::remove_file(file) {
            crate::log::log!(
                "bosund: cannot remove the operator token {}: {e}",
                file.display()
            );
        }
    }
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX))
}

/// The status of `served`'s identity.
fn status_of(state: &State, served: &Served) -> ControlStatus {
    let repos: Vec<String> = served.repos.iter().map(|(name, _)| name.clone()).collect();
    state.control.status(&served.name, &repos, now_ms())
}

/// The repository `repo` of `served`, by name; refused when the identity has none of that name.
fn check_repo(served: &Served, repo: Option<&str>) -> Result<(), ErrorObject> {
    match repo {
        Some(name) if !served.repos.iter().any(|(n, _)| n == name) => Err(ErrorObject::new(
            code::INVALID_PARAMS,
            format!("identity {} has no repository named {name:?}", served.name),
        )),
        _ => Ok(()),
    }
}

/// The `control.*` methods. The switches check the operator token before anything else, so a
/// caller without it learns nothing of the identities either.
pub(crate) async fn dispatch(state: &State, name: &str, p: Value) -> Result<Value, ErrorObject> {
    let control = &state.control;
    match name {
        method::ControlStatus::NAME => {
            call::<method::ControlStatus, _, _>(p, |p| async move {
                Ok(status_of(state, state.identity(&p.identity)?))
            })
            .await
        }
        method::ControlPause::NAME | method::ControlResume::NAME => {
            let pause = name == method::ControlPause::NAME;
            call::<method::ControlPause, _, _>(p, |p| async move {
                control.authorize(&p.operator)?;
                let served = state.identity(&p.identity)?;
                check_repo(served, p.repo.as_deref())?;
                if pause {
                    control.pause(&served.name, p.repo.as_deref());
                } else {
                    control.resume(&served.name, p.repo.as_deref(), now_ms());
                }
                served.events.publish(EventBody::ControlChanged {
                    state: if pause { "paused" } else { "running" }.to_owned(),
                    repo: p.repo.clone(),
                });
                Ok(status_of(state, served))
            })
            .await
        }
        method::ControlKill::NAME => {
            call::<method::ControlKill, _, _>(p, |p| async move {
                control.authorize(&p.operator)?;
                control.set_killed(!p.release);
                let state_name = if p.release { "running" } else { "killed" };
                crate::log::log!(
                    "bosund: the kill switch is {}",
                    if p.release { "released" } else { "thrown" }
                );
                for served in state.identities.values() {
                    served.events.publish(EventBody::ControlChanged {
                        state: state_name.to_owned(),
                        repo: None,
                    });
                }
                Ok(KillResult {
                    killed: control.killed(),
                })
            })
            .await
        }
        _ => Err(ErrorObject::new(
            code::METHOD_NOT_FOUND,
            format!("no method {name}"),
        )),
    }
}

/// An absolute path in the agent CLI's rule syntax: `//` and the path with forward slashes, a
/// Windows drive as its lowercase letter (`//c/Users/...`).
fn rule_path(path: &Path) -> String {
    let text = path.to_string_lossy().replace('\\', "/");
    let bytes = text.as_bytes();
    if bytes.len() >= 2 && bytes[1] == b':' && bytes[0].is_ascii_alphabetic() {
        format!(
            "//{}{}",
            char::from(bytes[0].to_ascii_lowercase()),
            &text[2..]
        )
    } else {
        format!("/{text}")
    }
}

/// Writes a fresh operator token to `file` (user-only: mode 0600 on Unix, the profile's ACL on
/// Windows) and returns it. Refuses a file inside a repository's checkout.
pub(crate) fn issue(file: &Path, checkouts: &[&Path]) -> Result<String, String> {
    if let Some(inside) = checkouts.iter().find(|c| file.starts_with(c)) {
        return Err(format!(
            "the operator token's file {} is inside the checkout {}: an agent works there; set {} \
             to a file outside every repository",
            file.display(),
            inside.display(),
            bosun_client::operator::FILE_ENV
        ));
    }
    let token = fresh_token();
    let shown = |e: std::io::Error| format!("the operator token's file {}: {e}", file.display());
    if let Some(dir) = file.parent() {
        std::fs::create_dir_all(dir).map_err(shown)?;
    }
    let tmp = file.with_extension("tmp");
    let _ = std::fs::remove_file(&tmp);
    {
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut out = options.open(&tmp).map_err(shown)?;
        std::io::Write::write_all(&mut out, format!("{token}\n").as_bytes()).map_err(shown)?;
    }
    std::fs::rename(&tmp, file).map_err(shown)?;
    Ok(token)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_token_is_checked() {
        let token = fresh_token();
        assert!(token.starts_with("bosun-operator-"), "{token}");
        let control = Control::with_token(&token, None);
        assert!(control.authorize(&token).is_ok());
        assert!(control.authorize("bosun-operator-forged").is_err());
        assert!(control.authorize("").is_err());
    }

    #[test]
    fn holds_and_their_order() {
        let control = Control::default();
        assert_eq!(control.hold("me", "a", 0), None);
        control.pause("me", Some("a"));
        assert_eq!(control.hold("me", "a", 0), Some(Hold::Pause));
        assert_eq!(
            control.hold("me", "b", 0),
            None,
            "a repository's pause is its own"
        );
        control.resume("me", Some("a"), 5);
        assert_eq!(control.hold("me", "a", 0), None);
        assert_eq!(control.resumed_at("a"), Some(5));

        control.pause("me", None);
        assert_eq!(control.hold("me", "b", 0), Some(Hold::Pause));
        assert_eq!(
            control.hold("other", "c", 0),
            None,
            "an identity's pause is its own"
        );
        control.resume("me", None, 6);
        assert_eq!(control.hold("me", "b", 0), None);

        assert!(control.set_auto("a", Some(100)));
        assert_eq!(control.hold("me", "a", 99), Some(Hold::Pause));
        assert_eq!(control.hold("me", "a", 100), None, "the idle time ran out");
        assert!(!control.set_auto("a", Some(200)));
        assert!(control.set_auto("a", None));

        assert!(control.set_daily("me", Some(50)));
        assert_eq!(control.hold("me", "a", 49), Some(Hold::DailyCap));
        assert_eq!(control.hold("me", "a", 50), None, "local midnight passed");

        control.set_killed(true);
        assert!(control.board_writes_stopped());
        assert_eq!(control.hold("other", "z", 0), Some(Hold::Kill));
        control.set_killed(false);
        assert!(!control.board_writes_stopped());
    }

    #[test]
    fn deny_rules_name_the_file() {
        assert_eq!(
            rule_path(Path::new(r"C:\Users\u\x.token")),
            "//c/Users/u/x.token"
        );
        assert_eq!(rule_path(Path::new("/home/u/x.token")), "//home/u/x.token");
        let control = Control::with_token("t", Some(PathBuf::from("/run/b.operator")));
        assert_eq!(
            control.deny_rules(),
            [
                "Read(//run/b.operator)",
                "Edit(//run/b.operator)",
                "Write(//run/b.operator)"
            ]
        );
    }

    #[test]
    fn the_token_file_is_written_and_never_in_a_checkout() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("run").join("bosund.operator");
        let token = issue(&file, &[]).unwrap();
        assert_eq!(std::fs::read_to_string(&file).unwrap().trim(), token);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&file).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600);
        }
        // A restart writes a new one.
        assert_ne!(issue(&file, &[]).unwrap(), token);
        let checkout = dir.path().join("run");
        assert!(issue(&file, &[&checkout]).is_err());
    }

    #[test]
    fn a_clean_stop_removes_only_its_own_token() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("bosund.operator");
        let token = issue(&file, &[]).unwrap();
        let control = Control::with_token(&token, Some(file.clone()));
        control.withdraw();
        assert!(!file.exists(), "the daemon's own token is left behind");
        // Withdrawing again, with the file gone, does nothing.
        control.withdraw();

        // Another daemon wrote its token there since: it stays.
        let mine = issue(&file, &[]).unwrap();
        let theirs = issue(&file, &[]).unwrap();
        Control::with_token(&mine, Some(file.clone())).withdraw();
        assert_eq!(std::fs::read_to_string(&file).unwrap().trim(), theirs);
    }
}
