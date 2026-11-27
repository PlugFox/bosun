//! The proposal machine of #11 (part D, "Heat: conclusions and Backlog proposals"):
//! `open -> (published) -> landed -> measured`, `open | published -> dismissed`,
//! `open -> expired`.
//!
//! A proposal is what a heat detector leaves when it fires: a metric of one subject (a file, a
//! directory, a test, a tool target, a command or the whole repository), its value over a
//! window, a target and a remedy. The store keeps one row per proposal; this module owns the
//! arrows, the dedup key that makes two findings of one subject one proposal, the cooldown after
//! a dismissal or a follow-up that did not move the metric, and the proposal's own key.
//!
//! `measured`, `dismissed` and `expired` are final: they take no event. A landed proposal waits
//! for its follow-up; it can be neither dismissed nor expire.

use crate::path::{PathStyle, canonical_path};

/// The state of one proposal.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ProposalState {
    /// Found by a detector, not yet published as a card.
    Open,
    /// Published as a Backlog card.
    Published,
    /// Its remedy landed; the follow-up is due.
    Landed,
    /// The follow-up measured the metric again (final).
    Measured,
    /// The user dismissed it (final; starts the cooldown).
    Dismissed,
    /// Not seen again for two windows (final; not counted in detector effectiveness).
    Expired,
}

impl ProposalState {
    /// Every state, in lifecycle order.
    pub const ALL: [Self; 6] = [
        Self::Open,
        Self::Published,
        Self::Landed,
        Self::Measured,
        Self::Dismissed,
        Self::Expired,
    ];

    /// The stable name stored in the `proposal` table.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Open => "open",
            Self::Published => "published",
            Self::Landed => "landed",
            Self::Measured => "measured",
            Self::Dismissed => "dismissed",
            Self::Expired => "expired",
        }
    }

    /// The state named `name`, the inverse of [`Self::as_str`].
    #[must_use]
    pub fn parse(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|s| s.as_str() == name)
    }
}

/// What the follow-up found.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum FollowupOutcome {
    /// The metric reached its target.
    Reached,
    /// The metric moved toward the target without reaching it.
    Moved,
    /// The metric did not move (starts the cooldown).
    NotMoved,
    /// Too small a sample after the landing to say.
    Insufficient,
}

impl FollowupOutcome {
    /// Every outcome.
    pub const ALL: [Self; 4] = [
        Self::Reached,
        Self::Moved,
        Self::NotMoved,
        Self::Insufficient,
    ];

    /// The stable name stored in the `proposal` table.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Reached => "reached",
            Self::Moved => "moved",
            Self::NotMoved => "not_moved",
            Self::Insufficient => "insufficient",
        }
    }

    /// The outcome named `name`, the inverse of [`Self::as_str`].
    #[must_use]
    pub fn parse(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|s| s.as_str() == name)
    }
}

/// What a proposal is about.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SubjectKind {
    /// A file, by its repository-relative path.
    File,
    /// A directory, by its repository-relative path.
    Dir,
    /// A test, by its name.
    Test,
    /// The target of a tool (a crate, a package).
    ToolTarget,
    /// A command line.
    Command,
    /// The whole repository.
    Repo,
}

impl SubjectKind {
    /// Every kind.
    pub const ALL: [Self; 6] = [
        Self::File,
        Self::Dir,
        Self::Test,
        Self::ToolTarget,
        Self::Command,
        Self::Repo,
    ];

    /// The stable name stored in the `proposal` table.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::File => "file",
            Self::Dir => "dir",
            Self::Test => "test",
            Self::ToolTarget => "tool_target",
            Self::Command => "command",
            Self::Repo => "repo",
        }
    }

    /// The kind named `name`, the inverse of [`Self::as_str`].
    #[must_use]
    pub fn parse(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|s| s.as_str() == name)
    }
}

/// How the landing of a proposal's remedy was recognised.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum LandedHow {
    /// An item of the proposal's card landed.
    Item,
    /// A commit carries the proposal's marker.
    Marker,
    /// The user said so.
    Manual,
}

impl LandedHow {
    /// Every way.
    pub const ALL: [Self; 3] = [Self::Item, Self::Marker, Self::Manual];

