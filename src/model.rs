use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum TaskState {
    Todo,
    Running,
    NeedsInput,
    NeedsReview,
    ChangesRequested,
    ReadyToMerge,
    Done,
}

impl TaskState {
    pub const fn label(self) -> &'static str {
        match self {
            Self::Todo => "TODO",
            Self::Running => "RUNNING",
            Self::NeedsInput => "NEEDS INPUT",
            Self::NeedsReview => "NEEDS REVIEW",
            Self::ChangesRequested => "CHANGES REQUESTED",
            Self::ReadyToMerge => "READY TO MERGE",
            Self::Done => "DONE",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Check {
    pub name: String,
    pub passed: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Task {
    pub id: String,
    pub title: String,
    pub state: TaskState,
    pub agent: String,
    pub worktree: String,
    pub branch: String,
    pub base_sha: String,
    pub changed_files: usize,
    pub insertions: usize,
    pub deletions: usize,
    pub handoff_summary: String,
    pub question: Option<String>,
    pub checks: Vec<Check>,
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

impl HandoffPacket {
    pub fn validate(&self) -> Result<(), &'static str> {
        if self.schema_version != 1 {
            return Err("unsupported handoff packet schema version");
        }
        if self.run_id.trim().is_empty() || self.task_id.trim().is_empty() {
            return Err("run_id and task_id are required");
        }
        if self.workspace_id.trim().is_empty() || self.pane_id.trim().is_empty() {
            return Err("a managed Herdr workspace and pane binding are required");
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

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BoardFixture {
    pub project: String,
    pub herdr_status: String,
    pub tasks: Vec<Task>,
}

impl BoardFixture {
    pub fn example() -> Self {
        serde_json::from_str(include_str!("../fixtures/demo-board.json"))
            .expect("embedded fixture must be valid")
    }

    pub fn handoff_packet_for(&self, task_index: usize) -> HandoffPacket {
        let task = &self.tasks[task_index];
        HandoffPacket {
            schema_version: 1,
            run_id: format!("fixture-{}", task.id.to_lowercase()),
            task_id: task.id.clone(),
            workspace_id: self.project.clone(),
            pane_id: task.agent.clone(),
            worktree: task.worktree.clone(),
            branch: task.branch.clone(),
            base_sha: task.base_sha.clone(),
            summary: task.handoff_summary.clone(),
            question: task.question.clone(),
            checks: task.checks.clone(),
        }
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
