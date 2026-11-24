//! A fake GitHub for the Projects v2 backend's tests (feature `testsuite`): an in-memory model
//! of projects and issues that answers the backend's GraphQL operations (by operation name)
//! with responses in GitHub's shapes, paginated as GitHub paginates. Everything in it is
//! synthetic.
//!
//! The model is edited as the user would edit the board ([`FakeGitHub::issue`],
//! [`FakeGitHub::set_items`], [`FakeGitHub::move_item`], [`FakeGitHub::set_options`],
//! [`FakeGitHub::delete_issue`], [`FakeGitHub::transfer_issue`],
//! [`FakeGitHub::convert_issue`]); every change to an issue moves its `updatedAt`, as on
//! GitHub, and a reorder or a Status change does not. Every request is logged by operation name
//! ([`FakeGitHub::requests`]).
//!
//! The mutations the backend sends (`UpdateStatus`, `AddComment`, `UpdateComment`,
//! `AddLabels`, `RemoveLabels`) are carried out as the viewer ([`FakeGitHub::set_viewer`]), the
//! login the token belongs to. A label exists in a repository once an issue of it carries the
//! label or [`FakeGitHub::define_label`] made it; a repository is private unless
//! [`FakeGitHub::set_visibility`] says otherwise. Faults are
//! injected per operation: [`FakeGitHub::fail_next`] (a lost response, a secondary rate limit,
//! a lost scope) and [`FakeGitHub::after_next`] (the user acting right after a request, before
//! the backend's next one).

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::sync::{Mutex, PoisonError};
use std::time::Duration;

use serde_json::{Value, json};

use super::transport::{GraphQl, RateLimit, Reply, TransportError};

/// The quota of the fake's rate limit; each request spends one point.
pub const QUOTA: u64 = 5000;

/// The viewer of a new fake: the login its token belongs to.
pub const VIEWER: &str = "viewer-example";

/// An issue of the fake.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FakeIssue {
    /// The title.
    pub title: String,
    /// The body.
    pub body: String,
    /// The login that opened it; `None` for a deleted account.
    pub author: Option<String>,
    /// The login that last edited the body, if anyone did.
    pub editor: Option<String>,
    /// Label names.
    pub labels: Vec<String>,
    /// Comments, oldest first.
    pub comments: Vec<FakeComment>,
    /// GitHub's issue dependencies: the issues blocking this one, as (`owner/name`, number).
    pub blocked_by: Vec<(String, u64)>,
    /// Closed (`state: CLOSED`).
    pub closed: bool,
}

/// A comment of the fake.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FakeComment {
    /// The node id; left empty, the fake assigns one.
    pub id: String,
    /// The login that wrote it.
    pub author: Option<String>,
    /// The login that last edited it, if anyone did.
    pub editor: Option<String>,
    /// The body, as GitHub stores it (a daemon comment carries its marker).
    pub body: String,
}

impl FakeComment {
    /// A comment by `author`.
    #[must_use]
    pub fn by(author: &str, body: &str) -> Self {
        Self {
            id: String::new(),
            author: Some(author.to_owned()),
            editor: None,
            body: body.to_owned(),
        }
    }
}

/// An item to put on a project with [`FakeGitHub::set_items`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FakeItem<'a> {
    /// The issue `number` of the repository `owner/name`, in the Status option named so
    /// (`None`: no Status).
    Issue(&'a str, u64, Option<&'a str>),
    /// A draft with this title, in the Status option named so.
    Draft(&'a str, Option<&'a str>),
    /// The issue, archived.
    Archived(&'a str, u64),
}

/// A fault for [`FakeGitHub::fail_next`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Fault {
    /// The request is carried out, but its response never arrives (a crash, a timeout): the
    /// backend sees a transport error.
    LoseResponse,
    /// Refused with a secondary rate limit: a 403 with `Retry-After`, nothing carried out.
    SecondaryRateLimit {
        /// The `Retry-After` header, in seconds.
        retry_after: u64,
    },
    /// Refused because the token lost the scope the request needs, nothing carried out.
    LostScope,
}

/// Something the user does right after a request; see [`FakeGitHub::after_next`].
struct Hook(Box<dyn FnOnce(&FakeGitHub) + Send>);

impl fmt::Debug for Hook {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Hook")
    }
}

