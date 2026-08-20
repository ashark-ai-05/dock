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
}
