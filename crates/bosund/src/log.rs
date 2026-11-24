//! The daemon's log (#137): each line goes to standard error and, once [`init`] opened it, to
//! the log file `bosun_client::log::path` names, with a UTC timestamp. The logon task discards
//! standard error; the file keeps the lines. The file is rotated by size: at
//! [`bosun_client::log::MAX_BYTES`] it becomes `<file>.1`, the earlier ones move up, and
//! [`bosun_client::log::KEEP`] of them are kept.
//!
//! A file that cannot be opened or written leaves standard error only; the daemon runs on.

use std::fs::{File, OpenOptions};
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock, PoisonError};
use std::time::{SystemTime, UNIX_EPOCH};

use bosun_client::log::{KEEP, MAX_BYTES, rotated};

/// Writes a line to standard error and the log file; takes `format!`'s arguments.
macro_rules! log {
    ($($arg:tt)*) => {
        $crate::log::line(&format!($($arg)*))
    };
}
pub(crate) use log;

struct LogFile {
    path: PathBuf,
    file: Option<File>,
    len: u64,
    max: u64,
}

static FILE: OnceLock<Mutex<LogFile>> = OnceLock::new();

/// Opens the log file at `path` for appending, creating its directory. The first call wins.
///
/// # Errors
///
/// The directory or the file cannot be created; the lines go to standard error only.
pub(crate) fn init(path: &Path) -> Result<(), String> {
    let log = LogFile::open(path, MAX_BYTES)?;
    let _ = FILE.set(Mutex::new(log));
    Ok(())
}

/// Writes one line to standard error and to the log file.
pub(crate) fn line(text: &str) {
    eprintln!("{text}");
    if let Some(log) = FILE.get() {
        log.lock()
            .unwrap_or_else(PoisonError::into_inner)
            .write(&format!("{} {text}\n", timestamp(now_ms())));
    }
}

impl LogFile {
    fn open(path: &Path, max: u64) -> Result<Self, String> {
        if let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
            std::fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
        }
        let file = append(path).map_err(|e| format!("{}: {e}", path.display()))?;
        let len = file.metadata().map_or(0, |m| m.len());
        Ok(Self {
            path: path.to_owned(),
            file: Some(file),
            len,
            max,
        })
    }

    fn write(&mut self, text: &str) {
        let len = text.len() as u64;
        if self.len > 0 && self.len + len > self.max {
            self.rotate();
        }
        if let Some(file) = &mut self.file
            && file.write_all(text.as_bytes()).is_ok()
        {
            self.len += len;
        }
    }

    /// `<file>` becomes `<file>.1`, `.1` becomes `.2`, and so on; the oldest is dropped.
    fn rotate(&mut self) {
        // Windows refuses to rename an open file: it is closed first.
        self.file = None;
        let _ = std::fs::remove_file(rotated(&self.path, KEEP));
        for n in (1..KEEP).rev() {
            let _ = std::fs::rename(rotated(&self.path, n), rotated(&self.path, n + 1));
        }
        let _ = std::fs::rename(&self.path, rotated(&self.path, 1));
        self.file = append(&self.path).ok();
        self.len = self
            .file
            .as_ref()
            .and_then(|f| f.metadata().ok())
            .map_or(0, |m| m.len());
    }
}

fn append(path: &Path) -> std::io::Result<File> {
    OpenOptions::new().create(true).append(true).open(path)
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX))
}

/// `at_ms` as `YYYY-MM-DDTHH:MM:SS.mmmZ`.
fn timestamp(at_ms: i64) -> String {
    let (days, ms) = (at_ms.div_euclid(86_400_000), at_ms.rem_euclid(86_400_000));
    // H. Hinnant's `civil_from_days`.
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}.{:03}Z",
        ms / 3_600_000,
        ms / 60_000 % 60,
        ms / 1_000 % 60,
        ms % 1_000
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn timestamps_are_utc() {
        assert_eq!(timestamp(0), "1970-01-01T00:00:00.000Z");
        assert_eq!(timestamp(1_709_210_096_789), "2024-02-29T12:34:56.789Z");
    }

    #[test]
    fn the_file_rotates_by_size_and_keeps_the_earlier_ones() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("log").join("bosund.log");
        let mut log = LogFile::open(&path, 10).unwrap();
        for n in 0..6 {
            log.write(&format!("line {n}\n"));
        }
        drop(log);
        let read = |p: &Path| std::fs::read_to_string(p).unwrap_or_default();
        assert_eq!(read(&path), "line 5\n");
        assert_eq!(read(&rotated(&path, 1)), "line 4\n");
        assert_eq!(read(&rotated(&path, KEEP)), "line 2\n");
        assert!(!rotated(&path, KEEP + 1).exists(), "only {KEEP} are kept");
        // Reopened, it appends.
        let mut log = LogFile::open(&path, 100).unwrap();
        log.write("line 6\n");
        drop(log);
        assert_eq!(read(&path), "line 5\nline 6\n");
    }
}