#[derive(Debug, Clone)]
struct Item {
    id: String,
    archived: bool,
    /// The option id.
    status: Option<String>,
    content: Content,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Content {
    Issue(String, u64),
    Draft(String),
}

#[derive(Debug)]
struct Project {
    id: String,
    owner: String,
    number: u64,
    field: String,
    /// (id, name).
    options: Vec<(String, String)>,
    items: Vec<Item>,
}

#[derive(Debug)]
struct Stored {
    id: String,
    updated_at: String,
    issue: FakeIssue,
}

#[derive(Debug, Default)]
struct State {
    projects: Vec<Project>,
    issues: BTreeMap<(String, u64), Stored>,
    /// Every repository that ever had an issue.
    repositories: BTreeSet<String>,
    /// Old (repository, number) to new, for transferred issues.
    transferred: BTreeMap<(String, u64), (String, u64)>,
    /// (repository, number) of discussions converted from issues.
    discussions: BTreeSet<(String, u64)>,
    /// (repository, name) of labels defined without an issue carrying them.
    labels: BTreeSet<(String, String)>,
    /// `PUBLIC`, `PRIVATE` or `INTERNAL`, by repository; private when not set.
    visibility: BTreeMap<String, String>,
    viewer: String,
    faults: Vec<(String, Fault)>,
    hooks: Vec<(String, Hook)>,
    clock: u64,
    next_id: u64,
    log: Vec<String>,
}

impl State {
    fn id(&mut self, prefix: &str) -> String {
        self.next_id += 1;
        format!("{prefix}_fake{:04}", self.next_id)
    }

    fn tick(&mut self) -> String {
        self.clock += 1;
        let t = self.clock;
        format!(
            "2030-01-01T{:02}:{:02}:{:02}Z",
            t / 3600 % 24,
            t / 60 % 60,
            t % 60
        )
    }

    fn option_id(&self, project: usize, name: &str) -> String {
        self.projects[project]
            .options
            .iter()
            .find(|(_, n)| n == name)
            .map_or_else(
                || panic!("the fake project has no option `{name}`"),
                |(id, _)| id.clone(),
            )
    }

    fn issue_by_node(&self, id: &str) -> Option<(&(String, u64), &Stored)> {
        self.issues.iter().find(|(_, s)| s.id == id)
    }

    /// Gives every comment without an id one.
    fn number_comments(&mut self, key: &(String, u64)) {
        let missing = self.issues[key]
            .issue
            .comments
            .iter()
            .filter(|c| c.id.is_empty())
            .count();
        let ids: Vec<String> = (0..missing).map(|_| self.id("IC")).collect();
        let mut ids = ids.into_iter();
        if let Some(stored) = self.issues.get_mut(key) {
            for comment in &mut stored.issue.comments {
                if comment.id.is_empty() {
                    comment.id = ids.next().unwrap_or_default();
                }
            }
        }
    }

    /// Removes the issue and its project items.
    fn remove_issue(&mut self, key: &(String, u64)) -> Option<Stored> {
        let content = Content::Issue(key.0.clone(), key.1);
        for project in &mut self.projects {
            project.items.retain(|i| i.content != content);
        }
        self.issues.remove(key)
    }
}

/// The fake GitHub; see the module docs.
#[derive(Debug)]
pub struct FakeGitHub {
    state: Mutex<State>,
}

impl Default for FakeGitHub {
    fn default() -> Self {
        Self::new()
    }
}

impl FakeGitHub {
    /// An empty GitHub, seen by [`VIEWER`].
    #[must_use]
    pub fn new() -> Self {
        Self {
            state: Mutex::new(State {
                viewer: VIEWER.to_owned(),
                ..State::default()
            }),
        }
    }

    /// Makes `login` the viewer: the login the backend's token belongs to, which writes its
    /// comments.
    pub fn set_viewer(&self, login: &str) {
        login.clone_into(&mut self.lock().viewer);
    }

    /// Adds project `number` of `owner`, with a single-select Status field holding `options`
    /// in this order. Returns its index for the other calls.
    pub fn add_project(&self, owner: &str, number: u64, options: &[&str]) -> usize {
        let mut state = self.lock();
        let id = state.id("PVT");
        state.projects.push(Project {
            id,
            owner: owner.to_owned(),
            number,
            field: "Status".to_owned(),
            options: Vec::new(),
            items: Vec::new(),
        });
        let index = state.projects.len() - 1;
        drop(state);
        self.set_options(index, options);
        index
    }

