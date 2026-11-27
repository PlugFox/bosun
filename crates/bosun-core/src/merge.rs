//! The one merge rule of #3 ("Dedup and assembly").
//!
//! Every rule is a commutative, associative choice over the records of a message, so the result
//! does not depend on the order in which files are scanned or lines arrive:
//!
//! - usage: the record with the largest output (the final record of a split message); ties are
//!   broken by the remaining token counts, the cwd, the model and the raw usage JSON, which
//!   makes the choice a maximum under a total order (the cwd before the model, so that an
//!   oracle that ignores models, `recount`, attributes the message to the same repository);
//! - tool calls: the union over all records, keyed by `tool_use` id; a call seen twice keeps
//!   the larger byte counts, the error flag if either has it, the larger name, target, command
//!   hash and command text, and the earliest known call and result times;
//! - origin (the owning file): the earliest by timestamp, ties broken by window (session id,
//!   then agent id), then file key; see [`crate::event::Origin`].

use std::cmp::Ordering;
use std::collections::BTreeMap;
use std::fmt;

use crate::event::{Message, ToolCall};

/// The two records do not belong to one message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeyMismatch {
    /// The (provider, id) of the existing record.
    pub existing: (String, String),
    /// The (provider, id) of the incoming record.
    pub incoming: (String, String),
}

impl fmt::Display for KeyMismatch {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "cannot merge message {}/{} into {}/{}",
            self.incoming.0, self.incoming.1, self.existing.0, self.existing.1
        )
    }
}

impl std::error::Error for KeyMismatch {}

/// Merges a record of a message into what is known of it.
///
/// `existing` is the stored message (itself a merge of earlier records), `incoming` a record
/// just parsed. The result does not depend on the order of the records: folding any
/// permutation of a message's records, from one file or several, gives the same message.
///
/// # Errors
///
/// [`KeyMismatch`] when the two have different (provider, message id).
pub fn merge(existing: Message, incoming: Message) -> Result<Message, KeyMismatch> {
    if existing.provider != incoming.provider || existing.id != incoming.id {
        return Err(KeyMismatch {
            existing: (existing.provider, existing.id),
            incoming: (incoming.provider, incoming.id),
        });
    }

    let origin = existing.origin.clone().min(incoming.origin.clone());

    let tool_calls = if cfg!(mutant_core_final_blocks_only) {
        incoming.tool_calls.clone()
    } else {
        union(&existing.tool_calls, &incoming.tool_calls)
    };

    let usage_from_incoming = if cfg!(mutant_core_first_record_wins) {
        false
    } else {
        usage_order(&incoming, &existing) == Ordering::Greater
    };
    let winner = if usage_from_incoming {
        incoming
    } else {
        existing
    };

    Ok(Message {
        origin,
        tool_calls,
        ..winner
    })
}

/// The total order that picks the record whose usage a message keeps.
fn usage_order(a: &Message, b: &Message) -> Ordering {
    a.usage
        .tokens
        .output
        .cmp(&b.usage.tokens.output)
        .then_with(|| a.usage.tokens.cmp(&b.usage.tokens))
        .then_with(|| a.cwd.cmp(&b.cwd))
        .then_with(|| a.model.cmp(&b.model))
        // serde_json orders object keys, so the text is canonical.
        .then_with(|| a.usage.raw.to_string().cmp(&b.usage.raw.to_string()))
}

fn union(
    a: &BTreeMap<String, ToolCall>,
    b: &BTreeMap<String, ToolCall>,
) -> BTreeMap<String, ToolCall> {
    let mut out = a.clone();
    for (id, call) in b {
        out.entry(id.clone())
            .and_modify(|known| *known = merge_call(known, call))
            .or_insert_with(|| call.clone());
    }
    out
}

fn merge_call(a: &ToolCall, b: &ToolCall) -> ToolCall {
    ToolCall {
        name: a.name.clone().max(b.name.clone()),
        target: a.target.clone().max(b.target.clone()),
        input_bytes: a.input_bytes.max(b.input_bytes),
        result_bytes: a.result_bytes.max(b.result_bytes),
        is_error: a.is_error || b.is_error,
        command_hash: a.command_hash.clone().max(b.command_hash.clone()),
        command_text: a.command_text.clone().max(b.command_text.clone()),
        call_ts_ms: earliest(a.call_ts_ms, b.call_ts_ms),
        result_ts_ms: earliest(a.result_ts_ms, b.result_ts_ms),
        result_media_bytes: a.result_media_bytes.max(b.result_media_bytes),
    }
}

