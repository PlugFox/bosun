//! Protocol types of the bosun local API (#4, #12, #24): the only thing clients share with the
//! daemon.
//!
//! - The transport is JSON-RPC 2.0, one message per line ([`rpc`]), on a Unix socket or a
//!   Windows named pipe; the endpoint itself is `bosun-client`'s concern.
//! - Each method is a type of [`method`] implementing [`Method`]: its wire name, its params and its result.
//! - The API is versioned by one integer, [`API_VERSION`], returned by [`method::Hello`]. Within a
//!   version changes are additive only (new optional fields, new methods); anything else bumps
//!   it, and a client refuses a daemon whose version it does not know.
//! - Every method except [`method::Hello`] and the `agent.*` group names an identity, and its
//!   result holds rows of that identity only (#2, rule 4). An `agent.*` method is bound to one
//!   identity by its credential ([`agent`]).
//! - [`schema`] is the JSON Schema of every method's params and result; `cargo xtask gen-api`
//!   writes it to `schema/bosun-api.schema.json` and CI fails when it is stale.
//!
//! # Wire rules (#12)
//!
//! - Every enum is tagged by an explicit `type` field; none is untagged.
//! - Integers that can exceed 2^53 are decimal strings, by type ([`Wide`], [`Picodollars`]).
//! - Timestamps are integer milliseconds UTC (`*_ms`).
//!
//! # Methods
//!
//! | Method | Params | Result |
//! | --- | --- | --- |
//! | `hello` | [`HelloParams`] | [`HelloResult`] |
//! | `status.daemon` | [`IdentityParams`] | [`DaemonStatus`] |
//! | `ingest.status` | [`IdentityParams`] | [`IngestStatus`] |
//! | `ingest.reingest` | [`IdentityParams`] | [`ReingestResult`] |
//! | `tokens.by_session` | [`TokensParams`] | [`TokensBySession`] |
//! | `tokens.by_repo` | [`TokensParams`] | [`TokensByRepo`] |
//! | `tokens.by_model` | [`TokensParams`] | [`TokensByModel`] |
//! | `report.exact` | [`ReportParams`] | [`Report`] |
//! | `report.estimates` | [`ReportParams`] | [`Estimates`] |
//! | `heat.metrics` | [`ReportParams`] | [`HeatMetrics`] |
//! | `heat.evaluate` | [`HeatEvaluateParams`] | [`HeatEvaluation`] |
//! | `heat.list` | [`HeatListParams`] | [`HeatProposals`] |
//! | `heat.dismiss` | [`HeatDismissParams`] | [`ProposalView`] |
//! | `events.subscribe` | [`SubscribeParams`] | [`SubscribeResult`], then the notifications of [`event`] |
//! | `agent.hello` | [`AgentHelloParams`] | [`AgentHelloResult`] |
//! | `agent.ask_question` | [`AskQuestionParams`] | [`AgentAck`] |
//! | `agent.submit_summary` | [`SubmitSummaryParams`] | [`AgentAck`] |
//! | `agent.note` | [`NoteParams`] | [`AgentAck`] |
//! | `control.status` | [`IdentityParams`] | [`ControlStatus`] |
//! | `control.pause` | [`PauseParams`] | [`ControlStatus`] |
//! | `control.resume` | [`PauseParams`] | [`ControlStatus`] |
//! | `control.kill` | [`KillParams`] | [`KillResult`] |
//! | `status.overview` | [`IdentityParams`] | [`Overview`] |
//! | `queue.get` | [`QueueParams`] | [`QueueView`] |
//! | `queue.lint` | [`LintParams`] | [`LintResult`] |
//! | `item.get` | [`ItemParams`] | [`ItemDetail`] |
//! | `item.restart` | [`RestartParams`] | [`RestartResult`] |
//! | `session.get` | [`SessionParams`] | [`SessionInfo`] |
//! | `session.tail` | [`TailParams`] | [`SessionTail`] |
//! | `tokens.pace` | [`PaceParams`] | [`Pace`] |
//! | `lease.acquire` | [`AcquireParams`] | [`AcquireResult`] |
//! | `lease.release` | [`ReleaseParams`] | [`ReleaseResult`] |
//! | `lease.list` | [`IdentityParams`] | [`Resources`] |
//! | `doctor.probe` | [`IdentityParams`] | [`ProbeResult`] |
//! | `notify.channel.add` | [`ChannelAddParams`] | [`ChannelAdded`] |
//! | `notify.channel.list` | [`IdentityParams`] | [`Channels`] |
//! | `notify.channel.remove` | [`ChannelParams`] | [`ChannelRemoved`] |
//! | `notify.channel.test` | [`ChannelParams`] | [`ChannelTested`] |
//!
//! The `agent.*` methods name no identity: their credential does ([`agent`]). `control.kill`
//! names none either: the kill switch is daemon-wide. The `control.*` methods that change
//! anything take the operator token ([`control`]), and so do `item.restart`, `heat.dismiss`, the `lease.*`
//! methods ([`lease`]) and the `notify.channel.*` methods that change or send anything
//! ([`notify`]); `lease.release` names no identity: its holder is a process.

