//! The tracked files of a commit (#11 D, #210): what heat's U1 compares with the files its
//! sessions opened. Read-only: `git ls-tree` of a revision's tree, no checkout and no index.

use crate::{Error, Git};

/// Whether the file name of the `/`-separated `path` ends with `.<suffix>` of one of
/// `suffixes`, case-insensitively.
fn has_suffix(path: &str, suffixes: &[&str]) -> bool {
    let name = path.rsplit('/').next().unwrap_or(path).as_bytes();
    suffixes.iter().any(|suffix| {
        let tail = suffix.len() + 1;
        name.len() > tail
            && name[name.len() - tail] == b'.'
            && name[name.len() - suffix.len()..].eq_ignore_ascii_case(suffix.as_bytes())
    })
}

/// The files of `rev`'s tree whose name ends with `.<suffix>` of one of `suffixes`
/// (case-insensitively), repository-relative with `/` and git's case, sorted.
///
/// # Errors
///
/// `git` cannot start, the directory is no repository, or `rev` names no tree.
pub fn files(git: &Git, rev: &str, suffixes: &[&str]) -> Result<Vec<String>, Error> {
    let listed = git.run_bytes(&["ls-tree", "-r", "-z", "--name-only", rev, "--"], None)?;
    let mut out: Vec<String> = listed
        .split(|&b| b == 0)
        .filter(|entry| !entry.is_empty())
        .map(|entry| String::from_utf8_lossy(entry).into_owned())
        .filter(|path| has_suffix(path, suffixes))
        .collect();
    out.sort();
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::files;
    use crate::Git;
    use crate::tests::{commit, repository};

    /// Only the committed files with the suffix, in any case, are listed; an untracked one is
    /// not; a directory that is no repository is an error.
    #[test]
    fn only_committed_files_with_the_suffix_are_listed() {
        let repo = repository();
        let dir = repo.path();
        std::fs::create_dir_all(dir.join("docs")).unwrap();
        std::fs::create_dir_all(dir.join("src")).unwrap();
        commit(dir, "README.md", "Add a readme");
        commit(dir, "docs/Guide.MD", "Add a guide");
        commit(dir, "src/lib.rs", "Add a library");
        std::fs::write(dir.join("notes.md"), "untracked").unwrap();

        let git = Git::new(dir);
        assert_eq!(
            files(&git, "HEAD", &["md"]).unwrap(),
            ["README.md", "docs/Guide.MD"]
        );

        let plain = tempfile::tempdir().unwrap();
        assert!(files(&Git::new(plain.path()), "HEAD", &["md"]).is_err());
    }
}
