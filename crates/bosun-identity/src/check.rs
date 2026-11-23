//! The fail-closed checks (#2, rule 2): before a session or a board write, the remote's owner,
//! the token's login and the resolved git author are each compared with the bundle. Any
//! mismatch, and any probe that cannot answer, refuses; there is no fallback.

use std::fmt;
use std::path::Path;
use std::process::{Command, Stdio};

use crate::{Author, Bundle, Env};

/// A probe could not answer. The text is for the daemon's log only; it never reaches the board.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProbeError(pub String);

impl fmt::Display for ProbeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for ProbeError {}

/// The URL of the remote a repository is pushed to and its board is read through.
pub trait RemoteUrl {
    /// The remote URL of the repository at `repo`.
    ///
    /// # Errors
    ///
    /// The URL cannot be read.
    fn remote_url(&self, repo: &Path) -> Result<String, ProbeError>;
}

/// The login the identity's token belongs to, asked of GitHub through the transport (D5 of #7).
pub trait TokenLogin {
    /// The token's login on `host`.
    ///
    /// # Errors
    ///
    /// GitHub cannot be asked, or does not answer with a login.
    fn login(&self, host: &str) -> Result<String, ProbeError>;
}

/// The git author an environment resolves to in a repository (`git var GIT_AUTHOR_IDENT` under
/// that environment, through the daemon's one git invoker; [`Author::from_ident`] parses it).
pub trait ResolvedAuthor {
    /// The author git resolves to in `repo`.
    ///
    /// # Errors
    ///
    /// git cannot be asked, or resolves no author.
    fn author(&self, repo: &Path) -> Result<Author, ProbeError>;
}

/// Why a check refused. [`fmt::Display`] is the Needs-you reason posted to the board: it never
/// names the value found (another identity's login or email, a URL that may carry a token);
/// the fields keep it for the daemon's log.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Refusal {
    /// The remote URL could not be read.
    RemoteUnreadable(ProbeError),
    /// The remote is not a repository on the identity's host.
    RemoteElsewhere {
        /// The remote URL.
        url: String,
    },
    /// The remote's owner is not on the identity's allowlist.
    OwnerNotAllowed {
        /// The owner found.
        owner: String,
    },
    /// The token's login could not be read.
    LoginUnreadable(ProbeError),
    /// The token belongs to another login.
    WrongLogin {
        /// The login found.
        found: String,
    },
    /// The resolved git author could not be read.
    AuthorUnreadable(ProbeError),
    /// git resolves to another author than the configured one.
    WrongAuthor {
        /// The author found.
        found: Author,
    },
}

impl fmt::Display for Refusal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::RemoteUnreadable(_) => "the repository's remote URL could not be read",
            Self::RemoteElsewhere { .. } => {
                "the repository's remote is not a repository on the identity's GitHub host"
            }
            Self::OwnerNotAllowed { .. } => {
                "the repository's owner is not on the identity's remote allowlist"
            }
            Self::LoginUnreadable(_) => "the login of the identity's token could not be read",
            Self::WrongLogin { .. } => {
                "the identity's token belongs to another login than the expected one"
            }
            Self::AuthorUnreadable(_) => "the git author of the identity could not be resolved",
            Self::WrongAuthor { .. } => {
                "git resolves to another author than the identity's configured one"
            }
        })
    }
}

impl std::error::Error for Refusal {}

/// Every check before a session is spawned for `repo`: the remote's owner, the token's login
/// and the resolved author. All three run; every refusal is returned.
///
/// # Errors
///
/// Each check that refused, in that order.
pub fn before_spawn(
    bundle: &Bundle,
    repo: &Path,
    remote: &dyn RemoteUrl,
    login: &dyn TokenLogin,
    author: &dyn ResolvedAuthor,
) -> Result<(), Vec<Refusal>> {
    let found: Vec<Refusal> = [
        remote
            .remote_url(repo)
            .map_err(Refusal::RemoteUnreadable)
            .and_then(|url| check_remote(bundle, &url)),
        check_login(bundle, login),
        check_author(bundle, repo, author),
    ]
    .into_iter()
    .filter_map(Result::err)
    .collect();
    if found.is_empty() { Ok(()) } else { Err(found) }
}

/// The check before a board write: the token's login.
///
/// # Errors
///
/// The login could not be read, or is another one.
pub fn before_write(bundle: &Bundle, login: &dyn TokenLogin) -> Result<(), Refusal> {
    check_login(bundle, login)
}

/// The remote at `url` is a repository on the identity's host whose owner is on its allowlist
/// (owners compare ignoring ASCII case, as GitHub does).
///
/// # Errors
///
/// The remote is elsewhere, or its owner is not allowed.
pub fn check_remote(bundle: &Bundle, url: &str) -> Result<(), Refusal> {
    let elsewhere = || Refusal::RemoteElsewhere {
        url: url.to_owned(),
    };
    let (host, owner) = host_and_owner(url).ok_or_else(elsewhere)?;
    if !host.eq_ignore_ascii_case(&bundle.host) {
        return Err(elsewhere());
    }
    #[cfg(not(mutant_identity_skip_allowlist))]
    if !bundle.owners.iter().any(|o| o.eq_ignore_ascii_case(owner)) {
        return Err(Refusal::OwnerNotAllowed {
            owner: owner.to_owned(),
        });
    }
    Ok(())
}

