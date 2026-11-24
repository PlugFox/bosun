//! Where the operator token lives (#67): the credential of the `control.*` methods that change
//! anything.
//!
//! The daemon writes a fresh token at every start; the CLI and the TUI read it. It is a file of
//! the OS user, outside every working tree, never in an agent session's environment:
//!
//! | Platform | File |
//! | --- | --- |
//! | Windows | `%LOCALAPPDATA%\bosun\operator\<pipe name>.token` (the profile's own ACL: the user, SYSTEM and administrators) |
//! | Unix | next to the socket, `<socket>.operator`, mode 0600, in the socket's directory of mode 0700 |
//!
//! The variable [`FILE_ENV`] names another file (tests, or a second daemon). It is denied in an
//! agent's environment by name (`bosun-identity`), and the agent CLI is told not to read or
//! edit the file. Neither is a boundary against a same-user agent with a shell: the token keeps
//! a confused or wandering session from the switches, not a hostile one; only running sessions
//! under an OS account of their own would.

use std::ffi::OsString;
use std::path::PathBuf;

use crate::Endpoint;

/// The variable that names the operator token's file.
pub const FILE_ENV: &str = "BOSUN_OPERATOR_FILE";

/// The prefix of every operator token, so one is recognisable.
pub const PREFIX: &str = "bosun-operator-";

/// The operator token's file for the daemon at `endpoint`: [`FILE_ENV`] when set, else the
/// default of the module documentation.
///
/// # Errors
///
/// The environment names no directory to put it in.
pub fn path(endpoint: &Endpoint) -> Result<PathBuf, String> {
    path_in(endpoint, &|name| std::env::var_os(name))
}

/// Reads the operator token of the daemon at `endpoint`.
///
/// # Errors
///
/// The file cannot be named or read, or holds no token; the message says which file.
pub fn read(endpoint: &Endpoint) -> Result<String, String> {
    let file = path(endpoint)?;
    let text = std::fs::read_to_string(&file).map_err(|e| {
        format!(
            "the operator token {} cannot be read ({e}); is the daemon running as this user?",
            file.display()
        )
    })?;
    let token = text.trim();
    if token.starts_with(PREFIX) {
        Ok(token.to_owned())
    } else {
        Err(format!("{} holds no operator token", file.display()))
    }
}

pub(crate) fn path_in(
    endpoint: &Endpoint,
    env: &dyn Fn(&str) -> Option<OsString>,
) -> Result<PathBuf, String> {
    if let Some(explicit) = env(FILE_ENV).filter(|v| !v.is_empty()) {
        return Ok(PathBuf::from(explicit));
    }
    let endpoint = endpoint.path();
    if cfg!(windows) {
        let base = env("LOCALAPPDATA")
            .map(PathBuf::from)
            .filter(|p| p.is_absolute())
            .ok_or_else(|| format!("cannot place the operator token: set {FILE_ENV}"))?;
        let name = endpoint
            .file_name()
            .map_or_else(String::new, |n| n.to_string_lossy().into_owned());
        let name: String = name
            .chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.') {
                    c
                } else {
                    '_'
                }
            })
            .collect();
        return Ok(base
            .join("bosun")
            .join("operator")
            .join(format!("{name}.token")));
    }
    let mut file = endpoint.as_os_str().to_owned();
    file.push(".operator");
    Ok(PathBuf::from(file))
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::*;

    #[test]
    fn the_variable_wins_else_it_is_per_endpoint() {
        let endpoint = Endpoint::new(if cfg!(windows) {
            "bosun-alice"
        } else {
            "/run/user/1000/bosun/bosund.sock"
        });
        let explicit = path_in(&endpoint, &|name| {
            (name == FILE_ENV).then(|| OsString::from("elsewhere.token"))
        });
        assert_eq!(explicit.unwrap(), Path::new("elsewhere.token"));
        if cfg!(windows) {
            let local = path_in(&endpoint, &|name| {
                (name == "LOCALAPPDATA").then(|| OsString::from(r"C:\Users\alice\AppData\Local"))
            });
            assert_eq!(
                local.unwrap(),
                Path::new(r"C:\Users\alice\AppData\Local\bosun\operator\bosun-alice.token")
            );
            assert!(path_in(&endpoint, &|_| None).is_err());
        } else {
            assert_eq!(
                path_in(&endpoint, &|_| None).unwrap(),
                Path::new("/run/user/1000/bosun/bosund.sock.operator")
            );
        }
    }
}