    /// Replaces the options of the project's Status field. An option whose name is kept keeps
    /// its id; a new name gets a new id.
    pub fn set_options(&self, project: usize, options: &[&str]) {
        let mut state = self.lock();
        let old = std::mem::take(&mut state.projects[project].options);
        let mut new = Vec::new();
        for name in options {
            let id = if let Some((id, _)) = old.iter().find(|(_, n)| n == name) {
                id.clone()
            } else {
                state.next_id += 1;
                format!("opt{:04}", state.next_id)
            };
            new.push((id, (*name).to_owned()));
        }
        state.projects[project].options = new;
    }

    /// Renames the option `from` to `to`, keeping its id: what a rename in GitHub's UI does.
    pub fn rename_option(&self, project: usize, from: &str, to: &str) {
        let mut state = self.lock();
        for (_, name) in &mut state.projects[project].options {
            if name == from {
                to.clone_into(name);
            }
        }
    }

    /// Edits (or creates, from the default) issue `number` of `repository`. When the edit
    /// changes anything, the issue's `updatedAt` moves. Comments added without an id get one.
    pub fn issue(&self, repository: &str, number: u64, edit: impl FnOnce(&mut FakeIssue)) {
        let mut state = self.lock();
        let key = (repository.to_owned(), number);
        state.repositories.insert(repository.to_owned());
        if !state.issues.contains_key(&key) {
            let id = state.id("I");
            let updated_at = state.tick();
            state.issues.insert(
                key.clone(),
                Stored {
                    id,
                    updated_at,
                    issue: FakeIssue::default(),
                },
            );
        }
        let before = state.issues[&key].issue.clone();
        let mut after = before.clone();
        edit(&mut after);
        if after != before {
            let updated_at = state.tick();
            if let Some(stored) = state.issues.get_mut(&key) {
                stored.issue = after;
                stored.updated_at = updated_at;
            }
            state.number_comments(&key);
        }
    }

    /// Issue `number` of `repository` as it is now.
    ///
    /// # Panics
    ///
    /// The issue does not exist.
    #[must_use]
    pub fn issue_now(&self, repository: &str, number: u64) -> FakeIssue {
        self.lock().issues[&(repository.to_owned(), number)]
            .issue
            .clone()
    }