/// The token's login equals the expected one (ignoring ASCII case, as GitHub does).
///
/// # Errors
///
/// The login could not be read, or is another one.
pub fn check_login(bundle: &Bundle, login: &dyn TokenLogin) -> Result<(), Refusal> {
    let found = login
        .login(&bundle.host)
        .map_err(Refusal::LoginUnreadable)?;
    #[cfg(mutant_identity_skip_login)]
    let found = {
        let _ = found;
        bundle.login.clone()
    };
    if found.trim().eq_ignore_ascii_case(&bundle.login) {
        Ok(())
    } else {
        Err(Refusal::WrongLogin { found })
    }
}

/// The author git resolves to in `repo` is the configured one.
///
/// # Errors
///
/// The author could not be resolved, or is another one.
pub fn check_author(
    bundle: &Bundle,
    repo: &Path,
    author: &dyn ResolvedAuthor,
) -> Result<(), Refusal> {
    let found = author.author(repo).map_err(Refusal::AuthorUnreadable)?;
    #[cfg(mutant_identity_skip_author)]
    let found = {
        let _ = found;
        bundle.author.clone()
    };
    if found.matches(&bundle.author) {
        Ok(())
    } else {
        Err(Refusal::WrongAuthor { found })
    }
}

/// The host (lower case, no user and no port) and the owner of a remote URL: `scheme://[user@]
/// host[:port]/owner/repo` or the scp form `[user@]host:owner/repo`. `None` for a local path or
/// anything without an owner.
#[must_use]
pub fn host_and_owner(url: &str) -> Option<(String, &str)> {
    let (authority, path) = if let Some((_, rest)) = url.split_once("://") {
        rest.split_once('/')?
    } else {
        // The scp form; a Windows drive (`C:\x`, `C:/x`) has a one-letter "host" and is local.
        let (authority, path) = url.split_once(':')?;
        if authority.contains('/') || authority.contains('\\') || authority.len() < 2 {
            return None;
        }
        (authority, path)
    };
    let host = authority.rsplit_once('@').map_or(authority, |(_, h)| h);
    let host = match host.rsplit_once(':') {
        Some((h, port)) if port.chars().all(|c| c.is_ascii_digit()) => h,
        _ => host,
    };
    let owner = path.trim_start_matches('/').split('/').next()?;
    if host.is_empty() || owner.is_empty() {
        return None;
    }
    Some((host.to_ascii_lowercase(), owner))
}

/// [`TokenLogin`] through `gh api graphql` (D5 of #7), run under the daemon's tool
/// environment ([`Env::tool`]), so `gh` reads the identity's own configuration and the token
/// never enters the daemon's memory.
#[derive(Debug, Clone)]
pub struct GhLogin {
    env: Env,
}

impl GhLogin {
    /// Asks through `gh` under `env`, the identity's [`Env::tool`].
    #[must_use]
    pub fn new(env: Env) -> Self {
        Self { env }
    }
}

impl TokenLogin for GhLogin {
    fn login(&self, host: &str) -> Result<String, ProbeError> {
        let mut command = Command::new("gh");
        command.args(["api", "graphql", "--hostname", host]).args([
            "-f",
            "query={viewer{login}}",
            "--jq",
            ".data.viewer.login",
        ]);
        self.env.apply(&mut command);
        command
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let out = command
            .output()
            .map_err(|e| ProbeError(format!("gh cannot be started: {e}")))?;
        if !out.status.success() {
            return Err(ProbeError(format!(
                "gh api graphql failed: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            )));
        }
        let login = String::from_utf8_lossy(&out.stdout).trim().to_owned();
        if login.is_empty() {
            Err(ProbeError("gh answered with no login".to_owned()))
        } else {
            Ok(login)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::host_and_owner;

    #[test]
    fn remote_urls_yield_their_host_and_owner() {
        let cases = [
            (
                "https://github.com/Owner/repo.git",
                Some(("github.com", "Owner")),
            ),
            (
                "https://x-access-token:t@GitHub.com/owner/repo",
                Some(("github.com", "owner")),
            ),
            (
                "http://ghe.example.test:8080/org/repo",
                Some(("ghe.example.test", "org")),
            ),
            (
                "ssh://git@github.com/owner/repo.git",
                Some(("github.com", "owner")),
            ),
            (
                "ssh://git@github.com:22/owner/repo.git",
                Some(("github.com", "owner")),
            ),
            (
                "git@github.com:owner/repo.git",
                Some(("github.com", "owner")),
            ),
            ("github.com:owner/repo", Some(("github.com", "owner"))),
            ("C:/src/repo", None),
            (r"C:\src\repo", None),
            ("/srv/git/repo.git", None),
            ("../repo", None),
            ("https://github.com/", None),
            ("https://github.com", None),
        ];
        for (url, expected) in cases {
            let found = host_and_owner(url);
            let found = found.as_ref().map(|(h, o)| (h.as_str(), *o));
            assert_eq!(found, expected, "{url}");
        }
    }
}
