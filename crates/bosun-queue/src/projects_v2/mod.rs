//! The GitHub Projects v2 backend (#5, #60), read side: the board is one or more Projects v2
//! projects, and the cards are the issues of one repository on them.
//!
//! - **Transport.** GraphQL only, through the [`GraphQl`] trait; [`GhCli`] runs `gh api graphql`
//!   under the identity's `GH_CONFIG_DIR` (D5 of #7). Every response's rate limit is kept as the
//!   quota metric ([`ProjectsV2::rate_limit`]); [`Pacer`] turns it and the activity into the
//!   poll interval (30 s while active, backing off to five minutes when idle).
//! - **Start-up resolution.** [`ProjectsV2::connect`] resolves every bound project (by owner and
//!   number, as in its URL), its Status field and the option of every [`Column`] by name
//!   ([`Column::from_name`] folding), and refuses with [`QueueError::Setup`] when one is missing
//!   or ambiguous. Options beyond the six columns are allowed; a card in one is left out of the
//!   board and listed in [`PollStats::unknown_status`]. Every poll re-reads the options: one
//!   renamed, deleted or duplicated since start fails the poll with [`QueueError::Setup`], which
//!   is not transient, so the loop pauses that repository with the error.
//! - **Polling.** Each poll lists the items (order, Status, and each issue's `updatedAt`) and
//!   fetches title, body, labels, comments and dependencies only for issues whose `updatedAt`
//!   moved since the backend last read them: an unchanged board costs one request per hundred
//!   items. The `updatedAt` marks live in the backend, so the first poll after a restart reads
//!   everything once; the [`Cursor`] stays the one [`Changes::since`] writes.
//! - **Cards.** Issues of the bound repository only; an issue on two bound projects is reported
//!   once, from the first project in [`Binding::projects`] that has it. Archived items are not
//!   on the board. Draft items have no issue, hence no number to be an [`ItemId`] and nowhere
//!   to comment: they are left out in every column and counted ([`PollStats::drafts`]); the
//!   daemon ignores Backlog anyway. An item without a Status is in Backlog.
//! - **Author allowlist (#2).** Only content written by a login on [`Binding::authors`] is
//!   reported: a comment whose author (or last editor) is off the list is dropped and counted
//!   in [`PollStats::dropped_comments`]; a body whose author or editor is off the list is
//!   replaced by an empty one (so the card fails lint rather than inlining it) and listed in
//!   [`PollStats::dropped_bodies`]. Logins compare ignoring ASCII case, as on GitHub.
//! - **Dependencies.** GitHub's own issue dependencies (`blockedBy`) within the repository are
//!   reported in [`Card::blocked_by`], beside the contract's `blocked-by` (see
//!   [`Card::blockers`]). A blocker in another repository cannot be named by an [`ItemId`] and
//!   is left out.
//!
//! The write half of [`Queue`] (`project`, `comment`, `upsert_comment`) is #61; until then it
//! fails with [`QueueError::Unsupported`].

mod pace;
mod transport;

#[cfg(any(test, feature = "testsuite"))]
pub mod fake;

pub use pace::{ACTIVE, IDLE_MAX, Pacer, RESERVE_PERCENT};
pub use transport::{
    DEFAULT_TIMEOUT, GhCli, GraphQl, RateLimit, Reply, ReplyError, TransportError, parse_include,
};

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::sync::{Mutex, PoisonError};

use serde_json::{Value, json};

use crate::card::{Card, Column, Comment, ItemId, Owned};
use crate::marker;
use crate::queue::{Changes, Cursor, Queue, QueueError};

/// The name of the Status field unless [`Binding::status_field`] says otherwise.
pub const STATUS_FIELD: &str = "Status";

/// What a repository's board is: the repository, the projects that hold its cards, and whose
/// content may reach a prompt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Binding {
    /// The repository whose issues are the cards, as `owner/name`.
    pub repository: String,
    /// The projects, in priority order: a card on several is reported from the first.
    pub projects: Vec<ProjectRef>,
    /// The single-select field whose options are the columns.
    pub status_field: String,
    /// The identity's author allowlist (#2): logins whose bodies and comments are reported.
    pub authors: BTreeSet<String>,
}

