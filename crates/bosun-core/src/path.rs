//! Path canonicalisation as a pure function of the text and the platform's rules.
//!
//! The platform is a parameter, not the host, so Windows paths are canonicalised (and tested)
//! the same way on every host. Nothing here touches the file system: symbolic links and
//! junctions are not resolved, and a path is not required to exist.

/// Which platform's path rules apply.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PathStyle {
    /// Case-insensitive; `\` and `/` are both separators; `\\?\` and `\\?\UNC\` prefixes are
    /// dropped; `.` and `..` are resolved lexically, as Win32 does.
    Windows,
    /// Case-sensitive; only `/` separates; `..` is kept, because through a symbolic link it is
    /// not the lexical parent.
    Unix,
}

impl PathStyle {
    /// The rules of the platform this code is compiled for.
    #[must_use]
    pub fn host() -> Self {
        if cfg!(windows) {
            Self::Windows
        } else {
            Self::Unix
        }
    }
}

/// Returns the canonical key of `path` under `style`: two paths that name the same location
/// by the platform's rules (case, separators, prefixes, repeated or trailing separators, `.`)
/// map to the same key.
///
/// Windows keys use `\`, are lower case, and keep the root (`c:\`, `\\server\share\`). Unix
/// keys keep the case and the root `/`.
#[must_use]
pub fn canonical_path(path: &str, style: PathStyle) -> String {
    match style {
        PathStyle::Windows => windows(path),
        PathStyle::Unix => unix(path),
    }
}

fn windows(path: &str) -> String {
    let path = path.replace('/', "\\");
    let path = if cfg!(mutant_core_case_sensitive) {
        path
    } else {
        path.to_lowercase()
    };

    // The verbatim prefixes `\\?\` and `\\?\UNC\`, and the device prefix `\\.\`.
    let (root, rest) = if let Some(unc) = path.strip_prefix("\\\\?\\unc\\") {
        unc_root(unc)
    } else if let Some(rest) = path
        .strip_prefix("\\\\?\\")
        .or_else(|| path.strip_prefix("\\\\.\\"))
    {
        drive_root(rest)
    } else if let Some(unc) = path.strip_prefix("\\\\") {
        unc_root(unc)
    } else {
        drive_root(&path)
    };

    let mut parts: Vec<&str> = Vec::new();
    for part in rest.split('\\') {
        match part {
            "" | "." => {}
            ".." => {
                parts.pop();
            }
            part => parts.push(part),
        }
    }
    let mut out = root;
    out.push_str(&parts.join("\\"));
    out
}

/// Splits `\\server\share\rest` (with the leading `\\` already removed) into the root
/// `\\server\share\` and the rest.
fn unc_root(unc: &str) -> (String, &str) {
    let mut pieces = unc.splitn(3, '\\');
    let server = pieces.next().unwrap_or_default();
    let share = pieces.next().unwrap_or_default();
    let rest = pieces.next().unwrap_or_default();
    (format!("\\\\{server}\\{share}\\"), rest)
}

/// Splits `c:\rest` into the root `c:\` and the rest. A path without a drive is rooted at `\`
/// when it starts with a separator, and relative otherwise.
fn drive_root(path: &str) -> (String, &str) {
    let mut chars = path.chars();
    if let (Some(letter), Some(':')) = (chars.next(), chars.next())
        && letter.is_ascii_alphabetic()
    {
        return (format!("{letter}:\\"), &path[2..]);
    }
    if path.starts_with('\\') {
        ("\\".to_owned(), path)
    } else {
        (String::new(), path)
    }
}

/// The platform a path's text implies: Windows for a drive letter (`C:`) or a `\\` prefix,
/// Unix otherwise. Transcripts from one machine can be read on another, so the host does not
/// decide.
#[must_use]
pub fn style_of(path: &str) -> PathStyle {
    if has_drive(path) || path.starts_with(r"\\") {
        PathStyle::Windows
    } else {
        PathStyle::Unix
    }
}

pub(crate) fn has_drive(path: &str) -> bool {
    let bytes = path.as_bytes();
    bytes.len() >= 2 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':'
}

/// What follows `root` in `path` when `path` is `root` or lies under it on whole components:
/// `""` for the root itself, the rest without its leading separator otherwise. Both are
/// canonical keys ([`canonical_path`]) under `style`.
#[must_use]
pub fn strip_root<'a>(path: &'a str, root: &str, style: PathStyle) -> Option<&'a str> {
    let separator = match style {
        PathStyle::Windows => '\\',
        PathStyle::Unix => '/',
    };
    let rest = path.strip_prefix(root)?;
    if rest.is_empty() || root.ends_with(separator) {
        Some(rest)
    } else {
        rest.strip_prefix(separator)
    }
}

