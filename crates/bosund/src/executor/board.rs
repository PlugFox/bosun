//! The board of a repository's loop (#99): the queue backend its configuration names, connected
//! when the loop first reads it.
//!
//! A local file needs no connection. A Projects v2 board is resolved by
//! [`bosun_queue::ProjectsV2::connect`] (its project, Status field and columns), which talks to
//! GitHub; that runs in the loop's first poll, not at daemon start, so a board that does not
//! match the configuration pauses its repository ([`QueueError::Setup`]) and nothing else.
//!
//! # While an item is in flight (#106)
//!
//! The loop keeps polling the board while an item is in flight, at the board's active pace
//! (the user's commands of #65 come from the same poll, [`super::commands`]). An issue
//! deleted, transferred or converted mid-run leaves the board's listing, and a card missing
//! from two polls cancels the item and kills its session, not only at claim.
//!
//! A closed issue stays on the board with [`Card::closed`]: the user's cancel, at once
//! (#111, [`super::commands`]); a closed card is never claimed.
//!
//! # One board per repository (#111)
//!
//! [`Board::of`] builds a repository's board once, at daemon start; the loop and the API's
//! `queue.*` and `item.restart` share it, so a Projects v2 board is read and written by the
//! same backend whoever asks, and connected once.
//!
//! # Visibility (#5, #111)
//!
//! The verdict output tails of the sticky report are published only on a repository known to
//! be private. [`Visibility`] asks GitHub once (`repository { visibility }` through the
//! identity's `gh`, for the repository `origin` names) and keeps the answer; until an answer
//! comes, and for a repository GitHub cannot tell about, the repository counts as public.
//!
//! # The pace (#60, #106)
//!
//! [`Pace`] says when the board is read next. A local file is read every tick. A Projects v2
//! board follows [`Pacer`]: every 30 s while an item is in flight or the last poll found a
//! change, backing off to five minutes while the board stays idle.
//!
//! The quota counts too ([`Queue::rate_limit`]): while less than [`RESERVE_PERCENT`] of it is
//! left, the next read waits for the quota's reset (on the pace's clock, never more than an
//! hour, the length of GitHub's window, so a wrong clock cannot stop the reads), keeping the
//! rest for the loop's writes. At or above the reserve the pace is the idleness one.

use std::fmt;
use std::path::Path;
use std::sync::{Arc, Mutex, OnceLock, PoisonError};
use std::time::Duration;

use bosun_config::{Config, Identity, Queue as QueueConfig, Repo};
use bosun_git::Git;
use bosun_identity::{Env, inherited};
use bosun_queue::projects_v2::{Binding, GhCli, Pacer, ProjectRef, RESERVE_PERCENT};
use bosun_queue::{
    Card, Changes, Cursor, ItemId, LocalFile, Owned, ProjectsV2, Queue, QueueError, RateLimit,
};

use super::identity;

/// When the board is read next, on the loop's monotonic clock.
#[derive(Debug, Clone)]
pub(crate) struct Pace {
    /// `None`: every tick.
    pacer: Option<Pacer>,
    /// How early a read may come: half a tick, so a tick that lands just short of the due
    /// time does not wait a whole tick more.
    slack: Duration,
    /// No read before this; `None` before the first.
    due: Option<Duration>,
}

impl Pace {
    /// A pace that backs off while idle (`idle_backoff`), or reads at every tick of `tick`.
    pub(crate) fn new(idle_backoff: bool, tick: Duration) -> Self {
        Self {
            // Mutant (`mutant_loop_no_idle_backoff`): an idle board is read at every tick.
            pacer: (idle_backoff && !cfg!(mutant_loop_no_idle_backoff)).then(Pacer::new),
            slack: tick / 2,
            due: None,
        }
    }

    /// Whether the board may be read at `now`.
    pub(crate) fn due(&self, now: Duration) -> bool {
        self.due.is_none_or(|due| now + self.slack >= due)
    }