pub mod agent;
pub mod control;
pub mod doctor;
mod estimates;
pub mod event;
mod heat;
pub mod item;
mod items;
pub mod lease;
pub mod notify;
pub mod pace;
pub mod queue;
mod report;
pub mod rpc;
pub mod status;
mod wire;

use schemars::generate::SchemaSettings;
use schemars::{JsonSchema, SchemaGenerator};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};

pub use agent::{
    AgentAck, AgentHelloParams, AgentHelloResult, AskQuestionParams, NoteParams,
    SubmitSummaryParams,
};
pub use control::{ControlStatus, KillParams, KillResult, PauseParams, RepoControl};
pub use doctor::ProbeResult;
pub use estimates::{
    ByFile, ByTool, Estimates, FileRow, Finding, FindingRow, RepeatedRead, RepeatedReads,
    ResultCost, ToolRow,
};
pub use event::{
    Event, EventBody, EventFilter, OutputStream, Resync, ResyncReason, SubscribeParams,
    SubscribeResult, Tail,
};
pub use heat::{
    HeatDismissParams, HeatEvaluateParams, HeatEvaluation, HeatInsufficient, HeatListParams,
    HeatMetric, HeatMetrics, HeatProposals, HeatRepoEvaluation, ProposalView,
};
pub use item::{
    AgentCallView, AttemptView, ContextSize, ItemDetail, ItemParams, RestartParams, RestartResult,
    SessionInfo, SessionParams, SessionTail, SnapshotView, TailParams, TransitionView, VerdictView,
};
pub use items::{
    AttemptReport, DeniedCall, Evidence, FlakyVerdict, InferredItem, InferredItems,
    InferredSession, ItemRow, Items, LeaseWaits, ParkedTime, RefusedEdit, RetriedItem, RunCommit,
    RunSession, TrailerMissing, Trailers, WorktreeUse,
};
pub use lease::{
    AcquireParams, AcquireResult, BUSY, ReleaseParams, ReleaseResult, ResourceView, Resources,
    WaitingFor, WorktreeView,
};
pub use notify::{
    ChannelAddParams, ChannelAdded, ChannelParams, ChannelRemoved, ChannelTested, ChannelView,
    Channels,
};
pub use pace::{Pace, PaceParams, PaceReference};
pub use queue::{
    CardView, ColumnView, LintFinding, LintParams, LintResult, QueueParams, QueueView, RepoQueue,
};
pub use report::{
    AbsentModel, Agreement, CategoryRow, CheckTokens, CommitPeriod, CommitRow, CommitScan,
    Comparison, ContextCurve, Health, KindRow, OldMethod, PerCommit, Report, ReportParams,
    ScanState, SessionAgreement, Trend, Undercount, VersionRow, WindowCurve,
};
pub use status::{
    CurrentItem, LastVerdict, OpenBlocker, Overview, RepoOverview, StreamStatus, UnhealthyResource,
    WaitingCard,
};
pub use wire::{Picodollars, Wide};

/// The version of this API. A client refuses a daemon that answers [`method::Hello`] with a version it
/// does not know.
pub const API_VERSION: u32 = 1;

/// A method of the API: its wire name, its params and its result.
pub trait Method {
    /// The `method` member of the request.
    const NAME: &'static str;
    /// What the method does, for the schema.
    const DOC: &'static str;
    type Params: Serialize + DeserializeOwned + JsonSchema;
    type Result: Serialize + DeserializeOwned + JsonSchema;
}

/// The method types: each names a method and its params and result.
pub mod method {
    use super::Method;

