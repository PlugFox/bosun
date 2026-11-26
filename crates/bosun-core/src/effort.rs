//! The effort levels of the Claude runner (#7, #9): what a card's `effort` key and
//! `repo.<name>.effort` may name, and what `bosun-runner` passes as `--effort <level>`.

/// The effort levels the Claude CLI accepts, from `claude --help` (Claude Code 2.1.x):
/// `--effort <level>  Effort level for the current session (low, medium, high, xhigh, max)`.
pub const EFFORT_LEVELS: [&str; 5] = ["low", "medium", "high", "xhigh", "max"];

/// Whether `word` is one of [`EFFORT_LEVELS`], exactly (case-sensitive, as the CLI takes it).
#[must_use]
pub fn is_effort_level(word: &str) -> bool {
    EFFORT_LEVELS.contains(&word)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_the_listed_levels_are_levels() {
        for level in EFFORT_LEVELS {
            assert!(is_effort_level(level));
        }
        for word in ["", "ultra", "High", " low", "medium "] {
            assert!(!is_effort_level(word), "{word:?}");
        }
    }
}