    /// Makes the project hold exactly `items`, in this order. An issue already on the project
    /// keeps its item; Status changes and moves do not touch the issue.
    ///
    /// # Panics
    ///
    /// An item names an issue that does not exist, or an option the project does not have.
    pub fn set_items(&self, project: usize, items: &[FakeItem<'_>]) {
        let mut state = self.lock();
        let old = std::mem::take(&mut state.projects[project].items);
        let mut new = Vec::new();
        for item in items {
            let (content, status, archived) = match *item {
                FakeItem::Issue(repository, number, status) => {
                    (Content::Issue(repository.to_owned(), number), status, false)
                }
                FakeItem::Archived(repository, number) => {
                    (Content::Issue(repository.to_owned(), number), None, true)
                }
                FakeItem::Draft(title, status) => (Content::Draft(title.to_owned()), status, false),
            };
            if let Content::Issue(repository, number) = &content {
                assert!(
                    state.issues.contains_key(&(repository.clone(), *number)),
                    "the fake has no issue {repository}#{number}"
                );
            }
            let status = status.map(|name| state.option_id(project, name));
            let id = old
                .iter()
                .find(|i| i.content == content)
                .map_or_else(|| state.id("PVTI"), |i| i.id.clone());
            new.push(Item {
                id,
                archived,
                status,
                content,
            });
        }
        state.projects[project].items = new;
    }

    /// Moves the card of issue `number` of `repository` on the project into the Status option
    /// named `status` (`None`: no Status), as the user dragging it.
    ///
    /// # Panics
    ///
    /// The issue has no card on the project, or the project has no such option.
    pub fn move_item(&self, project: usize, repository: &str, number: u64, status: Option<&str>) {
        let mut state = self.lock();
        let option = status.map(|name| state.option_id(project, name));
        let content = Content::Issue(repository.to_owned(), number);
        let item = state.projects[project]
            .items
            .iter_mut()
            .find(|i| i.content == content)
            .unwrap_or_else(|| panic!("{repository}#{number} has no card on the project"));
        item.status = option;
    }

    /// The name of the Status option of the card of issue `number` of `repository` on the
    /// project; `None` when it has no card or no Status.
    #[must_use]
    pub fn status_of(&self, project: usize, repository: &str, number: u64) -> Option<String> {
        let state = self.lock();
        let project = &state.projects[project];
        let content = Content::Issue(repository.to_owned(), number);
        let option = project
            .items
            .iter()
            .find(|i| i.content == content)?
            .status
            .clone()?;
        project
            .options
            .iter()
            .find(|(id, _)| *id == option)
            .map(|(_, name)| name.clone())
    }

    /// Deletes issue `number` of `repository`, and with it its cards.
    pub fn delete_issue(&self, repository: &str, number: u64) {
        self.lock().remove_issue(&(repository.to_owned(), number));
    }

    /// Transfers issue `number` of `repository` to `to`, where it gets the number `new_number`.
    /// Its cards stay on the projects, now holding the issue of `to`; asked by its old number,
    /// GitHub answers with the issue in its new repository.
    ///
    /// # Panics
    ///
    /// The issue does not exist.
    pub fn transfer_issue(&self, repository: &str, number: u64, to: &str, new_number: u64) {
        let mut state = self.lock();
        let old = (repository.to_owned(), number);
        let new = (to.to_owned(), new_number);
        let mut stored = state
            .issues
            .remove(&old)
            .unwrap_or_else(|| panic!("the fake has no issue {repository}#{number}"));
        stored.updated_at = state.tick();
        state.issues.insert(new.clone(), stored);
        state.repositories.insert(to.to_owned());
        for project in &mut state.projects {
            for item in &mut project.items {
                if item.content == Content::Issue(old.0.clone(), old.1) {
                    item.content = Content::Issue(new.0.clone(), new.1);
                }
            }
        }
        state.transferred.insert(old, new);
    }

    /// Converts issue `number` of `repository` to a discussion with the same number; the issue
    /// and its cards are gone.
    pub fn convert_issue(&self, repository: &str, number: u64) {
        let mut state = self.lock();
        let key = (repository.to_owned(), number);
        state.remove_issue(&key);
        state.discussions.insert(key);
    }

    /// Defines the label `name` in `repository`, carried by no issue yet.
    pub fn define_label(&self, repository: &str, name: &str) {
        let mut state = self.lock();
        state.repositories.insert(repository.to_owned());
        state
            .labels
            .insert((repository.to_owned(), name.to_owned()));
    }

    /// Makes `repository` `PUBLIC`, `PRIVATE` or `INTERNAL`.
    pub fn set_visibility(&self, repository: &str, visibility: &str) {
        let mut state = self.lock();
        state.repositories.insert(repository.to_owned());
        state
            .visibility
            .insert(repository.to_owned(), visibility.to_owned());
    }

    /// Closes issue `number` of `repository`, as the user would; its `updatedAt` moves.
    pub fn close_issue(&self, repository: &str, number: u64) {
        self.issue(repository, number, |issue| issue.closed = true);
    }

    /// The next request of `operation` meets `fault`.
    pub fn fail_next(&self, operation: &str, fault: Fault) {
        self.lock().faults.push((operation.to_owned(), fault));
    }

    /// Right after the next request of `operation` is carried out, and before its response
    /// reaches the backend, `then` runs: the user acting between two of the backend's requests.
    pub fn after_next(&self, operation: &str, then: impl FnOnce(&Self) + Send + 'static) {
        self.lock()
            .hooks
            .push((operation.to_owned(), Hook(Box::new(then))));
    }

    /// The operation names of every request so far, in order.
    #[must_use]
    pub fn requests(&self) -> Vec<String> {
        self.lock().log.clone()
    }

    /// How many requests of `operation` were made so far.
    #[must_use]
    pub fn count(&self, operation: &str) -> usize {
        self.lock().log.iter().filter(|o| *o == operation).count()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

impl GraphQl for FakeGitHub {
    fn execute(&self, query: &str, variables: &Value) -> Result<Reply, TransportError> {
        let mut state = self.lock();
        let trimmed = query.trim_start();
        let operation = trimmed
            .strip_prefix("query ")
            .or_else(|| trimmed.strip_prefix("mutation "))
            .and_then(|rest| rest.split(['(', ' ', '{']).next())
            .unwrap_or("anonymous")
            .to_owned();
        state.log.push(operation.clone());
        let used = state.log.len() as u64;
        let rate_limit = RateLimit {
            limit: QUOTA,
            remaining: QUOTA.saturating_sub(used),
            used,
            reset: 1_900_000_000,
        };
        let fault = state
            .faults
            .iter()
            .position(|(op, _)| *op == operation)
            .map(|at| state.faults.remove(at).1);
        match fault {
            Some(Fault::SecondaryRateLimit { retry_after }) => {
                return Err(TransportError::Status {
                    status: 403,
                    message: "You have exceeded a secondary rate limit. Please wait a few \
                              minutes before you try again."
                        .into(),
                    rate_limit: Some(rate_limit),
                    retry_after: Some(retry_after),
                });
            }
            Some(Fault::LostScope) => {
                let body = json!({
                    "data": null,
                    "errors": [{
                        "type": "INSUFFICIENT_SCOPES",
                        "message": "Your token has not been granted the required scopes to \
                                    execute this query.",
                    }],
                });
                return Reply::from_body(&body.to_string(), Some(rate_limit));
            }
            Some(Fault::LoseResponse) | None => {}
        }
        let body = match operation.as_str() {
            "Resolve" => resolve(&state, variables),
            "Items" => items(&state, variables),
            "Details" => details(&state, variables),
            "Comments" => comments_of(&state, variables),
            "Issue" => issue_query(&state, variables),
            "Discussion" => discussion(&state, variables),
            "UpdateStatus" => update_status(&mut state, variables),
            "AddComment" => add_comment(&mut state, variables),
            "UpdateComment" => update_comment(&mut state, variables),
            "RemoveLabels" => remove_labels(&mut state, variables),
            "Label" => label_query(&state, variables),
            "AddLabels" => add_labels(&mut state, variables),
            "Visibility" => visibility(&state, variables),
            other => {
                return Err(TransportError::Malformed(format!(
                    "the fake does not know the operation `{other}`"
                )));
            }
        };
        let hook = state
            .hooks
            .iter()
            .position(|(op, _)| *op == operation)
            .map(|at| state.hooks.remove(at).1);
        drop(state);
        if let Some(Hook(then)) = hook {
            then(self);
        }
        if fault == Some(Fault::LoseResponse) {
            return Err(TransportError::Timeout(Duration::from_secs(60)));
        }
        Reply::from_body(&body.to_string(), Some(rate_limit))
    }
}

fn var<'v>(variables: &'v Value, name: &str) -> &'v Value {
    &variables[name]
}

fn first(variables: &Value) -> usize {
    usize::try_from(var(variables, "first").as_u64().unwrap_or(100)).unwrap_or(usize::MAX)
}

/// A page of `all` after the cursor in `variables` (cursors are offsets).
fn page(all: &[Value], variables: &Value) -> Value {
    let start: usize = var(variables, "after")
        .as_str()
        .and_then(|c| c.strip_prefix("cursor:"))
        .and_then(|n| n.parse().ok())
        .unwrap_or(0);
    let end = (start + first(variables)).min(all.len());
    let has_next = end < all.len();
    json!({
        "pageInfo": {
            "hasNextPage": has_next,
            "endCursor": if end > start { json!(format!("cursor:{end}")) } else { Value::Null },
        },
        "nodes": all.get(start..end).unwrap_or_default(),
    })
}

fn not_found(message: &str) -> Value {
    json!([{ "type": "NOT_FOUND", "message": message }])
}

fn field_json(project: &Project, name: &str) -> Value {
    if project.field != name {
        return Value::Null;
    }
    json!({
        "__typename": "ProjectV2SingleSelectField",
        "id": format!("{}_status", project.id),
        "options": project.options.iter().map(|(id, name)| json!({ "id": id, "name": name }))
            .collect::<Vec<_>>(),
    })
}

fn resolve(state: &State, variables: &Value) -> Value {
    let owner = var(variables, "owner").as_str().unwrap_or_default();
    let number = var(variables, "number").as_u64().unwrap_or_default();
    let field = var(variables, "field").as_str().unwrap_or_default();
    let known_owner = state.projects.iter().any(|p| p.owner == owner)
        || state
            .repositories
            .iter()
            .any(|r| r.split('/').next() == Some(owner));
    if !known_owner {
        return json!({ "data": { "repositoryOwner": null } });
    }
    match state
        .projects
        .iter()
        .find(|p| p.owner == owner && p.number == number)
    {
        None => json!({
            "data": { "repositoryOwner": { "projectV2": null } },
            "errors": not_found("Could not resolve to a ProjectV2."),
        }),
        Some(project) => json!({
            "data": { "repositoryOwner": { "projectV2": {
                "id": project.id,
                "closed": false,
                "field": field_json(project, field),
            } } }
        }),
    }
}

fn status_json(project: &Project, item: &Item) -> Value {
    item.status.as_ref().map_or(Value::Null, |option| {
        // An option deleted from the field leaves the item without a Status, as on GitHub.
        if project.options.iter().any(|(id, _)| id == option) {
            json!({ "__typename": "ProjectV2ItemFieldSingleSelectValue", "optionId": option })
        } else {
            Value::Null
        }
    })
}

fn items(state: &State, variables: &Value) -> Value {
    let id = var(variables, "project").as_str().unwrap_or_default();
    let field = var(variables, "field").as_str().unwrap_or_default();
    let Some(project) = state.projects.iter().find(|p| p.id == id) else {
        return json!({ "data": { "node": null } });
    };
    let nodes: Vec<Value> = project
        .items
        .iter()
        .map(|item| {
            let (kind, content) = match &item.content {
                Content::Draft(_) => ("DRAFT_ISSUE", json!({ "__typename": "DraftIssue" })),
                Content::Issue(repository, number) => {
                    let stored = &state.issues[&(repository.clone(), *number)];
                    (
                        "ISSUE",
                        json!({
                            "__typename": "Issue",
                            "id": stored.id,
                            "number": number,
                            "updatedAt": stored.updated_at,
                            "repository": { "nameWithOwner": repository },
                        }),
                    )
                }
            };
            json!({
                "type": kind,
                "isArchived": item.archived,
                "status": status_json(project, item),
                "content": content,
            })
        })
        .collect();
    json!({ "data": { "node": {
        "field": field_json(project, field),
        "items": page(&nodes, variables),
    } } })
}

fn comment_json(state: &State, comment: &FakeComment) -> Value {
    json!({
        "id": comment.id,
        "viewerDidAuthor": comment.author.as_deref() == Some(state.viewer.as_str()),
        "author": comment.author.as_ref().map(|l| json!({ "login": l })),
        "editor": comment.editor.as_ref().map(|l| json!({ "login": l })),
        "body": comment.body,
    })
}

fn issue_json(state: &State, key: &(String, u64), stored: &Stored, variables: &Value) -> Value {
    let issue = &stored.issue;
    let comments: Vec<Value> = issue
        .comments
        .iter()
        .map(|c| comment_json(state, c))
        .collect();
    let comments_page = page(&comments, &json!({ "first": var(variables, "first") }));
    let blocked_by: Vec<Value> = issue
        .blocked_by
        .iter()
        .map(|(repository, number)| {
            json!({ "number": number, "repository": { "nameWithOwner": repository } })
        })
        .collect();
    let mut value = json!({
        "__typename": "Issue",
        "id": stored.id,
        "number": key.1,
        "title": issue.title,
        "body": issue.body,
        "closed": issue.closed,
        "state": if issue.closed { "CLOSED" } else { "OPEN" },
        "author": issue.author.as_ref().map(|l| json!({ "login": l })),
        "editor": issue.editor.as_ref().map(|l| json!({ "login": l })),
        "labels": { "nodes": issue.labels.iter().map(|n| json!({ "id": label_id(n), "name": n }))
            .collect::<Vec<_>>() },
        "blockedBy": { "nodes": blocked_by },
        "comments": comments_page,
        "repository": { "nameWithOwner": key.0 },
    });
    let field = var(variables, "field").as_str().unwrap_or_default();
    let project_items: Vec<Value> = state
        .projects
        .iter()
        .flat_map(|project| {
            project
                .items
                .iter()
                .filter(|i| i.content == Content::Issue(key.0.clone(), key.1))
                .map(move |item| {
                    json!({
                        "id": item.id,
                        "isArchived": item.archived,
                        "project": { "id": project.id },
                        "status": if project.field == field {
                            status_json(project, item)
                        } else {
                            Value::Null
                        },
                    })
                })
        })
        .collect();
    value["projectItems"] = json!({ "nodes": project_items });
    value
}

fn details(state: &State, variables: &Value) -> Value {
    let nodes: Vec<Value> = var(variables, "ids")
        .as_array()
        .into_iter()
        .flatten()
        .map(|id| {
            id.as_str()
                .and_then(|id| state.issue_by_node(id))
                .map_or(Value::Null, |(key, stored)| {
                    issue_json(state, key, stored, variables)
                })
        })
        .collect();
    json!({ "data": { "nodes": nodes } })
}

fn comments_of(state: &State, variables: &Value) -> Value {
    let id = var(variables, "issue").as_str().unwrap_or_default();
    match state.issue_by_node(id) {
        None => json!({ "data": { "node": null } }),
        Some((_, stored)) => {
            let comments: Vec<Value> = stored
                .issue
                .comments
                .iter()
                .map(|c| comment_json(state, c))
                .collect();
            json!({ "data": { "node": { "comments": page(&comments, variables) } } })
        }
    }
}

fn repository_of(variables: &Value) -> String {
    let owner = var(variables, "owner").as_str().unwrap_or_default();
    let name = var(variables, "name").as_str().unwrap_or_default();
    format!("{owner}/{name}")
}

fn issue_query(state: &State, variables: &Value) -> Value {
    let repository = repository_of(variables);
    let number = var(variables, "number").as_u64().unwrap_or_default();
    if !state.repositories.contains(&repository) {
        return json!({
            "data": { "repository": null },
            "errors": not_found("Could not resolve to a Repository."),
        });
    }
    let mut key = (repository, number);
    if let Some(new) = state.transferred.get(&key) {
        key = new.clone();
    }
    match state.issues.get(&key) {
        None => json!({
            "data": { "repository": { "issue": null } },
            "errors": not_found("Could not resolve to an Issue."),
        }),
        Some(stored) => json!({
            "data": { "repository": { "issue": issue_json(state, &key, stored, variables) } }
        }),
    }
}

fn discussion(state: &State, variables: &Value) -> Value {
    let key = (
        repository_of(variables),
        var(variables, "number").as_u64().unwrap_or_default(),
    );
    if state.discussions.contains(&key) {
        json!({ "data": { "repository": { "discussion": {
            "id": format!("D_fake_{}_{}", key.0, key.1),
        } } } })
    } else {
        json!({
            "data": { "repository": { "discussion": null } },
            "errors": not_found("Could not resolve to a Discussion."),
        })
    }
}

fn update_status(state: &mut State, variables: &Value) -> Value {
    let gone = || {
        json!({
            "data": { "updateProjectV2ItemFieldValue": null },
            "errors": not_found("Could not resolve to a node."),
        })
    };
    let text = |name: &str| var(variables, name).as_str().unwrap_or_default().to_owned();
    let (project, item, field, option) =
        (text("project"), text("item"), text("field"), text("option"));
    let Some(project) = state.projects.iter_mut().find(|p| p.id == project) else {
        return gone();
    };
    if field != format!("{}_status", project.id)
        || !project.options.iter().any(|(id, _)| *id == option)
    {
        return gone();
    }
    let Some(found) = project.items.iter_mut().find(|i| i.id == item) else {
        return gone();
    };
    found.status = Some(option);
    json!({ "data": { "updateProjectV2ItemFieldValue": { "projectV2Item": { "id": item } } } })
}

fn add_comment(state: &mut State, variables: &Value) -> Value {
    let subject = var(variables, "subject").as_str().unwrap_or_default();
    let body = var(variables, "body")
        .as_str()
        .unwrap_or_default()
        .to_owned();
    let Some(key) = state.issue_by_node(subject).map(|(key, _)| key.clone()) else {
        return json!({
            "data": { "addComment": null },
            "errors": not_found("Could not resolve to a node."),
        });
    };
    let id = state.id("IC");
    let updated_at = state.tick();
    let author = Some(state.viewer.clone());
    if let Some(stored) = state.issues.get_mut(&key) {
        stored.issue.comments.push(FakeComment {
            id: id.clone(),
            author,
            editor: None,
            body,
        });
        stored.updated_at = updated_at;
    }
    json!({ "data": { "addComment": { "commentEdge": { "node": { "id": id } } } } })
}

fn update_comment(state: &mut State, variables: &Value) -> Value {
    let id = var(variables, "id").as_str().unwrap_or_default();
    let body = var(variables, "body")
        .as_str()
        .unwrap_or_default()
        .to_owned();
    let viewer = state.viewer.clone();
    let updated_at = state.tick();
    for stored in state.issues.values_mut() {
        if let Some(comment) = stored.issue.comments.iter_mut().find(|c| c.id == id) {
            if comment.author.as_deref() != Some(viewer.as_str()) {
                return json!({
                    "data": { "updateIssueComment": null },
                    "errors": [{ "type": "FORBIDDEN", "message": "Viewer cannot update." }],
                });
            }
            comment.body = body;
            comment.editor = Some(viewer);
            stored.updated_at = updated_at;
            return json!({ "data": { "updateIssueComment": { "issueComment": { "id": id } } } });
        }
    }
    json!({
        "data": { "updateIssueComment": null },
        "errors": not_found("Could not resolve to a node."),
    })
}

/// The node id the fake gives the label `name`.
fn label_id(name: &str) -> String {
    format!("LA_{name}")
}

/// The label `name` of the repository, if an issue carries it or it was defined.
fn label_query(state: &State, variables: &Value) -> Value {
    let repository = repository_of(variables);
    if !state.repositories.contains(&repository) {
        return json!({
            "data": { "repository": null },
            "errors": not_found("Could not resolve to a Repository."),
        });
    }
    let name = var(variables, "label").as_str().unwrap_or_default();
    let exists = state
        .labels
        .contains(&(repository.clone(), name.to_owned()))
        || state
            .issues
            .iter()
            .any(|((r, _), s)| *r == repository && s.issue.labels.iter().any(|l| l == name));
    let label = if exists {
        json!({ "id": label_id(name), "name": name })
    } else {
        Value::Null
    };
    json!({ "data": { "repository": { "label": label } } })
}

fn add_labels(state: &mut State, variables: &Value) -> Value {
    let subject = var(variables, "labelable").as_str().unwrap_or_default();
    let names: Vec<String> = var(variables, "labels")
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .filter_map(|id| id.strip_prefix("LA_").map(str::to_owned))
        .collect();
    let Some(key) = state.issue_by_node(subject).map(|(key, _)| key.clone()) else {
        return json!({
            "data": { "addLabelsToLabelable": null },
            "errors": not_found("Could not resolve to a node."),
        });
    };
    let updated_at = state.tick();
    if let Some(stored) = state.issues.get_mut(&key) {
        for name in names {
            if !stored.issue.labels.contains(&name) {
                stored.issue.labels.push(name);
            }
        }
        stored.updated_at = updated_at;
    }
    json!({ "data": { "addLabelsToLabelable": { "clientMutationId": null } } })
}

fn visibility(state: &State, variables: &Value) -> Value {
    let repository = repository_of(variables);
    if !state.repositories.contains(&repository) {
        return json!({
            "data": { "repository": null },
            "errors": not_found("Could not resolve to a Repository."),
        });
    }
    let visibility = state
        .visibility
        .get(&repository)
        .map_or("PRIVATE", String::as_str);
    json!({ "data": { "repository": { "visibility": visibility } } })
}

fn remove_labels(state: &mut State, variables: &Value) -> Value {
    let subject = var(variables, "labelable").as_str().unwrap_or_default();
    let ids: Vec<&str> = var(variables, "labels")
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .collect();
    let Some(key) = state.issue_by_node(subject).map(|(key, _)| key.clone()) else {
        return json!({
            "data": { "removeLabelsFromLabelable": null },
            "errors": not_found("Could not resolve to a node."),
        });
    };
    let updated_at = state.tick();
    if let Some(stored) = state.issues.get_mut(&key) {
        stored
            .issue
            .labels
            .retain(|name| !ids.contains(&label_id(name).as_str()));
        stored.updated_at = updated_at;
    }
    json!({ "data": { "removeLabelsFromLabelable": { "clientMutationId": null } } })
}
