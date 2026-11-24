//! The GitHub Projects v2 backend (#5, #60, #61): the board is one or more Projects v2
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
//!   [`PollStats::dropped_bodies`]. Logins compare ignoring ASCII case, as on GitHub. The
//!   identity's own comments that carry a hidden key (`viewerDidAuthor`, edited by no one
//!   else) are kept whatever the list says, with their key and an empty body (#106): the
//!   daemon must see its own claim markers and stickies, and a second daemon's, even when its
//!   login is not on its own list. Their text is never reported, so never inlined.
//! - **Dependencies.** GitHub's own issue dependencies (`blockedBy`) within the repository are
//!   reported in [`Card::blocked_by`], beside the contract's `blocked-by` (see
//!   [`Card::blockers`]). A blocker in another repository cannot be named by an [`ItemId`] and
//!   is left out.
//!
//! # Writes (#61)
//!
//! Every write first reads the issue (one request: its project items with their Status, and
//! its comments), then sends at most one mutation, so the outbox may replay any write.
//!
//! - **Status.** [`Queue::project`] sets the Status of the card on the first bound project that
//!   has it (`updateProjectV2ItemFieldValue`), and sends nothing when the card is already in
//!   that column. Projects v2 has no place for the one-line note; it is not written.
//! - **Comments.** The daemon's comments carry the hidden key of [`marker`] on their first
//!   line. Before creating, the backend looks the key up among the comments the identity wrote
//!   (`viewerDidAuthor`; a comment by anyone else never counts, whatever marker it imitates),
//!   so a crash between the create and the record of it never yields a second comment. A keyed
//!   comment the user deleted is never reposted by [`Queue::comment`] and is recreated once by
//!   [`Queue::upsert_comment`]; deleted again, it stays deleted. The backend remembers which
//!   comments it posted or found, and which it recreated, for its lifetime.
//! - **Claim.** [`crate::claim()`] runs on these writes and on [`Queue::lint_target`] as the
//!   re-read.
//! - **Typed errors.** A secondary rate limit (a 403 or 429 with `Retry-After`, or a message
//!   naming it) and an exhausted quota are [`QueueError::RateLimited`], transient, with the
//!   wait; a token that lost a scope (`INSUFFICIENT_SCOPES`, `FORBIDDEN`, a 401, `gh` no longer
//!   logged in) is [`QueueError::Scope`], which pauses the repository. An issue transferred to
//!   another repository or converted to a discussion is [`QueueError::Gone`], and a deleted one
//!   [`QueueError::NotFound`]: both [`QueueError::is_cancel`].

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
use crate::marker::{self, valid_key};
use crate::queue::{Changes, Cursor, Gone, Keyed, Queue, QueueError, RateLimited};

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
    /// The Status field's node id.
    field: String,
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
    /// The node id of each label, by name: what a label removal names.
    label_ids: Vec<(String, String)>,
    comments: Vec<RawComment>,
    blocked_by: Vec<(String, u64)>,
}

#[derive(Debug, Clone)]
struct RawComment {
    /// The comment's node id.
    id: String,
    /// Whether the identity wrote it (`viewerDidAuthor`).
    mine: bool,
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

/// An issue of the repository, read for a write or the lint.
#[derive(Debug)]
struct Located {
    /// The issue's node id.
    id: String,
    /// The issue, all comment pages read.
    issue: Issue,
    /// Its card on the first bound project that has one.
    placement: Option<Placement>,
}

/// Where an issue's card is.
#[derive(Debug)]
struct Placement {
    /// The index of the project in [`ProjectsV2::projects`].
    project: usize,
    /// The project item's node id.
    item: String,
    /// The Status option id; `None` when it has no Status.
    option: Option<String>,
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
    /// The keyed comments this backend posted or found, for the deletion rules of #5.
    keyed: Keyed,
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
  labels(first: 100) { nodes { id name } }
  blockedBy(first: 100) { nodes { number repository { nameWithOwner } } }
  comments(first: $first) {
    pageInfo { hasNextPage endCursor }
    nodes { id viewerDidAuthor author { login } editor { login } body }
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
        nodes { id viewerDidAuthor author { login } editor { login } body }
      }
    }
  }
}";

