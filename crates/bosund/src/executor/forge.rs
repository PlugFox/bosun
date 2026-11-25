//! The forge seam of the `pr` landing mode (#8 "The pull request"): looking up the item's open
//! pull request by its head branch, opening one, and rewriting its body. The daemon's own
//! [`GhForge`] runs `gh` in the user's checkout (`repo.path`) under [`At::env`], the
//! environment of the repository's identity for the daemon's own tools
//! ([`bosun_identity::Env::tool`], #2), never an agent's. The scenario tests put a fake in
//! [`super::Deps::forge`].
//!
//! Every answer is one of two failures or a result: [`ForgeError::Refused`] (the service
//! answered no: branch protection, token scope, a stale branch), which needs the user, and
//! [`ForgeError::Unreachable`] (no answer: network, DNS, timeout, a 5xx), which the landing
//! waits out. The argument lists are built by pure functions ([`list_args`], [`create_args`],
//! [`edit_args`]); a body goes to `gh` in a file, never on its command line.

use std::fmt;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use bosun_identity::Env;

/// Where and as whom the forge is asked: the user's checkout and the identity's tool
/// environment.
#[derive(Debug, Clone)]
pub(crate) struct At {
    /// The repository `gh` runs in (`repo.path`): its `origin` names the GitHub repository.
    pub(crate) dir: PathBuf,
    /// The environment `gh` runs with.
    pub(crate) env: Env,
}

/// A pull request as the forge reports it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PullRequest {
    pub(crate) number: u64,
    pub(crate) url: String,
    /// Its body now: people may have edited it.
    pub(crate) body: String,
}

/// A pull request to open.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct NewPullRequest {
    /// The branch it proposes (`bosun/<slug>`).
    pub(crate) head: String,
    /// The branch it goes into: the item's target.
    pub(crate) base: String,
    pub(crate) title: String,
    pub(crate) body: String,
    pub(crate) draft: bool,
    pub(crate) reviewers: Vec<String>,
    pub(crate) labels: Vec<String>,
}

/// Why the forge did not do it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ForgeError {
    /// The service answered no: the item needs the user.
    Refused(String),
    /// No answer (network, DNS, timeout, 5xx): asked again later.
    Unreachable(String),
}

impl fmt::Display for ForgeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Refused(e) => write!(f, "refused: {e}"),
            Self::Unreachable(e) => write!(f, "unreachable: {e}"),
        }
    }
}

/// The pull requests of the repository at [`At`].
pub(crate) trait Forge: Send + Sync + fmt::Debug {
    /// The open pull request whose head is `head_branch`, if there is one.
    ///
    /// # Errors
    ///
    /// [`ForgeError`].
    fn open_pull_request(
        &self,
        at: &At,
        head_branch: &str,
    ) -> Result<Option<PullRequest>, ForgeError>;

    /// Opens `new`.
    ///
    /// # Errors
    ///
    /// [`ForgeError`].
    fn create(&self, at: &At, new: &NewPullRequest) -> Result<PullRequest, ForgeError>;

    /// Replaces the body of pull request `number` with `body`.
    ///
    /// # Errors
    ///
    /// [`ForgeError`].
    fn set_body(&self, at: &At, number: u64, body: &str) -> Result<(), ForgeError>;
}

/// The daemon's forge: `gh` in [`At::dir`] under [`At::env`].
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct GhForge;

impl Forge for GhForge {
    fn open_pull_request(
        &self,
        at: &At,
        head_branch: &str,
    ) -> Result<Option<PullRequest>, ForgeError> {
        let out = gh(at, &list_args(head_branch))?;
        parse_list(&out).map_err(ForgeError::Refused)
    }

    fn create(&self, at: &At, new: &NewPullRequest) -> Result<PullRequest, ForgeError> {
        let file = body_file(&new.body)?;
        let out = gh(at, &create_args(new, file.path()))?;
        let url = out
            .trim()
            .lines()
            .last()
            .unwrap_or_default()
            .trim()
            .to_owned();
        let number = number_of(&url).ok_or_else(|| {
            ForgeError::Refused(format!("`gh pr create` printed no pull request URL: {url}"))
        })?;
        Ok(PullRequest {
            number,
            url,
            body: new.body.clone(),
        })
    }

