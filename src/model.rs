use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Check {
    pub name: String,
    pub passed: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HandoffPacket {
    pub schema_version: u16,
    pub run_id: String,
    pub task_id: String,
    pub workspace_id: String,
    pub pane_id: String,
    pub worktree: String,
    pub branch: String,
    pub base_sha: String,
    pub summary: String,
    pub question: Option<String>,
    pub checks: Vec<Check>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HandoffEvidence {
    pub branch: String,
    pub base_sha: String,
    pub head_sha: String,
    pub status_entries: usize,
    pub changed_files: usize,
    pub insertions: usize,
    pub deletions: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HandoffRecord {
    pub packet: HandoffPacket,
    pub evidence: HandoffEvidence,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReviewRoute {
    AcceptScope,
    RequestChange,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReviewDecision {
    pub run_id: String,
    pub route: ReviewRoute,
    pub note: String,
    /// This invariant is durable and deliberately prevents a review route from being read as
    /// external task completion or authority to mutate Git.
    pub external_task_completed: bool,
    pub git_mutated: bool,
}

impl ReviewDecision {
    pub fn new(run_id: String, route: ReviewRoute, note: String) -> Result<Self, &'static str> {
        if note.trim().is_empty() {
            return Err("an explicit review decision note is required");
        }
        Ok(Self {
            run_id,
            route,
            note,
            external_task_completed: false,
            git_mutated: false,
        })
    }
}

impl HandoffPacket {
    pub fn validate(&self) -> Result<(), &'static str> {
        if self.schema_version != 1 {
            return Err("unsupported handoff packet schema version");
        }
        if self.run_id.trim().is_empty() || self.task_id.trim().is_empty() {
            return Err("run_id and task_id are required");
        }
        if self.workspace_id.trim().is_empty() || self.pane_id.trim().is_empty() {
            return Err("a Dock-owned workspace and pane binding are required");
        }
        if self.worktree.trim().is_empty()
            || self.branch.trim().is_empty()
            || self.base_sha.trim().is_empty()
        {
            return Err("worktree, branch, and base_sha are required");
        }
        if self.summary.trim().is_empty() {
            return Err("an explicit agent handoff summary is required");
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn valid_packet() -> HandoffPacket {
        HandoffPacket {
            schema_version: 1,
            run_id: "dock_01J9".into(),
            task_id: "DOCK-7".into(),
            workspace_id: "dock".into(),
            pane_id: "ledger-agent".into(),
            worktree: "../dock-ledger".into(),
            branch: "dock/fixture-handoff".into(),
            base_sha: "3fa91c2".into(),
            summary: "Implementation complete; one bounded decision remains.".into(),
            question: Some("Accept V0.1 scope?".into()),
            checks: vec![Check {
                name: "cargo test".into(),
                passed: true,
            }],
        }
    }

    #[test]
    fn packet_round_trip_is_lossless_and_validated() {
        let packet = valid_packet();
        let encoded = serde_json::to_string(&packet).expect("serialize packet");
        let decoded: HandoffPacket = serde_json::from_str(&encoded).expect("deserialize packet");
        assert_eq!(decoded, packet);
        assert_eq!(decoded.validate(), Ok(()));
    }

    #[test]
    fn packet_rejects_unknown_fields_and_future_schema() {
        let mut packet = valid_packet();
        packet.schema_version = 2;
        assert_eq!(
            packet.validate(),
            Err("unsupported handoff packet schema version")
        );
        let unknown_field = r#"{
            "schema_version":1,"run_id":"x","task_id":"t","workspace_id":"w","pane_id":"p",
            "worktree":"wt","branch":"b","base_sha":"sha","summary":"s","question":null,
            "checks":[],"raw_transcript":"prohibited"
        }"#;
        assert!(serde_json::from_str::<HandoffPacket>(unknown_field).is_err());
    }
}