const ISSUE: &str = "query Issue($owner: String!, $name: String!, $number: Int!, $field: String!, \
$first: Int!) {
  repository(owner: $owner, name: $name) {
    issue(number: $number) {
      ...Fields
      repository { nameWithOwner }
      projectItems(first: 50) {
        nodes {
          id
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

/// Asked only when an issue does not resolve: a conversion leaves a discussion with its number.
const DISCUSSION: &str = "query Discussion($owner: String!, $name: String!, $number: Int!) {
  repository(owner: $owner, name: $name) {
    discussion(number: $number) { id }
  }
}";

const UPDATE_STATUS: &str = "mutation UpdateStatus($project: ID!, $item: ID!, $field: ID!, \
$option: String!) {
  updateProjectV2ItemFieldValue(input: {projectId: $project, itemId: $item, fieldId: $field, \
value: {singleSelectOptionId: $option}}) {
    projectV2Item { id }
  }
}";

const ADD_COMMENT: &str = "mutation AddComment($subject: ID!, $body: String!) {
  addComment(input: {subjectId: $subject, body: $body}) {
    commentEdge { node { id } }
  }
}";

const REMOVE_LABELS: &str = "mutation RemoveLabels($labelable: ID!, $labels: [ID!]!) {
  removeLabelsFromLabelable(input: {labelableId: $labelable, labelIds: $labels}) {
    clientMutationId
  }
}";