    fn set_body(&self, at: &At, number: u64, body: &str) -> Result<(), ForgeError> {
        let file = body_file(body)?;
        gh(at, &edit_args(number, file.path())).map(drop)
    }
}

/// `gh pr list` for the open pull request of `head`.
pub(crate) fn list_args(head: &str) -> Vec<String> {
    [
        "pr",
        "list",
        "--head",
        head,
        "--state",
        "open",
        "--json",
        "number,url,body",
    ]
    .map(str::to_owned)
    .to_vec()
}

/// `gh pr create` for `new`, its body read from `body_file`.
pub(crate) fn create_args(new: &NewPullRequest, body_file: &Path) -> Vec<String> {
    let mut args: Vec<String> = ["pr", "create", "--head", &new.head, "--base", &new.base]
        .map(str::to_owned)
        .to_vec();
    args.extend(["--title".to_owned(), new.title.clone()]);
    args.extend([
        "--body-file".to_owned(),
        body_file.to_string_lossy().into_owned(),
    ]);
    if new.draft {
        args.push("--draft".to_owned());
    }
    for reviewer in &new.reviewers {
        args.extend(["--reviewer".to_owned(), reviewer.clone()]);
    }
    for label in &new.labels {
        args.extend(["--label".to_owned(), label.clone()]);
    }
    args
}

/// `gh pr edit` of pull request `number`, its body read from `body_file`.
pub(crate) fn edit_args(number: u64, body_file: &Path) -> Vec<String> {
    vec![
        "pr".to_owned(),
        "edit".to_owned(),
        number.to_string(),
        "--body-file".to_owned(),
        body_file.to_string_lossy().into_owned(),
    ]
}

/// The first pull request of `gh pr list --json number,url,body` output.
fn parse_list(out: &str) -> Result<Option<PullRequest>, String> {
    #[derive(serde::Deserialize)]
    struct Listed {
        number: u64,
        url: String,
        #[serde(default)]
        body: String,
    }
    let listed: Vec<Listed> = serde_json::from_str(out.trim())
        .map_err(|e| format!("`gh pr list` printed what is not a list of pull requests: {e}"))?;
    Ok(listed.into_iter().next().map(|l| PullRequest {
        number: l.number,
        url: l.url,
        body: l.body,
    }))
}

/// The number at the end of a pull request URL (`.../pull/12`).
fn number_of(url: &str) -> Option<u64> {
    url.rsplit_once("/pull/")?
        .1
        .trim_end_matches('/')
        .parse()
        .ok()
}

/// `body` in a temporary file, for `--body-file`.
fn body_file(body: &str) -> Result<tempfile::NamedTempFile, ForgeError> {
    let mut file = tempfile::Builder::new()
        .prefix("bosun-pr-body-")
        .suffix(".md")
        .tempfile()
        .map_err(|e| ForgeError::Refused(format!("the body's temporary file: {e}")))?;
    file.write_all(body.as_bytes())
        .and_then(|()| file.flush())
        .map_err(|e| ForgeError::Refused(format!("the body's temporary file: {e}")))?;
    Ok(file)
}

/// Runs `gh args` in `at`; its standard output, or the failure classified.
fn gh(at: &At, args: &[String]) -> Result<String, ForgeError> {
    let mut command = Command::new("gh");
    at.env.apply(&mut command);
    command
        .args(args)
        .current_dir(&at.dir)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let out = command
        .output()
        .map_err(|e| ForgeError::Refused(format!("cannot run gh: {e}")))?;
    if out.status.success() {
        return Ok(String::from_utf8_lossy(&out.stdout).into_owned());
    }
    let stderr = String::from_utf8_lossy(&out.stderr).trim().to_owned();
    Err(classify(format!("gh {}: {stderr}", args[..2].join(" "))))
}

