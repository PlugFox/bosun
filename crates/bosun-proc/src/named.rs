//! The processes of the current user by executable file name, for the process sweep of #8
//! (E12): after an attempt, the daemon kills the known strays a repository lists that started
//! during the attempt.

use std::io;

use crate::key::{ProcessKey, StartTime};
use crate::sys;

/// A running process found by its name: its key, for [`crate::reap`], and when it started.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Named {
    /// The pid and the start time: what [`crate::reap`] takes, so that a pid reused since the
    /// listing is never taken for this process.
    pub key: ProcessKey,
    /// When it started, in Unix milliseconds, rounded down: a doubt makes the process look
    /// older, and an older process is spared. (Linux: the boot time `btime` of `/proc/stat` is
    /// whole seconds, so this may be up to a second early.)
    pub started_unix_ms: i64,
}

/// The running processes of the current user whose executable's file name is `name`. On
/// Windows the name matches with or without `.exe`, case-insensitively; elsewhere exactly.
///
/// Windows: a Toolhelp process snapshot; a process that cannot be opened, or whose token names
/// another user, is skipped. Linux: the file name of `/proc/<pid>/exe` (not `comm`, which is
/// cut at 15 bytes); a process of another uid, or whose link cannot be read, is skipped. macOS:
/// `proc_listallpids` and the BSD info of each pid (`pbi_name`, else `pbi_comm`), the user's
/// processes only. Zombies are never listed.
///
/// # Errors
///
/// When the process table cannot be listed, or on a Unix that is neither Linux nor macOS.
pub fn processes_named(name: &str) -> io::Result<Vec<Named>> {
    Ok(sys::processes_named(name)?
        .into_iter()
        .map(|(pid, start)| Named {
            key: ProcessKey {
                pid,
                start: StartTime::from_raw(start),
            },
            started_unix_ms: sys::start_unix_ms(start),
        })
        .collect())
}

/// Whether the executable file name `file` is `name`: on Windows case-insensitively and with
/// or without `.exe`, elsewhere byte for byte.
pub(crate) fn file_name_is(file: &str, name: &str) -> bool {
    if cfg!(mutant_proc_named_matches_every_process) {
        // Mutant (`mutant_proc_named_matches_every_process`): the name comparison is skipped,
        // so every process of the user is listed.
        return true;
    }
    if !cfg!(windows) {
        return file == name;
    }
    if file.eq_ignore_ascii_case(name) {
        return true;
    }
    let Some(stem) = file.len().checked_sub(4) else {
        return false;
    };
    match (file.get(..stem), file.get(stem..)) {
        (Some(stem), Some(ext)) => {
            ext.eq_ignore_ascii_case(".exe") && stem.eq_ignore_ascii_case(name)
        }
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::file_name_is;

    #[test]
    fn a_file_name_matches_the_name_and_on_windows_also_with_exe_in_any_case() {
        assert!(file_name_is("devserver", "devserver"));
        assert!(file_name_is("devserver.exe", "devserver.exe"));
        for file in ["DevServer.EXE", "devserver.exe", "DEVSERVER"] {
            assert_eq!(file_name_is(file, "devserver"), cfg!(windows), "{file}");
        }
        for file in ["devserver2.exe", "devserve.exe", "devserver.com", "exe", ""] {
            assert!(!file_name_is(file, "devserver"), "{file}");
        }
    }
}
