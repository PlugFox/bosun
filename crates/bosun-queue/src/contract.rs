//! The item contract (#5): the fenced block in a card's body that the daemon parses. The rest of
//! the body is prose for the agent.
//!
//! The block is a fenced code block whose info string is `bosun`, holding TOML:
//!
//! ````markdown
//! ```bosun
//! verdict = "cargo test -p frob 2>&1 | tail -n 1"   # required; run by `sh -c`, exit 0 = done
//! verdict-timeout = "10m"            # "90s", "10m", "2h", "1h30m", or whole seconds
//! needs = ["phone"]                  # resources to lease (#8)
//! blocked-by = ["#12", 14]           # items that must land first
//! budget = { eqv = "2M", wall = "2h" } # eqv tokens (a number, or "500k" / "2M"), wall-clock
//! runner = "claude"                  # override of the repository's default
//! model = "opus"
//! allow-protected = ["tests/harness.rs"] # protected paths this item may change
//! ```
//! ````
//!
//! A fence of three or more backticks or tildes opens the block and a fence of the same
//! character, at least as long, closes it, as in `CommonMark`; fences inside a longer fence are
//! content. Exactly one `bosun` block per body. An unknown key is an error, so a misspelt field
//! is never silently ignored.

use std::fmt;
use std::ops::Range;
use std::time::Duration;

use toml_edit::{Document, Item, Value};

use crate::card::ItemId;

/// The info string that marks the contract block.
pub const INFO: &str = "bosun";

/// The parsed contract block.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Contract {
    /// The verdict command, run by the daemon through the verdict shell; exit 0 means done.
    pub verdict: String,
    /// The verdict's timeout; `None`: the repository's default.
    pub verdict_timeout: Option<Duration>,
    /// Resources to lease (#8), as named in the configuration.
    pub needs: Vec<String>,
    /// Items that must land before this one.
    pub blocked_by: Vec<ItemId>,
    /// The item's caps (#10).
    pub budget: Budget,
    /// The runner, overriding the repository's default within the identity.
    pub runner: Option<String>,
    /// The model, overriding the repository's default.
    pub model: Option<String>,
    /// Protected paths (#7) this item may change.
    pub allow_protected: Vec<String>,
}

/// The caps of one item; `None`: the repository's default.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Budget {
    /// Input-equivalent tokens.
    pub eqv: Option<u64>,
    /// Wall-clock time.
    pub wall: Option<Duration>,
}

/// Where the contract block is in a body.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Block<'a> {
    /// The TOML between the fences.
    pub content: &'a str,
    /// The byte range of the whole block, fences included.
    pub range: Range<usize>,
}

/// Why a body has no valid contract. Each names the field involved.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ContractError {
    /// No fenced block with the info string `bosun`.
    NoBlock,
    /// More than one such block; the count.
    SeveralBlocks(usize),
    /// The block is opened and never closed.
    Unclosed,
    /// The block is not TOML; the parser's message.
    Toml(String),
    /// The block has no `verdict`.
    MissingVerdict,
    /// A key that is not a field of the contract.
    UnknownKey(String),
    /// A field with a value of the wrong shape.
    Field {
        /// The field.
        key: String,
        /// What is wrong with it.
        problem: String,
    },
}

impl fmt::Display for ContractError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NoBlock => write!(f, "the body has no ```{INFO} block"),
            Self::SeveralBlocks(n) => {
                write!(f, "the body has {n} ```{INFO} blocks; one is allowed")
            }
            Self::Unclosed => write!(f, "the ```{INFO} block is never closed"),
            Self::Toml(message) => write!(f, "the ```{INFO} block is not TOML: {message}"),
            Self::MissingVerdict => f.write_str("the block has no `verdict`"),
            Self::UnknownKey(key) => write!(
                f,
                "unknown key `{key}`; the fields are verdict, verdict-timeout, needs, \
                 blocked-by, budget, runner, model and allow-protected"
            ),
            Self::Field { key, problem } => write!(f, "`{key}`: {problem}"),
        }
    }
}

impl std::error::Error for ContractError {}

