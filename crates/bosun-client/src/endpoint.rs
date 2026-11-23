//! Where the daemon listens (#4, #24).
//!
//! One endpoint per OS user. [`Endpoint::resolve`] takes the variable `BOSUN_ENDPOINT` when it
//! is set (tests, or a second daemon with a configuration of its own), otherwise:
//!
//! | Platform | Endpoint |
//! | --- | --- |
//! | Windows | the named pipe `\\.\pipe\bosun-<user>`, `<user>` from `USERNAME` |
//! | Unix | the socket `$XDG_RUNTIME_DIR/bosun/bosund.sock` when `XDG_RUNTIME_DIR` is set, else `${TMPDIR:-/tmp}/bosun-<user>/bosund.sock`, `<user>` from `USER` or `LOGNAME` |
//!
//! `<user>` keeps ASCII letters, digits, `-`, `_` and `.`, lowercased; anything else becomes
//! `_`. The endpoint is per user, not per configuration: two daemons with two configurations
//! naming the same database would otherwise both run.
//!
//! On Unix the daemon creates the directory with mode 0700 and the socket with mode 0600. On
//! Windows the pipe has the default security of the daemon's process in stage 0; the DACL that
//! admits the owning user only is stage 1 (#4).

use std::ffi::OsString;
use std::fmt;
use std::path::{Path, PathBuf};

/// The variable that overrides the endpoint: a socket path on Unix, a pipe name on Windows
/// (with or without the `\\.\pipe\` prefix).
pub const ENV: &str = "BOSUN_ENDPOINT";

/// The prefix of every named pipe's path on Windows.
pub const PIPE_PREFIX: &str = r"\\.\pipe\";

/// The address of the daemon's endpoint: a socket path on Unix, a pipe path on Windows.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Endpoint {
    path: PathBuf,
}

impl Endpoint {
    /// An explicit endpoint. On Windows a bare name gets the `\\.\pipe\` prefix.
    #[must_use]
    pub fn new(path: impl Into<PathBuf>) -> Self {
        let path = path.into();
        if cfg!(windows) {
            let text = path.to_string_lossy();
            if !text.starts_with(PIPE_PREFIX) {
                return Self {
                    path: PathBuf::from(format!("{PIPE_PREFIX}{text}")),
                };
            }
        }
        Self { path }
    }

    /// `BOSUN_ENDPOINT` when set, otherwise the per-user default (see the module
    /// documentation).
    ///
    /// # Errors
    ///
    /// The environment names no user to build the default from.
    pub fn resolve() -> Result<Self, String> {
        resolve_in(&|name| std::env::var_os(name))
    }

    /// The socket path or pipe path.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl fmt::Display for Endpoint {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(&self.path.display(), f)
    }
}

fn user_component(raw: &str) -> String {
    raw.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.') {
                c.to_ascii_lowercase()
            } else {
                '_'
            }
        })
        .collect()
}

pub(crate) fn resolve_in(env: &dyn Fn(&str) -> Option<OsString>) -> Result<Endpoint, String> {
    let var = |name: &str| {
        env(name)
            .filter(|v| !v.is_empty())
            .map(|v| v.to_string_lossy().into_owned())
    };
    if let Some(explicit) = var(ENV) {
        return Ok(Endpoint::new(explicit));
    }
    let user = || {
        let names: &[&str] = if cfg!(windows) {
            &["USERNAME"]
        } else {
            &["USER", "LOGNAME"]
        };
        names
            .iter()
            .find_map(|name| var(name))
            .map(|u| user_component(&u))
            .ok_or_else(|| {
                format!(
                    "cannot name the endpoint: none of {} is set; set {ENV}",
                    names.join(", ")
                )
            })
    };
    if cfg!(windows) {
        return Ok(Endpoint::new(format!("{PIPE_PREFIX}bosun-{}", user()?)));
    }
    let absolute = |name: &str| var(name).map(PathBuf::from).filter(|p| p.is_absolute());
    let dir = match absolute("XDG_RUNTIME_DIR") {
        Some(runtime) => runtime.join("bosun"),
        None => absolute("TMPDIR")
            .unwrap_or_else(|| PathBuf::from("/tmp"))
            .join(format!("bosun-{}", user()?)),
    };
    Ok(Endpoint::new(dir.join("bosund.sock")))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env<'a>(pairs: &'a [(&'a str, &'a str)]) -> impl Fn(&str) -> Option<OsString> + 'a {
        move |name| {
            pairs
                .iter()
                .find(|(k, _)| *k == name)
                .map(|(_, v)| OsString::from(v))
        }
    }

    #[test]
    fn the_variable_wins() {
        let found = resolve_in(&env(&[(ENV, "custom"), ("USERNAME", "u"), ("USER", "u")]));
        let expected = if cfg!(windows) {
            r"\\.\pipe\custom"
        } else {
            "custom"
        };
        assert_eq!(found.unwrap().path(), Path::new(expected));
    }

    #[test]
    fn the_default_is_per_user() {
        let a = resolve_in(&env(&[
            ("USERNAME", "Alice Smith"),
            ("USER", "Alice Smith"),
        ]));
        let b = resolve_in(&env(&[("USERNAME", "bob"), ("USER", "bob")]));
        let (a, b) = (a.unwrap(), b.unwrap());
        assert_ne!(a, b);
        if cfg!(windows) {
            assert_eq!(a.path(), Path::new(r"\\.\pipe\bosun-alice_smith"));
        } else {
            assert_eq!(a.path(), Path::new("/tmp/bosun-alice_smith/bosund.sock"));
            let runtime = resolve_in(&env(&[("XDG_RUNTIME_DIR", "/run/user/1000")])).unwrap();
            assert_eq!(
                runtime.path(),
                Path::new("/run/user/1000/bosun/bosund.sock")
            );
        }
        assert!(resolve_in(&env(&[])).is_err());
    }
}
