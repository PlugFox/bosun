//! Reading the TOML document into a draft: the stage-0 keys with their positions, the notices
//! for what a later stage reads, and the problems of shape (wrong type, missing key).

use std::ops::Range;
use std::path::PathBuf;
use std::time::Duration;

use bosun_core::{PathStyle, canonical_path};
use toml_edit::{Document, Item, TableLike};

use crate::error::{Location, Notice, Problem};

/// Sections of #4 that this version accepts and does not read.
const LATER_SECTIONS: &[&str] = &["budget", "notify"];

/// The default of `retention.reingest_window`: Claude Code's own default retention
/// (`cleanupPeriodDays`, 30 days), so `bosun doctor` is quiet on a default setup.
pub(crate) const DEFAULT_REINGEST_WINDOW: Duration = Duration::from_hours(30 * 24);

/// A path value: as written, its canonical key (#17) and where it is.
#[derive(Debug)]
pub(crate) struct PathValue {
    pub(crate) path: PathBuf,
    pub(crate) key: String,
    pub(crate) at: Option<Location>,
}

#[derive(Debug)]
pub(crate) struct DraftIdentity {
    pub(crate) name: String,
    pub(crate) claude_config_dir: Option<PathValue>,
    pub(crate) database: Option<PathValue>,
}

#[derive(Debug)]
pub(crate) struct DraftRepo {
    pub(crate) name: String,
    pub(crate) path: Option<PathValue>,
    pub(crate) identity: Option<(String, Option<Location>)>,
}

/// What the document says, in file order, before cross-checks.
#[derive(Debug)]
pub(crate) struct Draft {
    pub(crate) identities: Vec<DraftIdentity>,
    pub(crate) repos: Vec<DraftRepo>,
    pub(crate) reingest_window: Duration,
    pub(crate) notices: Vec<Notice>,
}

struct Reader<'t> {
    text: &'t str,
    problems: Vec<Problem>,
    notices: Vec<Notice>,
}

/// Reads `text`. Problems are appended to `problems`; a malformed document yields `None`.
pub(crate) fn read(text: &str, problems: &mut Vec<Problem>) -> Option<Draft> {
    let document = match Document::parse(text) {
        Ok(document) => document,
        Err(e) => {
            problems.push(Problem {
                location: e.span().map(|span| Location::of(text, span.start)),
                message: e.message().trim().to_owned(),
            });
            return None;
        }
    };
    let mut reader = Reader {
        text,
        problems: Vec::new(),
        notices: Vec::new(),
    };
    let mut draft = Draft {
        identities: Vec::new(),
        repos: Vec::new(),
        reingest_window: DEFAULT_REINGEST_WINDOW,
        notices: Vec::new(),
    };

    let root = document.as_table();
    for (name, item) in root {
        let at = reader.key_at(root, name, item);
        match name {
            "daemon" => {
                // Stage 0 reads no key of `[daemon]`; the section is valid and every key in it
                // belongs to a later stage.
                if let Some(table) = reader.table(item, "daemon", at) {
                    reader.ignore_all(table, "daemon");
                }
            }
            "identity" => {
                for (name, item, at) in reader.named_tables(item, "identity", at) {
                    draft.identities.push(reader.identity(name, item, at));
                }
            }
            "repo" => {
                for (name, item, at) in reader.named_tables(item, "repo", at) {
                    draft.repos.push(reader.repo(name, item, at));
                }
            }
            "retention" => {
                if let Some(window) = reader.retention(item, at) {
                    draft.reingest_window = window;
                }
            }
            later if LATER_SECTIONS.contains(&later) => reader.notice(
                at,
                format!("section `{later}` is not read in this version; ignored"),
            ),
            unknown => reader.problem(
                at,
                format!(
                    "unknown section `{unknown}`; the sections are daemon, identity, repo, \
                     retention, budget and notify"
                ),
            ),
        }
    }

    problems.append(&mut reader.problems);
    draft.notices = reader.notices;
    Some(draft)
}