/// A fence-opening or -closing line: its character, its length and what follows it.
fn fence(line: &str) -> Option<(char, usize, &str)> {
    let indent = line.len() - line.trim_start_matches(' ').len();
    if indent > 3 {
        return None;
    }
    let rest = &line[indent..];
    let ch = rest.chars().next().filter(|c| *c == '`' || *c == '~')?;
    let len = rest.len() - rest.trim_start_matches(ch).len();
    if len < 3 {
        return None;
    }
    let info = &rest[len..];
    // A backtick fence's info string may not contain a backtick (CommonMark).
    if ch == '`' && info.contains('`') {
        return None;
    }
    Some((ch, len, info))
}

/// Finds the contract block in `body`.
///
/// # Errors
///
/// [`ContractError::NoBlock`], [`ContractError::SeveralBlocks`] or
/// [`ContractError::Unclosed`].
pub fn find_block(body: &str) -> Result<Block<'_>, ContractError> {
    let mut blocks = Vec::new();
    // The open fence: its character, length, whether it is ours, where it starts and where
    // its content starts.
    let mut open: Option<(char, usize, bool, usize, usize)> = None;
    let mut at = 0;
    for line in body.split_inclusive('\n') {
        let start = at;
        at += line.len();
        let text = line.trim_end_matches(['\n', '\r']);
        match open {
            None => {
                if let Some((ch, len, info)) = fence(text) {
                    let ours = info.split_whitespace().next() == Some(INFO);
                    open = Some((ch, len, ours, start, at));
                }
            }
            Some((ch, len, ours, block_start, content_start)) => {
                if let Some((c, l, info)) = fence(text)
                    && c == ch
                    && l >= len
                    && info.trim().is_empty()
                {
                    if ours {
                        blocks.push(Block {
                            content: &body[content_start..start],
                            range: block_start..at,
                        });
                    }
                    open = None;
                }
            }
        }
    }
    if let Some((_, _, true, _, _)) = open {
        return Err(ContractError::Unclosed);
    }
    match blocks.len() {
        0 => Err(ContractError::NoBlock),
        1 => Ok(blocks.remove(0)),
        n => Err(ContractError::SeveralBlocks(n)),
    }
}

impl Contract {
    /// Parses the contract block of `body`, reporting every problem at once.
    ///
    /// # Errors
    ///
    /// Every [`ContractError`] found, in the block's key order.
    pub fn parse(body: &str) -> Result<Self, Vec<ContractError>> {
        let block = find_block(body).map_err(|e| vec![e])?;
        Self::parse_block(block.content)
    }

    /// Parses the TOML of a contract block.
    ///
    /// # Errors
    ///
    /// Every [`ContractError`] found, in key order.
    pub fn parse_block(content: &str) -> Result<Self, Vec<ContractError>> {
        let document = Document::parse(content)
            .map_err(|e| vec![ContractError::Toml(e.message().trim().to_owned())])?;
        let mut errors = Vec::new();
        let mut verdict = None;
        let mut contract = Self {
            verdict: String::new(),
            verdict_timeout: None,
            needs: Vec::new(),
            blocked_by: Vec::new(),
            budget: Budget::default(),
            runner: None,
            model: None,
            allow_protected: Vec::new(),
        };
        for (key, item) in document.as_table() {
            let result = match key {
                "verdict" => text(item).map(|v| verdict = Some(v)),
                "verdict-timeout" => duration(item).map(|d| contract.verdict_timeout = Some(d)),
                "needs" => strings(item).map(|v| contract.needs = v),
                "blocked-by" => items(item).map(|v| contract.blocked_by = v),
                "budget" => budget(item, &mut errors).map(|b| contract.budget = b),
                "runner" => text(item).map(|v| contract.runner = Some(v)),
                "model" => text(item).map(|v| contract.model = Some(v)),
                "allow-protected" => strings(item).map(|v| contract.allow_protected = v),
                unknown => {
                    errors.push(ContractError::UnknownKey(unknown.to_owned()));
                    continue;
                }
            };
            if let Err(problem) = result {
                errors.push(ContractError::Field {
                    key: key.to_owned(),
                    problem,
                });
            }
        }
        match verdict {
            Some(v) => contract.verdict = v,
            None if !errors
                .iter()
                .any(|e| matches!(e, ContractError::Field { key, .. } if key == "verdict")) =>
            {
                errors.push(ContractError::MissingVerdict);
            }
            None => {}
        }
        if errors.is_empty() {
            Ok(contract)
        } else {
            Err(errors)
        }
    }
}

