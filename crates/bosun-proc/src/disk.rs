//! The free space of a volume, for the disk floor of item worktrees (#8, #127).
//!
//! [`free_space`] answers the bytes available to the calling user on the volume that holds a
//! path: `GetDiskFreeSpaceExW` on Windows, `statvfs` (`f_bavail * f_frsize`) on Unix. Both
//! honour quotas and reserved blocks, so the answer is what a write can actually use. A path
//! that cannot be read is an error, never "unlimited": a floor check must fail closed.

use std::io;
use std::path::Path;

use crate::sys;

/// The bytes available to the calling user on the volume that holds `path`.
///
/// # Errors
///
/// When `path` does not exist or the system cannot report its volume.
pub fn free_space(path: &Path) -> io::Result<u64> {
    match sys::free_space(path) {
        // Mutant (`mutant_proc_free_space_missing_is_unlimited`): a missing path answers
        // "unlimited", so every floor check passes.
        Err(_) if cfg!(mutant_proc_free_space_missing_is_unlimited) => Ok(u64::MAX),
        result => result,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn free_space_is_reported_for_a_directory_and_refused_for_a_missing_one() {
        let dir = tempfile::tempdir().unwrap();

        let free = free_space(dir.path()).unwrap();
        assert!(free > 0, "a temp directory reports no free space");
        assert_ne!(free, u64::MAX, "a temp directory reports unlimited space");

        let missing = dir.path().join("missing").join("deeper");
        let answer = free_space(&missing);
        assert!(answer.is_err(), "a missing path answered {answer:?}");
    }
}
