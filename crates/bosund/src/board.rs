//! The `queue.*` methods (#4, #5, #70): each board as the Queue tab shows it, and the static
//! lint of one card.
//!
//! The board is read through the backend the loop reads (#111: the repository's
//! [`crate::executor::board::Board`], a local file re-read whole or the Projects v2 board the
//! loop connected), so the answer is the board now, not the loop's last poll. Only the
//! repositories of the identity the request names are read (#2, rule 4): their boards and their
//! items live in that identity's configuration and store only.
//!
//! The lint is the loop's static lint (`executor::claim::static_lint`): programs resolve through
//! the repository's verdict shell in its checkout, and `blocked-by` targets must be on the board.
//! It runs on the blocking pool, as the loop's does.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use bosun_api::rpc::{ErrorObject, code};
use bosun_api::{
    CardView, ColumnView, LintFinding, LintResult, Method, QueueView, RepoQueue, method,
};
use bosun_config::Repo;
use bosun_queue::lint::verdict_shell;
use bosun_queue::{Card, Column, Lint, LintContext, Severity, ShellResolver, Snapshot};
use bosun_store::StoredItem;
use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::api::{call, failed};
use crate::executor::board::Shared;
use crate::overview::queue_kind;
use crate::state::{Served, State};

/// A configured repository: its name and configuration.
type NamedRepo = (String, Repo);

/// The repositories a request of `served`'s identity may read: its own.
fn repos_of<'a>(state: &'a State, served: &'a Served) -> Vec<&'a NamedRepo> {
    if cfg!(mutant_api_queue_other_identity) {
        // Mutant: the repositories of every identity the daemon serves.
        let _ = served;
        return state
            .identities
            .values()
            .flat_map(|s| s.repos.iter())
            .collect();
    }
    served.repos.iter().collect()
}

/// The repository `name` of `served`'s identity; refused when it has none of that name.
pub(crate) fn repo_named<'a>(
    state: &'a State,
    served: &'a Served,
    name: &str,
) -> Result<&'a NamedRepo, ErrorObject> {
    repos_of(state, served)
        .into_iter()
        .find(|(n, _)| n == name)
        .ok_or_else(|| {
            ErrorObject::new(
                code::INVALID_PARAMS,
                format!("identity {} has no repository named {name:?}", served.name),
            )
        })
}