/// The earliest of two times, either of which may be unknown; `None` only when both are.
#[must_use]
pub fn earliest(a: Option<i64>, b: Option<i64>) -> Option<i64> {
    match (a, b) {
        (Some(a), Some(b)) => Some(a.min(b)),
        (a, b) => a.or(b),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::{Origin, Tokens, Usage, WindowKey};
    use proptest::prelude::*;

    fn window(session: &str) -> WindowKey {
        WindowKey {
            session_id: session.to_owned(),
            agent_id: None,
        }
    }

    fn call(name: &str) -> ToolCall {
        ToolCall {
            name: name.to_owned(),
            target: Some("src/lib.rs".to_owned()),
            input_bytes: 40,
            ..ToolCall::default()
        }
    }

    /// One record of message `msg_1`: one content block, the output so far.
    fn record(file: &str, ts_ms: i64, output: u64, tool_use: Option<&str>) -> Message {
        let tokens = Tokens {
            input: 3,
            cache_write_5m: 100,
            cache_write_1h: 2_000,
            cache_read: 50_000,
            output,
        };
        Message {
            provider: "claude".to_owned(),
            id: "msg_1".to_owned(),
            origin: Origin {
                ts_ms,
                file: file.to_owned(),
                window: window(file),
            },
            model: "claude-opus-5-5".to_owned(),
            cwd: "/work/repo".to_owned(),
            usage: Usage {
                tokens,
                raw: serde_json::json!({ "input_tokens": 3, "output_tokens": output }),
            },
            tool_calls: tool_use
                .map(|id| (id.to_owned(), call("Read")))
                .into_iter()
                .collect(),
        }
    }

    fn fold(records: Vec<Message>) -> Message {
        let mut records = records.into_iter();
        let first = records.next().expect("at least one record");
        records.fold(first, |acc, r| merge(acc, r).expect("same message"))
    }

    #[test]
    fn split_message_keeps_final_usage_and_every_tool_call() {
        // A thinking block, a text block, then three tool_use blocks, in file order.
        let records = vec![
            record("a", 1_000, 10, None),
            record("a", 1_001, 25, None),
            record("a", 1_002, 40, Some("toolu_1")),
            record("a", 1_003, 55, Some("toolu_2")),
            record("a", 1_004, 70, Some("toolu_3")),
        ];
        let last = records.last().cloned().unwrap();

        let merged = fold(records);

        assert_eq!(merged.usage, last.usage);
        assert_eq!(
            merged.tool_calls.keys().collect::<Vec<_>>(),
            ["toolu_1", "toolu_2", "toolu_3"]
        );
        assert_eq!(merged.origin.ts_ms, 1_000);
    }

    #[test]
    fn a_message_in_several_files_belongs_to_the_earliest() {
        let original = record("b-original", 2_000, 70, Some("toolu_1"));
        let fork = record("a-fork", 5_000, 70, Some("toolu_1"));
        let merged = merge(fork.clone(), original.clone()).unwrap();
        assert_eq!(merged.origin.file, "b-original");
        assert_eq!(merge(original, fork).unwrap(), merged);

        // Same timestamp, one window in two files: the smaller file key wins, whatever the order.
        let x = record("x", 2_000, 70, None);
        let mut y = record("y", 2_000, 70, None);
        y.origin.window = x.origin.window.clone();
        assert_eq!(merge(x.clone(), y.clone()).unwrap().origin.file, "x");
        assert_eq!(merge(y, x).unwrap().origin.file, "x");
    }

    /// A fork copies records with their timestamps. The tie goes to the smaller session id, not
    /// to the smaller file key: a file id is arbitrary and changes when the tree is copied
    /// (found on a real history, #6).
    #[test]
    fn a_timestamp_tie_goes_to_the_smaller_session_not_the_smaller_file() {
        let mut original = record("z-file-of-the-original", 2_000, 70, None);
        original.origin.window = window("s1");
        let mut fork = record("a-file-of-the-fork", 2_000, 70, None);
        fork.origin.window = window("s2");
        for merged in [
            merge(original.clone(), fork.clone()).unwrap(),
            merge(fork, original).unwrap(),
        ] {
            assert_eq!(merged.origin.window, window("s1"));
            assert_eq!(merged.origin.file, "z-file-of-the-original");
        }
    }

    /// Equal output and tokens: the cwd decides before the model, as in `recount`, which knows
    /// no models.
    #[test]
    fn a_usage_tie_is_broken_by_cwd_before_model() {
        let mut a = record("a", 1_000, 70, None);
        a.cwd = "/work/repo-b".to_owned();
        a.model = "claude-haiku-5-5".to_owned();
        let mut b = record("b", 1_000, 70, None);
        b.cwd = "/work/repo-a".to_owned();
        b.model = "claude-opus-5-5".to_owned();
        assert_eq!(merge(a.clone(), b.clone()).unwrap().cwd, "/work/repo-b");
        assert_eq!(merge(b, a).unwrap().cwd, "/work/repo-b");
    }

    #[test]
    fn records_of_another_message_are_refused() {
        let mut other = record("a", 1, 1, None);
        other.id = "msg_2".to_owned();
        assert!(merge(record("a", 1, 1, None), other).is_err());
    }

    #[test]
    fn a_tool_call_seen_twice_keeps_its_result() {
        let mut with_result = record("a", 1, 10, Some("toolu_1"));
        let entry = with_result.tool_calls.get_mut("toolu_1").unwrap();
        entry.result_bytes = 900;
        entry.is_error = true;
        let without = record("a", 1, 10, Some("toolu_1"));
        let merged = merge(without, with_result).unwrap();
        assert_eq!(merged.tool_calls["toolu_1"].result_bytes, 900);
        assert!(merged.tool_calls["toolu_1"].is_error);
    }

    /// Records of one message spread over up to three files, each with its own timestamp,
    /// output and optional tool call, paired with a shuffled copy of the same list.
    fn records_and_shuffle() -> impl Strategy<Value = (Vec<Message>, Vec<Message>)> {
        let one = (0..3usize, 0..5i64, 0..200u64, prop::option::of(0..6u8)).prop_map(
            |(file, ts, output, tool)| {
                let file = ["f0", "f1", "f2"][file];
                let tool = tool.map(|t| format!("toolu_{t}"));
                record(file, 1_000 + ts, output, tool.as_deref())
            },
        );
        prop::collection::vec(one, 1..12)
            .prop_flat_map(|records| (Just(records.clone()), Just(records).prop_shuffle()))
    }

    proptest! {
        // No regression files: a mutant run fails this test on purpose and must not write
        // into the source tree.
        #![proptest_config(ProptestConfig {
            failure_persistence: None,
            ..ProptestConfig::default()
        })]

        #[test]
        fn merge_is_order_independent((records, shuffled) in records_and_shuffle()) {
            prop_assert_eq!(fold(records), fold(shuffled));
        }
    }
}
