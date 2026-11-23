//! Lines to outcomes: the provider-neutral half of a parser (#3, "Ingest contract").
//!
//! - Only complete lines are parsed. A last line without its newline is still being written;
//!   [`parse_lines`] stops before it and says so in [`Batch::end`], so the tailer re-reads it
//!   once its newline arrives.
//! - Every line has an outcome: records, a skip (an unknown record type, counted by type) or a
//!   gap. A gap never aborts the rest of the bytes.
//! - A gap carries metadata only: never the line, nor any value from it.

use std::collections::BTreeMap;

use bosun_core::Gap;

use crate::record::{Record, Source};

/// A provider's parser of single lines.
pub trait LineParser {
    /// The provider name, the first half of a message key.
    fn provider(&self) -> &'static str;

    /// The version of the parser. Every row records the version that wrote it (#3).
    fn version(&self) -> u32;

    /// Parses one complete line, without its line terminator, that starts at `offset` in
    /// `source`.
    fn parse_line(&self, source: &Source, offset: u64, line: &[u8]) -> Outcome;

    /// Parses one line as [`LineParser::parse_line`] does, and says the line's own timestamp
    /// (milliseconds since the Unix epoch, UTC) when it has one, whatever its outcome. The
    /// default says none: a provider without a session check needs no timestamps.
    fn parse_timed(&self, source: &Source, offset: u64, line: &[u8]) -> (Outcome, Option<i64>) {
        (self.parse_line(source, offset, line), None)
    }
}

/// What one line yielded.
#[derive(Debug, Clone, PartialEq)]
pub enum Outcome {
    /// A record of a known type. It may yield nothing (a known type with nothing to bill, or
    /// a blank line).
    Records(Vec<Record>),
    /// A record of a type the parser does not know. Skipped, and counted by type.
    Skipped { record_type: String },
    /// A record whose shape the parser did not expect.
    Gap(Gap),
}

/// One complete line and what it yielded.
#[derive(Debug, Clone, PartialEq)]
pub struct ParsedLine {
    /// Where the line starts in its file.
    pub offset: u64,
    /// Where the next line starts: past this line's newline.
    pub end: u64,
    /// The line's own timestamp, when it has one.
    pub ts_ms: Option<i64>,
    pub outcome: Outcome,
}

/// What [`parse_lines`] made of a run of bytes.
#[derive(Debug, Clone, PartialEq)]
pub struct Batch {
    /// Every complete line, in file order.
    pub lines: Vec<ParsedLine>,
    /// The offset after the last complete line: where the next read starts. Bytes past it are
    /// an incomplete line.
    pub end: u64,
    /// The timestamp of the last timestamped line up to [`Batch::end`]: this batch's or, when
    /// it has none, the one carried in from the lines before it.
    pub last_ts_ms: Option<i64>,
}

impl Batch {
    /// Every record, in file order.
    pub fn records(&self) -> impl Iterator<Item = &Record> {
        self.lines.iter().flat_map(|line| match &line.outcome {
            Outcome::Records(records) => records.as_slice(),
            Outcome::Skipped { .. } | Outcome::Gap(_) => &[],
        })
    }

    /// Every gap, in file order.
    pub fn gaps(&self) -> impl Iterator<Item = &Gap> {
        self.lines.iter().filter_map(|line| match &line.outcome {
            Outcome::Gap(gap) => Some(gap),
            Outcome::Records(_) | Outcome::Skipped { .. } => None,
        })
    }

    /// The number of skipped lines per unknown record type.
    #[must_use]
    pub fn skipped(&self) -> BTreeMap<&str, u64> {
        let mut out = BTreeMap::new();
        for line in &self.lines {
            if let Outcome::Skipped { record_type } = &line.outcome {
                *out.entry(record_type.as_str()).or_default() += 1;
            }
        }
        out
    }
}

/// Parses every complete line of `bytes`, which start at `offset` in `source`, with no
/// timestamp carried in from lines before them. See [`parse_lines_after`].
pub fn parse_lines(parser: &impl LineParser, source: &Source, offset: u64, bytes: &[u8]) -> Batch {
    parse_lines_after(parser, source, offset, bytes, None)
}

