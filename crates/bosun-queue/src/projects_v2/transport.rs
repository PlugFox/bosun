//! The GraphQL transport (D5 of #7): a [`GraphQl`] trait, and [`GhCli`], which runs
//! `gh api graphql` under the identity's `GH_CONFIG_DIR` so the token never enters the daemon's
//! memory and no HTTP or TLS stack is linked.
//!
//! `gh api --include` prints the status line and the response headers before the body. GitHub
//! sends the rate-limit headers on every GraphQL response (checked on a real account for #60):
//! `X-RateLimit-Limit`, `X-RateLimit-Remaining`, `X-RateLimit-Used`, `X-RateLimit-Reset` (Unix
//! seconds) and `X-RateLimit-Resource` (`graphql`), with `X-OAuth-Scopes` beside them. `gh`
//! prints them title-cased (`X-Ratelimit-Remaining`), so they are matched ignoring case.

use std::ffi::OsString;
use std::fmt;
use std::io::{self, Read as _, Write as _};
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde_json::Value;

/// Sends one GraphQL request. Implemented by [`GhCli`] and by the tests' fake server.
pub trait GraphQl {
    /// Runs `query` with `variables` (a JSON object) and returns the decoded response.
    ///
    /// A response that carries GraphQL `errors` is still a [`Reply`]: some errors are answers
    /// (a project that does not exist), and the caller decides.
    ///
    /// # Errors
    ///
    /// The request could not be made, timed out, or got a non-200 status or an unreadable body.
    fn execute(&self, query: &str, variables: &Value) -> Result<Reply, TransportError>;
}

impl<T: GraphQl + ?Sized> GraphQl for &T {
    fn execute(&self, query: &str, variables: &Value) -> Result<Reply, TransportError> {
        (**self).execute(query, variables)
    }
}

impl<T: GraphQl + ?Sized> GraphQl for Arc<T> {
    fn execute(&self, query: &str, variables: &Value) -> Result<Reply, TransportError> {
        (**self).execute(query, variables)
    }
}

/// A decoded GraphQL response.
#[derive(Debug, Clone, PartialEq)]
pub struct Reply {
    /// The `data` member; `Null` when absent.
    pub data: Value,
    /// The `errors` member, one entry per error.
    pub errors: Vec<ReplyError>,
    /// The rate limit the response's headers reported, if any.
    pub rate_limit: Option<RateLimit>,
}

impl Reply {
    /// Decodes a GraphQL response body.
    ///
    /// # Errors
    ///
    /// The body is not a JSON object.
    pub fn from_body(body: &str, rate_limit: Option<RateLimit>) -> Result<Self, TransportError> {
        let mut value: Value = serde_json::from_str(body.trim())
            .map_err(|e| TransportError::Malformed(format!("the body is not JSON: {e}")))?;
        let Some(object) = value.as_object_mut() else {
            return Err(TransportError::Malformed(
                "the body is not a JSON object".into(),
            ));
        };
        let data = object.remove("data").unwrap_or(Value::Null);
        let errors = object
            .get("errors")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .map(|e| ReplyError {
                kind: e.get("type").and_then(Value::as_str).map(str::to_owned),
                message: e
                    .get("message")
                    .and_then(Value::as_str)
                    .unwrap_or("unknown error")
                    .to_owned(),
            })
            .collect();
        Ok(Self {
            data,
            errors,
            rate_limit,
        })
    }

    /// Whether an error of `kind` (`NOT_FOUND`, `FORBIDDEN`, ...) is among the errors.
    #[must_use]
    pub fn has_error(&self, kind: &str) -> bool {
        self.errors.iter().any(|e| e.kind.as_deref() == Some(kind))
    }
}

/// One entry of a response's `errors`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReplyError {
    /// The error's `type` (`NOT_FOUND`, `FORBIDDEN`, `RATE_LIMITED`), when GitHub gives one.
    pub kind: Option<String>,
    /// The message.
    pub message: String,
}

impl fmt::Display for ReplyError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.kind {
            Some(kind) => write!(f, "{kind}: {}", self.message),
            None => f.write_str(&self.message),
        }
    }
}

pub use crate::queue::RateLimit;

impl RateLimit {
    /// The rate limit in `headers` (name, value pairs, names in any case), if all four numbers
    /// are there.
    #[must_use]
    pub fn from_headers<'h>(headers: impl IntoIterator<Item = (&'h str, &'h str)>) -> Option<Self> {
        let (mut limit, mut remaining, mut used, mut reset) = (None, None, None, None);
        for (name, value) in headers {
            let slot = match name.trim().to_ascii_lowercase().as_str() {
                "x-ratelimit-limit" => &mut limit,
                "x-ratelimit-remaining" => &mut remaining,
                "x-ratelimit-used" => &mut used,
                "x-ratelimit-reset" => &mut reset,
                _ => continue,
            };
            *slot = value.trim().parse().ok();
        }
        Some(Self {
            limit: limit?,
            remaining: remaining?,
            used: used?,
            reset: reset?,
        })
    }
}

