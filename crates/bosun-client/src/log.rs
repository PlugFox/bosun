//! Where the daemon's log file is (#137): the logon task discards the daemon's standard error,
//! so `bosund` writes each log line to this file as well, and `bosun doctor` names it.
//!
//! The file is [`FILE_ENV`] when set, else `log/<endpoint name>.log` in the directory of the
//! configuration file (on Windows `%LOCALAPPDATA%\bosun\log\<pipe name>.log`): one file per
//! endpoint, so a second daemon (tests, a trial endpoint) keeps its own. The daemon rotates
//! it by size, keeping [`KEEP`] earlier files beside it (`<file>.1` the newest).

use std::ffi::OsString;
use std::path::{Path, PathBuf};

use crate::Endpoint;

/// The variable that names the daemon's log file.
pub const FILE_ENV: &str = "BOSUN_LOG_FILE";

/// The size at which the daemon starts a new log file, in bytes.
pub const MAX_BYTES: u64 = 8 * 1024 * 1024;

/// The earlier log files kept after a rotation.
pub const KEEP: u32 = 3;

/// The log file of the daemon at `endpoint` that reads the configuration file `config`:
/// [`FILE_ENV`] when set, else the default of the module documentation.
#[must_use]
pub fn path(config: &Path, endpoint: &Endpoint) -> PathBuf {
    path_in(config, endpoint, &|name| std::env::var_os(name))
}

/// The earlier file `n` (from 1, the newest) of the log file `file`.
#[must_use]
pub fn rotated(file: &Path, n: u32) -> PathBuf {
    let mut name = file.as_os_str().to_owned();
    name.push(format!(".{n}"));
    PathBuf::from(name)
}

pub(crate) fn path_in(
    config: &Path,
    endpoint: &Endpoint,
    env: &dyn Fn(&str) -> Option<OsString>,
) -> PathBuf {
    if let Some(explicit) = env(FILE_ENV).filter(|v| !v.is_empty()) {
        return PathBuf::from(explicit);
    }
    let name = endpoint
        .path()
        .file_name()
        .map_or_else(|| "bosund".to_owned(), |n| n.to_string_lossy().into_owned());
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
    let dir = config.parent().unwrap_or_else(|| Path::new("."));
    dir.join("log").join(format!("{name}.log"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_variable_wins_else_it_is_beside_the_configuration_per_endpoint() {
        let endpoint = Endpoint::new(if cfg!(windows) {
            "bosun-alice"
        } else {
            "/run/user/1000/bosun/bosund.sock"
        });
        let config = Path::new("home").join("bosun").join("config.toml");
        let explicit = path_in(&config, &endpoint, &|name| {
            (name == FILE_ENV).then(|| OsString::from("elsewhere.log"))
        });
        assert_eq!(explicit, Path::new("elsewhere.log"));
        let name = if cfg!(windows) {
            "bosun-alice.log"
        } else {
            "bosund.sock.log"
        };
        assert_eq!(
            path_in(&config, &endpoint, &|_| None),
            Path::new("home").join("bosun").join("log").join(name)
        );
        assert_eq!(
            rotated(Path::new("a.log"), 2),
            Path::new("a.log.2"),
            "the earlier files"
        );
    }
}