    /// The board was read at `now`; `active`: an item is in flight, `changed`: the read found
    /// a change; `rate`: the board's quota as last seen, `wall`: the Unix seconds at `now`.
    pub(crate) fn polled(
        &mut self,
        now: Duration,
        active: bool,
        changed: bool,
        rate: Option<&RateLimit>,
        wall: u64,
    ) {
        let Some(pacer) = &mut self.pacer else {
            return;
        };
        let mut wait = pacer.next(active, changed, None, 0);
        // Mutant (`mutant_loop_ignores_quota`): a nearly spent quota is read at the usual pace.
        if let Some(rate) = rate.filter(|r| low(r) && !cfg!(mutant_loop_ignores_quota)) {
            let until_reset = Duration::from_secs(rate.reset.saturating_sub(wall));
            wait = wait.max(until_reset.min(WINDOW));
        }
        self.due = Some(now + wait);
    }
}

/// The longest wait for a quota's reset: GitHub's quota window.
const WINDOW: Duration = Duration::from_hours(1);

/// Whether less than [`RESERVE_PERCENT`] of the quota is left.
fn low(rate: &RateLimit) -> bool {
    rate.remaining.saturating_mul(100) < rate.limit.saturating_mul(RESERVE_PERCENT)
}

/// A queue backend, shared by the loop and its tasks.
pub(crate) type Shared = Arc<dyn Queue + Send + Sync>;

/// A repository's board, built once at start and shared by its loop and the API.
#[derive(Clone)]
pub(crate) struct Board {
    /// The backend.
    pub(crate) queue: Shared,
    /// Whether its reads follow the poller's pace (a Projects v2 board) rather than every tick.
    pub(crate) idle_backoff: bool,
    /// Whether the repository is public.
    pub(crate) visibility: Arc<Visibility>,
}

impl fmt::Debug for Board {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Board")
            .field("idle_backoff", &self.idle_backoff)
            .field("visibility", &self.visibility)
            .finish_non_exhaustive()
    }
}

impl Board {
    /// The board of `repo` in `config`: a local file, or a Projects v2 board connected on
    /// first use. Nothing is read here. The error says why the repository has none this
    /// version reads.
    pub(crate) fn of(config: &Config, repo: &Repo) -> Result<Self, String> {
        let identity = config
            .identities
            .get(&repo.identity)
            .ok_or_else(|| format!("the identity {} is not configured", repo.identity))?;
        let visibility = Arc::new(visibility(identity, repo));
        match &repo.queue {
            Some(QueueConfig::LocalFile { file }) => Ok(Self {
                queue: Arc::new(LocalFile::new(file)),
                idle_backoff: false,
                visibility,
            }),
            Some(QueueConfig::Projects { owner, number }) => Ok(Self {
                queue: Arc::new(projects_v2(identity, repo, owner, *number)),
                idle_backoff: true,
                visibility,
            }),
            Some(_) => Err("this queue kind is not read by this version".to_owned()),
            None => Err("the repository has no queue".to_owned()),
        }
    }
}

/// How a [`Visibility`] asks: `Ok(true)` for a public repository.
pub(crate) type ReadVisibility = Box<dyn Fn() -> Result<bool, String> + Send + Sync>;

/// Whether a repository is public, asked once; see the module docs.
pub(crate) struct Visibility {
    read: ReadVisibility,
    known: OnceLock<bool>,
}

impl fmt::Debug for Visibility {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Visibility")
            .field("known", &self.known.get())
            .finish_non_exhaustive()
    }
}

impl Visibility {
    pub(crate) fn new(read: ReadVisibility) -> Self {
        Self {
            read,
            known: OnceLock::new(),
        }
    }

    /// A repository whose visibility is known already.
    #[cfg(test)]
    pub(crate) fn fixed(public: bool) -> Self {
        let visibility = Self::new(Box::new(|| Err("not asked".to_owned())));
        let _ = visibility.known.set(public);
        visibility
    }

    /// Whether the repository is public: the answer kept, else GitHub asked (blocking). A
    /// question that fails counts as public and is asked again next time.
    pub(crate) fn public(&self) -> bool {
        if let Some(public) = self.known.get() {
            return *public;
        }
        match (self.read)() {
            Ok(public) => *self.known.get_or_init(|| public),
            Err(e) => {
                crate::log::log!(
                    "bosund: the repository's visibility cannot be read ({e}); verdict output \
                     stays off its reports"
                );
                true
            }
        }
    }
}