/// A failed push or `gh` call, by what it printed: no answer from the service is
/// [`ForgeError::Unreachable`]; anything else, the service's no included, is
/// [`ForgeError::Refused`] (a wait without end is worse than asking the user).
pub(crate) fn classify(error: String) -> ForgeError {
    const REFUSED: &[&str] = &[
        "[rejected]",
        "[remote rejected]",
        "protected branch",
        "permission denied",
        "http 401",
        "http 403",
        "http 404",
        "http 422",
        "error: 401",
        "error: 403",
        "returned error: 4",
    ];
    const UNREACHABLE: &[&str] = &[
        "could not resolve host",
        "could not resolve",
        "no such host",
        "failed to connect",
        "connection refused",
        "connection timed out",
        "connection reset",
        "operation timed out",
        "timed out",
        "timeout",
        "network is unreachable",
        "temporary failure in name resolution",
        "error connecting to",
        "dial tcp",
        "unexpected eof",
        "the remote end hung up unexpectedly",
        "returned error: 5",
        "http 500",
        "http 502",
        "http 503",
        "http 504",
    ];
    let lower = error.to_lowercase();
    if REFUSED.iter().any(|m| lower.contains(m)) {
        return ForgeError::Refused(error);
    }
    if UNREACHABLE.iter().any(|m| lower.contains(m)) {
        return ForgeError::Unreachable(error);
    }
    ForgeError::Refused(error)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn new(draft: bool) -> NewPullRequest {
        NewPullRequest {
            head: "bosun/demo-1".to_owned(),
            base: "main".to_owned(),
            title: "A title".to_owned(),
            body: "A body that must not be on the command line".to_owned(),
            draft,
            reviewers: vec!["one".to_owned(), "two".to_owned()],
            labels: vec!["bosun".to_owned()],
        }
    }

    /// #8: `gh` is asked for the open pull request of the head branch, opens one with the
    /// configured draft, reviewers and labels, and edits only by number; every body goes in a
    /// file.
    #[test]
    fn gh_is_asked_with_the_head_branch_and_a_body_file() {
        assert_eq!(
            list_args("bosun/demo-1"),
            [
                "pr",
                "list",
                "--head",
                "bosun/demo-1",
                "--state",
                "open",
                "--json",
                "number,url,body"
            ]
        );

        let file = Path::new("body.md");
        let file_text = file.to_string_lossy().into_owned();
        let mut expected = vec![
            "pr",
            "create",
            "--head",
            "bosun/demo-1",
            "--base",
            "main",
            "--title",
            "A title",
            "--body-file",
            &file_text,
            "--draft",
            "--reviewer",
            "one",
            "--reviewer",
            "two",
            "--label",
            "bosun",
        ];
        assert_eq!(create_args(&new(true), file), expected);
        expected.retain(|a| *a != "--draft");
        assert_eq!(create_args(&new(false), file), expected);
        for draft in [true, false] {
            let args = create_args(&new(draft), file);
            assert!(
                !args.iter().any(|a| a.contains("must not be"))
                    && !args.contains(&"--body".to_owned()),
                "{args:?}"
            );
        }

        assert_eq!(
            edit_args(12, file),
            ["pr", "edit", "12", "--body-file", file_text.as_str()]
        );
    }

    #[test]
    fn a_listed_pull_request_and_a_created_url_are_read() {
        let listed = parse_list(r#"[{"number":7,"url":"https://x/o/r/pull/7","body":"b"}]"#)
            .unwrap()
            .unwrap();
        assert_eq!((listed.number, listed.body.as_str()), (7, "b"));
        assert_eq!(parse_list("[]\n").unwrap(), None);
        assert!(parse_list("nope").is_err());
        assert_eq!(number_of("https://github.com/o/r/pull/42"), Some(42));
        assert_eq!(number_of("https://github.com/o/r"), None);
    }

    #[test]
    fn a_rejection_is_refused_and_no_answer_is_unreachable() {
        let refused = [
            " ! [rejected]        abc -> bosun/demo-1 (non-fast-forward)",
            "remote: error: GH006: Protected branch update failed",
            "fatal: unable to access 'https://x/': The requested URL returned error: 403",
            "GraphQL: Resource not accessible by integration",
        ];
        for text in refused {
            assert!(
                matches!(classify(text.to_owned()), ForgeError::Refused(_)),
                "{text}"
            );
        }
        let unreachable = [
            "fatal: unable to access 'https://x/': Could not resolve host: github.com",
            "error connecting to api.github.com",
            "fatal: unable to access 'https://x/': The requested URL returned error: 502",
            "dial tcp: lookup api.github.com: no such host",
        ];
        for text in unreachable {
            assert!(
                matches!(classify(text.to_owned()), ForgeError::Unreachable(_)),
                "{text}"
            );
        }
    }
}