impl Reader<'_> {
    fn at(&self, span: Option<Range<usize>>) -> Option<Location> {
        span.map(|span| Location::of(self.text, span.start))
    }

    /// Where the key `name` of `table` is written, or its value when the key has no span.
    fn key_at(&self, table: &dyn TableLike, name: &str, item: &Item) -> Option<Location> {
        let span = table
            .get_key_value(name)
            .and_then(|(key, _)| key.span())
            .or_else(|| item.span());
        self.at(span)
    }

    fn problem(&mut self, location: Option<Location>, message: String) {
        self.problems.push(Problem { location, message });
    }

    fn notice(&mut self, location: Option<Location>, message: String) {
        self.notices.push(Notice { location, message });
    }

    fn table<'d>(
        &mut self,
        item: &'d Item,
        what: &str,
        at: Option<Location>,
    ) -> Option<&'d dyn TableLike> {
        let table = item.as_table_like();
        if table.is_none() {
            self.problem(at, format!("`{what}` must be a table"));
        }
        table
    }

    /// The tables `<section>.<name>`, each with its name and position.
    fn named_tables<'d>(
        &mut self,
        item: &'d Item,
        section: &str,
        at: Option<Location>,
    ) -> Vec<(&'d str, &'d Item, Option<Location>)> {
        let Some(table) = self.table(item, section, at) else {
            return Vec::new();
        };
        let mut out = Vec::new();
        for (name, item) in table.iter() {
            let at = self.key_at(table, name, item);
            let path = format!("{section}.{name}");
            if !valid_name(name) {
                self.problem(
                    at,
                    format!(
                        "`{path}`: a name is ASCII letters, digits, `-` and `_`, and is not empty"
                    ),
                );
            } else if item.as_table_like().is_none() {
                self.problem(at, format!("`{path}` must be a table"));
            } else {
                out.push((name, item, at));
            }
        }
        out
    }

    fn ignore_all(&mut self, table: &dyn TableLike, section: &str) {
        for (key, item) in table.iter() {
            let at = self.key_at(table, key, item);
            self.notice(
                at,
                format!("`{section}.{key}` is not read in this version; ignored"),
            );
        }
    }

    fn identity(&mut self, name: &str, item: &Item, at: Option<Location>) -> DraftIdentity {
        let path = format!("identity.{name}");
        let mut identity = DraftIdentity {
            name: name.to_owned(),
            claude_config_dir: None,
            database: None,
        };
        let Some(table) = item.as_table_like() else {
            return identity;
        };
        for (key, item) in table.iter() {
            let key_at = self.key_at(table, key, item);
            match key {
                "claude_config_dir" => {
                    identity.claude_config_dir = self.path(item, &format!("{path}.{key}"));
                }
                "database" => identity.database = self.path(item, &format!("{path}.{key}")),
                other => self.notice(
                    key_at,
                    format!("`{path}.{other}` is not read in this version; ignored"),
                ),
            }
        }
        for (key, value) in [
            ("claude_config_dir", identity.claude_config_dir.is_some()),
            ("database", identity.database.is_some()),
        ] {
            if !value && table.get(key).is_none() {
                self.problem(at, format!("`{path}` has no `{key}`"));
            }
        }
        identity
    }

    fn repo(&mut self, name: &str, item: &Item, at: Option<Location>) -> DraftRepo {
        let path = format!("repo.{name}");
        let mut repo = DraftRepo {
            name: name.to_owned(),
            path: None,
            identity: None,
        };
        let Some(table) = item.as_table_like() else {
            return repo;
        };
        for (key, item) in table.iter() {
            let key_at = self.key_at(table, key, item);
            match key {
                "path" => repo.path = self.path(item, &format!("{path}.path")),
                "identity" => {
                    repo.identity = self
                        .string(item, &format!("{path}.identity"))
                        .map(|(value, at)| (value.to_owned(), at));
                }
                other => self.notice(
                    key_at,
                    format!("`{path}.{other}` is not read in this version; ignored"),
                ),
            }
        }
        for key in ["path", "identity"] {
            if table.get(key).is_none() {
                self.problem(at, format!("`{path}` has no `{key}`"));
            }
        }
        repo
    }

    fn retention(&mut self, item: &Item, at: Option<Location>) -> Option<Duration> {
        let table = self.table(item, "retention", at)?;
        let mut window = None;
        for (key, item) in table.iter() {
            let key_at = self.key_at(table, key, item);
            if key == "reingest_window" {
                let (text, at) = self.string(item, "retention.reingest_window")?;
                window = parse_window(text);
                if window.is_none() {
                    self.problem(
                        at,
                        format!(
                            "`retention.reingest_window` is {text:?}; expected a positive \
                             whole number of hours, days or weeks, such as \"30d\""
                        ),
                    );
                }
            } else {
                self.notice(
                    key_at,
                    format!("`retention.{key}` is not read in this version; ignored"),
                );
            }
        }
        window
    }

    fn string<'d>(&mut self, item: &'d Item, what: &str) -> Option<(&'d str, Option<Location>)> {
        let at = self.at(item.span());
        let value = item.as_str();
        if value.is_none() {
            self.problem(at, format!("`{what}` must be a string"));
        }
        value.map(|value| (value, at))
    }

    fn path(&mut self, item: &Item, what: &str) -> Option<PathValue> {
        let (text, at) = self.string(item, what)?;
        let path = PathBuf::from(text);
        if !path.is_absolute() {
            self.problem(
                at,
                format!("`{what}` is {text:?}; a path in the configuration must be absolute"),
            );
            return None;
        }
        Some(PathValue {
            key: canonical_path(text, PathStyle::host()),
            path,
            at,
        })
    }
}

fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

/// Parses `<n>h`, `<n>d` or `<n>w` with `n > 0`.
fn parse_window(text: &str) -> Option<Duration> {
    let hours = match text.chars().last()? {
        'h' => 1,
        'd' => 24,
        'w' => 7 * 24,
        _ => return None,
    };
    let count = &text[..text.len() - 1];
    if count.is_empty() || !count.chars().all(|c| c.is_ascii_digit()) {
        return None;
    }
    let count: u64 = count.parse().ok().filter(|&n| n > 0)?;
    // `Duration::from_hours` panics on overflow; the seconds are checked here instead.
    let seconds = count.checked_mul(hours)?.checked_mul(60 * 60)?;
    Some(Duration::from_secs(seconds))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reingest_windows() {
        assert_eq!(parse_window("30d"), Some(Duration::from_hours(30 * 24)));
        assert_eq!(parse_window("12h"), Some(Duration::from_hours(12)));
        assert_eq!(parse_window("2w"), Some(Duration::from_hours(14 * 24)));
        for bad in [
            "",
            "d",
            "0d",
            "-1d",
            "+1d",
            "1.5d",
            "30",
            "30m",
            "30 d",
            "99999999999999999999d",
        ] {
            assert_eq!(parse_window(bad), None, "{bad:?}");
        }
    }

    #[test]
    fn names() {
        assert!(valid_name("work-2_x"));
        assert!(!valid_name(""));
        assert!(!valid_name("a b"));
        assert!(!valid_name("рабочий"));
    }
}