const UPDATE_COMMENT: &str = "mutation UpdateComment($id: ID!, $body: String!) {
  updateIssueComment(input: {id: $id, body: $body}) {
    issueComment { id }
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
            keyed: Keyed::default(),
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

    /// One request. GraphQL errors fail it unless `allow` names their type; a rate limit or a
    /// lost scope fails it with its own typed error.
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
                return Err(transport_error(e));
            }
        };
        if let Some(limit) = &reply.rate_limit {
            meter.rate_limit = Some(limit.clone());
        }
        drop(meter);
        let unexpected: Vec<&ReplyError> = reply
            .errors
            .iter()
            .filter(|e| !e.kind.as_deref().is_some_and(|k| allow.contains(&k)))
            .collect();
        if unexpected.is_empty() {
            return Ok(reply.data);
        }
        let joined = || {
            unexpected
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>()
                .join("; ")
        };
        let kind = |names: &[&str]| {
            unexpected
                .iter()
                .any(|e| e.kind.as_deref().is_some_and(|k| names.contains(&k)))
        };
        if kind(&["RATE_LIMITED", "RATE_LIMIT"]) {
            let secondary = unexpected.iter().any(|e| names_secondary(&e.message));
            return Err(QueueError::RateLimited(RateLimited {
                secondary,
                retry_after: None,
                reset: reply.rate_limit.map(|l| l.reset),
            }));
        }
        if kind(&["INSUFFICIENT_SCOPES", "FORBIDDEN"]) {
            return Err(QueueError::Scope(joined()));
        }
        Err(QueueError::Remote(joined()))
    }

    /// A mutation: `NOT_FOUND` is an answer (the node is gone), anything else unexpected fails.
    fn mutate(&self, query: &str, variables: &Value) -> Result<Value, QueueError> {
        self.request(query, variables, &["NOT_FOUND"])
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
        let field = text(node, "/field/id")?.to_owned();
        Ok(Resolved {
            label,
            id,
            field,
            columns,
        })
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
            label_ids: node["labels"]["nodes"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|l| {
                    Some((l["name"].as_str()?.to_owned(), l["id"].as_str()?.to_owned()))
                })
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

    /// The card of `issue` in `column`, with the allowlist applied: content by a login off the
    /// list is dropped, except the identity's own keyed comments, which keep their key (for the
    /// claim check and the sticky lookup) and lose their text.
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
            let marked = marker::extract(&comment.body);
            let allowed =
                !filter || self.allowed(comment.author.as_deref(), comment.editor.as_deref());
            // The identity's own keyed comments pass the allowlist (#106): the daemon must see
            // its claim markers and stickies, and a second daemon's, even when its login is off
            // its own list. Their text is not reported.
            let own = !cfg!(mutant_queue_markers_through_allowlist)
                && marked.is_some()
                && own_unedited(comment);
            if !allowed && !own {
                *stats.dropped_comments.entry(id).or_default() += 1;
                continue;
            }
            let (key, body) = match marked {
                Some((key, _)) if !allowed => (Some(key), String::new()),
                Some((key, body)) => (Some(key), body.to_owned()),
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

    /// The issue of `item` with its card, fresh from GitHub.
    ///
    /// # Errors
    ///
    /// [`QueueError::NotFound`] when no issue has the number (never had, or deleted),
    /// [`QueueError::Gone`] when it was transferred or converted, [`QueueError::Setup`] when the
    /// repository is not visible.
    fn locate(&self, item: ItemId) -> Result<Located, QueueError> {
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
            return Err(if self.converted(item)? {
                QueueError::Gone(item, Gone::Converted)
            } else {
                QueueError::NotFound(item)
            });
        }
        // GitHub answers for a transferred issue by its old number with the issue in its new
        // repository.
        if let Some(now) = node["repository"]["nameWithOwner"].as_str()
            && !now.eq_ignore_ascii_case(&self.binding.repository)
        {
            return Err(QueueError::Gone(item, Gone::Transferred(now.to_owned())));
        }
        let items: Vec<&Value> = node["projectItems"]["nodes"]
            .as_array()
            .into_iter()
            .flatten()
            .filter(|i| i["isArchived"].as_bool() != Some(true))
            .collect();
        let placement = self
            .projects
            .iter()
            .enumerate()
            .find_map(|(index, project)| {
                let found = items
                    .iter()
                    .find(|i| i["project"]["id"].as_str() == Some(project.id.as_str()))?;
                Some(Placement {
                    project: index,
                    item: found["id"].as_str()?.to_owned(),
                    option: found["status"]["optionId"].as_str().map(str::to_owned),
                })
            });
        let id = text(node, "/id")?.to_owned();
        let issue = self.issue(&id, node)?;
        Ok(Located {
            id,
            issue,
            placement,
        })
    }

    /// Whether a discussion has the number of `item`: what a conversion leaves.
    fn converted(&self, item: ItemId) -> Result<bool, QueueError> {
        let data = self.request(
            DISCUSSION,
            &json!({ "owner": self.owner, "name": self.name, "number": item.0 }),
            &["NOT_FOUND"],
        )?;
        Ok(!data["repository"]["discussion"].is_null())
    }

    /// The issue of `item`, which must have a card on a bound project.
    fn located_card(&self, item: ItemId) -> Result<(Located, Placement), QueueError> {
        let mut located = self.locate(item)?;
        let placement = located.placement.take().ok_or(QueueError::NotFound(item))?;
        Ok((located, placement))
    }

    /// The identity's comment with `key` on the issue, if there is one: (node id, body).
    fn own_by_key<'i>(issue: &'i Issue, key: &str) -> Option<(&'i str, &'i str)> {
        if cfg!(mutant_queue_no_comment_key) {
            // Mutant: no lookup by key, so every replay posts again.
            return None;
        }
        issue
            .comments
            .iter()
            .filter(|c| c.mine)
            .find(|c| marker::extract(&c.body).is_some_and(|(found, _)| found == key))
            .map(|c| (c.id.as_str(), c.body.as_str()))
    }

    /// Posts `body` (its marker included) on the issue with node id `subject`.
    fn add_comment(&self, item: ItemId, subject: &str, body: &str) -> Result<(), QueueError> {
        let data = self.mutate(ADD_COMMENT, &json!({ "subject": subject, "body": body }))?;
        if data["addComment"].is_null() {
            return Err(QueueError::NotFound(item));
        }
        Ok(())
    }

    /// Creates the comment with `key` that is missing from the issue, by the deletion rules.
    fn create_missing(
        &self,
        item: ItemId,
        subject: &str,
        key: &str,
        body: &str,
        upsert: bool,
    ) -> Result<(), QueueError> {
        let create = if cfg!(mutant_queue_no_comment_key) {
            Some(false)
        } else {
            self.keyed.create_missing(item, key, upsert)
        };
        // `None`: the user deleted it, and it is left deleted.
        if let Some(recreated) = create {
            self.add_comment(item, subject, body)?;
            self.keyed.created(item, key, recreated);
        }
        Ok(())
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

    fn project(&self, item: ItemId, state: Owned, _note: Option<&str>) -> Result<(), QueueError> {
        let (_, placement) = self.located_card(item)?;
        let project = &self.projects[placement.project];
        let target = &project.columns[&state.column()];
        if placement.option.as_deref() == Some(target.as_str()) {
            return Ok(());
        }
        let data = self.mutate(
            UPDATE_STATUS,
            &json!({
                "project": project.id,
                "item": placement.item,
                "field": project.field,
                "option": target,
            }),
        )?;
        if data["updateProjectV2ItemFieldValue"].is_null() {
            // The card was removed between the read and the write.
            return Err(QueueError::NotFound(item));
        }
        Ok(())
    }

    fn comment(&self, item: ItemId, body: &str, key: &str) -> Result<(), QueueError> {
        if !valid_key(key) {
            return Err(QueueError::InvalidKey(key.to_owned()));
        }
        if !cfg!(mutant_queue_no_comment_key)
            && self.keyed.create_missing(item, key, false).is_none()
        {
            // Posted or found before: a replay leaves the one comment as it is.
            return Ok(());
        }
        let (located, _) = self.located_card(item)?;
        if Self::own_by_key(&located.issue, key).is_some() {
            self.keyed.seen(item, key);
            return Ok(());
        }
        self.create_missing(item, &located.id, key, &marker::embed(key, body), false)
    }

    fn upsert_comment(&self, item: ItemId, key: &str, body: &str) -> Result<(), QueueError> {
        if !valid_key(key) {
            return Err(QueueError::InvalidKey(key.to_owned()));
        }
        let wanted = marker::embed(key, body);
        let (located, _) = self.located_card(item)?;
        if let Some((id, current)) = Self::own_by_key(&located.issue, key) {
            self.keyed.seen(item, key);
            if current.replace("\r\n", "\n") == wanted.replace("\r\n", "\n") {
                return Ok(());
            }
            let data = self.mutate(UPDATE_COMMENT, &json!({ "id": id, "body": wanted }))?;
            if !data["updateIssueComment"].is_null() {
                return Ok(());
            }
            // Deleted between the read and the edit: the deletion rules apply.
        }
        self.create_missing(item, &located.id, key, &wanted, true)
    }

    fn remove_label(&self, item: ItemId, label: &str) -> Result<(), QueueError> {
        let (located, _) = self.located_card(item)?;
        let Some((_, id)) = located
            .issue
            .label_ids
            .iter()
            .find(|(name, _)| name == label)
        else {
            return Ok(());
        };
        let data = self.mutate(
            REMOVE_LABELS,
            &json!({ "labelable": located.id, "labels": [id] }),
        )?;
        if data["removeLabelsFromLabelable"].is_null() {
            return Err(QueueError::NotFound(item));
        }
        Ok(())
    }

    fn lint_target(&self, item: ItemId) -> Result<Card, QueueError> {
        let (located, placement) = self.located_card(item)?;
        let column = match &placement.option {
            None => Column::Backlog,
            Some(option) => self.projects[placement.project]
                .column_of(option)
                .ok_or(QueueError::NotFound(item))?,
        };
        Ok(self.card(column, &located.issue, &mut PollStats::default()))
    }
}

