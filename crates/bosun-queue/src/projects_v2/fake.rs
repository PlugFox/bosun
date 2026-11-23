//! A fake GitHub for the Projects v2 backend's tests (feature `testsuite`): an in-memory model
//! of projects and issues that answers the backend's GraphQL operations (by operation name)
//! with responses in GitHub's shapes, paginated as GitHub paginates. Everything in it is
//! synthetic.
//!
//! The model is edited as the user would edit the board ([`FakeGitHub::issue`],
//! [`FakeGitHub::set_items`], [`FakeGitHub::set_options`]); every change to an issue moves its
//! `updatedAt`, as on GitHub, and a reorder or a Status change does not. Every request is
//! logged by operation name ([`FakeGitHub::requests`]).

use std::collections::BTreeMap;
use std::sync::{Mutex, PoisonError};

use serde_json::{Value, json};

use super::transport::{GraphQl, RateLimit, Reply, TransportError};

/// The quota of the fake's rate limit; each request spends one point.
pub const QUOTA: u64 = 5000;

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
}

/// A comment of the fake.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FakeComment {
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
}

/// The fake GitHub; see the module docs.
#[derive(Debug, Default)]
pub struct FakeGitHub {
    state: Mutex<State>,
}

impl FakeGitHub {
    /// An empty GitHub.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
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
    /// changes anything, the issue's `updatedAt` moves.
    pub fn issue(&self, repository: &str, number: u64, edit: impl FnOnce(&mut FakeIssue)) {
        let mut state = self.lock();
        let key = (repository.to_owned(), number);
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
        }
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

    /// The operation names of every request so far, in order.
    #[must_use]
    pub fn requests(&self) -> Vec<String> {
        self.lock().log.clone()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

impl GraphQl for FakeGitHub {
    fn execute(&self, query: &str, variables: &Value) -> Result<Reply, TransportError> {
        let mut state = self.lock();
        let operation = query
            .trim_start()
            .strip_prefix("query ")
            .and_then(|rest| rest.split(['(', ' ', '{']).next())
            .unwrap_or("anonymous")
            .to_owned();
        state.log.push(operation.clone());
        let used = state.log.len() as u64;
        let body = match operation.as_str() {
            "Resolve" => resolve(&state, variables),
            "Items" => items(&state, variables),
            "Details" => details(&state, variables),
            "Comments" => comments_of(&state, variables),
            "Issue" => issue_query(&state, variables),
            other => {
                return Err(TransportError::Malformed(format!(
                    "the fake does not know the operation `{other}`"
                )));
            }
        };
        let rate_limit = RateLimit {
            limit: QUOTA,
            remaining: QUOTA.saturating_sub(used),
            used,
            reset: 1_900_000_000,
        };
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
            .issues
            .keys()
            .any(|(r, _)| r.split('/').next() == Some(owner));
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

fn comment_json(comment: &FakeComment) -> Value {
    json!({
        "author": comment.author.as_ref().map(|l| json!({ "login": l })),
        "editor": comment.editor.as_ref().map(|l| json!({ "login": l })),
        "body": comment.body,
    })
}

fn issue_json(state: &State, key: &(String, u64), stored: &Stored, variables: &Value) -> Value {
    let issue = &stored.issue;
    let comments: Vec<Value> = issue.comments.iter().map(comment_json).collect();
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
        "author": issue.author.as_ref().map(|l| json!({ "login": l })),
        "editor": issue.editor.as_ref().map(|l| json!({ "login": l })),
        "labels": { "nodes": issue.labels.iter().map(|n| json!({ "name": n }))
            .collect::<Vec<_>>() },
        "blockedBy": { "nodes": blocked_by },
        "comments": comments_page,
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
            let comments: Vec<Value> = stored.issue.comments.iter().map(comment_json).collect();
            json!({ "data": { "node": { "comments": page(&comments, variables) } } })
        }
    }
}

fn issue_query(state: &State, variables: &Value) -> Value {
    let owner = var(variables, "owner").as_str().unwrap_or_default();
    let name = var(variables, "name").as_str().unwrap_or_default();
    let number = var(variables, "number").as_u64().unwrap_or_default();
    let repository = format!("{owner}/{name}");
    if !state.issues.keys().any(|(r, _)| *r == repository) {
        return json!({
            "data": { "repository": null },
            "errors": not_found("Could not resolve to a Repository."),
        });
    }
    let key = (repository, number);
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
