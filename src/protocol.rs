use serde::{Deserialize, Serialize};

pub const PROTOCOL_VERSION: u16 = 1;
pub const MAX_MESSAGE_BYTES: u64 = 64 * 1024;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Request {
    Hello(HelloRequest),
    Inspect(InspectRequest),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HelloRequest {
    pub version: u16,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InspectRequest {}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum Response {
    Hello { version: u16 },
    Snapshot { snapshot: RuntimeSnapshot },
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
    Internal,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RuntimeSnapshot {
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
    fn inspect_rejects_unknown_fields_and_requests_reject_unknown_variants() {
        assert!(serde_json::from_str::<Request>(r#"{"type":"inspect","pid":1}"#).is_err());
        assert!(serde_json::from_str::<Request>(r#"{"type":"stop","pid":1}"#).is_err());
    }

    #[test]
    fn hello_accepts_fields_added_by_newer_clients() {
        assert_eq!(
            serde_json::from_str::<Request>(
                r#"{"type":"hello","version":2,"capabilities":["future"]}"#
            )
            .expect("forward-compatible hello"),
            Request::Hello(HelloRequest { version: 2 })
        );
    }
}
