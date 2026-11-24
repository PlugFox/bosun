//! The operator actions (#4, #70): the one table the CLI and the TUI both act from.
//!
//! An action changes what the daemon does, so each takes the operator token ([`operator`]) and
//! each is behind a confirmation in the TUI. [`ACTIONS`] names, for each, the CLI command that
//! performs it, the TUI key, the confirmation text and the method; [`perform`] makes the call.
//! A client that offers an action takes it from here, so the CLI and the TUI cannot drift
//! apart: the CLI's tests hold that every action has its command.
//!
//! [`operator`]: crate::operator

use bosun_api::{
    ControlStatus, KillParams, KillResult, Method, PauseParams, RestartParams, RestartResult,
    method,
};

use crate::{Client, Error};

/// An operator action.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Action {
    /// Pause one repository, or every repository of an identity: the attempt in flight
    /// finishes, nothing new starts.
    Pause,
    /// Lift a pause.
    Resume,
    /// The kill switch: every child is killed, every item parks, no board is written.
    Kill,
    /// Release the kill switch: parked items resume and board writes start again.
    ReleaseKill,
    /// Restart an item in flight: kill its session, take a new snapshot, start again.
    RestartItem,
}

/// What an action needs to name.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scope {
    /// An identity, and optionally one of its repositories.
    Repository,
    /// Nothing: daemon-wide.
    Daemon,
    /// One item.
    Item,
}

/// One row of [`ACTIONS`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Spec {
    pub action: Action,
    /// The `bosun` subcommand that performs it, with its flags when it needs any.
    pub command: &'static str,
    /// The TUI's key for it.
    pub key: char,
    /// What it does, in a few words, for a menu or a help line.
    pub label: &'static str,
    /// The question the TUI asks before it acts; `{target}` stands for what it acts on.
    pub confirm: &'static str,
    /// What it acts on.
    pub scope: Scope,
    /// The method it calls.
    pub method: &'static str,
}

/// Every operator action, in the order a menu shows them.
pub const ACTIONS: &[Spec] = &[
    Spec {
        action: Action::Pause,
        command: "pause",
        key: 'p',
        label: "pause",
        confirm: "Pause {target}? The attempt in flight finishes; nothing new starts.",
        scope: Scope::Repository,
        method: method::ControlPause::NAME,
    },
    Spec {
        action: Action::Resume,
        command: "resume",
        key: 'r',
        label: "resume",
        confirm: "Resume {target}?",
        scope: Scope::Repository,
        method: method::ControlResume::NAME,
    },
    Spec {
        action: Action::Kill,
        command: "kill",
        key: 'K',
        label: "kill switch",
        confirm: "Throw the kill switch? Every child is killed, every item parks, and no board is \
                  written until it is released.",
        scope: Scope::Daemon,
        method: method::ControlKill::NAME,
    },
    Spec {
        action: Action::ReleaseKill,
        command: "kill --release",
        key: 'k',
        label: "release",
        confirm: "Release the kill switch? Parked items resume and board writes start again.",
        scope: Scope::Daemon,
        method: method::ControlKill::NAME,
    },
    Spec {
        action: Action::RestartItem,
        command: "restart-item",
        key: 'R',
        label: "restart",
        confirm: "Restart {target}? Its session is killed, a new snapshot is taken and it starts \
                  again.",
        scope: Scope::Item,
        method: method::ItemRestart::NAME,
    },
];

impl Action {
    /// This action's row of [`ACTIONS`].
    #[must_use]
    pub fn spec(self) -> &'static Spec {
        // The rows are in the order of the variants; a test holds it.
        &ACTIONS[self as usize]
    }
}

impl Spec {
    /// The confirmation text for `target`.
    #[must_use]
    pub fn confirmation(&self, target: &str) -> String {
        self.confirm.replace("{target}", target)
    }
}

/// What an action acts on.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Target {
    /// The identity, for a repository or an item.
    pub identity: String,
    /// One repository of the identity, for pause and resume; every repository when absent.
    pub repo: Option<String>,
    /// The item, for restart-item.
    pub item: Option<String>,
}

/// What an action answered.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// The identity's control state after a pause or a resume.
    Control(ControlStatus),
    /// The kill switch's state.
    Kill(KillResult),
    /// The restart was asked for.
    Restart(RestartResult),
}

/// Performs `action` on `target` with the operator token `operator`.
///
/// # Errors
///
/// As [`Client::call`]; the daemon refuses a wrong token with
/// [`bosun_api::rpc::code::UNAUTHORIZED`]. Restart-item without an item is refused here as
/// [`Error::Protocol`].
pub async fn perform(
    client: &mut Client,
    action: Action,
    operator: &str,
    target: &Target,
) -> Result<Outcome, Error> {
    let pause = || PauseParams {
        operator: operator.to_owned(),
        identity: target.identity.clone(),
        repo: target.repo.clone(),
    };
    Ok(match action {
        Action::Pause => Outcome::Control(client.call::<method::ControlPause>(&pause()).await?),
        Action::Resume => Outcome::Control(client.call::<method::ControlResume>(&pause()).await?),
        Action::Kill | Action::ReleaseKill => Outcome::Kill(
            client
                .call::<method::ControlKill>(&KillParams {
                    operator: operator.to_owned(),
                    release: action == Action::ReleaseKill,
                })
                .await?,
        ),
        Action::RestartItem => {
            let item = target
                .item
                .clone()
                .ok_or_else(|| Error::Protocol("restart-item names no item".to_owned()))?;
            Outcome::Restart(
                client
                    .call::<method::ItemRestart>(&RestartParams {
                        operator: operator.to_owned(),
                        identity: target.identity.clone(),
                        item,
                    })
                    .await?,
            )
        }
    })
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use super::*;

    #[test]
    fn every_action_has_one_row_with_its_own_key_and_command() {
        for action in [
            Action::Pause,
            Action::Resume,
            Action::Kill,
            Action::ReleaseKill,
            Action::RestartItem,
        ] {
            assert_eq!(action.spec().action, action);
        }
        let keys: BTreeSet<char> = ACTIONS.iter().map(|s| s.key).collect();
        let commands: BTreeSet<&str> = ACTIONS.iter().map(|s| s.command).collect();
        assert_eq!(keys.len(), ACTIONS.len());
        assert_eq!(commands.len(), ACTIONS.len());
        for spec in ACTIONS {
            assert!(bosun_api::METHODS.contains(&spec.method), "{spec:?}");
        }
        assert_eq!(
            Action::ReleaseKill.spec().method,
            Action::Kill.spec().method
        );
        assert_eq!(
            Action::RestartItem.spec().confirmation("demo#1"),
            "Restart demo#1? Its session is killed, a new snapshot is taken and it starts again."
        );
    }
}