/// Why a request produced no [`Reply`].
#[derive(Debug)]
pub enum TransportError {
    /// The transport program could not be started or talked to.
    Io(io::Error),
    /// The request did not finish within the timeout; the program was killed.
    Timeout(Duration),
    /// `gh` exited without an HTTP response (not logged in, no network): its exit code and
    /// the end of its standard error.
    Gh {
        /// The exit code, if it exited normally.
        code: Option<i32>,
        /// The end of its standard error.
        stderr: String,
    },
    /// The server answered with a status other than 200.
    Status {
        /// The HTTP status.
        status: u16,
        /// The body's `message`, or the start of the body.
        message: String,
        /// The rate limit the headers reported (a 403 for an exhausted quota carries it).
        rate_limit: Option<RateLimit>,
        /// The `Retry-After` header, in seconds (a secondary rate limit sends it).
        retry_after: Option<u64>,
    },
    /// The output could not be read as an HTTP response with a JSON body.
    Malformed(String),
}

impl fmt::Display for TransportError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(e) => write!(f, "cannot run the GraphQL transport: {e}"),
            Self::Timeout(after) => write!(f, "the GraphQL request timed out after {after:?}"),
            Self::Gh { code, stderr } => match code {
                Some(code) => write!(f, "gh exited with {code}: {stderr}"),
                None => write!(f, "gh was terminated: {stderr}"),
            },
            Self::Status {
                status, message, ..
            } => write!(f, "GitHub answered {status}: {message}"),
            Self::Malformed(why) => write!(f, "unreadable GraphQL response: {why}"),
        }
    }
}

impl std::error::Error for TransportError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(e) => Some(e),
            _ => None,
        }
    }
}

impl From<io::Error> for TransportError {
    fn from(e: io::Error) -> Self {
        Self::Io(e)
    }
}

/// The default timeout of one `gh` request.
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(60);

/// `gh api graphql --include --input -`, run with exactly the environment the caller gives.
///
/// The environment is not inherited: the caller builds it (#2) from the system essentials
/// (`SystemRoot`, `PATH`, `HOME` or `USERPROFILE`, ...) and the identity's `GH_CONFIG_DIR`. This
/// type never reads a token, and never looks at the daemon's own environment.
#[derive(Clone)]
pub struct GhCli {
    program: PathBuf,
    env: Vec<(OsString, OsString)>,
    hostname: Option<String>,
    timeout: Duration,
}

impl fmt::Debug for GhCli {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // The values may be sensitive; the names are not.
        let names: Vec<_> = self.env.iter().map(|(name, _)| name).collect();
        f.debug_struct("GhCli")
            .field("program", &self.program)
            .field("env", &names)
            .field("hostname", &self.hostname)
            .field("timeout", &self.timeout)
            .finish()
    }
}

impl GhCli {
    /// Runs `program` (a path to `gh`, or `gh` to find it on the `PATH` in `env`) with exactly
    /// `env`.
    #[must_use]
    pub fn new(
        program: impl Into<PathBuf>,
        env: impl IntoIterator<Item = (impl Into<OsString>, impl Into<OsString>)>,
    ) -> Self {
        Self {
            program: program.into(),
            env: env.into_iter().map(|(k, v)| (k.into(), v.into())).collect(),
            hostname: None,
            timeout: DEFAULT_TIMEOUT,
        }
    }

    /// Talks to `hostname` (a GitHub Enterprise Server) instead of `github.com`.
    #[must_use]
    pub fn with_hostname(mut self, hostname: impl Into<String>) -> Self {
        self.hostname = Some(hostname.into());
        self
    }

    /// Kills a request that takes longer than `timeout`.
    #[must_use]
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    fn command(&self) -> Command {
        let mut command = crate::child::command(&self.program);
        command.args(["api", "graphql", "--include", "--input", "-"]);
        if let Some(host) = &self.hostname {
            command.args(["--hostname", host]);
        }
        command
            .env_clear()
            .envs(self.env.iter().map(|(k, v)| (k, v)))
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        command
    }
}