/// Whether a message names GitHub's secondary rate limit.
fn names_secondary(message: &str) -> bool {
    message
        .to_ascii_lowercase()
        .contains("secondary rate limit")
}

/// The typed error of a request that produced no reply.
fn transport_error(e: TransportError) -> QueueError {
    match e {
        TransportError::Status {
            status: 403 | 429,
            ref message,
            ref rate_limit,
            retry_after,
        } if retry_after.is_some()
            || names_secondary(message)
            || rate_limit.as_ref().is_some_and(|l| l.remaining == 0) =>
        {
            QueueError::RateLimited(RateLimited {
                secondary: retry_after.is_some() || names_secondary(message),
                retry_after: retry_after.map(std::time::Duration::from_secs),
                reset: rate_limit.as_ref().map(|l| l.reset),
            })
        }
        TransportError::Status {
            status: 401 | 403, ..
        } => QueueError::Scope(e.to_string()),
        TransportError::Gh { ref stderr, .. } if stderr.contains("gh auth login") => {
            QueueError::Scope(e.to_string())
        }
        e => QueueError::Remote(e.to_string()),
    }
}

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

/// Whether the identity wrote `comment` (`viewerDidAuthor`) and no other login edited it since.
fn own_unedited(comment: &RawComment) -> bool {
    comment.mine
        && comment.editor.as_deref().is_none_or(|editor| {
            comment
                .author
                .as_deref()
                .is_some_and(|author| author.eq_ignore_ascii_case(editor))
        })
}

fn raw_comment(node: &Value) -> RawComment {
    RawComment {
        id: node["id"].as_str().unwrap_or_default().to_owned(),
        mine: node["viewerDidAuthor"].as_bool() == Some(true),
        author: login(&node["author"]),
        editor: login(&node["editor"]),
        body: node["body"].as_str().unwrap_or_default().to_owned(),
    }
}

#[cfg(test)]
mod tests;
