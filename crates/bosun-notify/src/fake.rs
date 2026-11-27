//! The `fake` kind, for tests: a channel that sends nowhere.
//!
//! Each text it is given is appended to a process-wide list keyed by its `sink` parameter
//! ([`sent`]). Its `fail` parameter (`fail = "2"`) makes the first N sends of that sink answer
//! [`SendError::Retry`] and record nothing; its `retry_after_ms` is the service's wait those
//! failures give (none without it). Its `delay_ms` makes every send take that long before it
//! is recorded, as a slow service would; [`sends`] counts the sends begun. [`crate::build`] accepts it only under
//! [`crate::Kinds::fake`], which a running daemon never sets.

use std::collections::BTreeMap;
use std::sync::{Mutex, PoisonError};
use std::time::Duration;

use crate::{Notifier, SendError, check_keys};

/// The kind's name.
pub const KIND: &str = "fake";

/// Its parameters.
const KEYS: &[&str] = &["sink", "fail", "retry_after_ms", "delay_ms"];

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
    retry_after: Option<Duration>,
    delay: Duration,
}

/// The parameter `key` of `params` as a count of milliseconds; `None` without it.
fn millis(params: &BTreeMap<String, String>, key: &str) -> Result<Option<Duration>, String> {
    params
        .get(key)
        .map(|n| {
            n.parse()
                .map(Duration::from_millis)
                .map_err(|_| format!("`{key}` is a count of milliseconds, not {n:?}"))
        })
        .transpose()
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
        Ok(Self {
            sink,
            fail,
            retry_after: millis(params, "retry_after_ms")?,
            delay: millis(params, "delay_ms")?.unwrap_or_default(),
        })
    }
}

impl Notifier for Fake {
    fn send(&self, text: &str) -> Result<(), SendError> {
        let sends = {
            let mut sinks = SINKS.lock().unwrap_or_else(PoisonError::into_inner);
            let sink = sinks.entry(self.sink.clone()).or_default();
            sink.sends += 1;
            sink.sends
        };
        std::thread::sleep(self.delay);
        if sends <= self.fail {
            return Err(SendError::Retry {
                after: self.retry_after,
                error: format!("the fake sink {} fails send {sends}", self.sink),
            });
        }
        SINKS
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .entry(self.sink.clone())
            .or_default()
            .sent
            .push(text.to_owned());
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

/// How many sends sink `sink` was asked for, failed and slow ones included.
#[must_use]
pub fn sends(sink: &str) -> u64 {
    SINKS
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .get(sink)
        .map_or(0, |s| s.sends)
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
        assert!(matches!(
            fake.send("one"),
            Err(SendError::Retry { after: None, .. })
        ));
        assert!(matches!(fake.send("two"), Err(SendError::Retry { .. })));
        fake.send("three").unwrap();
        assert_eq!(sent("fake-tests-fail"), ["three"]);
        assert_eq!(sends("fake-tests-fail"), 3);
    }

    #[test]
    fn a_failing_sink_gives_its_wait_and_a_slow_one_takes_its_time() {
        let params = BTreeMap::from([
            ("sink".to_owned(), "fake-tests-wait".to_owned()),
            ("fail".to_owned(), "1".to_owned()),
            ("retry_after_ms".to_owned(), "1500".to_owned()),
            ("delay_ms".to_owned(), "50".to_owned()),
        ]);
        let fake = Fake::new(&params).unwrap();
        let start = std::time::Instant::now();
        assert!(matches!(
            fake.send("one"),
            Err(SendError::Retry { after: Some(after), .. }) if after == Duration::from_millis(1_500)
        ));
        fake.send("two").unwrap();
        assert!(start.elapsed() >= Duration::from_millis(100));
        assert_eq!(sent("fake-tests-wait"), ["two"]);
        let bad = BTreeMap::from([
            ("sink".to_owned(), "fake-tests-bad".to_owned()),
            ("delay_ms".to_owned(), "soon".to_owned()),
        ]);
        assert!(Fake::new(&bad).is_err());
    }
}