impl GraphQl for GhCli {
    fn execute(&self, query: &str, variables: &Value) -> Result<Reply, TransportError> {
        let request = serde_json::json!({ "query": query, "variables": variables }).to_string();
        let mut child = self.command().spawn()?;
        let mut stdin = child.stdin.take().expect("stdin is piped");
        let mut stdout = child.stdout.take().expect("stdout is piped");
        let mut stderr = child.stderr.take().expect("stderr is piped");
        let writer = std::thread::spawn(move || stdin.write_all(request.as_bytes()));
        let out = std::thread::spawn(move || {
            let mut buf = Vec::new();
            stdout.read_to_end(&mut buf).map(|_| buf)
        });
        let err = std::thread::spawn(move || {
            let mut buf = Vec::new();
            stderr.read_to_end(&mut buf).map(|_| buf)
        });

        let started = Instant::now();
        let status = loop {
            if let Some(status) = child.try_wait()? {
                break status;
            }
            if started.elapsed() > self.timeout {
                let _ = child.kill();
                let _ = child.wait();
                return Err(TransportError::Timeout(self.timeout));
            }
            std::thread::sleep(Duration::from_millis(10));
        };
        // A broken pipe means gh exited before reading the request; its output says why.
        let _ = writer.join();
        let stdout = out.join().expect("the reader does not panic")?;
        let stderr = err.join().expect("the reader does not panic")?;
        let stdout = String::from_utf8_lossy(&stdout);
        if stdout.trim().is_empty() {
            let stderr = String::from_utf8_lossy(&stderr);
            return Err(TransportError::Gh {
                code: status.code(),
                stderr: tail(stderr.trim(), 500).to_owned(),
            });
        }
        // gh exits 1 when the response carries GraphQL errors; the body still has them.
        parse_include(&stdout)
    }
}

/// The last `max` bytes of `text`, on a character boundary.
fn tail(text: &str, max: usize) -> &str {
    let mut start = text.len().saturating_sub(max);
    while !text.is_char_boundary(start) {
        start += 1;
    }
    &text[start..]
}

/// Reads the output of `gh api --include`: a status line, headers, an empty line, the body.
///
/// # Errors
///
/// The output is not in that shape, the status is not 200, or the body is not a JSON object.
pub fn parse_include(output: &str) -> Result<Reply, TransportError> {
    // The head ends at the first empty line, whatever the line endings.
    let (head, body) = [("\r\n\r\n", 4), ("\n\n", 2)]
        .into_iter()
        .filter_map(|(separator, len)| output.find(separator).map(|at| (at, len)))
        .min()
        .map_or((output, ""), |(at, len)| {
            (&output[..at], &output[at + len..])
        });
    let mut lines = head.lines();
    let status_line = lines.next().unwrap_or_default();
    let status = status_line
        .strip_prefix("HTTP/")
        .and_then(|rest| rest.split_whitespace().nth(1))
        .and_then(|code| code.parse::<u16>().ok())
        .ok_or_else(|| {
            TransportError::Malformed(format!("no HTTP status line: {:?}", tail(status_line, 80)))
        })?;
    let headers: Vec<(&str, &str)> = lines.filter_map(|line| line.split_once(':')).collect();
    let rate_limit = RateLimit::from_headers(headers.iter().copied());
    if status != 200 {
        let message = serde_json::from_str::<Value>(body.trim())
            .ok()
            .and_then(|v| v.get("message").and_then(Value::as_str).map(str::to_owned))
            .unwrap_or_else(|| body.trim().chars().take(200).collect());
        let retry_after = headers
            .iter()
            .find(|(name, _)| name.trim().eq_ignore_ascii_case("retry-after"))
            .and_then(|(_, value)| value.trim().parse().ok());
        return Err(TransportError::Status {
            status,
            message,
            rate_limit,
            retry_after,
        });
    }
    Reply::from_body(body, rate_limit)
}

#[cfg(test)]
mod tests {
    use super::*;

    // The shape `gh api graphql --include` printed for a real request (#60), values synthetic.
    const OK: &str = "HTTP/2.0 200 OK\r\n\
        Content-Type: application/json; charset=utf-8\r\n\
        X-Oauth-Scopes: project, repo\r\n\
        X-Ratelimit-Limit: 5000\r\n\
        X-Ratelimit-Remaining: 4681\r\n\
        X-Ratelimit-Reset: 1700000000\r\n\
        X-Ratelimit-Resource: graphql\r\n\
        X-Ratelimit-Used: 319\r\n\
        \r\n\
        {\"data\":{\"viewer\":{\"login\":\"octo-example\"}}}";

