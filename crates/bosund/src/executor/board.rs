//! The board of a repository's loop (#99): the queue backend its configuration names, connected
//! when the loop first reads it.
//!
//! A local file needs no connection. A Projects v2 board is resolved by
//! [`bosun_queue::ProjectsV2::connect`] (its project, Status field and columns), which talks to
//! GitHub; that runs in the loop's first poll, not at daemon start, so a board that does not
//! match the configuration pauses its repository ([`QueueError::Setup`]) and nothing else.

use std::fmt;
use std::path::Path;
use std::sync::{Arc, Mutex, PoisonError};

use bosun_config::{Identity, Repo};
use bosun_git::Git;
use bosun_identity::{Env, inherited};
use bosun_queue::projects_v2::{Binding, GhCli, ProjectRef};
use bosun_queue::{Card, Changes, Cursor, ItemId, Owned, ProjectsV2, Queue, QueueError};

use super::identity;

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
    use super::repository_of;

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