/// The `queue.*` methods.
pub(crate) async fn dispatch(state: &State, name: &str, p: Value) -> Result<Value, ErrorObject> {
    match name {
        method::QueueGet::NAME => {
            call::<method::QueueGet, _, _>(p, |p| async move {
                let served = state.identity(&p.identity)?;
                let repos = match &p.repo {
                    Some(name) => vec![repo_named(state, served, name)?],
                    None => repos_of(state, served),
                };
                let items = served.store.reader().items(false).await.map_err(failed)?;
                let mut out = Vec::new();
                for named in repos {
                    out.push(repo_queue(state, served, named, &items).await);
                }
                Ok(QueueView {
                    identity: served.name.clone(),
                    repos: out,
                })
            })
            .await
        }
        method::QueueLint::NAME => {
            call::<method::QueueLint, _, _>(p, |p| async move {
                let served = state.identity(&p.identity)?;
                let (name, repo) = repo_named(state, served, &p.repo)?;
                let board =
                    board_of(served, name).map_err(|e| ErrorObject::new(code::FAILED, e))?;
                let card_id = bosun_queue::ItemId(p.card);
                let (cards, card) = blocking(move || {
                    let changes = board.poll(None)?;
                    let card = changes.card(card_id).cloned();
                    Ok::<_, bosun_queue::QueueError>((changes.cards, card))
                })
                .await
                .map_err(failed)?;
                let card = card.ok_or_else(|| {
                    ErrorObject::new(
                        code::NOT_FOUND,
                        format!("repository {name} has no card {}", p.card),
                    )
                })?;
                let on_board: BTreeSet<u64> = cards.iter().map(|c| c.id.0).collect();
                let repo = repo.clone();
                let resources = crate::executor::lint_resources(&repo, &served.resources);
                let lint = blocking(move || static_lint(&card, &repo, &on_board, &resources)).await;
                Ok(LintResult {
                    repo: name.clone(),
                    card: p.card,
                    item: item_id(name, p.card),
                    claimable: lint.claimable(),
                    findings: findings(&lint),
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

/// The item id of card `card` of repository `repo`, as the loop gives it.
pub(crate) fn item_id(repo: &str, card: u64) -> String {
    format!("{repo}#{card}")
}

/// The board of `served`'s repository `name`: the one its loop reads and writes (#111).
pub(crate) fn board_of(served: &Served, name: &str) -> Result<Shared, String> {
    match served.boards.get(name) {
        Some(Ok(board)) => Ok(board.queue.clone()),
        Some(Err(e)) => Err(e.clone()),
        None => Err("the repository has no queue".to_owned()),
    }
}

async fn blocking<T: Send + 'static>(f: impl FnOnce() -> T + Send + 'static) -> T {
    match tokio::task::spawn_blocking(f).await {
        Ok(value) => value,
        Err(e) => std::panic::resume_unwind(e.into_panic()),
    }
}

/// The board of one repository, with what the store and the lint say of each card.
async fn repo_queue(
    state: &State,
    served: &Served,
    (name, repo): &NamedRepo,
    items: &[StoredItem],
) -> RepoQueue {
    let mut view = RepoQueue {
        repo: name.clone(),
        queue: queue_kind(repo.queue.as_ref()).to_owned(),
        hash: None,
        error: None,
        columns: Vec::new(),
    };
    let board = match board_of(served, name) {
        Ok(board) => board,
        Err(e) => {
            view.error = Some(e);
            return view;
        }
    };
    let changes = match blocking(move || board.poll(None)).await {
        Ok(changes) => changes,
        Err(e) => {
            view.error = Some(e.to_string());
            return view;
        }
    };
    view.hash = Some(text_hash(changes.cursor.as_str()));
    let stored: BTreeMap<&str, &StoredItem> = items
        .iter()
        .filter(|i| crate::executor::of_repo(i, name, repo))
        .map(|i| (i.item.id.0.as_str(), i))
        .collect();
    let on_board: BTreeSet<u64> = changes.cards.iter().map(|c| c.id.0).collect();
    let ready: Vec<Card> = changes
        .cards
        .iter()
        .filter(|c| c.column == Column::Ready)
        .cloned()
        .collect();
    let lint_repo = repo.clone();
    let resources = crate::executor::lint_resources(repo, &served.resources);
    let lints: BTreeMap<u64, Vec<LintFinding>> = blocking(move || {
        ready
            .iter()
            .map(|card| {
                (
                    card.id.0,
                    findings(&static_lint(card, &lint_repo, &on_board, &resources)),
                )
            })
            .collect()
    })
    .await;
    // The cards the loop's last plan left waiting on open blockers (#5).
    let waiting: BTreeMap<String, Vec<String>> = state
        .blockers
        .of(name)
        .into_iter()
        .map(|b| {
            (
                b.item.to_string(),
                b.open.iter().map(ToString::to_string).collect(),
            )
        })
        .collect();
    for column in Column::ALL {
        let cards = changes
            .cards
            .iter()
            .filter(|c| c.column == column)
            .map(|card| {
                let item = item_id(name, card.id.0);
                let known = stored.get(item.as_str()).copied();
                CardView {
                    card: card.id.0,
                    title: card.title.clone(),
                    labels: card.labels.clone(),
                    draft: card.draft,
                    state: known.map(|i| i.item.state.as_str().to_owned()),
                    snapshot_hash: known.and_then(|i| i.snapshot_hash.clone()),
                    edit_pending: known.is_some_and(|i| edit_pending(served, card, i)),
                    lint: lints.get(&card.id.0).cloned().unwrap_or_default(),
                    waiting_on: waiting.get(item.as_str()).cloned().unwrap_or_default(),
                    item,
                }
            })
            .collect();
        view.columns.push(ColumnView {
            column: column.name().to_owned(),
            cards,
        });
    }
    view
}

/// Whether `card` differs from the snapshot its item runs from: the snapshot it would be
/// claimed with now has another hash. Only an item the daemon still owns has an edit waiting.
fn edit_pending(served: &Served, card: &Card, item: &StoredItem) -> bool {
    let owned = item.item.state.is_active() || item.item.state.as_str() == "needs_you";
    let Some(stored) = item.snapshot_hash.as_deref().filter(|_| owned) else {
        return false;
    };
    // The comments the claim inlines: by a login of the allowlist, never the daemon's (#2).
    let now = Snapshot::take(card, |c| {
        c.key.is_none()
            && served
                .author_allowlist
                .contains(&c.author.to_ascii_lowercase())
    });
    now.map_or(true, |snapshot| snapshot.hash != stored)
}

/// The static lint of `card`, as the loop runs it before a claim: `resources` are the names a
/// card may need ([`crate::executor::lint_resources`]).
fn static_lint(
    card: &Card,
    repo: &Repo,
    on_board: &BTreeSet<u64>,
    resources: &BTreeSet<String>,
) -> Lint {
    let shell =
        verdict_shell(repo.verdict.shell.as_deref().map(Path::new)).unwrap_or_else(|| "sh".into());
    let resolver = ShellResolver::new(shell, &repo.path);
    let exists = |id: bosun_queue::ItemId| on_board.contains(&id.0);
    bosun_queue::lint(
        card,
        &LintContext {
            resolver: &resolver,
            resources,
            item_exists: &exists,
        },
    )
}

fn findings(lint: &Lint) -> Vec<LintFinding> {
    lint.findings
        .iter()
        .map(|f| LintFinding {
            severity: match f.severity {
                Severity::Error => "error",
                Severity::Warning => "warning",
            }
            .to_owned(),
            // The text is the loop's, which says "error: " or "warning: " first.
            message: f
                .to_string()
                .split_once(": ")
                .map_or_else(|| f.to_string(), |(_, rest)| rest.to_owned()),
        })
        .collect()
}

/// The board's hash as `queue_changed` carries it: the loop's `text_hash` of the poll's cursor.
fn text_hash(text: &str) -> String {
    let digest = Sha256::digest(text.as_bytes());
    digest[..8].iter().fold(String::new(), |mut s, b| {
        use std::fmt::Write as _;
        let _ = write!(s, "{b:02x}");
        s
    })
}
