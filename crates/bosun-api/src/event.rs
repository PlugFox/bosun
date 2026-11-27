//! The event stream (#4, #69): `events.subscribe` and the notifications that follow it.
//!
//! Every event of an identity carries a sequence number, monotonic within the identity and
//! within one run of the daemon (its *epoch*: a daemon restart starts a new one). The daemon
//! keeps the last events of each identity in a bounded ring. A client that reconnects names
//! the epoch and the first sequence it has not seen; it gets every event from there, in order,
//! or a resync when the daemon no longer holds them, and never a silent gap.
//!
//! # On the wire
//!
//! `events.subscribe` is answered like any request ([`SubscribeResult`]); from then on the
//! connection belongs to the subscription. The daemon sends notifications (requests without an
//! `id`) and reads nothing more but the end of the stream:
//!
//! | Notification | Params |
//! | --- | --- |
//! | [`EVENT`] (`events.event`) | [`Event`] |
//! | [`RESYNC`] (`events.resync`) | [`Resync`]; the daemon closes the connection after it |
//!
//! A subscriber that falls behind the ring is sent a resync and dropped; one that stops reading
//! is dropped when a write to it stalls. Neither slows the daemon down.
//!
//! # Compatibility
//!
//! New event kinds are additive within an API version (#12): a client ignores an event whose
//! `type` it does not know, and still counts its sequence as seen. [`EventBody::KINDS`] lists
//! the kinds this version knows.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::{PassReport, TokenSums, Wide};

/// The method of the notification that carries an [`Event`].
pub const EVENT: &str = "events.event";

/// The method of the notification that carries a [`Resync`].
pub const RESYNC: &str = "events.resync";

/// Params of `events.subscribe`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct SubscribeParams {
    pub identity: String,
    /// The epoch of `from_seq`, as a previous [`SubscribeResult`] or [`Resync`] named it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub epoch: Option<String>,
    /// The first sequence wanted: one past the last the client has seen. Absent: only events
    /// published from now on, with no replay.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub from_seq: Option<u64>,
    /// Which events to deliver.
    #[serde(default)]
    pub filter: EventFilter,
}

/// Which events a subscription delivers.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct EventFilter {
    /// Only these kinds (the `type` of [`EventBody`]); every kind when absent. Session output
    /// lines need [`EventFilter::tail`] as well.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kinds: Option<Vec<String>>,
    /// Session output lines, the live tail: opt-in, because it is the heavy kind. Absent: no
    /// output line is delivered.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tail: Option<Tail>,
}

/// The live tail a subscription opted into.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct Tail {
    /// The output of this item's sessions only; of every item when absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub item: Option<String>,
}

/// Result of `events.subscribe`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct SubscribeResult {
    pub identity: String,
    /// The daemon's epoch: a reconnect names it with its `from_seq`.
    pub epoch: String,
    /// The sequence of the last event published so far; 0 before the first.
    pub head: u64,
    /// Present when the events asked for are not held: the client refetches its state, and the
    /// stream starts after `head`. Absent: the stream starts at `from_seq` (after `head` when
    /// none was given), with no gap.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resync: Option<ResyncReason>,
}

/// The params of an [`RESYNC`] notification: the subscriber missed events and is dropped.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct Resync {
    pub epoch: String,
    /// The sequence of the last event published when the subscriber was dropped. A client
    /// that refetches its state after this has seen everything up to `head`.
    pub head: u64,
    pub reason: ResyncReason,
}

/// Why a client must refetch its state.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ResyncReason {
    /// The daemon no longer holds the sequence asked for: the ring moved past it.
    Gone,
    /// The epoch asked for is not the daemon's: it restarted since.
    Restarted,
    /// The subscriber fell behind the ring while it was connected.
    Lagged,
}

/// One event of an identity.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct Event {
    /// The sequence number, from 1 within the identity and the epoch.
    pub seq: u64,
    /// When it was published.
    pub ts_ms: i64,
    pub body: EventBody,
}