/// The visibility of `repo`, asked through its identity's `gh` for the repository its
/// `origin` names.
fn visibility(identity: &Identity, repo: &Repo) -> Visibility {
    let (name, identity, path) = (repo.identity.clone(), identity.clone(), repo.path.clone());
    Visibility::new(Box::new(move || {
        let transport = gh(&name, &identity, &path).map_err(|e| e.to_string())?;
        let repository = origin_repository(&path).map_err(|e| e.to_string())?;
        bosun_queue::projects_v2::is_public(&transport, &repository).map_err(|e| e.to_string())
    }))
}

/// `gh api graphql` under the identity's own `gh` configuration (D5 of #7).
fn gh(name: &str, identity: &Identity, path: &Path) -> Result<GhCli, QueueError> {
    let bundle = identity::bundle(name, identity, path).map_err(QueueError::Setup)?;
    let env = Env::tool(&bundle, &inherited());
    let mut transport = GhCli::new("gh", env.iter().map(|(k, v)| (k.to_owned(), v.to_owned())));
    if bundle.host != bosun_config::DEFAULT_GITHUB_HOST {
        transport = transport.with_hostname(bundle.host.clone());
    }
    Ok(transport)
}

/// How a [`Lazy`] board is connected.
pub(crate) type Connect = Box<dyn Fn() -> Result<Shared, QueueError> + Send + Sync>;

/// A board connected on first use: every call before a connection succeeds tries to connect
/// and fails with the connection's error; afterwards each call goes to the backend.
pub(crate) struct Lazy {
    connect: Connect,
    queue: Mutex<Option<Shared>>,
}

impl fmt::Debug for Lazy {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Lazy")
            .field("connected", &self.lock().is_some())
            .finish_non_exhaustive()
    }
}