    macro_rules! method {
    ($(#[$meta:meta])* $ty:ident, $name:literal, $params:ty, $result:ty, $doc:literal) => {
        $(#[$meta])*
        #[doc = $doc]
        #[derive(Debug, Clone, Copy)]
        pub struct $ty;

        impl Method for $ty {
            const NAME: &'static str = $name;
            const DOC: &'static str = $doc;
            type Params = $params;
            type Result = $result;
        }
    };
}

    method!(
        Hello,
        "hello",
        crate::HelloParams,
        crate::HelloResult,
        "The API version and the daemon's own version. The only method that names no identity."
    );
    method!(
        StatusDaemon,
        "status.daemon",
        crate::IdentityParams,
        crate::DaemonStatus,
        "The daemon's version, process and start time, and whether it serves the identity."
    );
    method!(
        IngestStatus,
        "ingest.status",
        crate::IdentityParams,
        crate::IngestStatus,
        "The identity's transcript files, how far they are parsed, and the last ingest pass."
    );
    method!(
        IngestReingest,
        "ingest.reingest",
        crate::IdentityParams,
        crate::ReingestResult,
        "Re-reads every transcript file of the identity from zero with the current parser, then \
     reads the new commits of its repositories, and answers when both are done. Nothing is \
     deleted."
    );
    method!(
        TokensBySession,
        "tokens.by_session",
        crate::TokensParams,
        crate::TokensBySession,
        "Token sums of the identity's messages in the range, per context window (a session's main \
     transcript or one of its sub-agents)."
    );
    method!(
        TokensByRepo,
        "tokens.by_repo",
        crate::TokensParams,
        crate::TokensByRepo,
        "Token sums of the identity's messages in the range, per repository the working \
     directory is in."
    );
    method!(
        TokensByModel,
        "tokens.by_model",
        crate::TokensParams,
        crate::TokensByModel,
        "Token sums of the identity's messages in the range, per model."
    );
    method!(
        ReportExact,
        "report.exact",
        crate::ReportParams,
        crate::Report,
        "The exact sections of the report over the range: eqv by category, model and session \
     kind, eqv per commit against the previous period, the old method's number, the context \
     curve per context window, and ingest health with the agreement with the provider's own \
     totals. No estimate."
    );
    method!(
        ReportEstimates,
        "report.estimates",
        crate::ReportParams,
        crate::Estimates,
        "The estimate sections of the report over the range: the cost of tool results by tool \
     and by file with cache carry, repeated reads of a file, and the cost of finding before a \
     session's first write. Every number is an estimate and is never part of an exact total."
    );
    method!(
        HeatMetrics,
        "heat.metrics",
        crate::ReportParams,
        crate::HeatMetrics,
        "The generic metrics of each repository over the range, per provider and session kind:      eqv per commit, context at session start, repeated reads and the cost of finding per      session, the share of cache reads, the tool error rate, time parked and the flaky verdict      rate per closed item. Each says whether it is exact or an estimate and whether it compares      across providers."
    );
    method!(
        HeatEvaluate,
        "heat.evaluate",
        crate::HeatEvaluateParams,
        crate::HeatEvaluation,
        "Runs heat's detectors now over the 14 days before, for one repository of the identity      or each of them, and reconciles their findings with the stored proposals: a finding seen      in two evaluations in a row becomes a proposal, within 3 new per evaluation and 10 active      per repository. Returns what it did, per repository."
    );
    method!(
        HeatList,
        "heat.list",
        crate::HeatListParams,
        crate::HeatProposals,
        "The identity's heat proposals: active ones (open, published, landed), or every state      with `all`. Each holds its metric, value, threshold, target and window in exact units."
    );
    method!(
        HeatDismiss,
        "heat.dismiss",
        crate::HeatDismissParams,
        crate::ProposalView,
        "Dismisses an open or published heat proposal of the identity, by its key: its subject      is then not proposed again for 90 days. Takes the operator token."
    );
    method!(
        EventsSubscribe,
        "events.subscribe",
        crate::SubscribeParams,
        crate::SubscribeResult,
        "Subscribes to the identity's events from `from_seq` of `epoch`, or from now. The \
     connection then carries the `events.event` and `events.resync` notifications in \
     `x-notifications` and takes no more requests."
    );
    method!(
        AgentHello,
        "agent.hello",
        crate::AgentHelloParams,
        crate::AgentHelloResult,
        "The handshake of an agent session's `bosun-mcp`: what its credential is bound to. Refused \
     for a credential that is not live or names another item."
    );
    method!(
        AgentAskQuestion,
        "agent.ask_question",
        crate::AskQuestionParams,
        crate::AgentAck,
        "Records a question of the agent with its attempt and HEAD. A blocking one sends the item \
     to Needs you and ends the session after a short grace period; one that does not block goes \
     into the item report."
    );
    method!(
        AgentSubmitSummary,
        "agent.submit_summary",
        crate::SubmitSummaryParams,
        crate::AgentAck,
        "Records the agent's summary with its attempt and HEAD; the last one of an attempt wins. \
     It changes no state, whatever it says."
    );
    method!(
        AgentNote,
        "agent.note",
        crate::NoteParams,
        crate::AgentAck,
        "Records a note of the agent with its attempt and HEAD; it changes no state."
    );
    method!(
        ControlStatus,
        "control.status",
        crate::IdentityParams,
        crate::ControlStatus,
        "The control state of the identity and its repositories: the kill switch, pauses by hand \
     and automatic ones, the daily cap, and the hold in force on each repository."
    );
    method!(
        ControlPause,
        "control.pause",
        crate::PauseParams,
        crate::ControlStatus,
        "Pauses one repository of the identity, or all of them: the current attempt finishes and \
     no new session starts until resume. Takes the operator token."
    );
    method!(
        ControlResume,
        "control.resume",
        crate::PauseParams,
        crate::ControlStatus,
        "Lifts a pause by hand of one repository or of the identity; for a repository, also its \
     automatic pause until the next interactive activity. Takes the operator token."
    );
    method!(
        ControlKill,
        "control.kill",
        crate::KillParams,
        crate::KillResult,
        "The kill switch: every child is killed now, every item parks, and no further board write \
     is made until it is released (`release`). Daemon-wide. Takes the operator token."
    );
    method!(
        StatusOverview,
        "status.overview",
        crate::IdentityParams,
        crate::Overview,
        "The Overview of the identity: per repository its state, the item in flight with its \
     attempt and time in state, the last verdict, and the control state."
    );
    method!(
        QueueGet,
        "queue.get",
        crate::QueueParams,
        crate::QueueView,
        "The board of one repository of the identity, or of each: its columns in board order, its \
     hash, each card's state and snapshot hash, whether an edit waits for the next claim, and the \
     lint findings of the cards in Ready."
    );
    method!(
        QueueLint,
        "queue.lint",
        crate::LintParams,
        crate::LintResult,
        "The static lint of one card of a repository of the identity, read fresh from its board."
    );
    method!(
        ItemGet,
        "item.get",
        crate::ItemParams,
        crate::ItemDetail,
        "One item of the identity: its state, snapshot, transitions, attempts with their sessions \
     and verdict runs, and its agents' calls."
    );
    method!(
        ItemRestart,
        "item.restart",
        crate::RestartParams,
        crate::RestartResult,
        "Restarts an item in flight: puts the `restart` label on its card, so the loop kills its \
     session, takes a new snapshot and starts again. Refused while the kill switch is thrown. \
     Takes the operator token."
    );
    method!(
        SessionGet,
        "session.get",
        crate::SessionParams,
        crate::SessionInfo,
        "One session bosun spawned, by provider session id or by item (its latest), and whether it \
     still runs: what `session tail` follows."
    );
    method!(
        SessionTail,
        "session.tail",
        crate::TailParams,
        crate::SessionTail,
        "The output lines of an item's sessions the daemon still holds, contiguous up to the \
     stream's head, with the item's latest session and its context size: what a live tail \
     refetches after a resync."
    );
    method!(
        TokensPace,
        "tokens.pace",
        crate::PaceParams,
        crate::Pace,
        "Eqv per minute over the last hour, the odometers of today and of this run of the daemon, and the reference pace with where it came from (the configured daily budget until limit windows are measured)."
    );
    method!(
        LeaseAcquire,
        "lease.acquire",
        crate::AcquireParams,
        crate::AcquireResult,
        "Asks for the leases of the named resources for a running process (a `bosun with` \
     command): the first call files the request, later calls with the same pid and set report \
     whether it is granted. Never says who holds a resource. Takes the operator token."
    );
    method!(
        LeaseRelease,
        "lease.release",
        crate::ReleaseParams,
        crate::ReleaseResult,
        "Releases what a process holds and withdraws its request; releasing nothing is not an \
     error. Takes the operator token."
    );
    method!(
        LeaseList,
        "lease.list",
        crate::IdentityParams,
        crate::Resources,
        "The resource instances the identity can lease, with their holders and waiting requests, \
     and its worktrees on disk or quarantined. Another identity's holder is shown only as `busy`."
    );
    method!(
        DoctorProbe,
        "doctor.probe",
        crate::IdentityParams,
        crate::ProbeResult,
        "Starts the daemon's `gh` once (`gh --version`) and says whether it ran: a daemon that \
     cannot start children fails here (#131). Reads nothing of the identity's; needs no operator \
     token."
    );
    method!(
        NotifyChannelAdd,
        "notify.channel.add",
        crate::ChannelAddParams,
        crate::ChannelAdded,
        "Adds a notification channel to the identity: its kind, non-secret parameters, events and      minimum gap in its database, its secret in the OS credential store under      `notify/<identity>/<name>`. The secret is in no row, log, error or result. Takes the      operator token."
    );
    method!(
        NotifyChannelList,
        "notify.channel.list",
        crate::IdentityParams,
        crate::Channels,
        "The identity's live notification channels, each with its credential-store entry and      whether the entry is present, never the secret. Needs no operator token."
    );
    method!(
        NotifyChannelRemove,
        "notify.channel.remove",
        crate::ChannelParams,
        crate::ChannelRemoved,
        "Removes a notification channel of the identity: deletes its credential-store entry, then      marks its row removed. Takes the operator token."
    );
    method!(
        NotifyChannelTest,
        "notify.channel.test",
        crate::ChannelParams,
        crate::ChannelTested,
        "Sends a test message on a notification channel of the identity now, outside any outbox,      and says whether its service accepted it. Takes the operator token."
    );
}

/// The names of every method, in the order of the module documentation.
pub const METHODS: &[&str] = &[
    method::Hello::NAME,
    method::StatusDaemon::NAME,
    method::IngestStatus::NAME,
    method::IngestReingest::NAME,
    method::TokensBySession::NAME,
    method::TokensByRepo::NAME,
    method::TokensByModel::NAME,
    method::ReportExact::NAME,
    method::ReportEstimates::NAME,
    method::HeatMetrics::NAME,
    method::HeatEvaluate::NAME,
    method::HeatList::NAME,
    method::HeatDismiss::NAME,
    method::EventsSubscribe::NAME,
    method::AgentHello::NAME,
    method::AgentAskQuestion::NAME,
    method::AgentSubmitSummary::NAME,
    method::AgentNote::NAME,
    method::ControlStatus::NAME,
    method::ControlPause::NAME,
    method::ControlResume::NAME,
    method::ControlKill::NAME,
    method::StatusOverview::NAME,
    method::QueueGet::NAME,
    method::QueueLint::NAME,
    method::ItemGet::NAME,
    method::ItemRestart::NAME,
    method::SessionGet::NAME,
    method::SessionTail::NAME,
    method::TokensPace::NAME,
    method::LeaseAcquire::NAME,
    method::LeaseRelease::NAME,
    method::LeaseList::NAME,
    method::DoctorProbe::NAME,
    method::NotifyChannelAdd::NAME,
    method::NotifyChannelList::NAME,
    method::NotifyChannelRemove::NAME,
    method::NotifyChannelTest::NAME,
];

/// The prefix of the `agent.*` group: the only methods an agent credential admits.
pub const AGENT_GROUP: &str = "agent.";

/// Params of `hello`: none yet.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct HelloParams {
    /// Mutant `mutant_api_extra_field` (#24): a field added without regenerating the schema.
    #[cfg(mutant_api_extra_field)]
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub extra: Option<u32>,
}

/// Result of `hello`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct HelloResult {
    /// The API version of the daemon (`bosun_api::API_VERSION`).
    pub api_version: u32,
    /// The daemon's package version.
    pub daemon_version: String,
    /// The daemon's process id.
    pub pid: u32,
}

