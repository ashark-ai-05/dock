use base64::{Engine as _, engine::general_purpose::STANDARD};
use serde::{Deserialize, Serialize};

use crate::{
    adapter::{AdapterCapabilities, AdapterId, AdapterSelection, ProcessCapabilities},
    layout::{LayoutSnapshot, PaneKind, SplitAxis, WorkspaceLayout},
    model::{HandoffPacket, HandoffRecord, ReviewDecision, ReviewRoute},
};
use dock_detect::{AgentKind, AgentState};

pub const PROTOCOL_VERSION: u16 = 18;
pub const MAX_MESSAGE_BYTES: u64 = 64 * 1024;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Request {
    Hello(HelloRequest),
    Inspect(InspectRequest),
    Dispatch(DispatchRequest),
    Lifecycle(LifecycleRequest),
    SubmitHandoff(SubmitHandoffRequest),
    ReportAgentState(ReportAgentStateRequest),
    ReviewInbox(ReviewInboxRequest),
    Decide(DecideRequest),
    QueueGated(QueueGatedRequest),
    ReleaseGate(ReleaseGateRequest),
    InspectProgramme(InspectProgrammeRequest),
    LaunchIntoPane(LaunchIntoPaneRequest),
    TerminalLaunch(TerminalLaunchRequest),
    Workspace(WorkspaceRequest),
    PaneInput(PaneInputRequest),
    PaneResize(PaneResizeRequest),
    Subscribe(SubscribeRequest),
    Queue(QueueRequest),
    PaneHistory(PaneHistoryRequest),
}

/// Dashboard-safe launch authority.
///
/// Its closed shape deliberately cannot carry repository, worktree, executable, argument,
/// environment, or shell data. The task reference it does carry is an opaque bounded label —
/// recorded in the binding and exported as `DOCK_TASK`, never resolved, never a path.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TerminalLaunchRequest {
    pub workspace_id: String,
    pub pane_id: String,
    pub run_id: String,
    pub profile: DashboardProfile,
    pub runtime_directory: String,
    /// Extra arguments for the launched agent, which is how a resume asks it to continue its most
    /// recent session rather than start a new one. Empty for an ordinary launch, and defaulted so
    /// every existing caller and stored request stays valid.
    #[serde(default)]
    pub arguments: Vec<String>,
    /// Which task on the board this run is for, so the pairing outlives the client that made it.
    ///
    /// Recorded here rather than remembered client-side because a dashboard that quits used to
    /// take the answer with it, and a second dashboard never had it at all. It selects nothing:
    /// the repository and worktree are still derived from `runtime_directory`, and this is echoed
    /// back and exported, never resolved. [`validate_external_task_ref`] is what keeps it that
    /// way.
    #[serde(default)]
    pub external_task_ref: String,
}