/// What happened. States, causes and reasons are the stable names the store records (#3).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum EventBody {
    /// An item took an arrow of its machine (#7).
    ItemTransition {
        item: String,
        /// The transition's number within the item, from 1.
        transition: u32,
        /// The attempt current after the transition; 0 before the first.
        attempt: u32,
        /// The item's state before and after (`ready`, `claimed`, `running`, `parked`, ...).
        from: String,
        to: String,
        /// The attempt's state before and after, when there is an attempt (`spawned`,
        /// `exited`, `verdict_running`, `pass`, ...).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        attempt_from: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        attempt_to: Option<String>,
        /// The event that caused it.
        cause: String,
        /// Why the item is where it went, for `parked`, `done` and `needs_you`.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        reason: Option<String>,
    },
    /// A session of an attempt started.
    SessionStarted {
        item: String,
        attempt: u32,
        /// The session's number within the item.
        spawn: u32,
        /// `fresh`, `resume` or `reset`.
        mode: String,
        /// The provider's session id, once known.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        session: Option<String>,
    },
    /// A session ended.
    SessionEnded {
        item: String,
        attempt: u32,
        spawn: u32,
        /// How it ended: `ok`, `limit`, `auth`, `crash` or `killed`.
        exit: String,
    },
    /// One line a session printed: the live tail. Delivered only to subscriptions that opted
    /// in ([`EventFilter::tail`]).
    SessionOutput {
        item: String,
        attempt: u32,
        spawn: u32,
        stream: OutputStream,
        line: String,
        /// The tool's name, when the line is a tool call the session made: a client may
        /// collapse a run of them into one row (#74).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        tool: Option<String>,
    },
    /// Usage of a session's new messages since the last delta.
    UsageDelta {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        item: Option<String>,
        /// The provider's session id.
        session: String,
        sums: TokenSums,
    },
    /// A verdict run started.
    VerdictStarted {
        item: String,
        attempt: u32,
        run: u32,
    },
    /// A verdict run finished.
    VerdictFinished {
        item: String,
        attempt: u32,
        run: u32,
        /// `pass`, `fail` or `timeout`.
        result: String,
        duration_ms: u64,
    },
    /// An item parked on a provider limit.
    LimitParked {
        item: String,
        attempt: u32,
        /// When the limit resets, if the exit named it.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        until_ms: Option<i64>,
    },
    /// An item parked on a limit woke up.
    LimitResumed { item: String, attempt: u32 },
    /// The queue of a repository changed: cards added, removed, reordered or edited.
    QueueChanged {
        repo: String,
        /// The hash of the queue's new snapshot.
        hash: String,
    },
    /// The lint findings of a card changed; empty when it passes now.
    LintFindings {
        repo: String,
        card: String,
        findings: Vec<String>,
    },
    /// Syncing a repository's board failed.
    BoardSyncError { repo: String, error: String },
    /// A pause, a resume or the kill switch.
    ControlChanged {
        /// `running`, `paused` or `killed`.
        state: String,
        /// The repository paused or resumed; every repository when absent.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        repo: Option<String>,
    },
    /// The fast-death breaker tripped (#7): `sessions` sessions in a row of the repository, the
    /// last one of `item`, crashed by themselves within `within_ms` of their spawn. The
    /// repository is paused until `bosun resume` (or a daemon restart).
    FastDeaths {
        /// The repository paused.
        repo: String,
        /// The item whose session tripped the breaker.
        item: String,
        /// How many sessions in a row.
        sessions: u32,
        /// The window after a spawn within which a crash counts, in ms.
        within_ms: u64,
    },
    /// A resource's probe (#8), run before the claim of `item`, which needs it: a failing
    /// streak's start (`healthy: false`, once per streak; the card waits in Ready, no session
    /// spent), or its end (`healthy: true`). Only the identity that ran the probe gets it (#2).
    ResourceProbe {
        /// The repository whose loop ran the probe.
        repo: String,
        /// The resource instance: `<name>`, `<name>/<identity>` or `<name>/<repo>`.
        resource: String,
        /// The card the probe ran for.
        item: String,
        /// Whether the probe passed.
        healthy: bool,
        /// Of a failure: `exit <code>`, `timeout after <n>s` or `cannot start: <error>`, then
        /// the probe's last line, redacted and at most 200 characters.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        detail: Option<String>,
    },
    /// A budget cap was reached (#7, #67): an item's eqv or wall-clock cap (the item is killed
    /// and goes to Needs you), or the identity's daily eqv cap (the queue parks until
    /// `until_ms`, local midnight).
    BudgetExceeded {
        /// `item_eqv`, `item_wall_clock` or `daily_eqv`.
        cap: String,
        /// The item, for a per-item cap.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        item: Option<String>,
        /// What was used: eqv tokens, or milliseconds for the wall-clock cap.
        used: Wide,
        /// The cap, in the same unit.
        limit: Wide,
        /// When the daily cap releases the queue.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        until_ms: Option<i64>,
    },
    /// The ingest parsed more bytes.
    IngestProgress {
        /// Bytes of complete lines parsed since the daemon started.
        parsed_since_start: Wide,
    },
    /// An ingest pass finished: a timed pass whose outcome differs from the previous one's,
    /// or a reingest.
    IngestPass { reingest: bool, pass: PassReport },
    /// An ingest pass stopped: the store failed.
    IngestFailed { reingest: bool, error: String },
}