    /// The stable name stored in the `proposal` table.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Item => "item",
            Self::Marker => "marker",
            Self::Manual => "manual",
        }
    }

    /// The way named `name`, the inverse of [`Self::as_str`].
    #[must_use]
    pub fn parse(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|s| s.as_str() == name)
    }
}

/// What happened to a proposal.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ProposalEvent {
    /// The detector found the subject again.
    Seen,
    /// The proposal was published as a card.
    Published,
    /// Its remedy landed.
    Landed,
    /// The follow-up measured it.
    Measured(FollowupOutcome),
    /// The user dismissed it.
    Dismissed,
    /// It was not seen for two windows.
    Expired,
}

/// The state `event` takes `state` to; `None` when there is no such arrow.
#[must_use]
pub fn step(state: ProposalState, event: ProposalEvent) -> Option<ProposalState> {
    use ProposalEvent as E;
    use ProposalState as S;
    match (state, event) {
        (S::Open | S::Published, E::Seen) => Some(state),
        (S::Open, E::Published) => Some(S::Published),
        (S::Open | S::Published, E::Landed) => Some(S::Landed),
        (S::Landed, E::Measured(_)) => Some(S::Measured),
        (S::Open | S::Published, E::Dismissed) => Some(S::Dismissed),
        (S::Open, E::Expired) => Some(S::Expired),
        // Mutant (`mutant_proposal_measured_reopens`): a measured proposal seen again opens.
        (S::Measured, E::Seen) if cfg!(mutant_proposal_measured_reopens) => Some(S::Open),
        _ => None,
    }
}

/// Whether a proposal in `state` holds its dedup key: `open`, `published` or `landed`.
#[must_use]
pub fn is_active(state: ProposalState) -> bool {
    matches!(
        state,
        ProposalState::Open | ProposalState::Published | ProposalState::Landed
    )
}

/// The key that makes two findings of one subject one proposal, within one repository:
/// detector, provider, subject kind and subject joined by `\n`. A file or directory subject is
/// canonicalised first ([`canonical_path`] in the repository's `style`), so two spellings of one
/// path are one key. The repository is not in the string: the store pairs the key with its row.
#[must_use]
pub fn dedup_key(
    detector: &str,
    provider: &str,
    kind: SubjectKind,
    subject: &str,
    style: PathStyle,
) -> String {
    let subject = match kind {
        SubjectKind::File | SubjectKind::Dir => canonical_path(subject, style),
        _ => subject.to_owned(),
    };
    format!("{detector}\n{provider}\n{}\n{subject}", kind.as_str())
}

/// How long a dismissal or a follow-up that did not move the metric keeps its dedup key from
/// opening again: 90 days.
pub const COOLDOWN_MS: i64 = 90 * 24 * 60 * 60 * 1000;

/// Whether a proposal that ended in `state` with `outcome` starts the cooldown of its key:
/// `dismissed`, or `measured` with `not_moved`.
#[must_use]
pub fn starts_cooldown(state: ProposalState, outcome: Option<FollowupOutcome>) -> bool {
    match state {
        ProposalState::Dismissed => true,
        ProposalState::Measured => outcome == Some(FollowupOutcome::NotMoved),
        _ => false,
    }
}

