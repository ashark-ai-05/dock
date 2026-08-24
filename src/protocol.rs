use base64::{Engine as _, engine::general_purpose::STANDARD};
use serde::{Deserialize, Serialize};

use crate::{
    adapter::{AdapterCapabilities, AdapterId, AdapterSelection, ProcessCapabilities},
    detect::{AgentKind, AgentState},
    layout::{LayoutSnapshot, PaneKind, SplitAxis, WorkspaceLayout},
    model::{HandoffPacket, HandoffRecord, ReviewDecision, ReviewRoute},
};

pub const PROTOCOL_VERSION: u16 = 11;
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
    },
    LayoutChanged,
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

    #[test]
    fn protocol_version_is_eleven() {
        assert_eq!(PROTOCOL_VERSION, 11);
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
                r#"{"event":"pane_attached","run_id":"r1","revision":4,"rows":40,"cols":120,"scrollback_rows":2000,"screen":"AA==","extra":1}"#
            )
            .is_err(),
            "the event shape must stay closed"
        );
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