impl Binding {
    /// A binding with the default Status field name.
    #[must_use]
    pub fn new(
        repository: impl Into<String>,
        projects: impl IntoIterator<Item = ProjectRef>,
        authors: impl IntoIterator<Item = impl Into<String>>,
    ) -> Self {
        Self {
            repository: repository.into(),
            projects: projects.into_iter().collect(),
            status_field: STATUS_FIELD.to_owned(),
            authors: authors.into_iter().map(Into::into).collect(),
        }
    }
}

/// A Projects v2 project, as in its URL: `github.com/users/<owner>/projects/<number>` or
/// `github.com/orgs/<owner>/projects/<number>`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProjectRef {
    /// The user or organisation login that owns it.
    pub owner: String,
    /// Its number.
    pub number: u64,
}

impl ProjectRef {
    /// The project `number` of `owner`.
    #[must_use]
    pub fn new(owner: impl Into<String>, number: u64) -> Self {
        Self {
            owner: owner.into(),
            number,
        }
    }
}

/// What the last [`Queue::poll`] left out or spent.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PollStats {
    /// GraphQL requests the poll made.
    pub requests: usize,
    /// Issues whose details the poll fetched (their `updatedAt` moved, or they are new).
    pub fetched: usize,
    /// Per card, the comments dropped because their author or editor is off the allowlist.
    pub dropped_comments: BTreeMap<ItemId, usize>,
    /// Cards whose body was dropped because its author or editor is off the allowlist.
    pub dropped_bodies: Vec<ItemId>,
    /// Draft items, which are never cards.
    pub drafts: usize,
    /// Issues in a Status option that is none of the six columns.
    pub unknown_status: Vec<ItemId>,
}

/// One bound project, resolved at start.
#[derive(Debug, Clone)]
struct Resolved {
    /// `owner/number`, for messages.
    label: String,
    /// The project's node id.
    id: String,
    /// The option id of every column.
    columns: BTreeMap<Column, String>,
}

impl Resolved {
    fn column_of(&self, option: &str) -> Option<Column> {
        self.columns
            .iter()
            .find(|(_, id)| id.as_str() == option)
            .map(|(column, _)| *column)
    }
}

/// An issue as GitHub returned it, before the allowlist.
#[derive(Debug, Clone)]
struct Issue {
    number: u64,
    title: String,
    body: String,
    author: Option<String>,
    editor: Option<String>,
    labels: Vec<String>,
    comments: Vec<RawComment>,
    blocked_by: Vec<(String, u64)>,
}

#[derive(Debug, Clone)]
struct RawComment {
    author: Option<String>,
    editor: Option<String>,
    body: String,
}

/// An issue on the board, from the item listing.
#[derive(Debug, Clone)]
struct Entry {
    number: u64,
    node: String,
    updated_at: String,
    column: Column,
}

#[derive(Debug, Default)]
struct Meter {
    requests: usize,
    rate_limit: Option<RateLimit>,
}

#[derive(Debug, Clone, Copy)]
struct Sizes {
    items: u64,
    details: usize,
    comments: u64,
}

const SIZES: Sizes = Sizes {
    items: 100,
    details: 50,
    comments: 100,
};

/// The Projects v2 backend; see the module docs.
#[derive(Debug)]
pub struct ProjectsV2<T> {
    transport: T,
    binding: Binding,
    owner: String,
    name: String,
    authors: BTreeSet<String>,
    projects: Vec<Resolved>,
    sizes: Sizes,
    meter: Mutex<Meter>,
    /// Issue node id to the `updatedAt` it was read at and what it held then.
    cache: Mutex<HashMap<String, (String, Issue)>>,
    stats: Mutex<PollStats>,
}