/// The proposal's own key: 16 lower-case hex digits of the 64-bit FNV-1a of `dedup_key`, `\n`
/// and `created_ms`. A card body names its proposal as `Proposal: <key>`.
#[must_use]
pub fn proposal_key(dedup_key: &str, created_ms: i64) -> String {
    const OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
    const PRIME: u64 = 0x0000_0100_0000_01b3;
    let text = format!("{dedup_key}\n{created_ms}");
    let hash = text
        .bytes()
        .fold(OFFSET, |h, b| (h ^ u64::from(b)).wrapping_mul(PRIME));
    format!("{hash:016x}")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn every_event() -> Vec<ProposalEvent> {
        let mut events = vec![
            ProposalEvent::Seen,
            ProposalEvent::Published,
            ProposalEvent::Landed,
            ProposalEvent::Dismissed,
            ProposalEvent::Expired,
        ];
        events.extend(FollowupOutcome::ALL.map(ProposalEvent::Measured));
        events
    }

    /// #11 D: the final states take no event, a landed proposal is neither dismissed nor
    /// expires, and the arrows of the lifecycle hold (`mutant_proposal_measured_reopens` opens a
    /// measured proposal seen again).
    #[test]
    fn a_measured_proposal_cannot_reopen() {
        use ProposalEvent as E;
        use ProposalState as S;
        for state in [S::Measured, S::Dismissed, S::Expired] {
            for event in every_event() {
                assert_eq!(step(state, event), None, "{state:?} took {event:?}");
            }
        }
        for event in [E::Dismissed, E::Expired, E::Seen] {
            assert_eq!(step(S::Landed, event), None, "landed took {event:?}");
        }
        assert_eq!(step(S::Published, E::Expired), None);

        assert_eq!(step(S::Open, E::Seen), Some(S::Open));
        assert_eq!(step(S::Published, E::Seen), Some(S::Published));
        assert_eq!(step(S::Open, E::Published), Some(S::Published));
        assert_eq!(step(S::Published, E::Landed), Some(S::Landed));
        for outcome in FollowupOutcome::ALL {
            assert_eq!(step(S::Landed, E::Measured(outcome)), Some(S::Measured));
        }
        assert_eq!(step(S::Open, E::Landed), Some(S::Landed));
        assert_eq!(step(S::Open, E::Dismissed), Some(S::Dismissed));
        assert_eq!(step(S::Published, E::Dismissed), Some(S::Dismissed));
        assert_eq!(step(S::Open, E::Expired), Some(S::Expired));

        for state in S::ALL {
            let active = matches!(state, S::Open | S::Published | S::Landed);
            assert_eq!(is_active(state), active, "{state:?}");
            for outcome in FollowupOutcome::ALL
                .map(Some)
                .into_iter()
                .chain(std::iter::once(None))
            {
                let cooldown = state == S::Dismissed
                    || (state == S::Measured && outcome == Some(FollowupOutcome::NotMoved));
                assert_eq!(
                    starts_cooldown(state, outcome),
                    cooldown,
                    "{state:?} {outcome:?}"
                );
            }
        }

        for x in ProposalState::ALL {
            assert_eq!(ProposalState::parse(x.as_str()), Some(x));
        }
        for x in FollowupOutcome::ALL {
            assert_eq!(FollowupOutcome::parse(x.as_str()), Some(x));
        }
        for x in SubjectKind::ALL {
            assert_eq!(SubjectKind::parse(x.as_str()), Some(x));
        }
        for x in LandedHow::ALL {
            assert_eq!(LandedHow::parse(x.as_str()), Some(x));
        }
        assert_eq!(ProposalState::parse("nonsense"), None);
        assert_eq!(FollowupOutcome::parse("nonsense"), None);
        assert_eq!(SubjectKind::parse("nonsense"), None);
        assert_eq!(LandedHow::parse("nonsense"), None);
    }

    #[test]
    fn two_spellings_of_one_path_are_one_dedup_key() {
        let a = dedup_key(
            "d1",
            "claude",
            SubjectKind::File,
            "docs/guide.md",
            PathStyle::Windows,
        );
        let b = dedup_key(
            "d1",
            "claude",
            SubjectKind::File,
            "docs\\Guide.md",
            PathStyle::Windows,
        );
        assert_eq!(a, b);
        assert_eq!(a, "d1\nclaude\nfile\ndocs\\guide.md");
        let unix = dedup_key(
            "d1",
            "claude",
            SubjectKind::File,
            "docs/Guide.md",
            PathStyle::Unix,
        );
        assert_ne!(a, unix);
        let test = dedup_key(
            "d1",
            "claude",
            SubjectKind::Test,
            "A\\B",
            PathStyle::Windows,
        );
        assert_eq!(test, "d1\nclaude\ntest\nA\\B");

        let key = proposal_key(&a, 1_000);
        assert_eq!(key.len(), 16);
        assert!(
            key.bytes()
                .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
        );
        assert_eq!(key, proposal_key(&a, 1_000));
        assert_ne!(key, proposal_key(&a, 1_001));
        assert_ne!(key, proposal_key(&unix, 1_000));
    }
}
