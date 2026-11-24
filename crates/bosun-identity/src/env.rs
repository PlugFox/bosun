//! The constructed environment (#2, rule 1): nothing a child process gets is inherited unless
//! the allowlist names it.

use std::ffi::{OsStr, OsString};
use std::path::Path;
use std::process::Command;

use crate::Bundle;

/// System essentials a child may inherit from the daemon: what the OS and ordinary tools need
/// to start, and nothing that carries a credential.
#[cfg(windows)]
pub const ESSENTIALS: &[&str] = &[
    "ALLUSERSPROFILE",
    "APPDATA",
    "CommonProgramFiles",
    "CommonProgramFiles(x86)",
    "CommonProgramW6432",
    "COMPUTERNAME",
    "ComSpec",
    "HOME",
    "HOMEDRIVE",
    "HOMEPATH",
    "LANG",
    "LOCALAPPDATA",
    "NUMBER_OF_PROCESSORS",
    "OS",
    "PATH",
    "PATHEXT",
    "PROCESSOR_ARCHITECTURE",
    "ProgramData",
    "ProgramFiles",
    "ProgramFiles(x86)",
    "ProgramW6432",
    "SystemDrive",
    "SystemRoot",
    "TEMP",
    "TMP",
    "TZ",
    "USERDOMAIN",
    "USERNAME",
    "USERPROFILE",
    "windir",
];

/// System essentials a child may inherit from the daemon: what the OS and ordinary tools need
/// to start, and nothing that carries a credential.
#[cfg(not(windows))]
pub const ESSENTIALS: &[&str] = &[
    "HOME",
    "LANG",
    "LC_ALL",
    "LC_CTYPE",
    "LC_MESSAGES",
    "LOGNAME",
    "PATH",
    "SHELL",
    "TERM",
    "TMPDIR",
    "TZ",
    "USER",
];

/// Variables that carry a credential by name. With the suffixes in [`CREDENTIAL_SUFFIXES`] and
/// the prefixes in [`RESERVED_PREFIXES`], a repository can never declare one (#2, rule 1).
pub const CREDENTIALS: &[&str] = &[
    "ANTHROPIC_API_KEY",
    "ANTHROPIC_AUTH_TOKEN",
    "AWS_ACCESS_KEY_ID",
    "AWS_SECRET_ACCESS_KEY",
    "AWS_SESSION_TOKEN",
    "AZURE_CLIENT_SECRET",
    // The file of the daemon's operator token (`bosun_client::operator`, #67): the credential
    // of the control switches, never an agent's.
    "BOSUN_OPERATOR_FILE",
    "CLAUDE_CODE_OAUTH_TOKEN",
    "GEMINI_API_KEY",
    "GH_ENTERPRISE_TOKEN",
    "GH_TOKEN",
    "GITHUB_ENTERPRISE_TOKEN",
    "GITHUB_TOKEN",
    "GOOGLE_API_KEY",
    "GOOGLE_APPLICATION_CREDENTIALS",
    "NPM_TOKEN",
    "OPENAI_API_KEY",
    "SSH_AUTH_SOCK",
];

/// Name endings that mark a credential.
pub const CREDENTIAL_SUFFIXES: &[&str] = &[
    "_TOKEN",
    "_API_KEY",
    "_SECRET",
    "_PASSWORD",
    "_ACCESS_KEY",
    "_CREDENTIALS",
];

/// Prefixes the bundle and the closures below own: git's, `gh`'s, GitHub's and SSH's own
/// variables. Neither a repository nor [`Bundle::runner_env`] sets them.
pub const RESERVED_PREFIXES: &[&str] = &["GIT_", "GH_", "GITHUB_", "SSH_", "GCM_"];

/// What an agent's `GH_TOKEN` and its siblings hold: not a token. `gh` prefers the variable to
/// the OS credential store, and with an empty `GH_CONFIG_DIR` alone it still finds the user's
/// token there (observed with `gh` 2.93 on Windows), so the variable has to be set to something.
pub const NO_TOKEN: &str = "bosun-agent-sessions-have-no-github-token";

/// What an agent's git runs instead of `ssh`: a refusal, through the shell git runs it with.
pub const NO_SSH: &str = "sh -c 'echo \"bosun: agent sessions have no SSH access\" >&2; exit 1'";

/// A constructed environment, ready to replace a child's.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Env {
    vars: Vec<(String, OsString)>,
    denied: Vec<String>,
}

impl Env {
    /// The environment of the git and `gh` processes the daemon itself runs for the identity:
    /// the essentials and the full bundle (`GH_CONFIG_DIR`, `GIT_CONFIG_GLOBAL`, the SSH key).
    /// No runner variable and nothing a repository declares.
    #[must_use]
    pub fn tool(bundle: &Bundle, inherited: &[(OsString, OsString)]) -> Self {
        let mut env = Self::essentials(inherited);
        env.set("GH_CONFIG_DIR", bundle.gh_config_dir.as_os_str());
        env.set("GH_HOST", bundle.host.as_str());
        env.set("GH_PROMPT_DISABLED", "1");
        env.set("GH_NO_UPDATE_NOTIFIER", "1");
        env.set("GIT_CONFIG_GLOBAL", bundle.git_config.as_os_str());
        env.set("GIT_CONFIG_NOSYSTEM", "1");
        env.set("GIT_TERMINAL_PROMPT", "0");
        env.set("GCM_INTERACTIVE", "never");
        if let Some(key) = &bundle.ssh_key {
            env.set(
                "GIT_SSH_COMMAND",
                format!("ssh -i {} -o IdentitiesOnly=yes", sh_quote(key)),
            );
        }
        env
    }

