//! Normalised tool targets: canonical absolute paths and command heads (#3, `tool_call`).

use bosun_core::{PathStyle, canonical_path};

pub use bosun_core::style_of;

fn has_drive(path: &str) -> bool {
    let bytes = path.as_bytes();
    bytes.len() >= 2 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':'
}

/// The canonical absolute form of `path` as a tool named it in working directory `cwd`: a
/// relative path is joined to `cwd`, a rooted path without a drive takes the drive of a
/// Windows `cwd`, and the result is canonicalised by the rules of the platform it implies.
#[must_use]
pub fn absolute(path: &str, cwd: &str) -> String {
    let rooted = path.starts_with(['/', '\\']);
    let joined = if has_drive(path) || path.starts_with(r"\\") {
        path.to_owned()
    } else if rooted && has_drive(cwd) {
        format!("{}{path}", &cwd[..2])
    } else if rooted || cwd.is_empty() {
        path.to_owned()
    } else {
        let separator = match style_of(cwd) {
            PathStyle::Windows => '\\',
            PathStyle::Unix => '/',
        };
        format!("{cwd}{separator}{path}")
    };
    canonical_path(&joined, style_of(&joined))
}

/// The head of a shell command: its first word, without leading variable assignments, quotes
/// or directories. The rest of the command is dropped, so that arguments, which may hold
/// secrets, never reach the database.
#[must_use]
pub fn command_head(command: &str) -> Option<String> {
    let word = command
        .split_whitespace()
        .filter(|word| !is_assignment(word))
        .map(|word| word.trim_matches(|c: char| "\"'`()[]{}&;|<>".contains(c)))
        .find(|word| !word.is_empty())?;
    let base = word.rsplit(['/', '\\']).next().unwrap_or(word);
    (!base.is_empty() && !base.contains('=')).then(|| base.to_owned())
}

/// `NAME=value`, as in `NAME=value command`.
fn is_assignment(word: &str) -> bool {
    word.split_once('=').is_some_and(|(name, _)| {
        name.starts_with(|c: char| c.is_ascii_alphabetic() || c == '_')
            && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn styles_follow_the_text() {
        assert_eq!(style_of(r"C:\fixture"), PathStyle::Windows);
        assert_eq!(style_of(r"\\server\share"), PathStyle::Windows);
        assert_eq!(style_of("/fixture"), PathStyle::Unix);
        assert_eq!(style_of("src/lib.rs"), PathStyle::Unix);
    }

    #[test]
    fn paths_become_canonical_and_absolute() {
        assert_eq!(
            absolute("/fixture/repo-a/./src//lib.rs", "/elsewhere"),
            "/fixture/repo-a/src/lib.rs"
        );
        assert_eq!(
            absolute("src/lib.rs", "/fixture/repo-a"),
            "/fixture/repo-a/src/lib.rs"
        );
        assert_eq!(
            absolute(r"src\Lib.rs", r"C:\Fixture\Repo-A"),
            r"c:\fixture\repo-a\src\lib.rs"
        );
        assert_eq!(
            absolute("C:/Fixture/repo-a/x.rs", "/fixture"),
            r"c:\fixture\repo-a\x.rs"
        );
        assert_eq!(
            absolute(r"\fixture\x.rs", r"C:\fixture"),
            r"c:\fixture\x.rs"
        );
    }

    #[test]
    fn a_command_keeps_its_first_word_only() {
        let secret = "s3cr3t-value";
        let cases = [
            ("cargo test --all".to_owned(), Some("cargo")),
            (format!("API_TOKEN={secret} gh pr list"), Some("gh")),
            (
                format!("/usr/bin/git push https://{secret}@host"),
                Some("git"),
            ),
            (
                format!("& \"C:\\tools\\run.exe\" {secret}"),
                Some("run.exe"),
            ),
            (format!("(cd dir && make {secret})"), Some("cd")),
            ("   ".to_owned(), None),
        ];
        for (command, head) in cases {
            let got = command_head(&command);
            assert_eq!(got.as_deref(), head, "{command}");
            assert!(!got.unwrap_or_default().contains(secret));
        }
    }
}