impl Lazy {
    pub(crate) fn new(connect: Connect) -> Self {
        Self {
            connect,
            queue: Mutex::new(None),
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Option<Shared>> {
        self.queue.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn queue(&self) -> Result<Shared, QueueError> {
        if let Some(queue) = self.lock().as_ref() {
            return Ok(queue.clone());
        }
        // Connected outside the lock: a slow GitHub holds no other caller.
        let queue = (self.connect)()?;
        Ok(self.lock().get_or_insert(queue).clone())
    }
}

impl Queue for Lazy {
    fn poll(&self, cursor: Option<&Cursor>) -> Result<Changes, QueueError> {
        self.queue()?.poll(cursor)
    }

    fn project(&self, item: ItemId, state: Owned, note: Option<&str>) -> Result<(), QueueError> {
        self.queue()?.project(item, state, note)
    }

    fn comment(&self, item: ItemId, body: &str, key: &str) -> Result<(), QueueError> {
        self.queue()?.comment(item, body, key)
    }

    fn upsert_comment(&self, item: ItemId, key: &str, body: &str) -> Result<(), QueueError> {
        self.queue()?.upsert_comment(item, key, body)
    }

    fn add_label(&self, item: ItemId, label: &str) -> Result<(), QueueError> {
        self.queue()?.add_label(item, label)
    }

    fn remove_label(&self, item: ItemId, label: &str) -> Result<(), QueueError> {
        self.queue()?.remove_label(item, label)
    }

    fn lint_target(&self, item: ItemId) -> Result<Card, QueueError> {
        self.queue()?.lint_target(item)
    }

    /// The connected backend's quota; none before a connection (asking does not connect).
    fn rate_limit(&self) -> Option<RateLimit> {
        self.lock().as_ref()?.rate_limit()
    }
}

/// `owner/name` of a GitHub remote URL (`https://host/owner/name.git`, `git@host:owner/name`).
pub(crate) fn repository_of(url: &str) -> Option<String> {
    // `host_and_owner` tells a remote from a local path and finds the owner.
    let (_, owner) = bosun_identity::host_and_owner(url)?;
    let path = match url.split_once("://") {
        Some((_, rest)) => rest.split_once('/')?.1,
        None => url.split_once(':')?.1,
    };
    let mut segments = path.trim_matches('/').split('/');
    let (Some(first), Some(name), None) = (segments.next(), segments.next(), segments.next())
    else {
        return None;
    };
    let name = name.strip_suffix(".git").unwrap_or(name);
    (first == owner && !name.is_empty()).then(|| format!("{owner}/{name}"))
}

/// The Projects v2 board `owner`/`number` of `repo`, under its `identity`: `gh api graphql`
/// with the identity's own `gh` configuration (D5 of #7), the issues of the repository its
/// `origin` names, the identity's author allowlist (#2).
pub(crate) fn projects_v2(identity: &Identity, repo: &Repo, owner: &str, number: u64) -> Lazy {
    let (name, identity, path) = (repo.identity.clone(), identity.clone(), repo.path.clone());
    let project = ProjectRef::new(owner, number);
    Lazy::new(Box::new(move || {
        let transport = gh(&name, &identity, &path)?;
        let binding = Binding::new(
            origin_repository(&path)?,
            [project.clone()],
            identity.author_allowlist.iter().cloned(),
        );
        Ok(Arc::new(ProjectsV2::connect(transport, binding)?) as Shared)
    }))
}

/// `owner/name` of the repository's `origin`.
fn origin_repository(path: &Path) -> Result<String, QueueError> {
    let url = Git::new(path)
        .run(&["remote", "get-url", "origin"], None)
        .map_err(|e| QueueError::Setup(format!("the repository's origin: {e}")))?;
    let url = url.trim();
    repository_of(url).ok_or_else(|| {
        QueueError::Setup(format!(
            "the origin `{url}` names no GitHub repository as owner/name"
        ))
    })
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use bosun_queue::projects_v2::{ACTIVE, IDLE_MAX};

    use super::{Pace, Visibility, repository_of};

    /// #111: the visibility is asked until GitHub answers, then kept; no answer is public.
    #[test]
    fn the_visibility_is_asked_until_answered_and_then_kept() {
        use std::sync::Arc;
        use std::sync::atomic::{AtomicUsize, Ordering};
        let asked = Arc::new(AtomicUsize::new(0));
        let counter = asked.clone();
        let visibility = Visibility::new(Box::new(move || {
            match counter.fetch_add(1, Ordering::SeqCst) {
                0 => Err("offline".to_owned()),
                _ => Ok(false),
            }
        }));
        assert!(visibility.public(), "no answer: public");
        assert!(!visibility.public(), "private");
        assert!(!visibility.public());
        assert_eq!(asked.load(Ordering::SeqCst), 2);
        assert!(Visibility::fixed(true).public());
    }

    /// The reads of `pace` over `span` of a fake clock ticking every `tick`, each reporting
    /// `active` and no change.
    fn reads(pace: &mut Pace, tick: Duration, span: Duration, active: bool) -> Vec<u64> {
        let mut at = Vec::new();
        let mut now = Duration::ZERO;
        while now <= span {
            if pace.due(now) {
                at.push(now.as_secs());
                pace.polled(now, active, false, None, 0);
            }
            now += tick;
        }
        at
    }

    #[test]
    fn an_idle_board_is_read_less_often_than_an_active_one() {
        let hour = Duration::from_hours(1);
        let active = reads(&mut Pace::new(true, ACTIVE), ACTIVE, hour, true);
        let idle = reads(&mut Pace::new(true, ACTIVE), ACTIVE, hour, false);
        // Every 30 s tick while active.
        assert_eq!(active.len(), 121, "{active:?}");
        assert!(active.windows(2).all(|w| w[1] - w[0] == 30), "{active:?}");
        // Idle: 60 s, 120 s, 240 s, then the cap.
        assert_eq!(&idle[..5], [0, 60, 180, 420, 720], "{idle:?}");
        assert!(
            idle.windows(2)
                .skip(4)
                .all(|w| w[1] - w[0] == IDLE_MAX.as_secs()),
            "{idle:?}"
        );
        assert!(
            idle.len() * 5 < active.len(),
            "{} vs {}",
            idle.len(),
            active.len()
        );

        // A change, or an item in flight, brings the board back to every tick.
        let mut pace = Pace::new(true, ACTIVE);
        reads(&mut pace, ACTIVE, hour, false);
        let resumed = hour + IDLE_MAX;
        assert!(pace.due(resumed));
        pace.polled(resumed, true, false, None, 0);
        assert!(pace.due(resumed + ACTIVE));
    }

    /// #106: under 10% of the quota left, the next read waits for the reset; at 50% the pace
    /// is the normal one. The quota is the board's, read through the loop's [`Lazy`] wrapper.
    #[test]
    fn a_low_quota_waits_for_the_reset() {
        use std::sync::Arc;

        use bosun_queue::projects_v2::fake::FakeGitHub;
        use bosun_queue::projects_v2::{Binding, ProjectRef};
        use bosun_queue::{Column, ProjectsV2, Queue, RateLimit};

        use super::{Lazy, Shared};

        let github = Arc::new(FakeGitHub::new());
        github.add_project("synthetic-owner", 1, &Column::ALL.map(Column::name));
        let board = {
            let github = github.clone();
            Lazy::new(Box::new(move || {
                let binding = Binding::new(
                    "synthetic-owner/demo",
                    [ProjectRef::new("synthetic-owner", 1)],
                    ["synthetic-owner"],
                );
                Ok(Arc::new(ProjectsV2::connect(github.clone(), binding)?) as Shared)
            }))
        };
        assert_eq!(board.rate_limit(), None, "not connected yet");
        let quota = |remaining: u64, reset: u64| RateLimit {
            limit: 5000,
            remaining,
            used: 5000 - remaining,
            reset,
        };
        let wall = 1_800_000_000;
        let reset_in = Duration::from_mins(20);
        let reset = wall + reset_in.as_secs();
        let start = Duration::from_secs(100);

        // 5% left, the reset 20 minutes away: no read before it, one right after.
        github.set_rate_limit(quota(250, reset));
        board.poll(None).unwrap();
        let rate = board.rate_limit();
        assert_eq!(rate, Some(quota(250, reset)), "forwarded by the wrapper");
        let mut pace = Pace::new(true, ACTIVE);
        assert!(pace.due(start));
        pace.polled(start, true, false, rate.as_ref(), wall);
        let mut now = start + ACTIVE;
        while now + ACTIVE / 2 < start + reset_in {
            assert!(
                !pace.due(now),
                "read at {now:?}, the reset at {:?}",
                start + reset_in
            );
            now += ACTIVE;
        }
        assert!(pace.due(start + reset_in), "read right after the reset");

        // 50% left: the normal pace, active and idle.
        github.set_rate_limit(quota(2500, reset));
        board.poll(None).unwrap();
        let rate = board.rate_limit();
        assert_eq!(rate, Some(quota(2500, reset)));
        for active in [true, false] {
            let mut normal = Pace::new(true, ACTIVE);
            let mut pace = Pace::new(true, ACTIVE);
            normal.polled(start, active, false, None, 0);
            pace.polled(start, active, false, rate.as_ref(), wall);
            assert!(pace.due(start + ACTIVE * if active { 1 } else { 2 }));
            for tick in 1..=40 {
                let at = start + ACTIVE * tick;
                assert_eq!(pace.due(at), normal.due(at), "{active} {tick}");
            }
        }
    }

    #[test]
    fn a_local_file_is_read_at_every_tick() {
        let tick = Duration::from_secs(2);
        let mut pace = Pace::new(false, tick);
        let at = reads(&mut pace, tick, Duration::from_mins(1), false);
        assert_eq!(at.len(), 31, "{at:?}");
    }

    #[test]
    fn the_repository_of_a_remote_url() {
        for (url, want) in [
            ("https://github.com/acme/demo.git", Some("acme/demo")),
            ("https://github.com/acme/demo", Some("acme/demo")),
            ("https://github.com/acme/demo/", Some("acme/demo")),
            ("git@github.com:acme/demo.git", Some("acme/demo")),
            (
                "ssh://git@github.example:22/acme/demo.git",
                Some("acme/demo"),
            ),
            ("C:/repos/demo", None),
            ("https://github.com/acme", None),
        ] {
            assert_eq!(repository_of(url).as_deref(), want, "{url}");
        }
    }
}
