//! Finding the programs the configuration relies on, on a given `PATH` (#71): `git`, `gh`, the
//! agent CLI and the verdict shell (D4 of #7). `bosun doctor` asks with the `PATH` the daemon
//! gets; the daemon asks with its own.
//!
//! [`verdict_shell`] follows the resolution of `bosun-queue`'s lint (`verdict_shell` there,
//! #51), which a client cannot link (#12); the two must agree.

use std::ffi::OsStr;
use std::path::{Path, PathBuf};

/// `name` found in the first directory of `path` that holds it as a file: on Windows, without
/// an extension, as `<name>.exe` or else `<name>.cmd` (the shim npm installs, as for Claude
/// Code). A `name` with a directory part is taken as a path and only checked.
#[must_use]
pub fn find_program(name: &str, path: Option<&OsStr>) -> Option<PathBuf> {
    let as_path = Path::new(name);
    if as_path.components().count() > 1 {
        return Some(as_path.to_owned()).filter(|p| p.is_file());
    }
    let files: Vec<String> = if as_path.extension().is_some() || !cfg!(windows) {
        vec![name.to_owned()]
    } else {
        vec![format!("{name}.exe"), format!("{name}.cmd")]
    };
    dirs(path)
        .iter()
        .flat_map(|dir| files.iter().map(move |file| dir.join(file)))
        .find(|candidate| candidate.is_file())
}

/// The verdict shell: `configured` when given (`repo.<name>.verdict.shell`, a path or a program
/// on `path`), otherwise `sh` from the Git installation on Windows, otherwise `sh` on `path`,
/// then `/bin/sh`. `None` when nothing is found.
#[must_use]
pub fn verdict_shell(configured: Option<&str>, path: Option<&OsStr>) -> Option<PathBuf> {
    if let Some(program) = configured {
        return find_program(program, path);
    }
    if cfg!(windows) {
        git_shell("sh", path)
    } else {
        find_program("sh", path).or_else(|| Some(PathBuf::from("/bin/sh")).filter(|p| p.is_file()))
    }
}

fn dirs(path: Option<&OsStr>) -> Vec<PathBuf> {
    path.map_or_else(Vec::new, |p| std::env::split_paths(p).collect())
}

/// The shell `name` (`sh` or `bash`) of Git for Windows: `<root>\bin\<name>.exe`, else
/// `<root>\usr\bin\<name>.exe`, of the installation that provides `git.exe` on `path`, else of
/// the default installation directories (`%ProgramFiles%`, `%ProgramW6432%`, `%LOCALAPPDATA%`,
/// each with `Git` and `Programs\Git`). Not `<name>.exe` from `PATH` itself: that may be another
/// toolchain's (MSYS2, Cygwin, a WSL launcher). The verdict shell's `sh` on Windows
/// ([`verdict_shell`]) and the `bash`/`sh` of `bosun with` are both found here, so the two
/// cannot drift.
#[must_use]
pub fn git_shell(name: &str, path: Option<&OsStr>) -> Option<PathBuf> {
    let exe = format!("{name}.exe");
    let mut roots: Vec<PathBuf> = dirs(path)
        .iter()
        .filter(|d| d.join("git.exe").is_file())
        .filter_map(|d| {
            // `<root>\cmd`, `<root>\bin` or `<root>\mingw64\bin`.
            let parent = d.parent()?;
            if parent
                .file_name()
                .is_some_and(|n| n.eq_ignore_ascii_case("mingw64"))
            {
                parent.parent().map(Path::to_owned)
            } else {
                Some(parent.to_owned())
            }
        })
        .collect();
    for var in ["ProgramFiles", "ProgramW6432", "LOCALAPPDATA"] {
        if let Some(base) = std::env::var_os(var) {
            let base = PathBuf::from(base);
            roots.push(base.join("Git"));
            roots.push(base.join("Programs").join("Git"));
        }
    }
    roots
        .iter()
        .flat_map(|root| {
            [
                root.join("bin").join(&exe),
                root.join("usr").join("bin").join(&exe),
            ]
        })
        .find(|p| p.is_file())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_program_is_found_on_the_given_path_only() {
        let dir = tempfile::tempdir().unwrap();
        let exe = format!("tool{}", std::env::consts::EXE_SUFFIX);
        std::fs::write(dir.path().join(&exe), "").unwrap();
        let path = std::env::join_paths([dir.path()]).unwrap();
        assert_eq!(
            find_program("tool", Some(&path)),
            Some(dir.path().join(&exe))
        );
        assert_eq!(find_program("absent", Some(&path)), None);
        assert_eq!(find_program("tool", None), None);
        // A path is checked, not searched.
        let full = dir.path().join(&exe);
        assert_eq!(
            find_program(full.to_str().unwrap(), None),
            Some(full.clone())
        );
        assert_eq!(verdict_shell(full.to_str(), None), Some(full));
        let other = std::env::join_paths([dir.path().join("elsewhere")]).unwrap();
        assert_eq!(verdict_shell(Some("tool"), Some(&other)), None);
    }

    #[cfg(windows)]
    #[test]
    fn the_windows_shell_comes_from_the_git_next_to_git_exe() {
        let root = tempfile::tempdir().unwrap();
        let cmd = root.path().join("cmd");
        let bin = root.path().join("bin");
        std::fs::create_dir_all(&cmd).unwrap();
        std::fs::create_dir_all(&bin).unwrap();
        std::fs::write(cmd.join("git.exe"), "").unwrap();
        std::fs::write(bin.join("sh.exe"), "").unwrap();
        let path = std::env::join_paths([&cmd]).unwrap();
        assert_eq!(verdict_shell(None, Some(&path)), Some(bin.join("sh.exe")));
        // An npm shim counts; an executable in the same directory wins over it.
        std::fs::write(cmd.join("agent.cmd"), "").unwrap();
        assert_eq!(
            find_program("agent", Some(&path)),
            Some(cmd.join("agent.cmd"))
        );
        std::fs::write(cmd.join("agent.exe"), "").unwrap();
        assert_eq!(
            find_program("agent", Some(&path)),
            Some(cmd.join("agent.exe"))
        );
    }
}