const RESOLVE: &str = "query Resolve($owner: String!, $number: Int!, $field: String!) {
  repositoryOwner(login: $owner) {
    ... on ProjectV2Owner {
      projectV2(number: $number) {
        id
        closed
        field(name: $field) {
          __typename
          ... on ProjectV2SingleSelectField { id options { id name } }
        }
      }
    }
  }
}";

const ITEMS: &str = "query Items($project: ID!, $field: String!, $first: Int!, $after: String) {
  node(id: $project) {
    ... on ProjectV2 {
      field(name: $field) {
        __typename
        ... on ProjectV2SingleSelectField { id options { id name } }
      }
      items(first: $first, after: $after, orderBy: {field: POSITION, direction: ASC}) {
        pageInfo { hasNextPage endCursor }
        nodes {
          type
          isArchived
          status: fieldValueByName(name: $field) {
            __typename
            ... on ProjectV2ItemFieldSingleSelectValue { optionId }
          }
          content {
            __typename
            ... on Issue { id number updatedAt repository { nameWithOwner } }
          }
        }
      }
    }
  }
}";

/// The fields of an issue the cards are built from, shared by `Details` and `Issue`.
const ISSUE_FIELDS: &str = "fragment Fields on Issue {
  id
  number
  title
  body
  author { login }
  editor { login }
  labels(first: 100) { nodes { name } }
  blockedBy(first: 100) { nodes { number repository { nameWithOwner } } }
  comments(first: $first) {
    pageInfo { hasNextPage endCursor }
    nodes { author { login } editor { login } body }
  }
}";

const DETAILS: &str = "query Details($ids: [ID!]!, $first: Int!) {
  nodes(ids: $ids) { __typename ...Fields }
}";

const COMMENTS: &str = "query Comments($issue: ID!, $first: Int!, $after: String) {
  node(id: $issue) {
    ... on Issue {
      comments(first: $first, after: $after) {
        pageInfo { hasNextPage endCursor }
        nodes { author { login } editor { login } body }
      }
    }
  }
}";

const ISSUE: &str = "query Issue($owner: String!, $name: String!, $number: Int!, $field: String!, \
$first: Int!) {
  repository(owner: $owner, name: $name) {
    issue(number: $number) {
      ...Fields
      projectItems(first: 50) {
        nodes {
          isArchived
          project { id }
          status: fieldValueByName(name: $field) {
            __typename
            ... on ProjectV2ItemFieldSingleSelectValue { optionId }
          }
        }
      }
    }
  }
}";

impl<T: GraphQl> ProjectsV2<T> {
    /// Resolves every project of `binding`, its Status field and the option of every column.
    ///
    /// # Errors
    ///
    /// [`QueueError::Setup`] when the repository name is not `owner/name`, no project is
    /// bound, or a project, its Status field or a column's option is missing, closed or
    /// ambiguous; [`QueueError::Remote`] when GitHub cannot be asked.
    pub fn connect(transport: T, binding: Binding) -> Result<Self, QueueError> {
        let (owner, name) = binding
            .repository
            .split_once('/')
            .filter(|(o, n)| !o.is_empty() && !n.is_empty() && !n.contains('/'))
            .ok_or_else(|| {
                QueueError::Setup(format!(
                    "the repository `{}` is not `owner/name`",
                    binding.repository
                ))
            })?;
        if binding.projects.is_empty() {
            return Err(QueueError::Setup(format!(
                "no project is bound to {}",
                binding.repository
            )));
        }
        let mut backend = Self {
            owner: owner.to_owned(),
            name: name.to_owned(),
            authors: binding
                .authors
                .iter()
                .map(|a| a.to_ascii_lowercase())
                .collect(),
            transport,
            projects: Vec::new(),
            sizes: SIZES,
            meter: Mutex::new(Meter::default()),
            cache: Mutex::new(HashMap::new()),
            stats: Mutex::new(PollStats::default()),
            binding,
        };
        let mut projects = Vec::new();
        for project in &backend.binding.projects {
            projects.push(backend.resolve(project)?);
        }
        backend.projects = projects;
        Ok(backend)
    }