/// Params of a method that names only an identity.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct IdentityParams {
    /// The identity's name in the configuration.
    pub identity: String,
}

/// Result of `status.daemon`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct DaemonStatus {
    pub identity: String,
    pub api_version: u32,
    pub daemon_version: String,
    pub pid: u32,
    /// When the daemon started.
    pub started_ms: i64,
}

/// Result of `ingest.status`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct IngestStatus {
    pub identity: String,
    /// The identity's database file.
    pub database: String,
    /// The directory its transcripts are discovered in.
    pub transcripts: String,
    /// Source files known to the store (transcripts and their meta files).
    pub files: u64,
    /// Their sizes when last seen, summed.
    pub size: Wide,
    /// Their parsed offsets, summed: what is in the store.
    pub parsed: Wide,
    /// Bytes of complete lines parsed since the daemon started.
    pub parsed_since_start: Wide,
    /// The last timed pass.
    pub last_pass: LastPass,
    /// What tells that the transcript format moved under the parser. Default when the daemon
    /// is older than the field.
    #[serde(default)]
    pub drift: IngestDrift,
}

/// The lines of unknown record types and the usage-bearing lines that billed nothing, over an
/// identity's source files. Lines, not API messages: a message split over several lines
/// counts once per line.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct IngestDrift {
    /// Files whose lines and drift are all counted.
    pub counted_files: u64,
    /// Files parsed before the counts existed: counted after `bosun reingest`.
    pub uncounted_files: u64,
    /// Lines parsed, summed over the counted files.
    pub lines: u64,
    /// Lines of record types the parser does not know, summed over the counted files.
    pub unknown: u64,
    /// Lines of the usage-bearing record type, billed or not, summed over the counted files.
    pub usage: u64,
    /// Those of `usage` that billed nothing, summed over the counted files.
    pub unbilled: u64,
    /// The counted files with unknown or unbilled lines: most unbilled first, then most
    /// unknown, then by path; at most [`IngestDrift::MAX_FILES`].
    pub files: Vec<DriftedFile>,
    /// The drifted files past `files`.
    #[serde(default)]
    pub more_files: u64,
}

