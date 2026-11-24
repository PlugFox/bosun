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
//! # The pace (#60, #106)
//!
//! [`Pace`] says when the board is read next. A local file is read every tick. A Projects v2
//! board follows [`Pacer`]: every 30 s while an item is in flight or the last poll found a
//! change, backing off to five minutes while the board stays idle.

use std::fmt;
use std::path::Path;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use bosun_config::{Identity, Repo};
use bosun_git::Git;
use bosun_identity::{Env, inherited};
use bosun_queue::projects_v2::{Binding, GhCli, Pacer, ProjectRef};
use bosun_queue::{Card, Changes, Cursor, ItemId, Owned, ProjectsV2, Queue, QueueError};

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
    /// a change.
    pub(crate) fn polled(&mut self, now: Duration, active: bool, changed: bool) {
        if let Some(pacer) = &mut self.pacer {
            self.due = Some(now + pacer.next(active, changed, None, 0));
        }
    }
}

/// A queue backend, shared by the loop and its tasks.
pub(crate) type Shared = Arc<dyn Queue + Send + Sync>;

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

    fn remove_label(&self, item: ItemId, label: &str) -> Result<(), QueueError> {
        self.queue()?.remove_label(item, label)
    }

    fn lint_target(&self, item: ItemId) -> Result<Card, QueueError> {
        self.queue()?.lint_target(item)
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
        let bundle = identity::bundle(&name, &identity, &path).map_err(QueueError::Setup)?;
        let binding = Binding::new(
            origin_repository(&path)?,
            [project.clone()],
            identity.author_allowlist.iter().cloned(),
        );
        let env = Env::tool(&bundle, &inherited());
        let mut transport = GhCli::new("gh", env.iter().map(|(k, v)| (k.to_owned(), v.to_owned())));
        if bundle.host != bosun_config::DEFAULT_GITHUB_HOST {
            transport = transport.with_hostname(bundle.host.clone());
        }
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

    use super::{Pace, repository_of};

    /// The reads of `pace` over `span` of a fake clock ticking every `tick`, each reporting
    /// `active` and no change.
    fn reads(pace: &mut Pace, tick: Duration, span: Duration, active: bool) -> Vec<u64> {
        let mut at = Vec::new();
        let mut now = Duration::ZERO;
        while now <= span {
            if pace.due(now) {
                at.push(now.as_secs());
                pace.polled(now, active, false);
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
        pace.polled(resumed, true, false);
        assert!(pace.due(resumed + ACTIVE));
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