    /// The binding this backend was connected with.
    #[must_use]
    pub fn binding(&self) -> &Binding {
        &self.binding
    }

    /// The rate limit of the last response that reported one: the remaining quota metric.
    #[must_use]
    pub fn rate_limit(&self) -> Option<RateLimit> {
        self.meter
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .rate_limit
            .clone()
    }

    /// GraphQL requests made since the backend was connected, resolution included.
    #[must_use]
    pub fn requests(&self) -> usize {
        self.meter
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .requests
    }

    /// What the last successful poll left out or spent.
    #[must_use]
    pub fn last_poll(&self) -> PollStats {
        self.stats
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    #[cfg(test)]
    fn with_sizes(mut self, items: u64, details: usize, comments: u64) -> Self {
        self.sizes = Sizes {
            items,
            details,
            comments,
        };
        self
    }

    /// One request. GraphQL errors fail it unless `allow` names their type.
    fn request(&self, query: &str, variables: &Value, allow: &[&str]) -> Result<Value, QueueError> {
        let result = self.transport.execute(query, variables);
        let mut meter = self.meter.lock().unwrap_or_else(PoisonError::into_inner);
        meter.requests += 1;
        let reply = match result {
            Ok(reply) => reply,
            Err(e) => {
                if let TransportError::Status {
                    rate_limit: Some(limit),
                    ..
                } = &e
                {
                    meter.rate_limit = Some(limit.clone());
                }
                return Err(QueueError::Remote(e.to_string()));
            }
        };
        if let Some(limit) = reply.rate_limit {
            meter.rate_limit = Some(limit);
        }
        drop(meter);
        let unexpected: Vec<String> = reply
            .errors
            .iter()
            .filter(|e| !e.kind.as_deref().is_some_and(|k| allow.contains(&k)))
            .map(ToString::to_string)
            .collect();
        if unexpected.is_empty() {
            Ok(reply.data)
        } else {
            Err(QueueError::Remote(unexpected.join("; ")))
        }
    }

    fn resolve(&self, project: &ProjectRef) -> Result<Resolved, QueueError> {
        let label = format!("{}/{}", project.owner, project.number);
        let data = self.request(
            RESOLVE,
            &json!({
                "owner": project.owner,
                "number": project.number,
                "field": self.binding.status_field,
            }),
            &["NOT_FOUND"],
        )?;
        let owner = &data["repositoryOwner"];
        if owner.is_null() {
            return Err(QueueError::Setup(format!(
                "no user or organisation `{}` owns project {label}",
                project.owner
            )));
        }
        let node = &owner["projectV2"];
        if node.is_null() {
            return Err(QueueError::Setup(format!(
                "project {label} does not exist or is not visible to this identity"
            )));
        }
        if node["closed"].as_bool() == Some(true) {
            return Err(QueueError::Setup(format!("project {label} is closed")));
        }
        let id = text(node, "/id")?.to_owned();
        let columns = self.columns(&label, &node["field"])?;
        Ok(Resolved { label, id, columns })
    }

    /// The option id of every column in a Status field's JSON.
    fn columns(&self, label: &str, field: &Value) -> Result<BTreeMap<Column, String>, QueueError> {
        let name = &self.binding.status_field;
        if field.is_null() {
            return Err(QueueError::Setup(format!(
                "project {label} has no field `{name}`"
            )));
        }
        if field["__typename"] != "ProjectV2SingleSelectField" {
            return Err(QueueError::Setup(format!(
                "the field `{name}` of project {label} is not a single-select field"
            )));
        }
        let options: Vec<(&str, &str)> = field["options"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|o| Some((o["id"].as_str()?, o["name"].as_str()?)))
            .collect();
        let mut columns = BTreeMap::new();
        let mut problems = Vec::new();
        for column in Column::ALL {
            let matching: Vec<&(&str, &str)> = options
                .iter()
                .filter(|(_, n)| Column::from_name(n) == Some(column))
                .collect();
            match matching.as_slice() {
                [] => problems.push(format!("no option for the column `{column}`")),
                [(id, _)] => {
                    columns.insert(column, (*id).to_owned());
                }
                [(id, _), ..] if cfg!(mutant_queue_ambiguous_ok) => {
                    // Mutant: the first of several matching options is taken.
                    columns.insert(column, (*id).to_owned());
                }
                many => {
                    let names: Vec<String> = many.iter().map(|(_, n)| format!("`{n}`")).collect();
                    problems.push(format!(
                        "the column `{column}` matches several options: {}",
                        names.join(", ")
                    ));
                }
            }
        }
        if problems.is_empty() {
            Ok(columns)
        } else {
            Err(QueueError::Setup(format!(
                "the field `{name}` of project {label}: {}",
                problems.join("; ")
            )))
        }
    }

    /// Every issue of the repository on `project`, in board order.
    fn list(&self, project: &Resolved, stats: &mut PollStats) -> Result<Vec<Entry>, QueueError> {
        let mut entries = Vec::new();
        let mut after = Value::Null;
        loop {
            let data = self.request(
                ITEMS,
                &json!({
                    "project": project.id,
                    "field": self.binding.status_field,
                    "first": self.sizes.items,
                    "after": after,
                }),
                &[],
            )?;
            let node = &data["node"];
            if node.is_null() {
                return Err(QueueError::Setup(format!(
                    "project {} is gone or no longer visible to this identity",
                    project.label
                )));
            }
            let now = self.columns(&project.label, &node["field"]).map_err(|e| {
                QueueError::Setup(format!(
                    "{e}; it changed since start, the repository pauses"
                ))
            })?;
            if now != project.columns {
                return Err(QueueError::Setup(format!(
                    "the options of the field `{}` of project {} were renamed or replaced since \
                     start; the repository pauses until the daemon is restarted",
                    self.binding.status_field, project.label
                )));
            }
            let items = &node["items"];
            for item in items["nodes"].as_array().into_iter().flatten() {
                if item["isArchived"].as_bool() == Some(true) {
                    continue;
                }
                let content = &item["content"];
                if item["type"] == "DRAFT_ISSUE" || content["__typename"] == "DraftIssue" {
                    stats.drafts += 1;
                    continue;
                }
                if content["__typename"] != "Issue" {
                    continue;
                }
                let repository = text(content, "/repository/nameWithOwner")?;
                if !repository.eq_ignore_ascii_case(&self.binding.repository) {
                    continue;
                }
                let number = number(content, "/number")?;
                let column = match item["status"]["optionId"].as_str() {
                    None => Some(Column::Backlog),
                    Some(option) => project.column_of(option),
                };
                let Some(column) = column else {
                    stats.unknown_status.push(ItemId(number));
                    continue;
                };
                entries.push(Entry {
                    number,
                    node: text(content, "/id")?.to_owned(),
                    updated_at: text(content, "/updatedAt")?.to_owned(),
                    column,
                });
            }
            if items["pageInfo"]["hasNextPage"].as_bool() == Some(true) {
                after = items["pageInfo"]["endCursor"].clone();
            } else {
                return Ok(entries);
            }
        }
    }

    /// The issues with these node ids; one that no longer exists is left out.
    fn details(&self, ids: &[&str]) -> Result<Vec<(String, Issue)>, QueueError> {
        let query = format!("{DETAILS}\n{ISSUE_FIELDS}");
        let data = self.request(
            &query,
            &json!({ "ids": ids, "first": self.sizes.comments }),
            &["NOT_FOUND"],
        )?;
        let mut issues = Vec::new();
        for node in data["nodes"].as_array().into_iter().flatten() {
            if node["__typename"] != "Issue" {
                continue;
            }
            let id = text(node, "/id")?.to_owned();
            let issue = self.issue(&id, node)?;
            issues.push((id, issue));
        }
        Ok(issues)
    }

    /// An issue from the `Fields` fragment, its remaining comment pages fetched.
    fn issue(&self, id: &str, node: &Value) -> Result<Issue, QueueError> {
        let mut comments = Vec::new();
        let mut page = node["comments"].clone();
        loop {
            comments.extend(
                page["nodes"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .map(raw_comment),
            );
            if page["pageInfo"]["hasNextPage"].as_bool() != Some(true) {
                break;
            }
            let data = self.request(
                COMMENTS,
                &json!({
                    "issue": id,
                    "first": self.sizes.comments,
                    "after": page["pageInfo"]["endCursor"],
                }),
                &[],
            )?;
            page = data["node"]["comments"].clone();
        }
        Ok(Issue {
            number: number(node, "/number")?,
            title: text(node, "/title")?.to_owned(),
            body: node["body"].as_str().unwrap_or_default().to_owned(),
            author: login(&node["author"]),
            editor: login(&node["editor"]),
            labels: node["labels"]["nodes"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|l| l["name"].as_str().map(str::to_owned))
                .collect(),
            comments,
            blocked_by: node["blockedBy"]["nodes"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|b| {
                    Some((
                        b["repository"]["nameWithOwner"].as_str()?.to_owned(),
                        b["number"].as_u64()?,
                    ))
                })
                .collect(),
        })
    }

    fn allowed(&self, author: Option<&str>, editor: Option<&str>) -> bool {
        let on_list = |login: &str| self.authors.contains(&login.to_ascii_lowercase());
        author.is_some_and(on_list) && editor.is_none_or(on_list)
    }

    /// The card of `issue` in `column`, with the allowlist applied.
    fn card(&self, column: Column, issue: &Issue, stats: &mut PollStats) -> Card {
        let id = ItemId(issue.number);
        let filter = !cfg!(mutant_queue_no_author_filter);
        let body = if !filter || self.allowed(issue.author.as_deref(), issue.editor.as_deref()) {
            issue.body.clone()
        } else {
            stats.dropped_bodies.push(id);
            String::new()
        };
        let mut comments = Vec::new();
        for comment in &issue.comments {
            if filter && !self.allowed(comment.author.as_deref(), comment.editor.as_deref()) {
                *stats.dropped_comments.entry(id).or_default() += 1;
                continue;
            }
            let (key, body) = match marker::extract(&comment.body) {
                Some((key, body)) => (Some(key.to_owned()), body.to_owned()),
                None => (None, comment.body.clone()),
            };
            comments.push(Comment {
                author: comment.author.clone().unwrap_or_default(),
                key,
                body,
            });
        }
        let mut blocked_by: Vec<ItemId> = issue
            .blocked_by
            .iter()
            .filter(|(repository, _)| repository.eq_ignore_ascii_case(&self.binding.repository))
            .map(|(_, n)| ItemId(*n))
            .collect();
        blocked_by.sort_unstable();
        blocked_by.dedup();
        Card {
            id,
            title: issue.title.clone(),
            column,
            body,
            labels: issue.labels.clone(),
            draft: false,
            comments,
            blocked_by,
        }
    }
}

impl<T: GraphQl> Queue for ProjectsV2<T> {
    fn poll(&self, cursor: Option<&Cursor>) -> Result<Changes, QueueError> {
        let requests_before = self.requests();
        let mut stats = PollStats::default();
        let mut entries = Vec::new();
        let mut seen = HashSet::new();
        for project in &self.projects {
            for entry in self.list(project, &mut stats)? {
                // A card on two bound projects is reported once, from the first.
                if seen.insert(entry.number) {
                    entries.push(entry);
                }
            }
        }

        let mut cache = self.cache.lock().unwrap_or_else(PoisonError::into_inner);
        let stale: Vec<&Entry> = entries
            .iter()
            .filter(|e| {
                // Mutant: every issue is fetched again on every poll.
                cfg!(mutant_queue_no_cursor)
                    || cache
                        .get(&e.node)
                        .is_none_or(|(updated_at, _)| *updated_at != e.updated_at)
            })
            .collect();
        stats.fetched = stale.len();
        for chunk in stale.chunks(self.sizes.details.max(1)) {
            let ids: Vec<&str> = chunk.iter().map(|e| e.node.as_str()).collect();
            for (id, issue) in self.details(&ids)? {
                // The listing's `updatedAt`, not a newer one: a change in between is fetched
                // again next time rather than missed.
                if let Some(entry) = chunk.iter().find(|e| e.node == id) {
                    cache.insert(id, (entry.updated_at.clone(), issue));
                }
            }
        }
        let live: HashSet<&str> = entries.iter().map(|e| e.node.as_str()).collect();
        cache.retain(|id, _| live.contains(id.as_str()));

        let cards: Vec<Card> = entries
            .iter()
            .filter_map(|e| {
                // An issue deleted between the listing and its details is gone.
                let (_, issue) = cache.get(&e.node)?;
                Some(self.card(e.column, issue, &mut stats))
            })
            .collect();
        drop(cache);
        stats.requests = self.requests() - requests_before;
        *self.stats.lock().unwrap_or_else(PoisonError::into_inner) = stats;
        Ok(Changes::since(cursor, cards))
    }

    fn project(&self, _item: ItemId, _state: Owned, _note: Option<&str>) -> Result<(), QueueError> {
        Err(QueueError::Unsupported(WRITES))
    }

    fn comment(&self, _item: ItemId, _body: &str, _key: &str) -> Result<(), QueueError> {
        Err(QueueError::Unsupported(WRITES))
    }

    fn upsert_comment(&self, _item: ItemId, _key: &str, _body: &str) -> Result<(), QueueError> {
        Err(QueueError::Unsupported(WRITES))
    }

    fn lint_target(&self, item: ItemId) -> Result<Card, QueueError> {
        let query = format!("{ISSUE}\n{ISSUE_FIELDS}");
        let data = self.request(
            &query,
            &json!({
                "owner": self.owner,
                "name": self.name,
                "number": item.0,
                "field": self.binding.status_field,
                "first": self.sizes.comments,
            }),
            &["NOT_FOUND"],
        )?;
        let repository = &data["repository"];
        if repository.is_null() {
            return Err(QueueError::Setup(format!(
                "the repository {} does not exist or is not visible to this identity",
                self.binding.repository
            )));
        }
        let node = &repository["issue"];
        if node.is_null() {
            return Err(QueueError::NotFound(item));
        }
        let items: Vec<&Value> = node["projectItems"]["nodes"]
            .as_array()
            .into_iter()
            .flatten()
            .filter(|i| i["isArchived"].as_bool() != Some(true))
            .collect();
        let column = self
            .projects
            .iter()
            .find_map(|project| {
                let item = items
                    .iter()
                    .find(|i| i["project"]["id"].as_str() == Some(project.id.as_str()))?;
                match item["status"]["optionId"].as_str() {
                    None => Some(Some(Column::Backlog)),
                    Some(option) => Some(project.column_of(option)),
                }
            })
            .flatten()
            .ok_or(QueueError::NotFound(item))?;
        let id = text(node, "/id")?.to_owned();
        let issue = self.issue(&id, node)?;
        Ok(self.card(column, &issue, &mut PollStats::default()))
    }
}

const WRITES: &str = "the Projects v2 backend does not write yet (#61)";

fn text<'v>(value: &'v Value, pointer: &str) -> Result<&'v str, QueueError> {
    value
        .pointer(pointer)
        .and_then(Value::as_str)
        .ok_or_else(|| QueueError::Remote(format!("the response has no text at `{pointer}`")))
}

fn number(value: &Value, pointer: &str) -> Result<u64, QueueError> {
    value
        .pointer(pointer)
        .and_then(Value::as_u64)
        .ok_or_else(|| QueueError::Remote(format!("the response has no number at `{pointer}`")))
}

fn login(actor: &Value) -> Option<String> {
    actor["login"].as_str().map(str::to_owned)
}

fn raw_comment(node: &Value) -> RawComment {
    RawComment {
        author: login(&node["author"]),
        editor: login(&node["editor"]),
        body: node["body"].as_str().unwrap_or_default().to_owned(),
    }
}

#[cfg(test)]
mod tests;