impl IngestDrift {
    /// The most drifted files `ingest.status` names.
    pub const MAX_FILES: usize = 10;
}

/// A counted source file with unknown or unbilled lines.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct DriftedFile {
    pub path: String,
    /// Lines of record types the parser does not know.
    pub unknown: u64,
    /// Lines of the usage-bearing record type.
    pub usage: u64,
    /// Those of `usage` that billed nothing.
    pub unbilled: u64,
}

/// The outcome of the last timed ingest pass.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum LastPass {
    /// No pass has finished since the daemon started.
    Pending,
    /// The pass finished; some files may have failed on their own.
    Done(PassReport),
    /// The pass stopped: the store failed.
    Failed { error: String },
}

/// What one pass over an identity's files did.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct PassReport {
    /// Transcript files found.
    pub files: u64,
    /// Bytes of complete lines parsed in the pass.
    pub parsed_bytes: Wide,
    /// Files re-read from zero.
    pub restarted: u64,
    /// Files that could not be read; the pass went on without them.
    pub failed: Vec<FailedFile>,
}

/// A file a pass could not read.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct FailedFile {
    pub path: String,
    pub error: String,
}

/// Result of `ingest.reingest`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ReingestResult {
    pub identity: String,
    pub pass: PassReport,
}

/// Params of `tokens.*`: an identity and a half-open time range of message timestamps.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct TokensParams {
    pub identity: String,
    /// Messages at or after this time; unbounded when absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub from_ms: Option<i64>,
    /// Messages before this time; unbounded when absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub to_ms: Option<i64>,
}