    /// The environment of an agent session: the essentials, the toolchain variables the
    /// repository declares (credentials denied), the runners' directories and variables, and
    /// git's author and signing configuration. Every ambient path to a GitHub credential is
    /// closed:
    ///
    /// - `GH_CONFIG_DIR` is `empty_dir` (an empty directory the daemon owns), and `GH_TOKEN`,
    ///   `GITHUB_TOKEN` and the Enterprise pair hold [`NO_TOKEN`], so `gh` finds no token in
    ///   either the configuration or the OS credential store;
    /// - `GIT_CONFIG_NOSYSTEM=1` drops the system configuration (Git Credential Manager on
    ///   Windows), and a command-scope `credential.helper=` empties the helper list that the
    ///   identity's own git configuration may have filled;
    /// - no `SSH_AUTH_SOCK`, and `GIT_SSH_COMMAND` is [`NO_SSH`];
    /// - `GIT_TERMINAL_PROMPT=0`.
    #[must_use]
    pub fn agent(
        bundle: &Bundle,
        inherited: &[(OsString, OsString)],
        declared: &[String],
        empty_dir: &Path,
    ) -> Self {
        #[cfg(mutant_identity_agent_gets_bundle)]
        let mut env = {
            let _ = empty_dir;
            Self::tool(bundle, inherited)
        };
        #[cfg(not(mutant_identity_agent_gets_bundle))]
        let mut env = Self::essentials(inherited);
        for name in declared {
            if declarable(name, bundle) {
                if let Some(value) = lookup(inherited, name) {
                    env.set(name, value);
                }
            } else {
                env.denied.push(name.clone());
            }
        }
        for (name, dir) in &bundle.runner_dirs {
            if reserved(name) {
                env.denied.push(name.clone());
            } else {
                env.set(name, dir.as_os_str());
            }
        }
        for (name, value) in &bundle.runner_env {
            if reserved(name) {
                env.denied.push(name.clone());
            } else {
                env.set(name, value);
            }
        }
        #[cfg(not(mutant_identity_agent_gets_bundle))]
        {
            env.set("GIT_CONFIG_GLOBAL", bundle.git_config.as_os_str());
            env.set("GIT_CONFIG_NOSYSTEM", "1");
            env.set("GIT_CONFIG_COUNT", "1");
            env.set("GIT_CONFIG_KEY_0", "credential.helper");
            env.set("GIT_CONFIG_VALUE_0", "");
            env.set("GIT_SSH_COMMAND", NO_SSH);
            env.set("GIT_TERMINAL_PROMPT", "0");
            env.set("GCM_INTERACTIVE", "never");
            env.set("GH_CONFIG_DIR", empty_dir.as_os_str());
            env.set("GH_PROMPT_DISABLED", "1");
            for name in [
                "GH_TOKEN",
                "GITHUB_TOKEN",
                "GH_ENTERPRISE_TOKEN",
                "GITHUB_ENTERPRISE_TOKEN",
            ] {
                env.set(name, NO_TOKEN);
            }
        }
        env
    }

    /// Appends command-scope git configuration after the entries this environment already
    /// carries: `GIT_CONFIG_KEY_<n>` and `GIT_CONFIG_VALUE_<n>` from the current
    /// `GIT_CONFIG_COUNT` on, and the count raised. This is how `bosun-git`'s injected trailer
    /// hooks (`Hooks::git_config`, #59) reach the agent's git without displacing the
    /// `credential.helper` reset at entry 0.
    pub fn add_git_config<K, V>(&mut self, entries: impl IntoIterator<Item = (K, V)>)
    where
        K: AsRef<str>,
        V: Into<OsString>,
    {
        let mut count: usize = self
            .get("GIT_CONFIG_COUNT")
            .and_then(OsStr::to_str)
            .and_then(|n| n.parse().ok())
            .unwrap_or(0);
        for (key, value) in entries {
            self.set(&format!("GIT_CONFIG_KEY_{count}"), key.as_ref());
            self.set(&format!("GIT_CONFIG_VALUE_{count}"), value);
            count += 1;
            self.set("GIT_CONFIG_COUNT", count.to_string());
        }
    }

    /// Sets a variable the daemon itself gives the session, such as the trailer values of #59
    /// (`bosun-git`'s `Hooks::env`). A reserved or credential name is refused into
    /// [`Env::denied`], as a repository's would be.
    pub fn add_session_var(&mut self, name: &str, value: impl Into<OsString>) {
        if reserved(name) || credential(name) {
            self.denied.push(name.to_owned());
        } else {
            self.set(name, value);
        }
    }

