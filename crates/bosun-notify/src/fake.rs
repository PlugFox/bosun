//! The `fake` kind, for tests: a channel that sends nowhere.
//!
//! Each text it is given is appended to a process-wide list keyed by its `sink` parameter
//! ([`sent`]). Its `fail` parameter (`fail = "2"`) makes the first N sends of that sink answer
//! [`SendError::Retry`] and record nothing. [`crate::build`] accepts it only under
//! [`crate::Kinds::fake`], which a running daemon never sets.

use std::collections::BTreeMap;
use std::sync::{Mutex, PoisonError};

use crate::{Notifier, SendError, check_keys};

/// The kind's name.
pub const KIND: &str = "fake";

/// Its parameters.
const KEYS: &[&str] = &["sink", "fail"];

/// What every sink of the process received, and how many sends it was asked for.
static SINKS: Mutex<BTreeMap<String, Sink>> = Mutex::new(BTreeMap::new());

#[derive(Debug, Default)]
struct Sink {
    sends: u64,
    sent: Vec<String>,
}

/// A `fake` channel.
#[derive(Debug)]
pub struct Fake {
    sink: String,
    fail: u64,
}

impl Fake {
    pub(crate) fn new(params: &BTreeMap<String, String>) -> Result<Self, String> {
        check_keys(KIND, params, KEYS)?;
        let sink = params
            .get("sink")
            .cloned()
            .ok_or_else(|| "a fake channel needs its `sink` parameter".to_owned())?;
        let fail = match params.get("fail") {
            Some(n) => n
                .parse()
                .map_err(|_| format!("`fail` is a count of sends, not {n:?}"))?,
            None => 0,
        };
        Ok(Self { sink, fail })
    }
}

impl Notifier for Fake {
    fn send(&self, text: &str) -> Result<(), SendError> {
        let mut sinks = SINKS.lock().unwrap_or_else(PoisonError::into_inner);
        let sink = sinks.entry(self.sink.clone()).or_default();
        sink.sends += 1;
        if sink.sends <= self.fail {
            return Err(SendError::Retry {
                after: None,
                error: format!("the fake sink {} fails send {}", self.sink, sink.sends),
            });
        }
        sink.sent.push(text.to_owned());
        Ok(())
    }
}

/// Every text sink `sink` received, in order.
#[must_use]
pub fn sent(sink: &str) -> Vec<String> {
    SINKS
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .get(sink)
        .map_or_default(|s| s.sent.clone())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_failing_sink_retries_its_first_sends() {
        let params = BTreeMap::from([
            ("sink".to_owned(), "fake-tests-fail".to_owned()),
            ("fail".to_owned(), "2".to_owned()),
        ]);
        let fake = Fake::new(&params).unwrap();
        assert!(matches!(fake.send("one"), Err(SendError::Retry { .. })));
        assert!(matches!(fake.send("two"), Err(SendError::Retry { .. })));
        fake.send("three").unwrap();
        assert_eq!(sent("fake-tests-fail"), ["three"]);
    }
}