/// Sums over a set of messages. Raw token counts are exact whatever the prices; the cost and
/// eqv cover the priced messages only.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct TokenSums {
    /// API messages (distinct message ids), not transcript records.
    pub messages: u64,
    /// Uncached input.
    pub input: u64,
    /// Input written to the 5-minute cache.
    pub cache_write_5m: u64,
    /// Input written to the 1-hour cache.
    pub cache_write_1h: u64,
    /// Input read from the cache.
    pub cache_read: u64,
    pub output: u64,
    /// The cost of the priced messages, each at the price valid at its time.
    pub cost: Picodollars,
    /// `cost` in input-equivalent tokens of the reference model, rounded for display.
    pub eqv: f64,
    /// Messages of a model with no price at their time: in the token counts, not in the cost.
    pub unpriced_messages: u64,
}

/// One context window of `tokens.by_session`. A message counts in the window where it was
/// first seen. These fields are what `recount --compare` reads (shape version 1, #16).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct SessionRow {
    /// The provider's session id.
    pub session: String,
    /// The sub-agent id; `null` for the session's main transcript.
    pub agent: Option<String>,
    pub sums: TokenSums,
}

/// One repository of `tokens.by_repo`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct RepoRow {
    /// `null` for messages whose working directory is in no configured repository.
    pub repo: Option<RepoRef>,
    pub sums: TokenSums,
}

