//! The `notify.channel.*` methods (#9, the decision on channels): the notification channels of
//! an identity, kept in its own database and managed through the API, not `config.toml`.
//!
//! # Identities
//!
//! Every method names an identity and works on that identity's channels and credential-store
//! entries only: another identity's channel is never listed, removed or tested, and a name is
//! unique within one identity only. There is no channel shared across identities (#2).
//!
//! # The secret
//!
//! A channel's secret (a bot token, a signing key) is sent once, in
//! [`ChannelAddParams::secret`], and the daemon stores it in the OS credential store under
//! [`ChannelView::secret_entry`] (`notify/<identity>/<name>`, service `bosun`). It is in no row,
//! log line, error, event or result; [`ChannelView::secret_present`] says only whether the
//! entry is there. The `Debug` form of [`ChannelAddParams`] hides it.
//!
//! # The operator token
//!
//! `notify.channel.add`, `notify.channel.remove` and `notify.channel.test` take the operator
//! token, as `control.pause` does ([`crate::control`]); without the current one a call is
//! refused with [`crate::rpc::code::UNAUTHORIZED`] and changes nothing. `notify.channel.list`
//! reads only and takes none.
//!
//! # Content
//!
//! Messages are plain text, redacted and capped, and carry ids, states and numbers (#208).
//! `notify.channel.test` sends `bosun <identity>: test message from channel <name>`, now and
//! outside any outbox.

use std::collections::BTreeMap;
use std::fmt;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// The events a channel can get.
pub const EVENTS: &[&str] = &["needs_you", "repo_paused", "ready_drained"];

/// Params of `notify.channel.add`.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ChannelAddParams {
    /// The operator token the daemon wrote at start. Missing, it is refused like a wrong one.
    #[serde(default)]
    pub operator: String,
    pub identity: String,
    /// The channel's name within the identity: 1 to 32 of `a-z`, `0-9` and `-`.
    pub name: String,
    /// The channel's kind.
    pub kind: String,
    /// The kind's non-secret parameters (a chat id, a URL). An unknown key is refused.
    #[serde(default)]
    pub params: BTreeMap<String, String>,
    /// The events it gets, each one of [`EVENTS`]; at least one.
    pub events: Vec<String>,
    /// The least time between two of its messages, in seconds.
    pub min_gap_s: u64,
    /// The secret, for a kind that needs one; stored in the OS credential store only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub secret: Option<String>,
}

impl fmt::Debug for ChannelAddParams {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ChannelAddParams")
            .field("operator", &"<hidden>")
            .field("identity", &self.identity)
            .field("name", &self.name)
            .field("kind", &self.kind)
            .field("params", &self.params)
            .field("events", &self.events)
            .field("min_gap_s", &self.min_gap_s)
            .field("secret", &self.secret.as_ref().map(|_| "<hidden>"))
            .finish()
    }
}

/// Params of `notify.channel.remove` and `notify.channel.test`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ChannelParams {
    /// The operator token the daemon wrote at start. Missing, it is refused like a wrong one.
    #[serde(default)]
    pub operator: String,
    pub identity: String,
    /// The channel, by name.
    pub name: String,
}

/// One live channel of the identity.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ChannelView {
    pub name: String,
    pub kind: String,
    /// Its non-secret parameters.
    pub params: BTreeMap<String, String>,
    /// The events it gets.
    pub events: Vec<String>,
    /// The least time between two of its messages, in seconds.
    pub min_gap_s: u64,
    /// The credential-store entry of its secret (service `bosun`): a name, never the secret.
    pub secret_entry: String,
    /// Whether the credential store holds that entry.
    pub secret_present: bool,
    /// When its last message went out.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_sent_ms: Option<i64>,
}

/// Result of `notify.channel.list`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct Channels {
    pub identity: String,
    /// Its live channels, by name.
    pub channels: Vec<ChannelView>,
}

/// Result of `notify.channel.add`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ChannelAdded {
    pub identity: String,
    pub channel: ChannelView,
}

/// Result of `notify.channel.remove`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ChannelRemoved {
    pub identity: String,
    pub name: String,
}

/// Result of `notify.channel.test`. A send that failed is a result, not an API error.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ChannelTested {
    pub identity: String,
    pub name: String,
    /// The channel's service took the message.
    pub accepted: bool,
    /// Why not, when it did not; never the secret.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_debug_form_hides_the_secret_and_the_token() {
        let params = ChannelAddParams {
            operator: "synthetic-operator-1".to_owned(),
            identity: "a".to_owned(),
            name: "ops".to_owned(),
            kind: "fake".to_owned(),
            params: BTreeMap::new(),
            events: vec!["needs_you".to_owned()],
            min_gap_s: 60,
            secret: Some("synthetic-secret-4f1c9a".to_owned()),
        };
        let shown = format!("{params:?}");
        assert!(!shown.contains("synthetic-secret-4f1c9a"), "{shown}");
        assert!(!shown.contains("synthetic-operator-1"), "{shown}");
    }
}