    /// The variables, in the order they were first set.
    pub fn iter(&self) -> impl Iterator<Item = (&str, &OsStr)> {
        self.vars.iter().map(|(k, v)| (k.as_str(), v.as_os_str()))
    }

    /// The value of `name` (ignoring case on Windows, as the OS does).
    #[must_use]
    pub fn get(&self, name: &str) -> Option<&OsStr> {
        self.vars
            .iter()
            .find(|(k, _)| same_name(k, name))
            .map(|(_, v)| v.as_os_str())
    }

    /// Names a repository declared or the bundle supplied that were refused, for the log.
    #[must_use]
    pub fn denied(&self) -> &[String] {
        &self.denied
    }

    /// Replaces `command`'s environment with this one: nothing of the daemon's own survives.
    pub fn apply(&self, command: &mut Command) {
        command.env_clear();
        command.envs(self.vars.iter().map(|(k, v)| (k, v)));
    }

    fn essentials(inherited: &[(OsString, OsString)]) -> Self {
        let mut env = Self::default();
        #[cfg(mutant_identity_inherit_env)]
        for (name, value) in inherited {
            if let Some(name) = name.to_str() {
                env.set(name, value);
            }
        }
        #[cfg(not(mutant_identity_inherit_env))]
        for name in ESSENTIALS {
            if let Some(value) = lookup(inherited, name) {
                env.set(name, value);
            }
        }
        env
    }

    fn set(&mut self, name: &str, value: impl Into<OsString>) {
        let value = value.into();
        match self.vars.iter_mut().find(|(k, _)| same_name(k, name)) {
            Some(slot) => slot.1 = value,
            None => self.vars.push((name.to_owned(), value)),
        }
    }
}

/// The daemon's own environment, as [`Env::tool`] and [`Env::agent`] take it.
#[must_use]
pub fn inherited() -> Vec<(OsString, OsString)> {
    std::env::vars_os().collect()
}

/// Whether `name` carries a credential by name (#2, rule 1).
#[must_use]
pub fn credential(name: &str) -> bool {
    let upper = name.to_ascii_uppercase();
    CREDENTIALS.contains(&upper.as_str()) || CREDENTIAL_SUFFIXES.iter().any(|s| upper.ends_with(s))
}

/// Whether `name` belongs to the bundle and the closures: git's, `gh`'s, GitHub's or SSH's.
#[must_use]
pub fn reserved(name: &str) -> bool {
    let upper = name.to_ascii_uppercase();
    RESERVED_PREFIXES.iter().any(|p| upper.starts_with(p))
}

/// Whether a repository may declare `name`: not a credential, not reserved, not a runner
/// directory of the bundle.
fn declarable(name: &str, bundle: &Bundle) -> bool {
    #[cfg(mutant_identity_declared_credentials)]
    {
        let _ = (name, bundle);
        true
    }
    #[cfg(not(mutant_identity_declared_credentials))]
    {
        !credential(name)
            && !reserved(name)
            && !bundle.runner_dirs.iter().any(|(k, _)| same_name(k, name))
            && !bundle.runner_env.iter().any(|(k, _)| same_name(k, name))
    }
}

fn lookup<'a>(inherited: &'a [(OsString, OsString)], name: &str) -> Option<&'a OsStr> {
    inherited
        .iter()
        .find(|(k, _)| k.to_str().is_some_and(|k| same_name(k, name)))
        .map(|(_, v)| v.as_os_str())
}

fn same_name(a: &str, b: &str) -> bool {
    if cfg!(windows) {
        a.eq_ignore_ascii_case(b)
    } else {
        a == b
    }
}

/// `path` as one POSIX shell word (git runs `GIT_SSH_COMMAND` through `sh`).
fn sh_quote(path: &Path) -> String {
    format!("'{}'", path.to_string_lossy().replace('\'', r"'\''"))
}

#[cfg(test)]
mod tests {
    use super::{credential, reserved, sh_quote};
    use std::path::Path;

    #[test]
    fn credentials_are_recognised_by_name_and_suffix() {
        for name in [
            "GH_TOKEN",
            "github_token",
            "OPENAI_API_KEY",
            "CARGO_REGISTRY_TOKEN",
            "SSH_AUTH_SOCK",
            "MY_SERVICE_PASSWORD",
        ] {
            assert!(credential(name), "{name}");
        }
        for name in ["PATH", "CARGO_HOME", "JAVA_HOME", "TOKENIZER_THREADS"] {
            assert!(!credential(name), "{name}");
        }
    }

    #[test]
    fn git_gh_and_ssh_variables_are_reserved() {
        for name in [
            "GIT_CONFIG_GLOBAL",
            "GH_CONFIG_DIR",
            "GITHUB_API_URL",
            "SSH_ASKPASS",
        ] {
            assert!(reserved(name), "{name}");
        }
        assert!(!reserved("CARGO_HOME"));
    }

    #[test]
    fn ssh_key_paths_are_quoted_for_sh() {
        assert_eq!(sh_quote(Path::new("/k/it's")), r"'/k/it'\''s'");
    }
}