/// A repository as configured.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct RepoRef {
    /// Its name in the configuration; `null` when the store knows a root the configuration no
    /// longer names.
    pub name: Option<String>,
    /// Its root as configured, or the store's key of it when the configuration no longer
    /// names it.
    pub path: String,
}

/// One model of `tokens.by_model`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct ModelRow {
    /// The model id as transcripts name it.
    pub model: String,
    pub sums: TokenSums,
}

/// A model with no price at the time of some messages.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct UnpricedModel {
    pub model: String,
    pub messages: u64,
}

/// What every `tokens.*` result holds besides its rows.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct TokensSummary {
    pub identity: String,
    /// The range asked for.
    pub from_ms: Option<i64>,
    pub to_ms: Option<i64>,
    /// The model every eqv is measured against.
    pub reference_model: String,
    /// The sums over every row.
    pub total: TokenSums,
    /// Models with no price at the time of some of their messages, by name.
    pub unpriced: Vec<UnpricedModel>,
}

/// Result of `tokens.by_session`. Rows are sorted by session, then agent (`null` first).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct TokensBySession {
    pub summary: TokensSummary,
    pub rows: Vec<SessionRow>,
}

/// Result of `tokens.by_repo`. Rows are sorted by path, with `null` last.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct TokensByRepo {
    pub summary: TokensSummary,
    pub rows: Vec<RepoRow>,
}

/// Result of `tokens.by_model`. Rows are sorted by model.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct TokensByModel {
    pub summary: TokensSummary,
    pub rows: Vec<ModelRow>,
}

fn add_method<M: Method>(generator: &mut SchemaGenerator, methods: &mut Map<String, Value>) {
    let params = generator.subschema_for::<M::Params>();
    let result = generator.subschema_for::<M::Result>();
    methods.insert(
        M::NAME.to_owned(),
        json!({ "description": M::DOC, "params": params, "result": result }),
    );
}

