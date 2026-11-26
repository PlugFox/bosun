//! The forge seam of the `pr` landing mode (#8 "The pull request"): looking up the item's open
//! pull request by its head branch, opening one, rewriting its body, and asking whether it was
//! merged or closed since the item was Done ([`super::pulls`]). The daemon's own
//! [`GhForge`] runs `gh` in the user's checkout (`repo.path`) under [`At::env`], the
//! environment of the repository's identity for the daemon's own tools
//! ([`bosun_identity::Env::tool`], #2), never an agent's. The scenario tests put a fake in
//! [`super::Deps::forge`].
//!
//! Every answer is one of two failures or a result: [`ForgeError::Refused`] (the service
//! answered no: branch protection, token scope, a stale branch), which needs the user, and
//! [`ForgeError::Unreachable`] (no answer: network, DNS, timeout, a 5xx), which the landing
//! waits out. The argument lists are built by pure functions ([`list_args`], [`create_args`],
//! [`edit_args`], [`view_args`]); a body goes to `gh` in a file, never on its command line.
//!
//! Every `gh` call, and every git of the landing that talks to the forge, ends within
//! [`TOOL_TIMEOUT`] ([`output_within`]): one that hangs is killed and is
//! [`ForgeError::Unreachable`], so neither the repository's loop nor its landing stalls on it.

use std::fmt;
use std::io::{Read as _, Write as _};
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::time::{Duration, Instant};

use bosun_identity::Env;

/// Where and as whom the forge is asked: the user's checkout, the identity's tool environment
/// and the GitHub repository its `origin` names.
#[derive(Debug, Clone)]
pub(crate) struct At {
    /// The repository `gh` runs in (`repo.path`).
    pub(crate) dir: PathBuf,
    /// The environment `gh` runs with.
    pub(crate) env: Env,
    /// `[HOST/]OWNER/NAME` of the repository `origin` names: every `gh` call is pinned to it
    /// (`-R`), so the lookup and the pull request go to the repository the push went to, not
    /// one `gh` would pick on its own (another remote, a default it was told).
    pub(crate) repo: String,
}

impl At {
    /// [`At`] of the user's checkout `dir` under `env`: `origin`, as the identity's git reads
    /// it (`git remote get-url origin`), must name a GitHub repository as owner and name.
    ///
    /// # Errors
    ///
    /// [`ForgeError::Refused`] when `origin` cannot be read or names no such repository.
    pub(crate) fn new(dir: PathBuf, env: Env) -> Result<Self, ForgeError> {
        let mut command = bosun_git::Git::new(&dir).command();
        env.apply(&mut command);
        command.args(["remote", "get-url", "origin"]);
        let out = output_within(&mut command, TOOL_TIMEOUT, "git remote")?;
        let url = String::from_utf8_lossy(&out.stdout).trim().to_owned();
        if !out.status.success() {
            let stderr = String::from_utf8_lossy(&out.stderr).trim().to_owned();
            return Err(ForgeError::Refused(format!(
                "the repository's origin: {stderr}"
            )));
        }
        let repo = repo_spec(&url).ok_or_else(|| {
            ForgeError::Refused(format!(
                "the origin `{url}` names no GitHub repository as owner/name"
            ))
        })?;
        Ok(Self { dir, env, repo })
    }

    /// The owner of [`At::repo`].
    fn owner(&self) -> &str {
        let mut parts = self.repo.rsplit('/');
        parts.next();
        parts.next().unwrap_or_default()
    }
}

/// `gh -R` of the remote URL `url`: `OWNER/NAME` on github.com, `HOST/OWNER/NAME` elsewhere.
pub(crate) fn repo_spec(url: &str) -> Option<String> {
    let (host, _) = bosun_identity::host_and_owner(url)?;
    let repo = super::board::repository_of(url)?;
    Some(if host == "github.com" {
        repo
    } else {
        format!("{host}/{repo}")
    })
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

/// Where a pull request stands.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PullRequestState {
    Open,
    Merged,
    /// Closed without being merged.
    Closed,
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

    /// Where pull request `number` stands.
    ///
    /// # Errors
    ///
    /// [`ForgeError`].
    fn state(&self, at: &At, number: u64) -> Result<PullRequestState, ForgeError>;
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
        let out = gh(at, &list_args(&at.repo, head_branch))?;
        parse_list(&out, at.owner()).map_err(ForgeError::Refused)
    }

    fn create(&self, at: &At, new: &NewPullRequest) -> Result<PullRequest, ForgeError> {
        let file = body_file(&new.body)?;
        let out = gh(at, &create_args(&at.repo, new, file.path()))?;
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
        gh(at, &edit_args(&at.repo, number, file.path())).map(drop)
    }

    fn state(&self, at: &At, number: u64) -> Result<PullRequestState, ForgeError> {
        let out = gh(at, &view_args(&at.repo, number))?;
        parse_state(&out).map_err(ForgeError::Refused)
    }
}