impl EventBody {
    /// The kinds this API version knows, as their `type` names.
    pub const KINDS: &'static [&'static str] = &[
        "item_transition",
        "session_started",
        "session_ended",
        "session_output",
        "usage_delta",
        "verdict_started",
        "verdict_finished",
        "limit_parked",
        "limit_resumed",
        "queue_changed",
        "lint_findings",
        "board_sync_error",
        "control_changed",
        "fast_deaths",
        "resource_probe",
        "budget_exceeded",
        "ingest_progress",
        "ingest_pass",
        "ingest_failed",
    ];

    /// The `type` name of this kind.
    #[must_use]
    pub fn kind(&self) -> &'static str {
        match self {
            Self::ItemTransition { .. } => "item_transition",
            Self::SessionStarted { .. } => "session_started",
            Self::SessionEnded { .. } => "session_ended",
            Self::SessionOutput { .. } => "session_output",
            Self::UsageDelta { .. } => "usage_delta",
            Self::VerdictStarted { .. } => "verdict_started",
            Self::VerdictFinished { .. } => "verdict_finished",
            Self::LimitParked { .. } => "limit_parked",
            Self::LimitResumed { .. } => "limit_resumed",
            Self::QueueChanged { .. } => "queue_changed",
            Self::LintFindings { .. } => "lint_findings",
            Self::BoardSyncError { .. } => "board_sync_error",
            Self::ControlChanged { .. } => "control_changed",
            Self::FastDeaths { .. } => "fast_deaths",
            Self::ResourceProbe { .. } => "resource_probe",
            Self::BudgetExceeded { .. } => "budget_exceeded",
            Self::IngestProgress { .. } => "ingest_progress",
            Self::IngestPass { .. } => "ingest_pass",
            Self::IngestFailed { .. } => "ingest_failed",
        }
    }
}

/// Which stream of a session a line came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum OutputStream {
    Stdout,
    Stderr,
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn kinds_name_every_branch_of_the_schema() {
        let schema = crate::schema();
        let branches = schema["$defs"]["EventBody"]["oneOf"].as_array().unwrap();
        let names: Vec<&str> = branches
            .iter()
            .map(|b| b["properties"]["type"]["const"].as_str().unwrap())
            .collect();
        assert_eq!(names, EventBody::KINDS);
    }

    #[test]
    fn an_event_is_tagged_and_round_trips() {
        let event = Event {
            seq: 3,
            ts_ms: 10,
            body: EventBody::SessionOutput {
                item: "a".to_owned(),
                attempt: 1,
                spawn: 2,
                stream: OutputStream::Stderr,
                line: "x".to_owned(),
                tool: None,
            },
        };
        let value = serde_json::to_value(&event).unwrap();
        assert_eq!(value["body"]["type"], "session_output");
        assert_eq!(value["body"]["stream"], json!({"type": "stderr"}));
        assert_eq!(event.body.kind(), "session_output");
        assert_eq!(serde_json::from_value::<Event>(value).unwrap(), event);
    }

    #[test]
    fn subscribe_params_default_to_live_without_tail() {
        let p: SubscribeParams = serde_json::from_value(json!({"identity": "i"})).unwrap();
        assert_eq!(p.from_seq, None);
        assert_eq!(p.filter, EventFilter::default());
        assert_eq!(p.filter.tail, None);
    }
}
