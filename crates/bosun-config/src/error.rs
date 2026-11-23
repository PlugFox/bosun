//! Errors, notices and their positions in the file.

use std::fmt;
use std::path::PathBuf;

/// A position in the configuration file: one-based line and column, the column counted in
/// characters.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Location {
    /// One-based line.
    pub line: usize,
    /// One-based column, in characters.
    pub column: usize,
}

impl Location {
    /// The location of byte `offset` in `text`. An offset past the end, or inside a character,
    /// is clamped back to the nearest character boundary.
    pub(crate) fn of(text: &str, offset: usize) -> Self {
        let mut offset = offset.min(text.len());
        while !text.is_char_boundary(offset) {
            offset -= 1;
        }
        let before = &text[..offset];
        let line_start = before.rfind('\n').map_or(0, |at| at + 1);
        Self {
            line: before.matches('\n').count() + 1,
            column: before[line_start..].chars().count() + 1,
        }
    }
}

impl fmt::Display for Location {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "line {}, column {}", self.line, self.column)
    }
}

/// Something in the file that makes it unusable.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Problem {
    /// Where the problem is, when it has a place in the file.
    pub location: Option<Location>,
    /// What is wrong, naming every key or identity involved.
    pub message: String,
}

impl fmt::Display for Problem {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.location {
            Some(at) => write!(f, "{at}: {}", self.message),
            None => f.write_str(&self.message),
        }
    }
}

/// Something in the file that this version accepts and does not read: a section or a key of
/// #4 that a later stage implements.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Notice {
    /// Where the ignored section or key is.
    pub location: Option<Location>,
    /// What is ignored.
    pub message: String,
}

impl fmt::Display for Notice {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.location {
            Some(at) => write!(f, "{at}: {}", self.message),
            None => f.write_str(&self.message),
        }
    }
}

/// Why a configuration could not be loaded.
#[derive(Debug)]
pub enum Error {
    /// The file could not be read.
    Read {
        /// The file.
        path: PathBuf,
        /// The cause.
        source: std::io::Error,
    },
    /// The file is malformed TOML or fails validation. Every problem found is listed, in file
    /// order where they have a location.
    Invalid(Vec<Problem>),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Read { path, source } => write!(f, "cannot read {}: {source}", path.display()),
            Self::Invalid(problems) => {
                f.write_str("invalid configuration")?;
                for problem in problems {
                    write!(f, "\n  {problem}")?;
                }
                Ok(())
            }
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Read { source, .. } => Some(source),
            Self::Invalid(_) => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::Location;

    #[test]
    fn locations_are_one_based_and_count_characters() {
        let text = "a = 1\nкей = 2\n";
        assert_eq!(Location::of(text, 0), Location { line: 1, column: 1 });
        assert_eq!(Location::of(text, 4), Location { line: 1, column: 5 });
        let eq = text.rfind('=').unwrap();
        assert_eq!(Location::of(text, eq), Location { line: 2, column: 5 });
        assert_eq!(Location::of(text, 1000), Location { line: 3, column: 1 });
        // Inside a two-byte character: clamped to its start.
        assert_eq!(Location::of(text, 7), Location { line: 2, column: 1 });
    }
}