/// `gh pr list` of the repository `repo` for the open pull request of `head`.
pub(crate) fn list_args(repo: &str, head: &str) -> Vec<String> {
    [
        "pr",
        "list",
        "-R",
        repo,
        "--head",
        head,
        "--state",
        "open",
        "--json",
        LIST_FIELDS,
    ]
    .map(str::to_owned)
    .to_vec()
}

/// The fields `gh pr list` is asked for: whether each pull request comes from a fork is what
/// [`parse_list`] keeps them apart by.
const LIST_FIELDS: &str = "number,url,body,isCrossRepository,headRepositoryOwner";

/// `gh pr create` in the repository `repo` for `new`, its body read from `body_file`.
pub(crate) fn create_args(repo: &str, new: &NewPullRequest, body_file: &Path) -> Vec<String> {
    let mut args: Vec<String> = [
        "pr", "create", "-R", repo, "--head", &new.head, "--base", &new.base,
    ]
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

/// `gh pr edit` of pull request `number` of `repo`, its body read from `body_file`.
pub(crate) fn edit_args(repo: &str, number: u64, body_file: &Path) -> Vec<String> {
    vec![
        "pr".to_owned(),
        "edit".to_owned(),
        "-R".to_owned(),
        repo.to_owned(),
        number.to_string(),
        "--body-file".to_owned(),
        body_file.to_string_lossy().into_owned(),
    ]
}

/// `gh pr view` of pull request `number` of `repo`: its state and when it was merged.
pub(crate) fn view_args(repo: &str, number: u64) -> Vec<String> {
    vec![
        "pr".to_owned(),
        "view".to_owned(),
        "-R".to_owned(),
        repo.to_owned(),
        number.to_string(),
        "--json".to_owned(),
        "state,mergedAt".to_owned(),
    ]
}

/// The state in `gh pr view --json state,mergedAt` output: a merge time is a merge, whatever
/// the state says.
fn parse_state(out: &str) -> Result<PullRequestState, String> {
    #[derive(serde::Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct Viewed {
        state: String,
        #[serde(default)]
        merged_at: Option<String>,
    }
    let viewed: Viewed = serde_json::from_str(out.trim())
        .map_err(|e| format!("`gh pr view` printed what is not a pull request's state: {e}"))?;
    if viewed
        .merged_at
        .as_deref()
        .is_some_and(|m| !m.trim().is_empty())
    {
        return Ok(PullRequestState::Merged);
    }
    match viewed.state.to_ascii_uppercase().as_str() {
        "OPEN" => Ok(PullRequestState::Open),
        "MERGED" => Ok(PullRequestState::Merged),
        "CLOSED" => Ok(PullRequestState::Closed),
        other => Err(format!("`gh pr view` printed an unknown state: {other}")),
    }
}