/// A non-blank string.
fn text(item: &Item) -> Result<String, String> {
    let value = item.as_str().ok_or("expected a string")?;
    if value.trim().is_empty() {
        return Err("is empty".to_owned());
    }
    Ok(value.to_owned())
}

/// An array of non-blank strings.
fn strings(item: &Item) -> Result<Vec<String>, String> {
    let array = item.as_array().ok_or("expected an array of strings")?;
    array
        .iter()
        .map(|v| match v.as_str() {
            Some(s) if !s.trim().is_empty() => Ok(s.trim().to_owned()),
            Some(_) => Err("holds an empty string".to_owned()),
            None => Err("expected an array of strings".to_owned()),
        })
        .collect()
}

/// An array of `"#N"` strings or numbers.
fn items(item: &Item) -> Result<Vec<ItemId>, String> {
    let array = item
        .as_array()
        .ok_or("expected an array of items, like [\"#12\", 14]")?;
    array
        .iter()
        .map(|v| {
            let number = match v {
                Value::Integer(n) => u64::try_from(*n.value()).ok(),
                Value::String(s) => s.value().trim().trim_start_matches('#').parse().ok(),
                _ => None,
            };
            let shown = match v {
                Value::Integer(n) => n.value().to_string(),
                Value::String(s) => format!("{:?}", s.value()),
                other => other.type_name().to_owned(),
            };
            number
                .filter(|n| *n > 0)
                .map(ItemId)
                .ok_or_else(|| format!("{shown} is not an item like \"#12\""))
        })
        .collect()
}

fn duration(item: &Item) -> Result<Duration, String> {
    let parsed = if let Some(seconds) = item.as_integer() {
        u64::try_from(seconds)
            .ok()
            .filter(|s| *s > 0)
            .map(Duration::from_secs)
    } else if let Some(text) = item.as_str() {
        parse_duration(text)
    } else {
        None
    };
    parsed.ok_or_else(|| "expected a duration like \"90s\", \"10m\", \"1h30m\"".to_owned())
}

/// Parses `1h30m`, `90s`, `10m`: one or more `<n><unit>` with the units `h`, `m`, `s`, in that
/// order, each at most once, the total above zero.
#[must_use]
pub fn parse_duration(text: &str) -> Option<Duration> {
    let mut seconds: u64 = 0;
    let mut rest = text.trim();
    let mut last_unit = 0;
    if rest.is_empty() {
        return None;
    }
    while !rest.is_empty() {
        let digits = rest.len() - rest.trim_start_matches(|c: char| c.is_ascii_digit()).len();
        if digits == 0 {
            return None;
        }
        let count: u64 = rest[..digits].parse().ok()?;
        let (unit, rank) = match rest[digits..].chars().next()? {
            'h' => (3600, 1),
            'm' => (60, 2),
            's' => (1, 3),
            _ => return None,
        };
        if rank <= last_unit {
            return None;
        }
        last_unit = rank;
        seconds = seconds.checked_add(count.checked_mul(unit)?)?;
        rest = &rest[digits + 1..];
    }
    (seconds > 0).then(|| Duration::from_secs(seconds))
}

fn budget(item: &Item, errors: &mut Vec<ContractError>) -> Result<Budget, String> {
    let table = item
        .as_table_like()
        .ok_or("expected a table like { eqv = \"2M\", wall = \"2h\" }")?;
    let mut budget = Budget::default();
    for (key, item) in table.iter() {
        let field = format!("budget.{key}");
        let result = match key {
            "eqv" => eqv(item).map(|n| budget.eqv = Some(n)),
            "wall" => duration(item).map(|d| budget.wall = Some(d)),
            _ => {
                errors.push(ContractError::UnknownKey(field));
                continue;
            }
        };
        if let Err(problem) = result {
            errors.push(ContractError::Field {
                key: field,
                problem,
            });
        }
    }
    Ok(budget)
}

