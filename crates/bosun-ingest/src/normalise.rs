//! Normalised tool targets: canonical absolute paths, command heads and command fingerprints
//! (#3, `tool_call`; #210).

use std::fmt::Write as _;

use bosun_core::{PathStyle, canonical_path};
use sha2::{Digest as _, Sha256};

use crate::shell::Dialect;

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

/// The canonical absolute form of `path` as a shell command named it in `cwd`: as
/// [`absolute`], and with a Git Bash drive path (`/c/fixture/x.rs`) read as the Windows path
/// it names (`c:\fixture\x.rs`) when `cwd` is a Windows path.
#[must_use]
pub(crate) fn shell_absolute(path: &str, cwd: &str) -> String {
    let bytes = path.as_bytes();
    let drive = bytes.len() >= 2
        && bytes[0] == b'/'
        && bytes[1].is_ascii_alphabetic()
        && (bytes.len() == 2 || bytes[2] == b'/');
    if drive && style_of(cwd) == PathStyle::Windows {
        let letter = char::from(bytes[1]);
        absolute(
            &format!("{letter}:/{}", path.get(3..).unwrap_or_default()),
            cwd,
        )
    } else {
        absolute(path, cwd)
    }
}

/// The head of a shell command: its first word, without leading variable assignments, quotes
/// or directories. It is the call's target; the arguments, which may hold secrets, reach the
/// database only through [`command_fingerprint`], redacted, and only for a message attributed
/// to a repository.
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

/// The most bytes of a command's text that are kept (#210).
pub(crate) const COMMAND_TEXT_MAX: usize = 200;

/// The fingerprint of a shell command and its kept text.
///
/// The command is normalised (trimmed; a line continuation, `\` then a newline in a POSIX
/// shell or a backtick then a newline in PowerShell, is whitespace; every run of whitespace is
/// one space) and scrubbed ([`bosun_core::publish::scrub`]). The hash is the lowercase hex
/// SHA-256 (64 digits) of that text, so that two spellings of one command share it; the text is
/// the same text cut to at most [`COMMAND_TEXT_MAX`] bytes at a char boundary. The cut comes
/// after scrubbing: a secret cut in half no longer has the shape the redaction knows.
///
/// In PowerShell a `\` before a newline is the last character of a path, not a continuation,
/// and stays.
#[must_use]
pub(crate) fn command_fingerprint(command: &str, dialect: Dialect) -> (String, String) {
    let normalised = normalise_command(command, dialect);
    let scrubbed = if cfg!(mutant_ingest_command_text_unredacted) {
        // Mutant `mutant_ingest_command_text_unredacted`: the text is kept as it was written.
        normalised
    } else {
        // Removing an invisible character may join two runs of whitespace: normalise again.
        collapse(&bosun_core::publish::scrub(&normalised))
    };
    let hashed = if cfg!(mutant_ingest_command_hash_of_head) {
        // Mutant `mutant_ingest_command_hash_of_head`: the hash is of the first word only.
        command_head(command).unwrap_or_default()
    } else {
        scrubbed.clone()
    };
    let hash = Sha256::digest(hashed.as_bytes()).iter().fold(
        String::with_capacity(64),
        |mut hex, byte| {
            let _ = write!(hex, "{byte:02x}");
            hex
        },
    );
    let mut end = scrubbed.len().min(COMMAND_TEXT_MAX);
    while !scrubbed.is_char_boundary(end) {
        end -= 1;
    }
    (hash, scrubbed[..end].to_owned())
}

/// `command` with its line continuations as spaces and its whitespace collapsed.
fn normalise_command(command: &str, dialect: Dialect) -> String {
    let escape = match dialect {
        Dialect::Posix => '\\',
        Dialect::PowerShell => '`',
    };
    let joined = command
        .replace(&format!("{escape}\r\n"), " ")
        .replace(&format!("{escape}\n"), " ");
    collapse(&joined)
}

/// `text` trimmed, with every run of whitespace one space.
fn collapse(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
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
    fn a_git_bash_drive_path_is_a_windows_path() {
        assert_eq!(
            shell_absolute("/c/Fixture/repo-a/x.rs", r"C:\fixture"),
            r"c:\fixture\repo-a\x.rs"
        );
        assert_eq!(
            shell_absolute("src/x.rs", r"C:\fixture\repo-a"),
            r"c:\fixture\repo-a\src\x.rs"
        );
        // On a Unix machine `/c/...` is a Unix path.
        assert_eq!(
            shell_absolute("/c/fixture/x.rs", "/fixture"),
            "/c/fixture/x.rs"
        );
        assert_eq!(
            shell_absolute("/cfixture/x.rs", r"C:\fixture"),
            r"c:\cfixture\x.rs"
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