/// Parses every complete line of `bytes`, which start at `offset` in `source`; `last_ts_ms` is
/// the timestamp of the last timestamped line before `offset` (the [`Batch::last_ts_ms`] of
/// the read before).
///
/// A line ends with `\n`; a `\r` before it is dropped. The bytes after the last `\n` are not
/// parsed: they are a line still being written.
///
/// A session check ends where the provider last wrote before it: its
/// [`SessionCheck::end_ms`](bosun_core::SessionCheck::end_ms) is set to the timestamp of the
/// last timestamped line before it in the file, of any type (#46). The carried timestamp keeps
/// this true when a read begins right at the check's line.
pub fn parse_lines_after(
    parser: &impl LineParser,
    source: &Source,
    offset: u64,
    bytes: &[u8],
    last_ts_ms: Option<i64>,
) -> Batch {
    let mut lines = Vec::new();
    let mut start = 0;
    while let Some(len) = bytes[start..].iter().position(|&b| b == b'\n') {
        let line = &bytes[start..start + len];
        let line = line.strip_suffix(b"\r").unwrap_or(line);
        let line_offset = offset + len64(start);
        start += len + 1;
        let (outcome, ts_ms) = if line.iter().all(u8::is_ascii_whitespace) {
            (Outcome::Records(Vec::new()), None)
        } else {
            parser.parse_timed(source, line_offset, line)
        };
        let is_gap = matches!(outcome, Outcome::Gap(_));
        lines.push(ParsedLine {
            offset: line_offset,
            end: offset + len64(start),
            ts_ms,
            outcome,
        });
        if cfg!(mutant_ingest_abort_on_gap) && is_gap {
            // Mutant: a gap aborts the rest of the file.
            break;
        }
    }
    if cfg!(mutant_ingest_last_block) {
        last_block_only(&mut lines);
    }
    let end = lines.last().map_or(offset, |line| line.end);
    let last_ts_ms = end_checks(&mut lines, last_ts_ms);
    Batch {
        lines,
        end,
        last_ts_ms,
    }
}

/// Sets the end of every session check of `lines` to the timestamp of the last timestamped
/// line before it, starting from `last_ts_ms`, and returns the last timestamp of all.
fn end_checks(lines: &mut [ParsedLine], mut last_ts_ms: Option<i64>) -> Option<i64> {
    for line in lines {
        if let Outcome::Records(records) = &mut line.outcome {
            for record in records {
                if let Record::SessionCheck(check) = record {
                    check.end_ms = last_ts_ms;
                }
            }
        }
        last_ts_ms = line.ts_ms.or(last_ts_ms);
    }
    last_ts_ms
}

/// Mutant `mutant_ingest_last_block`: keeps only the last message record of the batch, as the
/// old accounting took the last usage block of a session.
fn last_block_only(lines: &mut [ParsedLine]) {
    let is_message = |record: &Record| matches!(record, Record::Message(_));
    let last = lines.iter().rposition(
        |line| matches!(&line.outcome, Outcome::Records(records) if records.iter().any(is_message)),
    );
    for (i, line) in lines.iter_mut().enumerate() {
        if Some(i) != last
            && let Outcome::Records(records) = &mut line.outcome
        {
            records.retain(|record| !is_message(record));
        }
    }
}

/// The gap of `line`, which starts at `offset`. `reason` is a fixed text, never built from the
/// line, and `top_level_keys` are names only.
pub(crate) fn gap(
    offset: u64,
    line: &[u8],
    record_type: Option<&str>,
    top_level_keys: Vec<String>,
    reason: &'static str,
) -> Gap {
    let reason = if cfg!(mutant_ingest_gap_keeps_line) {
        // Mutant: the line is written into the reason.
        format!("{reason}: {}", String::from_utf8_lossy(line))
    } else {
        reason.to_owned()
    };
    Gap {
        offset,
        length: len64(line.len()),
        record_type: record_type.map(str::to_owned),
        top_level_keys,
        reason,
    }
}

pub(crate) fn len64(n: usize) -> u64 {
    u64::try_from(n).unwrap_or(u64::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;
    use bosun_core::WindowKey;

    /// A parser that yields nothing for `ok`, skips `new`, and makes a gap of anything else.
    struct Toy;

    impl LineParser for Toy {
        fn provider(&self) -> &'static str {
            "toy"
        }

        fn version(&self) -> u32 {
            1
        }

        fn parse_line(&self, _: &Source, offset: u64, line: &[u8]) -> Outcome {
            match line {
                b"ok" => Outcome::Records(Vec::new()),
                b"new" => Outcome::Skipped {
                    record_type: "new".to_owned(),
                },
                _ => Outcome::Gap(gap(offset, line, None, Vec::new(), "toy")),
            }
        }
    }

    fn source() -> Source {
        Source {
            file: "f".to_owned(),
            window: WindowKey {
                session_id: "s".to_owned(),
                agent_id: None,
            },
        }
    }

    #[test]
    fn only_complete_lines_are_parsed() {
        let batch = parse_lines(&Toy, &source(), 100, b"ok\r\nnew\nnew\n\nbad\nok");
        let offsets: Vec<(u64, u64)> = batch.lines.iter().map(|l| (l.offset, l.end)).collect();
        assert_eq!(
            offsets,
            [(100, 104), (104, 108), (108, 112), (112, 113), (113, 117)]
        );
        assert_eq!(batch.end, 117);
        assert_eq!(batch.skipped(), BTreeMap::from([("new", 2)]));
        let gaps: Vec<&Gap> = batch.gaps().collect();
        assert_eq!(gaps.len(), 1);
        assert_eq!((gaps[0].offset, gaps[0].length), (113, 3));
    }

    #[test]
    fn no_newline_means_no_line() {
        let batch = parse_lines(&Toy, &source(), 7, b"ok");
        assert_eq!(batch.lines, []);
        assert_eq!(batch.end, 7);
    }
}