/// The first pull request of `gh pr list --json` [`LIST_FIELDS`] output whose head branch is
/// in the repository itself. `--head` matches the branch name only, so on a public repository
/// anyone may open a pull request from `bosun/<slug>` of a fork before the daemon does: one
/// whose head is in another repository (or that does not say), or whose head repository's
/// owner is not `owner`, is never taken for the item's.
fn parse_list(out: &str, owner: &str) -> Result<Option<PullRequest>, String> {
    #[derive(serde::Deserialize)]
    struct Owner {
        #[serde(default)]
        login: String,
    }
    #[derive(serde::Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct Listed {
        number: u64,
        url: String,
        #[serde(default)]
        body: String,
        /// `None` when `gh` did not say: taken for a fork's.
        #[serde(default)]
        is_cross_repository: Option<bool>,
        #[serde(default)]
        head_repository_owner: Option<Owner>,
    }
    let listed: Vec<Listed> = serde_json::from_str(out.trim())
        .map_err(|e| format!("`gh pr list` printed what is not a list of pull requests: {e}"))?;
    // Mutant (`mutant_forge_adopts_fork_pull_request`): a fork's pull request of the same
    // branch name is taken for the item's.
    let own = |l: &Listed| {
        cfg!(mutant_forge_adopts_fork_pull_request)
            || (l.is_cross_repository == Some(false)
                && l.head_repository_owner
                    .as_ref()
                    .is_some_and(|o| o.login.eq_ignore_ascii_case(owner)))
    };
    Ok(listed.into_iter().find(own).map(|l| PullRequest {
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

/// How long one `gh` call or one git that talks to the forge may take before it is killed and
/// taken for no answer.
pub(crate) const TOOL_TIMEOUT: Duration = Duration::from_mins(2);

/// Runs `command` (`what` names it) to its end, at most `limit`: its output, or, when it
/// cannot be started, [`ForgeError::Refused`]. One still running at `limit` is killed and is
/// [`ForgeError::Unreachable`]; whatever it started and left holding its pipes is not waited
/// for. Standard input is closed and both outputs are read as it runs.
pub(crate) fn output_within(
    command: &mut Command,
    limit: Duration,
    what: &str,
) -> Result<Output, ForgeError> {
    command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = command
        .spawn()
        .map_err(|e| ForgeError::Refused(format!("cannot run {what}: {e}")))?;
    let drain = |pipe: Option<Box<dyn std::io::Read + Send>>| {
        std::thread::spawn(move || {
            let mut bytes = Vec::new();
            if let Some(mut pipe) = pipe {
                let _ = pipe.read_to_end(&mut bytes);
            }
            bytes
        })
    };
    let stdout = drain(child.stdout.take().map(|p| Box::new(p) as _));
    let stderr = drain(child.stderr.take().map(|p| Box::new(p) as _));
    let deadline = Instant::now() + limit;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) => {}
            Err(e) => return Err(ForgeError::Unreachable(format!("{what}: {e}"))),
        }
        // Mutant (`mutant_forge_tool_without_timeout`): a call that hangs is waited for.
        if Instant::now() >= deadline && !cfg!(mutant_forge_tool_without_timeout) {
            child.kill().ok();
            child.wait().ok();
            return Err(ForgeError::Unreachable(format!(
                "{what} did not answer within {} s",
                limit.as_secs()
            )));
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    Ok(Output {
        status,
        stdout: stdout.join().unwrap_or_default(),
        stderr: stderr.join().unwrap_or_default(),
    })
}

/// Runs `gh args` in `at`; its standard output, or the failure classified.
fn gh(at: &At, args: &[String]) -> Result<String, ForgeError> {
    let mut command = Command::new("gh");
    at.env.apply(&mut command);
    command.args(args).current_dir(&at.dir);
    let out = output_within(&mut command, TOOL_TIMEOUT, "gh")?;
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
            list_args("o/r", "bosun/demo-1"),
            [
                "pr",
                "list",
                "-R",
                "o/r",
                "--head",
                "bosun/demo-1",
                "--state",
                "open",
                "--json",
                "number,url,body,isCrossRepository,headRepositoryOwner"
            ]
        );

        let file = Path::new("body.md");
        let file_text = file.to_string_lossy().into_owned();
        let mut expected = vec![
            "pr",
            "create",
            "-R",
            "o/r",
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
        assert_eq!(create_args("o/r", &new(true), file), expected);
        expected.retain(|a| *a != "--draft");
        assert_eq!(create_args("o/r", &new(false), file), expected);
        for draft in [true, false] {
            let args = create_args("o/r", &new(draft), file);
            assert!(
                !args.iter().any(|a| a.contains("must not be"))
                    && !args.contains(&"--body".to_owned()),
                "{args:?}"
            );
        }

        assert_eq!(
            edit_args("o/r", 12, file),
            [
                "pr",
                "edit",
                "-R",
                "o/r",
                "12",
                "--body-file",
                file_text.as_str()
            ]
        );
        assert_eq!(
            view_args("o/r", 12),
            ["pr", "view", "-R", "o/r", "12", "--json", "state,mergedAt"]
        );
    }

    /// #8: every `gh` call names the repository of `origin`: `OWNER/NAME` on github.com, with
    /// the host elsewhere; a local path or a URL without owner and name names none.
    #[test]
    fn the_repository_of_origin_pins_every_gh_call() {
        for url in [
            "https://github.com/o/r.git",
            "git@github.com:o/r.git",
            "ssh://git@github.com/o/r",
        ] {
            assert_eq!(repo_spec(url).as_deref(), Some("o/r"), "{url}");
        }
        assert_eq!(
            repo_spec("https://ghe.example.com/o/r.git").as_deref(),
            Some("ghe.example.com/o/r")
        );
        for url in ["/srv/git/r.git", "C:/git/r", "https://github.com/o"] {
            assert_eq!(repo_spec(url), None, "{url}");
        }
    }

    #[test]
    fn a_viewed_state_is_read_and_a_merge_time_is_a_merge() {
        let read = |text: &str| parse_state(text).unwrap();
        assert_eq!(
            read(r#"{"state":"OPEN","mergedAt":null}"#),
            PullRequestState::Open
        );
        assert_eq!(
            read(r#"{"state":"MERGED","mergedAt":"2026-01-01T00:00:00Z"}"#),
            PullRequestState::Merged
        );
        assert_eq!(
            read(r#"{"state":"CLOSED","mergedAt":""}"#),
            PullRequestState::Closed
        );
        assert_eq!(
            read(r#"{"state":"CLOSED","mergedAt":"2026-01-01T00:00:00Z"}"#),
            PullRequestState::Merged
        );
        assert!(parse_state(r#"{"state":"DRAFTY"}"#).is_err());
        assert!(parse_state("nope").is_err());
    }

    #[test]
    fn a_listed_pull_request_and_a_created_url_are_read() {
        let listed = parse_list(
            r#"[{"number":7,"url":"https://x/o/r/pull/7","body":"b","isCrossRepository":false,
                "headRepositoryOwner":{"login":"o"}}]"#,
            "o",
        )
        .unwrap()
        .unwrap();
        assert_eq!((listed.number, listed.body.as_str()), (7, "b"));
        assert_eq!(parse_list("[]\n", "o").unwrap(), None);
        assert!(parse_list("nope", "o").is_err());
        assert_eq!(number_of("https://github.com/o/r/pull/42"), Some(42));
        assert_eq!(number_of("https://github.com/o/r"), None);
    }

    /// #2, #8: `gh pr list --head` matches the branch name in forks too. A pull request from
    /// a fork listed first, and one that does not say where its head is, are passed over; the
    /// repository's own is taken, and with none of its own there is none.
    #[test]
    fn a_pull_request_from_a_fork_is_never_adopted() {
        let fork = r#"{"number":3,"url":"https://x/o/r/pull/3","body":"fork",
            "isCrossRepository":true,"headRepositoryOwner":{"login":"someone"}}"#;
        let silent = r#"{"number":4,"url":"https://x/o/r/pull/4","body":"silent"}"#;
        let own = r#"{"number":5,"url":"https://x/o/r/pull/5","body":"own",
            "isCrossRepository":false,"headRepositoryOwner":{"login":"o"}}"#;
        let found = parse_list(&format!("[{fork},{silent},{own}]"), "o").unwrap();
        assert_eq!(found.map(|p| p.number), Some(5));
        assert_eq!(
            parse_list(&format!("[{fork},{silent}]"), "o").unwrap(),
            None
        );
        // Not a fork by its flag, yet another owner's head: passed over all the same.
        assert_eq!(parse_list(&format!("[{own}]"), "p").unwrap(), None);
    }

    /// #8: a call that hangs is killed at its limit and taken for no answer, so the loop and
    /// the landing do not stall on it.
    #[test]
    fn a_tool_that_hangs_is_killed_and_unreachable() {
        let mut command = if cfg!(windows) {
            let mut c = Command::new("powershell");
            c.args(["-NoProfile", "-Command", "Start-Sleep -Seconds 10"]);
            c
        } else {
            let mut c = Command::new("sleep");
            c.arg("10");
            c
        };
        let started = Instant::now();
        let out = output_within(&mut command, Duration::from_millis(500), "a sleeper");
        let took = started.elapsed();
        assert!(
            matches!(&out, Err(ForgeError::Unreachable(why)) if why.contains("did not answer")),
            "{out:?}"
        );
        assert!(took < Duration::from_secs(5), "{took:?}");
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