/// A positive token count: a number, or a string with an optional `k` or `M` suffix.
fn eqv(item: &Item) -> Result<u64, String> {
    let parsed = if let Some(n) = item.as_integer() {
        u64::try_from(n).ok()
    } else if let Some(text) = item.as_str() {
        let text = text.trim();
        let (digits, scale) = match text.chars().last() {
            Some('k' | 'K') => (&text[..text.len() - 1], 1_000),
            Some('M') => (&text[..text.len() - 1], 1_000_000),
            _ => (text, 1),
        };
        digits
            .parse::<u64>()
            .ok()
            .and_then(|n| n.checked_mul(scale))
    } else {
        None
    };
    parsed
        .filter(|n| *n > 0)
        .ok_or_else(|| "expected a positive token count like 2000000 or \"2M\"".to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn body(block: &str) -> String {
        format!("Do the thing.\n\n```bosun\n{block}```\n\nMore prose.\n")
    }

    #[test]
    fn every_field_parses() {
        let contract = Contract::parse(&body(
            "verdict = \"cargo test\"\nverdict-timeout = \"1h30m\"\nneeds = [\"phone\"]\n\
             blocked-by = [\"#12\", 14]\nbudget = { eqv = \"2M\", wall = 7200 }\n\
             runner = \"claude\"\nmodel = \"opus\"\nallow-protected = [\"tests/h.rs\"]\n",
        ))
        .unwrap();
        assert_eq!(
            contract,
            Contract {
                verdict: "cargo test".into(),
                verdict_timeout: Some(Duration::from_mins(90)),
                needs: vec!["phone".into()],
                blocked_by: vec![ItemId(12), ItemId(14)],
                budget: Budget {
                    eqv: Some(2_000_000),
                    wall: Some(Duration::from_hours(2)),
                },
                runner: Some("claude".into()),
                model: Some("opus".into()),
                allow_protected: vec!["tests/h.rs".into()],
            }
        );
    }

    #[test]
    fn the_block_is_found_among_other_fences() {
        let text = "~~~~\n```bosun\nverdict = \"no\"\n```\n~~~~\n\n```rust\nfn x() {}\n```\n\
                    ```` bosun extra\nverdict = \"yes\"\n````\n";
        let block = find_block(text).unwrap();
        assert_eq!(block.content, "verdict = \"yes\"\n");
        assert!(text[block.range.clone()].starts_with("```` bosun"));
        assert!(text[block.range].ends_with("````\n"));
    }

    #[test]
    fn missing_several_and_unclosed_blocks_are_errors() {
        assert_eq!(
            Contract::parse("prose only"),
            Err(vec![ContractError::NoBlock])
        );
        let two = format!("{}{}", body("verdict = \"a\"\n"), body("verdict = \"b\"\n"));
        assert_eq!(
            Contract::parse(&two),
            Err(vec![ContractError::SeveralBlocks(2)])
        );
        assert_eq!(
            Contract::parse("```bosun\nverdict = \"a\"\n"),
            Err(vec![ContractError::Unclosed])
        );
    }

    #[test]
    fn every_problem_is_reported_at_once() {
        let errors = Contract::parse(&body(
            "verdcit = \"x\"\nneeds = \"phone\"\nblocked-by = [\"#x\"]\nbudget = { eqv = 0, cpu = 1 }\n",
        ))
        .unwrap_err();
        assert_eq!(errors.len(), 6, "{errors:?}");
        assert!(errors.contains(&ContractError::UnknownKey("verdcit".into())));
        assert!(errors.contains(&ContractError::UnknownKey("budget.cpu".into())));
        assert!(errors.contains(&ContractError::MissingVerdict));
        assert!(errors.iter().all(|e| !e.to_string().is_empty()));
    }

    #[test]
    fn a_blank_verdict_is_not_a_missing_one() {
        let errors = Contract::parse(&body("verdict = \"  \"\n")).unwrap_err();
        assert_eq!(
            errors,
            [ContractError::Field {
                key: "verdict".into(),
                problem: "is empty".into()
            }]
        );
        assert!(matches!(
            Contract::parse(&body("verdict = [\n")).unwrap_err()[..],
            [ContractError::Toml(_)]
        ));
    }

    #[test]
    fn durations() {
        assert_eq!(parse_duration("90s"), Some(Duration::from_secs(90)));
        assert_eq!(parse_duration("2h"), Some(Duration::from_hours(2)));
        assert_eq!(parse_duration("1h1m1s"), Some(Duration::from_secs(3661)));
        for bad in ["", "0s", "m", "10", "1m1h", "1h1h", "1d", "-1s"] {
            assert_eq!(parse_duration(bad), None, "{bad}");
        }
    }
}
