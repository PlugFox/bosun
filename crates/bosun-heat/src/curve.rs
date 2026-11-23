//! Context windows and their compactions: the context curve of the exact report, and the
//! stretches of a window that carry a tool result for the estimates.

use std::collections::BTreeMap;

use bosun_api::{ContextCurve, WindowCurve};
use bosun_store::UsageRow;

/// A context window: provider, session id, sub-agent id (`None` for the main transcript).
pub(crate) type WindowKey<'a> = (&'a str, &'a str, Option<&'a str>);

/// The messages of `rows` per context window, each in the order of `rows` (oldest first,
/// then by message id).
pub(crate) fn windows<'a>(rows: &[&'a UsageRow]) -> BTreeMap<WindowKey<'a>, Vec<&'a UsageRow>> {
    let mut out: BTreeMap<WindowKey<'a>, Vec<&'a UsageRow>> = BTreeMap::new();
    for row in rows {
        out.entry((&row.provider, &row.session, row.agent.as_deref()))
            .or_default()
            .push(row);
    }
    out
}

/// A compaction (#3): the prompt size drops from one message of a window to the next.
pub(crate) fn is_compaction(previous: &UsageRow, next: &UsageRow) -> bool {
    next.tokens.prompt_size() < previous.tokens.prompt_size()
}

/// For each message `i` of a window, the end (exclusive) of the stretch that carries what
/// entered the context after it: the first `k > i` where a compaction happens, else the
/// window's length.
pub(crate) fn stretch_ends(messages: &[&UsageRow]) -> Vec<usize> {
    let mut ends = vec![messages.len(); messages.len()];
    for i in (0..messages.len().saturating_sub(1)).rev() {
        ends[i] = if is_compaction(messages[i], messages[i + 1]) {
            i + 1
        } else {
            ends[i + 1]
        };
    }
    ends
}

/// The context curve of the messages of the scope.
pub(crate) fn context_curve(rows: &[&UsageRow]) -> ContextCurve {
    let rows: Vec<WindowCurve> = windows(rows)
        .into_iter()
        .map(|((_, session, agent), messages)| {
            let sizes: Vec<u64> = messages.iter().map(|m| m.tokens.prompt_size()).collect();
            WindowCurve {
                session: session.to_owned(),
                agent: agent.map(str::to_owned),
                messages: messages.len() as u64,
                start: sizes.first().copied().unwrap_or(0),
                peak: sizes.iter().copied().max().unwrap_or(0),
                compactions: messages
                    .windows(2)
                    .filter(|pair| is_compaction(pair[0], pair[1]))
                    .count() as u64,
            }
        })
        .collect();
    ContextCurve {
        windows: rows.len() as u64,
        compactions: rows.iter().map(|r| r.compactions).sum(),
        rows,
    }
}