/// The repository root a working directory belongs to: the longest of `roots` that is `cwd`
/// or an ancestor of it on whole path components. Each path is canonicalised by the platform
/// its own text implies ([`style_of`]), and a root of one style never matches a path of the
/// other.
#[must_use]
pub fn repo_root<'a>(cwd: &str, roots: impl IntoIterator<Item = &'a str>) -> Option<&'a str> {
    let style = style_of(cwd);
    let cwd = canonical_path(cwd, style);
    roots
        .into_iter()
        .filter(|root| style_of(root) == style)
        .map(|root| (root, canonical_path(root, style)))
        .filter(|(_, key)| strip_root(&cwd, key, style).is_some())
        .max_by_key(|(_, key)| key.len())
        .map(|(root, _)| root)
}

fn unix(path: &str) -> String {
    let parts: Vec<&str> = path
        .split('/')
        .filter(|part| !part.is_empty() && *part != ".")
        .collect();
    let root = if path.starts_with('/') { "/" } else { "" };
    format!("{root}{}", parts.join("/"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn windows_paths_differing_by_case_or_separator_share_a_key() {
        let same = [
            r"C:\git\Bosun",
            r"c:\git\bosun",
            "C:/git/bosun",
            r"C:\GIT\BOSUN\",
            r"C:\git\\bosun\.",
            r"\\?\C:\git\bosun",
            r"c:\git\other\..\bosun",
            "c:/Git//Bosun/",
        ];
        for path in same {
            assert_eq!(
                canonical_path(path, PathStyle::Windows),
                r"c:\git\bosun",
                "{path}"
            );
        }
        assert_ne!(
            canonical_path(r"C:\git\bosun", PathStyle::Windows),
            canonical_path(r"D:\git\bosun", PathStyle::Windows)
        );
    }

    #[test]
    fn windows_roots_and_shares() {
        let w = |p| canonical_path(p, PathStyle::Windows);
        assert_eq!(w(r"C:\"), r"c:\");
        assert_eq!(w("C:"), r"c:\");
        assert_eq!(w(r"\\Server\Share\Dir\"), r"\\server\share\dir");
        assert_eq!(w(r"\\?\UNC\Server\Share\Dir"), r"\\server\share\dir");
        assert_eq!(w(r"\\Server\Share"), r"\\server\share\");
        assert_eq!(w(r"src\Lib.rs"), r"src\lib.rs");
    }

    #[test]
    fn a_cwd_belongs_to_the_longest_root_on_whole_components() {
        let roots = [
            r"C:\Fixture\repo-a",
            r"C:\fixture\repo-a\nested",
            r"C:\fixture\repo-b",
            "/fixture/repo-a",
        ];
        let root = |cwd| repo_root(cwd, roots.iter().copied());
        assert_eq!(root(r"c:\fixture\REPO-A\src"), Some(roots[0]));
        assert_eq!(root(r"C:\fixture\repo-a\nested\x"), Some(roots[1]));
        assert_eq!(root(r"C:\fixture\repo-ab"), None);
        assert_eq!(root("C:/fixture/repo-b"), Some(roots[2]));
        assert_eq!(root("/fixture/repo-a/src"), Some(roots[3]));
        assert_eq!(root("/fixture/repo-ab"), None);
        assert_eq!(root("/fixture/Repo-a"), None);

        let w = PathStyle::Windows;
        assert_eq!(strip_root(r"c:\a\b", r"c:\a", w), Some("b"));
        assert_eq!(strip_root(r"c:\a", r"c:\a", w), Some(""));
        assert_eq!(strip_root(r"c:\a\b", r"c:\", w), Some(r"a\b"));
        assert_eq!(strip_root(r"c:\ab", r"c:\a", w), None);
        assert_eq!(strip_root("/a/b", "/", PathStyle::Unix), Some("a/b"));
    }

    #[test]
    fn unix_paths_keep_case_and_dotdot() {
        let u = |p| canonical_path(p, PathStyle::Unix);
        assert_eq!(u("/home/me/Repo/"), "/home/me/Repo");
        assert_eq!(u("/home//me/./Repo"), "/home/me/Repo");
        assert_ne!(u("/home/me/Repo"), u("/home/me/repo"));
        assert_eq!(u("/a/../b"), "/a/../b");
        assert_eq!(u("/"), "/");
        assert_eq!(u(r"C:\x"), r"C:\x");
    }
}
