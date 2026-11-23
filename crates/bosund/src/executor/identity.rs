//! The identity bundle from the configuration, the fail-closed checks wired to git and `gh`
//! (#2, rule 2; #54), and the environment of an agent session.

use std::fmt;
use std::path::Path;

use bosun_config::Identity;
use bosun_git::Git;
use bosun_identity::{
    Author, Bundle, Env, ProbeError, RemoteUrl, ResolvedAuthor, TokenLogin, inherited,
};

/// Where the login of an identity's token comes from: `gh` under the identity's configuration
/// ([`GhLogins`]), or a fake in tests.
pub(crate) trait Logins: Send + Sync + fmt::Debug {
    /// The probe of `bundle`'s token.
    fn probe(&self, bundle: &Bundle) -> Box<dyn TokenLogin>;
}

/// The real probe: `gh api graphql` under the daemon's own environment for the identity (D5).
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct GhLogins;

impl Logins for GhLogins {
    fn probe(&self, bundle: &Bundle) -> Box<dyn TokenLogin> {
        Box::new(bosun_identity::GhLogin::new(Env::tool(
            bundle,
            &inherited(),
        )))
    }
}

/// The bundle of identity `name` (#2) as `bosun-identity` reads it. The author is the one its
/// git configuration file names. A key the checks need that is not configured refuses.
pub(crate) fn bundle(name: &str, identity: &Identity, repo: &Path) -> Result<Bundle, String> {
    let missing = |key: &str| format!("identity {name}: `{key}` is not configured");
    let login = identity
        .github_login
        .clone()
        .ok_or_else(|| missing("github_login"))?;
    let gh_config_dir = identity
        .gh_config_dir
        .clone()
        .ok_or_else(|| missing("gh_config_dir"))?;
    let git_config = identity
        .git_config
        .clone()
        .ok_or_else(|| missing("git_config"))?;
    let author = configured_author(repo, &git_config)
        .map_err(|e| format!("identity {name}: the author of `git_config`: {e}"))?;
    let mut runner_dirs = vec![(
        "CLAUDE_CONFIG_DIR".to_owned(),
        identity.claude_config_dir.clone(),
    )];
    if let Some(codex) = &identity.codex_home {
        runner_dirs.push(("CODEX_HOME".to_owned(), codex.clone()));
    }
    Ok(Bundle {
        identity: name.to_owned(),
        host: identity.github_host.to_ascii_lowercase(),
        login,
        gh_config_dir,
        git_config,
        ssh_key: identity.ssh_key.clone(),
        author,
        owners: identity.remote_allowlist.iter().cloned().collect(),
        runner_dirs,
        runner_env: Vec::new(),
    })
}

/// `user.name` and `user.email` of the identity's git configuration file.
fn configured_author(repo: &Path, file: &Path) -> Result<Author, String> {
    let git = Git::new(repo);
    let get = |key: &str| -> Result<String, String> {
        let file = file.to_string_lossy();
        git.run(&["config", "--file", &file, "--get", key], None)
            .map(|v| v.trim().to_owned())
            .map_err(|e| e.to_string())
    };
    Ok(Author {
        name: get("user.name")?,
        email: get("user.email")?,
    })
}

/// The empty `gh` configuration directory of agent sessions, under `work_dir`.
pub(crate) fn empty_gh_dir(work_dir: &Path) -> std::io::Result<std::path::PathBuf> {
    let dir = work_dir.join("gh-empty");
    std::fs::create_dir_all(&dir)?;
    Ok(dir)
}

/// The environment of an agent session (and of a verdict, which runs what the agent wrote):
/// no GitHub credential of the identity (#2, #54).
pub(crate) fn agent_env(bundle: &Bundle, work_dir: &Path) -> std::io::Result<Env> {
    let empty = empty_gh_dir(work_dir)?;
    Ok(Env::agent(bundle, &inherited(), &[], &empty))
}

/// The repository's `origin` URL, through the invoker.
#[derive(Debug)]
struct Origin;

impl RemoteUrl for Origin {
    fn remote_url(&self, repo: &Path) -> Result<String, ProbeError> {
        Git::new(repo)
            .run(&["remote", "get-url", "origin"], None)
            .map(|url| url.trim().to_owned())
            .map_err(|e| ProbeError(e.to_string()))
    }
}

/// The author git resolves to in the repository under the agent environment: what the agent's
/// commits will carry, a repository-local `user.email` included.
struct AgentAuthor<'a> {
    env: &'a Env,
}

impl ResolvedAuthor for AgentAuthor<'_> {
    fn author(&self, repo: &Path) -> Result<Author, ProbeError> {
        let mut command = Git::new(repo).command();
        self.env.apply(&mut command);
        command.args(["var", "GIT_AUTHOR_IDENT"]);
        let out = command
            .output()
            .map_err(|e| ProbeError(format!("cannot run git: {e}")))?;
        if !out.status.success() {
            return Err(ProbeError(
                String::from_utf8_lossy(&out.stderr).trim().to_owned(),
            ));
        }
        Author::from_ident(String::from_utf8_lossy(&out.stdout).trim())
            .ok_or_else(|| ProbeError("git printed no author".to_owned()))
    }
}

/// Every check before a session of `bundle` in `repo`: the remote's owner, the token's login
/// and the resolved author. The error is the refusals' text (it names no value found).
pub(crate) fn check(bundle: &Bundle, repo: &Path, logins: &dyn Logins) -> Result<(), String> {
    let env = Env::agent(bundle, &inherited(), &[], &std::env::temp_dir());
    let login = logins.probe(bundle);
    bosun_identity::before_spawn(
        bundle,
        repo,
        &Origin,
        login.as_ref(),
        &AgentAuthor { env: &env },
    )
    .map_err(|refusals| {
        let text: Vec<String> = refusals.iter().map(ToString::to_string).collect();
        format!("refused before spawn (#2): {}", text.join("; "))
    })
}