/// Refuses a task reference that could be anything but a label.
///
/// The shape above is a security boundary, so widening it comes with a bound: at most 64 bytes of
/// ASCII letters, digits, underscore and hyphen. No dot, no slash, no separator of any kind, so a
/// value that arrives here can never be walked into a path by anything downstream that forgets
/// what it was promised.
pub fn validate_external_task_ref(reference: &str) -> Result<(), String> {
    if reference.len() > 64 {
        return Err("external task reference must be at most 64 bytes".into());
    }
    if !reference
        .bytes()
        .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-')
    {
        return Err(
            "external task reference must be letters, digits, underscore or hyphen only".into(),
        );
    }
    Ok(())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum DashboardProfile {
    Fixture,
    Amp,
    ClaudeCode,
    CodexCli,
    GithubCopilotCli,
    Shell,
}

/// The profile a terminal launch names for an adapter, where one exists.
///
/// Not every adapter has one: `Generic` and the shell are launched by other routes and are not
/// offered as dashboard profiles, so resuming them is not a thing a terminal launch can express.
impl TryFrom<AdapterId> for DashboardProfile {
    type Error = ();

    fn try_from(value: AdapterId) -> Result<Self, Self::Error> {
        Ok(match value {
            AdapterId::Fixture => Self::Fixture,
            AdapterId::Amp => Self::Amp,
            AdapterId::ClaudeCode => Self::ClaudeCode,
            AdapterId::CodexCli => Self::CodexCli,
            AdapterId::GithubCopilotCli => Self::GithubCopilotCli,
            AdapterId::Shell => Self::Shell,
            AdapterId::Generic => return Err(()),
        })
    }
}

impl From<DashboardProfile> for AdapterId {
    fn from(value: DashboardProfile) -> Self {
        match value {
            DashboardProfile::Fixture => Self::Fixture,
            DashboardProfile::Amp => Self::Amp,
            DashboardProfile::ClaudeCode => Self::ClaudeCode,
            DashboardProfile::CodexCli => Self::CodexCli,
            DashboardProfile::GithubCopilotCli => Self::GithubCopilotCli,
            DashboardProfile::Shell => Self::Shell,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LaunchIntoPaneRequest {
    pub workspace_id: String,
    pub pane_id: String,
    pub dispatch: DispatchRequest,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PaneInputRequest {
    pub workspace_id: String,
    pub pane_id: String,
    pub input: String,
}

impl PaneInputRequest {
    /// Key bytes are base64 so raw control sequences survive JSON transport intact.
    pub fn encode(bytes: &[u8]) -> String {
        STANDARD.encode(bytes)
    }

    pub fn decode(&self) -> Result<Vec<u8>, String> {
        STANDARD
            .decode(&self.input)
            .map_err(|error| format!("pane input is not valid base64: {error}"))
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PaneResizeRequest {
    pub workspace_id: String,
    pub pane_id: String,
    pub rows: u16,
    pub cols: u16,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SubscribeRequest {}

/// A request for output older than the caller already holds.
///
/// The caller names the sequence it starts at rather than an offset or a line count, because
/// the log is addressed by byte sequence and a line count cannot survive a resize. `epoch` in
/// the response is what makes a stale cursor safe: a run that restarted has a new byte stream,
/// and a cursor from the old one names a position in it that means nothing.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PaneHistoryRequest {
    pub run_id: String,
    /// The sequence the caller's own history begins at. The answer ends exactly here.
    pub before: u64,
    /// An upper bound on the answer, clamped daemon-side to what the log can hold.
    pub max_bytes: u32,
}

/// Everything a client may ask of the per-pane prompt queues.
///
/// An inner tagged enum in the shape `WorkspaceRequest` already uses, because these are one
/// subsystem's operations rather than seven unrelated requests, and grouping them keeps the outer
/// `Request` readable as a list of subsystems.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "operation", rename_all = "snake_case", deny_unknown_fields)]
pub enum QueueRequest {
    /// Every queue the daemon holds, not one, so a board fills its runs lane in one round trip.
    Inspect,
    /// `prompt` is the literal text fed to the agent. The daemon never resolves a task id: it has
    /// never read a board file and this does not change that.
    Add {
        workspace_id: String,
        pane_id: String,
        prompt: String,
        label: String,
    },
    Remove {
        workspace_id: String,
        pane_id: String,
        entry_id: u64,
    },
    Clear {
        workspace_id: String,
        pane_id: String,
    },
    /// Arm or disarm auto-feed for one pane. `enabled: true` is refused when the pane's agent has
    /// never reported a state and the daemon is on the default trust setting.
    SetAuto {
        workspace_id: String,
        pane_id: String,
        enabled: bool,
    },
    /// The kill switch. Daemon-wide, persisted, and independent of every pane's own arming.
    SetPaused { paused: bool },
    /// Which "the agent finished" signal auto-feed will act on. Does not arm anything.
    SetTrust { trust: AutoFeedTrustSetting },
}

/// Wire form of [`crate::queue::AutoFeedTrust`]. Named here so a client can change it without
/// restarting dockd.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AutoFeedTrustSetting {
    #[default]
    Reported,
    Screen,
}

impl From<crate::queue::AutoFeedTrust> for AutoFeedTrustSetting {
    fn from(trust: crate::queue::AutoFeedTrust) -> Self {
        match trust {
            crate::queue::AutoFeedTrust::Reported => Self::Reported,
            crate::queue::AutoFeedTrust::Screen => Self::Screen,
        }
    }
}

impl From<AutoFeedTrustSetting> for crate::queue::AutoFeedTrust {
    fn from(trust: AutoFeedTrustSetting) -> Self {
        match trust {
            AutoFeedTrustSetting::Reported => Self::Reported,
            AutoFeedTrustSetting::Screen => Self::Screen,
        }
    }
}

/// One pane's queue as a listing sees it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PaneQueueSnapshot {
    pub workspace_id: String,
    pub pane_id: String,
    pub run_id: Option<String>,
    pub auto_feed: bool,
    pub awaiting_ack: bool,
    /// Why auto-feed last declined to fire, so a stalled queue explains itself instead of looking
    /// broken.
    pub holding_because: Option<String>,
    pub entries: Vec<QueueEntrySnapshot>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct QueueEntrySnapshot {
    pub entry_id: u64,
    pub label: String,
    /// First `QUEUE_PREVIEW_BYTES` of the prompt, never the whole thing: a full listing of sixteen
    /// 8 KiB prompts across several panes would exceed `MAX_MESSAGE_BYTES`.
    pub preview: String,
    pub bytes: usize,
}

/// One pane's queue on disk.
///
/// What is **not** here is the point of it: `auto_feed`, `awaiting_ack`, the last observed state,
/// the settle clock and the last feed time are all deliberately absent. Every one of them
/// describes the last few seconds of a process that no longer exists, and restoring them would
/// let a pre-restart observation authorise a post-restart feed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DurablePaneQueue {
    pub schema_version: u16,
    pub workspace_id: String,
    pub pane_id: String,
    pub next_entry_id: u64,
    pub entries: Vec<DurableQueueEntry>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DurableQueueEntry {
    pub entry_id: u64,
    pub label: String,
    pub prompt: String,
}

/// Pushed by the daemon to subscribed clients. Replaces polling entirely: an unchanged
/// pane produces no event at all, where the previous protocol re-sent full scrollback
/// for every run five times a second regardless of activity.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "snake_case", deny_unknown_fields)]
pub enum Event {
    /// A full screen snapshot, sent when a subscriber first sees a run, again whenever the
    /// pane's geometry changes, and again if a subscriber ever falls further behind the pane's
    /// output than the daemon retains. `rows`/`cols` are carried so a client that did not
    /// originate the resize can size its parser from this frame alone: without them a
    /// subscriber keeps its old geometry and renders only the tail rows of the snapshot.
    ///
    /// `scrollback_rows` is the daemon's own retention, so the replica this frame seeds keeps
    /// exactly the history the daemon keeps. A client cannot infer it: it is a `dockd` option,
    /// and a client that assumed the default would silently retain a different amount.
    PaneAttached {
        run_id: String,
        revision: u64,
        rows: u16,
        cols: u16,
        scrollback_rows: u32,
        /// The sequence the seeded bytes begin at: the caller's cursor for paging further
        /// back. Without it a client cannot name where its own history starts.
        history_from: u64,
        /// Identity of the byte stream these sequences belong to. A run that restarted gets a
        /// new one, so a client holding a cursor from before the restart discards it rather
        /// than paging into the middle of a different stream.
        epoch: u64,
        screen: String,
    },
    /// The raw bytes the pane's child wrote since this subscriber's last frame, so the
    /// client's parser scrolls exactly as the daemon's did and accumulates the same history.
    /// A repaint-style diff could never do that: it is cursor-addressed, and addressing a cell
    /// never scrolls a row into scrollback.
    PaneDelta {
        run_id: String,
        revision: u64,
        bytes: String,
    },
    PaneState {
        run_id: String,
        state: ProcessState,
    },
    AgentStateChanged {
        run_id: String,
        agent: Option<AgentKind>,
        state: AgentState,
        /// What the agent last said it was doing, when a hook named a tool.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        activity: Option<String>,
    },
    LayoutChanged,
    /// One pane's queue changed — an entry added, removed, or fed to its agent.
    ///
    /// Pushed rather than polled because, unlike agent state, queue depth lives only in the daemon:
    /// nothing else a subscriber already receives would tell it that a drain happened, so an open
    /// board would show a stale depth until the next unrelated keystroke.
    QueueChanged {
        workspace_id: String,
        pane_id: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HelloRequest {
    pub version: u16,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InspectRequest {
    #[serde(default)]
    pub run_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DispatchRequest {
    pub repository_root: String,
    pub external_task_ref: String,
    pub run_id: String,
    pub worktree: String,
    pub adapter: AdapterSelection,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LifecycleOperation {
    Attach,
    Focus,
    Interrupt,
    Stop,
    Restart,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LifecycleRequest {
    pub run_id: String,
    pub operation: LifecycleOperation,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SubmitHandoffRequest {
    pub packet: HandoffPacket,
}

/// An agent saying what it is doing, rather than Dock working it out from the screen.
///
/// Every agent CLI worth integrating has an event system — Claude Code fires `UserPromptSubmit`
/// when a turn starts, `Stop` when it ends, `PermissionRequest` when it needs a decision — and a
/// hook wired to those knows exactly what a pattern can only infer. A reported state is sticky:
/// it holds until the agent reports something else, because "finished" stays true until the next
/// turn starts, and a timeout would invent a transition nobody observed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReportAgentStateRequest {
    pub run_id: String,
    pub state: AgentState,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub transcript_path: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hook_event_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_input: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub activity: Option<String>,
    /// The identifying argument untruncated — a command line, a full path — for the receipt's
    /// observed column. `activity` stays a truncated summary for the roster; this is what a
    /// verdict rule matches against, so a client built against v17 (which never sends it) must
    /// still parse: additive and defaulted, never required.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_detail: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReviewInboxRequest {}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DecideRequest {
    pub run_id: String,
    pub route: ReviewRoute,
    pub note: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct QueueGatedRequest {
    pub dispatch: DispatchRequest,
    pub upstream_run_id: String,
    pub required_route: ReviewRoute,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReleaseGateRequest {
    pub downstream_run_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InspectProgrammeRequest {}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "operation", rename_all = "snake_case", deny_unknown_fields)]
pub enum WorkspaceRequest {
    Inspect,
    Create {
        workspace_id: String,
        name: String,
        pane_id: String,
    },
    Split {
        workspace_id: String,
        pane_id: String,
        new_pane_id: String,
        axis: SplitAxis,
        /// What the new half is for. Defaulted so a client built before board panes existed asks
        /// for exactly what it always asked for, and so this is one field on an existing request
        /// rather than a second request that means "split, but".
        #[serde(default)]
        kind: PaneKind,
    },
    Focus {
        workspace_id: String,
        pane_id: String,
    },
    Resize {
        workspace_id: String,
        pane_id: String,
        ratio_milli: u16,
    },
    Rename {
        workspace_id: String,
        #[serde(default)]
        pane_id: Option<String>,
        name: String,
    },
    Close {
        workspace_id: String,
        pane_id: String,
    },
    /// Gives a pane whose shell has exited a fresh Dock-owned shell. The keyboard recovery path
    /// out of an exited pane, so a pane that dies is never a pane the user cannot use again.
    Respawn {
        workspace_id: String,
        pane_id: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DurableProgrammeGate {
    pub schema_version: u16,
    pub dispatch: DispatchRequest,
    pub upstream_run_id: String,
    pub upstream_repository_id: String,
    pub downstream_repository_id: String,
    pub required_route: ReviewRoute,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum Response {
    Hello {
        version: u16,
    },
    Snapshot {
        snapshot: RuntimeSnapshot,
    },
    Snapshots {
        snapshots: Vec<RuntimeSnapshot>,
    },
    Dispatched {
        snapshot: RuntimeSnapshot,
    },
    LifecycleApplied {
        operation: LifecycleOperation,
        snapshot: RuntimeSnapshot,
    },
    HandoffSubmitted {
        record: HandoffRecord,
    },
    ReviewInbox {
        items: Vec<HandoffRecord>,
    },
    /// A report accepted. Carries nothing: the agent is telling Dock something, not asking.
    AgentStateRecorded {},
    DecisionRecorded {
        decision: ReviewDecision,
    },
    GateQueued {
        gate: DependencyGateSnapshot,
    },
    GateReleased {
        snapshot: RuntimeSnapshot,
    },
    Programme {
        portfolio: ProgrammeSnapshot,
    },
    Layout {
        layout: LayoutSnapshot,
    },
    WorkspaceChanged {
        workspace: Option<WorkspaceLayout>,
    },
    PaneInputAccepted {
        workspace_id: String,
        pane_id: String,
        bytes: usize,
    },
    /// One pushed event on a subscribed connection. The connection stays newline-delimited
    /// `Response` frames throughout, so a subscriber never needs a second parser.
    Stream {
        event: Event,
    },
    Queues {
        queues: Vec<PaneQueueSnapshot>,
        paused: bool,
        #[serde(default)]
        trust: AutoFeedTrustSetting,
    },
    /// Output older than the caller's cursor. `complete` says the answer reaches the oldest
    /// byte still retained, so there is nothing further back to ask for.
    PaneHistory {
        run_id: String,
        epoch: u64,
        from: u64,
        bytes: String,
        complete: bool,
    },
    /// Acknowledges a request that has no payload of its own to report, such as a resize.
    Ack,
    Error {
        code: ErrorCode,
        message: String,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorCode {
    MalformedRequest,
    ProtocolMismatch,
    HandshakeRequired,
    RequestTooLarge,
    RequestTimeout,
    ServerBusy,
    InvalidBinding,
    AdapterUnavailable,
    UnsupportedOperation,
    DuplicateRunId,
    RunNotFound,
    InvalidHandoff,
    DuplicateHandoff,
    HandoffNotFound,
    DecisionAlreadyRecorded,
    CapacityExceeded,
    GateNotFound,
    GateBlocked,
    /// A queue operation the daemon declined: a depth cap, an over-long prompt, or an arming
    /// request that cannot be honoured. Its own code rather than `GateBlocked`, which already
    /// carries five distinct meanings and would stop being useful for diagnosis at a sixth.
    QueueRefused,
    DuplicateGate,
    InvalidLayout,
    WorkspaceNotFound,
    PaneNotFound,
    Internal,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GateState {
    AwaitingHandoff,
    AwaitingDecision,
    DecisionMismatch,
    Ready,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DependencyGateSnapshot {
    pub upstream_run_id: String,
    pub downstream_run_id: String,
    pub upstream_repository_id: String,
    pub downstream_repository_id: String,
    pub required_route: ReviewRoute,
    pub state: GateState,
    pub validation_reason: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RepositoryPortfolioSnapshot {
    pub repository_id: String,
    pub active_run_ids: Vec<String>,
    pub queued_run_ids: Vec<String>,
    pub active_capacity: usize,
    pub run_capacity: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProgrammeSnapshot {
    pub global_active: usize,
    pub global_run_capacity: usize,
    pub human_review_reserved: usize,
    pub repositories: Vec<RepositoryPortfolioSnapshot>,
    pub gates: Vec<DependencyGateSnapshot>,
    /// Every pane queue, beside the gates, so `dock programme` shows both rather than making an
    /// operator hold two mental models of "queued work".
    pub queues: Vec<PaneQueueSnapshot>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RuntimeSnapshot {
    pub binding_kind: BindingKind,
    pub repository_root: String,
    pub external_task_ref: String,
    pub run_id: String,
    pub worktree: String,
    pub branch: String,
    pub base_sha: String,
    pub workspace_id: String,
    pub pane_id: String,
    pub state: ProcessState,
    pub pid: Option<u32>,
    pub process_group_id: Option<i32>,
    pub command: Vec<String>,
    pub adapter: AdapterId,
    pub process_capabilities: ProcessCapabilities,
    pub adapter_capabilities: AdapterCapabilities,
    pub provider_state: ProviderState,
    pub rows: u16,
    pub cols: u16,
    pub agent: Option<AgentKind>,
    pub agent_state: AgentState,
    pub title: Option<String>,
    pub cwd: Option<String>,
    pub diagnostic: Option<String>,
    /// Latest hook activity for this run, when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub activity: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BindingKind {
    Terminal,
    Repository,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderState {
    Running,
    Exited,
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProcessState {
    Running,
    Exited { code: Option<i32> },
    FailedToLaunch,
    Unavailable,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snapshot_fixture() -> RuntimeSnapshot {
        RuntimeSnapshot {
            binding_kind: BindingKind::Repository,
            repository_root: "/repo/real".into(),
            external_task_ref: "TASK-61".into(),
            run_id: "dock_real".into(),
            worktree: "/repo/real".into(),
            branch: "main".into(),
            base_sha: "abc".into(),
            workspace_id: "w".into(),
            pane_id: "a".into(),
            state: ProcessState::Running,
            pid: Some(1),
            process_group_id: Some(1),
            command: vec!["sh".into()],
            adapter: AdapterId::Fixture,
            process_capabilities: ProcessCapabilities::OWNED_RUNTIME,
            adapter_capabilities: AdapterCapabilities::NONE,
            provider_state: ProviderState::Running,
            rows: 24,
            cols: 80,
            agent: Some(AgentKind::Claude),
            agent_state: AgentState::Idle,
            title: Some("dock".into()),
            cwd: Some("/repo/real".into()),
            diagnostic: None,
            activity: None,
        }
    }

    #[test]
    fn a_task_reference_can_never_be_walked_into_a_path() {
        // The launch request's shape is a security boundary — it exists so a dashboard cannot ask
        // the daemon to run something of its choosing. Letting it carry a task reference widens
        // that shape, and this bound is the whole reason widening it is safe: the value is a
        // label, is recorded and echoed, and has no character in it that any downstream reader
        // could mistake for a path.
        for allowed in ["7", "TASK-61", "dock_task_9", "", &"a".repeat(64)] {
            assert!(validate_external_task_ref(allowed).is_ok(), "{allowed:?}");
        }
        for refused in [
            "../../etc/passwd",
            "/etc/passwd",
            "task/7",
            "task.7",
            "task 7",
            "task;rm -rf /",
            "task\n7",
            "täsk",
            &"a".repeat(65),
        ] {
            assert!(
                validate_external_task_ref(refused).is_err(),
                "{refused:?} must be refused"
            );
        }
    }

    /// The v12 wire shapes, written out, because a client is being wired to them in a separate
    /// change and a rename here would otherwise be discovered at runtime.
    #[test]
    fn a_queue_request_is_an_inner_tagged_operation_like_a_workspace_request() {
        let json = serde_json::to_string(&Request::Queue(QueueRequest::Add {
            workspace_id: "w1".into(),
            pane_id: "p1".into(),
            prompt: "keep going".into(),
            label: "card 7".into(),
        }))
        .expect("serialize");
        assert_eq!(
            json,
            r#"{"type":"queue","operation":"add","workspace_id":"w1","pane_id":"p1","prompt":"keep going","label":"card 7"}"#
        );
        let paused =
            serde_json::to_string(&Request::Queue(QueueRequest::SetPaused { paused: true }))
                .expect("serialize");
        assert_eq!(
            paused,
            r#"{"type":"queue","operation":"set_paused","paused":true}"#
        );
        assert_eq!(
            serde_json::from_str::<Request>(&json).expect("round trips"),
            Request::Queue(QueueRequest::Add {
                workspace_id: "w1".into(),
                pane_id: "p1".into(),
                prompt: "keep going".into(),
                label: "card 7".into(),
            })
        );
    }

    /// `QueueChanged` names a pane rather than a run, for the same reason the queue is keyed by
    /// one: a run dies and is replaced, and a client that had to follow the substitution to find
    /// out its queue moved would miss exactly the changes a restart caused.
    #[test]
    fn a_queue_changed_event_names_the_pane_rather_than_the_run() {
        let json = serde_json::to_string(&Event::QueueChanged {
            workspace_id: "w1".into(),
            pane_id: "p1".into(),
        })
        .expect("serialize");
        assert_eq!(
            json,
            r#"{"event":"queue_changed","workspace_id":"w1","pane_id":"p1"}"#
        );
    }

    #[test]
    fn the_protocol_version_records_the_pane_history_request() {
        assert_eq!(PROTOCOL_VERSION, 18);
    }

    /// The field is additive and defaulted, so a client built against v17 still parses.
    #[test]
    fn a_state_report_may_carry_the_untruncated_tool_detail() {
        assert_eq!(PROTOCOL_VERSION, 18);
        // Deserialized directly into the inner struct (not through the `Request` enum), so no
        // `type` tag field here — `ReportAgentStateRequest` carries `deny_unknown_fields` and has
        // no member by that name.
        let request: ReportAgentStateRequest = serde_json::from_str(
            r#"{"run_id":"dock_1","state":"working","session_id":"s","tool_name":"Bash","activity":"Bash git reset --hard","tool_detail":"git reset --hard HEAD~3"}"#,
        ).expect("parse v18 report");
        assert_eq!(
            request.tool_detail.as_deref(),
            Some("git reset --hard HEAD~3")
        );
        // A v17 report has no such field and must still parse.
        let older: ReportAgentStateRequest =
            serde_json::from_str(r#"{"run_id":"dock_1","state":"working","session_id":"s"}"#)
                .expect("parse v17 report");
        assert_eq!(older.tool_detail, None);
    }

    #[test]
    fn a_state_report_may_carry_hook_fields_and_still_accepts_argv_only_json() {
        let minimal: crate::protocol::ReportAgentStateRequest =
            serde_json::from_str(r#"{"run_id":"dock_1","state":"working"}"#)
                .expect("argv-only report");
        assert_eq!(minimal.run_id, "dock_1");
        assert_eq!(minimal.activity, None);
        let rich = serde_json::from_str::<crate::protocol::Request>(
            r#"{"type":"report_agent_state","run_id":"dock_1","state":"working","session_id":"s","tool_name":"Read","activity":"Read src/lib.rs"}"#,
        )
        .expect("hook-rich report");
        let crate::protocol::Request::ReportAgentState(request) = rich else {
            panic!("expected a report");
        };
        assert_eq!(request.session_id.as_deref(), Some("s"));
        assert_eq!(request.activity.as_deref(), Some("Read src/lib.rs"));
    }

    #[test]
    fn a_split_request_written_before_pane_kinds_existed_still_asks_for_a_terminal() {
        // What makes `kind` one field on an existing request rather than a second request: every
        // split ever sent, and every one an older client will send, means the same thing it
        // always meant. The version bump is what tells such a client it is behind; the default is
        // what makes the wire safe until it notices.
        let request: Request = serde_json::from_str(
            r#"{"type":"workspace","operation":"split","workspace_id":"w","pane_id":"p","new_pane_id":"q","axis":"vertical"}"#,
        )
        .expect("a split with no kind is still a split");
        let Request::Workspace(WorkspaceRequest::Split { kind, .. }) = request else {
            panic!("expected a split");
        };
        assert_eq!(kind, PaneKind::Terminal);
    }

    #[test]
    fn pane_input_round_trips_arbitrary_key_bytes() {
        let raw = vec![0x1b, b'[', b'A', 0x00, 0xff];
        let request = PaneInputRequest {
            workspace_id: "w".into(),
            pane_id: "p".into(),
            input: PaneInputRequest::encode(&raw),
        };
        assert_eq!(request.decode().expect("decodes"), raw);
    }

    #[test]
    fn snapshot_no_longer_carries_scrollback_and_reports_geometry() {
        let encoded = serde_json::to_string(&snapshot_fixture()).expect("serialize");
        assert!(!encoded.contains("scrollback"));
        assert!(encoded.contains("\"rows\":24"));
        assert!(encoded.contains("\"cols\":80"));
    }

    #[test]
    fn events_round_trip_losslessly() {
        let event = Event::PaneDelta {
            run_id: "dock_1".into(),
            revision: 7,
            bytes: "aGk=".into(),
        };
        let encoded = serde_json::to_string(&event).expect("serialize");
        let decoded: Event = serde_json::from_str(&encoded).expect("deserialize");
        assert_eq!(decoded, event);
    }

    #[test]
    fn resize_request_rejects_unknown_fields() {
        let json = r#"{"workspace_id":"w","pane_id":"p","rows":24,"cols":80,"extra":1}"#;
        assert!(serde_json::from_str::<PaneResizeRequest>(json).is_err());
    }

    #[test]
    fn an_attach_event_carries_the_geometry_a_client_needs_to_size_its_parser() {
        let event = Event::PaneAttached {
            run_id: "r1".into(),
            revision: 4,
            rows: 40,
            cols: 120,
            scrollback_rows: 2000,
            history_from: 0,
            epoch: 1,
            screen: "AA==".into(),
        };
        let wire = serde_json::to_string(&event).expect("serialise attach event");
        assert!(wire.contains(r#""rows":40"#), "{wire}");
        assert!(wire.contains(r#""cols":120"#), "{wire}");
        assert!(wire.contains(r#""scrollback_rows":2000"#), "{wire}");
        assert_eq!(
            serde_json::from_str::<Event>(&wire).expect("round trip"),
            event
        );
        // A subscriber must not be able to size its parser from a frame that omits geometry.
        assert!(
            serde_json::from_str::<Event>(
                r#"{"event":"pane_attached","run_id":"r1","revision":4,"screen":"AA=="}"#
            )
            .is_err()
        );
        // A subscriber must not be able to size its replica's history from a frame that omits
        // the daemon's retention either.
        assert!(
            serde_json::from_str::<Event>(
                r#"{"event":"pane_attached","run_id":"r1","revision":4,"rows":40,"cols":120,"screen":"AA=="}"#
            )
            .is_err()
        );
        assert!(
            serde_json::from_str::<Event>(
                r#"{"event":"pane_attached","run_id":"r1","revision":4,"rows":40,"cols":120,"scrollback_rows":2000,"history_from":0,"epoch":1,"screen":"AA==","extra":1}"#
            )
            .is_err(),
            "the event shape must stay closed"
        );
    }

    #[test]
    fn a_pane_history_request_round_trips_and_rejects_unknown_fields() {
        let request = Request::PaneHistory(PaneHistoryRequest {
            run_id: "run_1".into(),
            before: 4096,
            max_bytes: 2 << 20,
        });
        let wire = serde_json::to_string(&request).expect("encode");
        assert_eq!(
            serde_json::from_str::<Request>(&wire).expect("decode"),
            request
        );
        assert!(
            serde_json::from_str::<Request>(
                r#"{"type":"pane_history","run_id":"r","before":0,"max_bytes":1,"extra":1}"#
            )
            .is_err(),
            "an unknown field must be refused like every other request"
        );
    }

    #[test]
    fn an_attach_frame_carries_the_cursor_and_epoch_a_client_needs_to_page_back() {
        let event = Event::PaneAttached {
            run_id: "run_1".into(),
            revision: 4,
            rows: 40,
            cols: 120,
            scrollback_rows: 2000,
            history_from: 8192,
            epoch: 7,
            screen: String::new(),
        };
        let wire = serde_json::to_string(&event).expect("encode");
        assert!(wire.contains(r#""history_from":8192"#), "{wire}");
        assert!(wire.contains(r#""epoch":7"#), "{wire}");
    }

    #[test]
    fn strict_versioned_messages_reject_unknown_fields_and_variants() {
        assert!(serde_json::from_str::<Request>(r#"{"type":"inspect","pid":1}"#).is_err());
        assert!(serde_json::from_str::<Request>(r#"{"type":"stop","pid":1}"#).is_err());
        assert!(serde_json::from_str::<Request>(r#"{"type":"dispatch","repository_root":"r","external_task_ref":"t","run_id":"dock_1","worktree":"w","adapter":{"id":"fixture","arguments":[]},"pid":1}"#).is_err());
        assert!(
            serde_json::from_str::<Request>(r#"{"type":"review_inbox","future":true}"#).is_err()
        );
        assert!(serde_json::from_str::<Request>(r#"{"type":"decide","run_id":"dock_1","route":"accept_scope","note":"ok","completed":true}"#).is_err());
        assert!(
            serde_json::from_str::<Request>(r#"{"type":"inspect_programme","future":true}"#)
                .is_err()
        );
        assert!(serde_json::from_str::<Request>(r#"{"type":"queue_gated","dispatch":{"repository_root":"r","external_task_ref":"t","run_id":"dock_1","worktree":"w","adapter":{"id":"fixture","arguments":[]}},"upstream_run_id":"dock_0","required_route":"accept_scope","future":true}"#).is_err());
        assert!(
            serde_json::from_str::<Request>(
                r#"{"type":"release_gate","downstream_run_id":"dock_1","future":true}"#
            )
            .is_err()
        );
    }
    #[test]
    fn hello_remains_forward_compatible_for_negotiation() {
        assert_eq!(
            serde_json::from_str::<Request>(
                r#"{"type":"hello","version":6,"capabilities":["future"]}"#
            )
            .unwrap(),
            Request::Hello(HelloRequest { version: 6 })
        );
    }
    #[test]
    fn workspace_contract_is_strict_and_typed() {
        let request: Request = serde_json::from_str(r#"{"type":"workspace","operation":"split","workspace_id":"work_1","pane_id":"pane_1","new_pane_id":"pane_2","axis":"vertical"}"#).unwrap();
        assert!(matches!(
            request,
            Request::Workspace(WorkspaceRequest::Split {
                axis: SplitAxis::Vertical,
                ..
            })
        ));
        assert!(serde_json::from_str::<Request>(r#"{"type":"workspace","operation":"focus","workspace_id":"work_1","pane_id":"pane_1","pid":42}"#).is_err());
        assert_eq!(
            serde_json::from_str::<Request>(r#"{"type":"workspace","operation":"rename","workspace_id":"work_1","name":"renamed"}"#).unwrap(),
            Request::Workspace(WorkspaceRequest::Rename {
                workspace_id: "work_1".into(),
                pane_id: None,
                name: "renamed".into(),
            })
        );
        assert!(serde_json::from_str::<Request>(r#"{"type":"workspace","operation":"rename","workspace_id":"work_1","name":"renamed","future":true}"#).is_err());
        assert!(
            serde_json::from_str::<Request>(r#"{"type":"workspace","operation":"adopt","pid":42}"#)
                .is_err()
        );
    }

    #[test]
    fn pane_input_has_no_pid_or_external_authority_shape() {
        let request: Request = serde_json::from_str(
            r#"{"type":"pane_input","workspace_id":"w","pane_id":"p","input":"hello\\n"}"#,
        )
        .unwrap();
        assert!(matches!(
            request,
            Request::PaneInput(PaneInputRequest { .. })
        ));
        assert!(
            serde_json::from_str::<Request>(
                r#"{"type":"pane_input","workspace_id":"w","pane_id":"p","input":"x","pid":42}"#
            )
            .is_err()
        );
    }

    #[test]
    fn terminal_launch_is_strict_and_cannot_carry_control_plane_or_command_fields() {
        let json = r#"{"type":"terminal_launch","workspace_id":"w","pane_id":"p","run_id":"dock_12","profile":"fixture","runtime_directory":"/tmp"}"#;
        assert!(matches!(
            serde_json::from_str::<Request>(json).unwrap(),
            Request::TerminalLaunch(_)
        ));
        // Nothing that selects what runs, or where. These are the fields whose absence is what
        // makes it safe to hand this request to a dashboard.
        for forbidden in [
            "repository_root",
            "worktree",
            "executable",
            "environment",
            "shell",
        ] {
            let malicious =
                json.strip_suffix('}').unwrap().to_owned() + &format!(r#", "{forbidden}":"x"}}"#);
            assert!(
                serde_json::from_str::<Request>(&malicious).is_err(),
                "accepted {forbidden}"
            );
        }
        // `external_task_ref` is carried, and was on the list above until a run needed to
        // remember which card it was for across the death of the client that dispatched it. It
        // selects nothing — the repository and worktree are still derived from
        // `runtime_directory` — and `validate_external_task_ref` is what holds it to a label.
        let with_task =
            json.strip_suffix('}').unwrap().to_owned() + r#", "external_task_ref":"61"}"#;
        let Request::TerminalLaunch(request) =
            serde_json::from_str::<Request>(&with_task).expect("a task reference is carried")
        else {
            panic!("expected a terminal launch");
        };
        assert_eq!(request.external_task_ref, "61");
        // Accepted by serde and refused by the validation, which is the layer that matters: a
        // path arriving in this field must not reach a binding.
        assert!(validate_external_task_ref("../../etc/passwd").is_err());

        // `arguments` is carried too, and the old spelling of this test only appeared to refuse
        // it: it sent a string where a list belongs, so serde rejected the type rather than the
        // field, and the assertion would have passed even if the field had been forbidden.
        let with_arguments =
            json.strip_suffix('}').unwrap().to_owned() + r#", "arguments":["--continue"]}"#;
        assert!(serde_json::from_str::<Request>(&with_arguments).is_ok());

        assert!(serde_json::from_str::<Request>(&json.replace("fixture", "generic")).is_err());
    }
}