    #[test]
    fn include_output_yields_the_body_and_the_rate_limit() {
        let reply = parse_include(OK).unwrap();
        assert_eq!(reply.data["viewer"]["login"], "octo-example");
        assert_eq!(reply.errors, Vec::new());
        assert_eq!(
            reply.rate_limit,
            Some(RateLimit {
                limit: 5000,
                remaining: 4681,
                used: 319,
                reset: 1_700_000_000,
            })
        );
        let lf = OK.replace("\r\n", "\n");
        assert_eq!(parse_include(&lf).unwrap(), reply);
    }

    #[test]
    fn graphql_errors_are_part_of_the_reply() {
        let text = "HTTP/2.0 200 OK\n\n{\"data\":{\"repositoryOwner\":{\"projectV2\":null}},\
            \"errors\":[{\"type\":\"NOT_FOUND\",\"message\":\"Could not resolve\"}]}";
        let reply = parse_include(text).unwrap();
        assert!(reply.has_error("NOT_FOUND"));
        assert_eq!(reply.rate_limit, None);
        assert!(reply.data["repositoryOwner"]["projectV2"].is_null());
    }

    #[test]
    fn a_non_200_status_is_an_error_with_its_rate_limit() {
        let text = "HTTP/2.0 403 Forbidden\nX-RateLimit-Limit: 5000\nX-RateLimit-Remaining: 0\n\
            X-RateLimit-Used: 5000\nX-RateLimit-Reset: 1700000000\n\n\
            {\"message\":\"API rate limit exceeded\"}";
        match parse_include(text) {
            Err(TransportError::Status {
                status: 403,
                message,
                rate_limit: Some(limit),
                retry_after: None,
            }) => {
                assert_eq!(message, "API rate limit exceeded");
                assert_eq!(limit.remaining, 0);
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn a_secondary_rate_limit_carries_its_retry_after() {
        // The shape GitHub documents for a secondary rate limit; values synthetic.
        let text = "HTTP/2.0 403 Forbidden\r\nRetry-After: 60\r\nX-Ratelimit-Remaining: 4000\r\n\r\n\
            {\"message\":\"You have exceeded a secondary rate limit. Please wait a few minutes \
            before you try again.\"}";
        match parse_include(text) {
            Err(TransportError::Status {
                status: 403,
                message,
                retry_after: Some(60),
                ..
            }) => assert!(message.contains("secondary rate limit"), "{message}"),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn output_without_a_status_line_is_malformed() {
        assert!(matches!(
            parse_include("{\"data\":{}}"),
            Err(TransportError::Malformed(_))
        ));
        assert!(matches!(
            parse_include("HTTP/2.0 200 OK\n\nnot json"),
            Err(TransportError::Malformed(_))
        ));
    }

    #[test]
    fn a_missing_program_is_an_io_error() {
        let gh = GhCli::new(
            "bosun-test-no-such-gh-program",
            [("GH_CONFIG_DIR", "unused")],
        );
        assert!(matches!(
            gh.execute("{viewer{login}}", &serde_json::json!({})),
            Err(TransportError::Io(_))
        ));
        assert!(
            !format!("{gh:?}").contains("unused"),
            "values are not printed"
        );
    }

    /// A live request through the real `gh`, by hand only: it needs a network and an account.
    /// The environment is built here, as the daemon builds it, from `BOSUN_LIVE_GH_CONFIG_DIR`
    /// and, when set, `BOSUN_LIVE_GH_TOKEN`; nothing else is passed but the system essentials.
    #[test]
    #[ignore = "needs gh, a network and an account"]
    fn live_gh_reports_the_rate_limit() {
        let mut env: Vec<(String, String)> = Vec::new();
        for name in [
            "SystemRoot",
            "SYSTEMROOT",
            "PATH",
            "HOME",
            "USERPROFILE",
            "APPDATA",
            "LOCALAPPDATA",
            "TEMP",
            "TMP",
        ] {
            if let Ok(value) = std::env::var(name) {
                env.push((name.to_owned(), value));
            }
        }
        if let Ok(dir) = std::env::var("BOSUN_LIVE_GH_CONFIG_DIR") {
            env.push(("GH_CONFIG_DIR".into(), dir));
        }
        if let Ok(token) = std::env::var("BOSUN_LIVE_GH_TOKEN") {
            env.push(("GH_TOKEN".into(), token));
        }
        let reply = GhCli::new("gh", env)
            .execute("{ viewer { login } }", &serde_json::json!({}))
            .unwrap();
        assert!(reply.errors.is_empty(), "{:?}", reply.errors);
        let limit = reply
            .rate_limit
            .expect("the rate-limit headers are visible");
        assert!(
            limit.limit > 0 && limit.remaining <= limit.limit,
            "{limit:?}"
        );
    }
}
