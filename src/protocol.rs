use serde::{Deserialize, Serialize};

use crate::model::{HandoffPacket, HandoffRecord, ReviewDecision, ReviewRoute};

pub const PROTOCOL_VERSION: u16 = 3;
pub const MAX_MESSAGE_BYTES: u64 = 64 * 1024;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Request {
    Hello(HelloRequest),
    Inspect(InspectRequest),
    Dispatch(DispatchRequest),
    SubmitHandoff(SubmitHandoffRequest),
    ReviewInbox(ReviewInboxRequest),
    Decide(DecideRequest),
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
    pub command: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SubmitHandoffRequest {
    pub packet: HandoffPacket,
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
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum Response {
    Hello { version: u16 },
    Snapshot { snapshot: RuntimeSnapshot },
    Snapshots { snapshots: Vec<RuntimeSnapshot> },
    Dispatched { snapshot: RuntimeSnapshot },
    HandoffSubmitted { record: HandoffRecord },
    ReviewInbox { items: Vec<HandoffRecord> },
    DecisionRecorded { decision: ReviewDecision },
    Error { code: ErrorCode, message: String },
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
    DuplicateRunId,
    RunNotFound,
    InvalidHandoff,
    DuplicateHandoff,
    HandoffNotFound,
    DecisionAlreadyRecorded,
    Internal,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RuntimeSnapshot {
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
    pub scrollback: String,
    pub scrollback_bytes: usize,
    pub scrollback_capacity_bytes: usize,
    pub scrollback_truncated: bool,
    pub diagnostic: Option<String>,
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
    #[test]
    fn strict_versioned_messages_reject_unknown_fields_and_variants() {
        assert!(serde_json::from_str::<Request>(r#"{"type":"inspect","pid":1}"#).is_err());
        assert!(serde_json::from_str::<Request>(r#"{"type":"stop","pid":1}"#).is_err());
        assert!(serde_json::from_str::<Request>(r#"{"type":"dispatch","repository_root":"r","external_task_ref":"t","run_id":"dock_1","worktree":"w","command":[],"pid":1}"#).is_err());
        assert!(
            serde_json::from_str::<Request>(r#"{"type":"review_inbox","future":true}"#).is_err()
        );
        assert!(serde_json::from_str::<Request>(r#"{"type":"decide","run_id":"dock_1","route":"accept_scope","note":"ok","completed":true}"#).is_err());
    }
    #[test]
    fn hello_remains_forward_compatible_for_negotiation() {
        assert_eq!(
            serde_json::from_str::<Request>(
                r#"{"type":"hello","version":3,"capabilities":["future"]}"#
            )
            .unwrap(),
            Request::Hello(HelloRequest { version: 3 })
        );
    }
}
