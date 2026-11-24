//! The free disk space of a volume (#8): the disk floor of worktrees asks how many bytes the
//! user may still write to the volume that holds a path. [`free_space`] is the platform's:
//! `GetDiskFreeSpaceExW` on Windows, `statvfs` on Unix.
//!
//! A path that does not exist is an error, never an unlimited volume: a floor checked against a
//! mistyped path would otherwise always pass.

use std::io;
use std::path::Path;

use crate::sys;

/// The bytes available to the user on the volume holding `path`: the figure that honours quotas,
/// not the bytes free to everyone.
///
/// # Errors
///
/// When `path` does not exist, or the operating system does not report the volume's space.
pub fn free_space(path: &Path) -> io::Result<u64> {
    match sys::free_space(path) {
        // Mutant (`mutant_proc_free_space_missing_is_unlimited`): a missing path answers
        // `u64::MAX`, so a floor checked against it always passes.
        Err(e)
            if e.kind() == io::ErrorKind::NotFound
                && cfg!(mutant_proc_free_space_missing_is_unlimited) =>
        {
            Ok(u64::MAX)
        }
        other => other,
    }
}

#[cfg(test)]
mod tests {
    use super::free_space;

    #[test]
    fn free_space_is_reported_for_a_directory_and_refused_for_a_missing_one() {
        let dir = tempfile::tempdir().expect("tempdir");
        let free = free_space(dir.path()).expect("the free space of a directory");
        assert!(free > 0, "a temporary directory has room: {free}");
        assert_ne!(free, u64::MAX, "a directory is not unlimited");
        let missing = dir.path().join("missing");
        assert!(
            free_space(&missing).is_err(),
            "a missing path has no free space"
        );
    }
}