/// The JSON Schema (draft 2020-12) of the API: `x-api-version` is [`API_VERSION`],
/// `x-methods` maps each method name to the schemas of its params and result,
/// `x-notifications` maps each notification the daemon sends to the schema of its params, and
/// `$defs` holds every type they reference.
#[must_use]
pub fn schema() -> Value {
    let mut generator = SchemaSettings::draft2020_12().into_generator();
    let mut methods = Map::new();
    add_method::<method::Hello>(&mut generator, &mut methods);
    add_method::<method::StatusDaemon>(&mut generator, &mut methods);
    add_method::<method::IngestStatus>(&mut generator, &mut methods);
    add_method::<method::IngestReingest>(&mut generator, &mut methods);
    add_method::<method::TokensBySession>(&mut generator, &mut methods);
    add_method::<method::TokensByRepo>(&mut generator, &mut methods);
    add_method::<method::TokensByModel>(&mut generator, &mut methods);
    add_method::<method::ReportExact>(&mut generator, &mut methods);
    add_method::<method::ReportEstimates>(&mut generator, &mut methods);
    add_method::<method::HeatMetrics>(&mut generator, &mut methods);
    add_method::<method::HeatEvaluate>(&mut generator, &mut methods);
    add_method::<method::HeatList>(&mut generator, &mut methods);
    add_method::<method::HeatDismiss>(&mut generator, &mut methods);
    add_method::<method::EventsSubscribe>(&mut generator, &mut methods);
    add_method::<method::AgentHello>(&mut generator, &mut methods);
    add_method::<method::AgentAskQuestion>(&mut generator, &mut methods);
    add_method::<method::AgentSubmitSummary>(&mut generator, &mut methods);
    add_method::<method::AgentNote>(&mut generator, &mut methods);
    add_method::<method::ControlStatus>(&mut generator, &mut methods);
    add_method::<method::ControlPause>(&mut generator, &mut methods);
    add_method::<method::ControlResume>(&mut generator, &mut methods);
    add_method::<method::ControlKill>(&mut generator, &mut methods);
    add_method::<method::StatusOverview>(&mut generator, &mut methods);
    add_method::<method::QueueGet>(&mut generator, &mut methods);
    add_method::<method::QueueLint>(&mut generator, &mut methods);
    add_method::<method::ItemGet>(&mut generator, &mut methods);
    add_method::<method::ItemRestart>(&mut generator, &mut methods);
    add_method::<method::SessionGet>(&mut generator, &mut methods);
    add_method::<method::SessionTail>(&mut generator, &mut methods);
    add_method::<method::TokensPace>(&mut generator, &mut methods);
    add_method::<method::LeaseAcquire>(&mut generator, &mut methods);
    add_method::<method::LeaseRelease>(&mut generator, &mut methods);
    add_method::<method::LeaseList>(&mut generator, &mut methods);
    add_method::<method::DoctorProbe>(&mut generator, &mut methods);
    add_method::<method::NotifyChannelAdd>(&mut generator, &mut methods);
    add_method::<method::NotifyChannelList>(&mut generator, &mut methods);
    add_method::<method::NotifyChannelRemove>(&mut generator, &mut methods);
    add_method::<method::NotifyChannelTest>(&mut generator, &mut methods);
    let notifications = json!({
        event::EVENT: {
            "description": "One event of the subscribed identity, in sequence order.",
            "params": generator.subschema_for::<Event>(),
        },
        event::RESYNC: {
            "description": "The subscriber missed events: it refetches its state; the daemon \
                            closes the connection.",
            "params": generator.subschema_for::<Resync>(),
        },
    });
    let defs = generator.take_definitions(true);
    json!({
        "$schema": "https://json-schema.org/draft/2020-12/schema",
        "title": "bosun local API",
        "description": "Generated from the bosun-api crate by `cargo xtask gen-api`; do not \
                        edit. Transport: JSON-RPC 2.0, one message per line, on a Unix socket \
                        or a Windows named pipe.",
        "x-api-version": API_VERSION,
        "x-methods": methods,
        "x-notifications": notifications,
        "$defs": defs,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_method_is_in_the_schema() {
        let schema = schema();
        let methods = schema["x-methods"].as_object().unwrap();
        let names: Vec<&str> = methods.keys().map(String::as_str).collect();
        let mut expected = METHODS.to_vec();
        expected.sort_unstable();
        assert_eq!(names, expected);
        assert_eq!(schema["x-api-version"], API_VERSION);
    }

    /// Every enum in the schema is tagged by `type` (#12): each `oneOf` branch requires a
    /// `type` property with a constant value.
    #[test]
    fn every_enum_is_tagged_by_type() {
        fn walk(value: &Value, found: &mut usize) {
            match value {
                Value::Object(object) => {
                    if let Some(branches) = object.get("oneOf").and_then(Value::as_array) {
                        for branch in branches {
                            assert!(
                                branch["properties"]["type"].get("const").is_some(),
                                "an untagged branch: {branch}"
                            );
                            assert!(
                                branch["required"]
                                    .as_array()
                                    .is_some_and(|r| r.contains(&"type".into())),
                                "{branch}"
                            );
                            *found += 1;
                        }
                    }
                    assert!(object.get("anyOf").is_none_or(|any| {
                        // `Option<T>` of a referenced type is `anyOf [T, null]`; nothing else is.
                        any.as_array()
                            .is_some_and(|a| a.len() == 2 && a.iter().any(|v| v["type"] == "null"))
                    }));
                    object.values().for_each(|v| walk(v, found));
                }
                Value::Array(items) => items.iter().for_each(|v| walk(v, found)),
                _ => {}
            }
        }
        let mut found = 0;
        walk(&schema(), &mut found);
        assert!(
            found > 0,
            "no tagged enum was found: the test checks nothing"
        );
    }

    #[test]
    fn last_pass_is_tagged() {
        let pending = serde_json::to_value(LastPass::Pending).unwrap();
        assert_eq!(pending, json!({"type": "pending"}));
        let failed = serde_json::to_value(LastPass::Failed {
            error: "x".to_owned(),
        })
        .unwrap();
        assert_eq!(failed, json!({"type": "failed", "error": "x"}));
        let done = LastPass::Done(PassReport {
            files: 1,
            parsed_bytes: Wide(2),
            restarted: 0,
            failed: vec![],
        });
        let value = serde_json::to_value(&done).unwrap();
        assert_eq!(value["type"], "done");
        assert_eq!(value["parsed_bytes"], "2");
        assert_eq!(serde_json::from_value::<LastPass>(value).unwrap(), done);
    }
}
