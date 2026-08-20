use std::{path::PathBuf, process::Command};

use serde::Deserialize;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandSpec {
    pub program: String,
    pub args: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct KanbanTask {
    pub id: u64,
    pub title: String,
    pub status: String,
    #[serde(default)]
    pub claimed_by: Option<String>,
    pub file: PathBuf,
}

#[derive(Debug, Clone)]
pub struct KanbanMdAdapter {
    binary: String,
    board_dir: PathBuf,
}

impl KanbanMdAdapter {
    pub fn new(board_dir: impl Into<PathBuf>) -> Self {
        Self {
            binary: "kanban-md".into(),
            board_dir: board_dir.into(),
        }
    }

    #[cfg(test)]
    pub fn with_binary(binary: impl Into<String>, board_dir: impl Into<PathBuf>) -> Self {
        Self {
            binary: binary.into(),
            board_dir: board_dir.into(),
        }
    }

    pub fn list_spec(&self) -> CommandSpec {
        self.spec(["list", "--json"])
    }

    pub fn pick_spec(&self, claim: &str, from_status: &str, to_status: &str) -> CommandSpec {
        self.spec([
            "pick",
            "--claim",
            claim,
            "--status",
            from_status,
            "--move",
            to_status,
            "--json",
        ])
    }

    pub fn list(&self) -> Result<Vec<KanbanTask>, String> {
        let stdout = self.run(self.list_spec())?;
        serde_json::from_str(&stdout)
            .map_err(|error| format!("invalid kanban-md list JSON: {error}"))
    }

    pub fn pick(
        &self,
        claim: &str,
        from_status: &str,
        to_status: &str,
    ) -> Result<KanbanTask, String> {
        if [claim, from_status, to_status]
            .iter()
            .any(|value| value.trim().is_empty())
        {
            return Err("claim and statuses must be non-empty".into());
        }
        let stdout = self.run(self.pick_spec(claim, from_status, to_status))?;
        serde_json::from_str(&stdout)
            .map_err(|error| format!("invalid kanban-md pick JSON: {error}"))
    }

    fn spec<const N: usize>(&self, command_args: [&str; N]) -> CommandSpec {
        let mut args = vec!["--dir".to_owned(), self.board_dir.display().to_string()];
        args.extend(command_args.into_iter().map(str::to_owned));
        CommandSpec {
            program: self.binary.clone(),
            args,
        }
    }

    fn run(&self, spec: CommandSpec) -> Result<String, String> {
        let output = Command::new(&spec.program)
            .args(&spec.args)
            .output()
            .map_err(|error| format!("failed to start {}: {error}", spec.program))?;
        if !output.status.success() {
            return Err(format!(
                "kanban-md failed ({}): {}",
                output.status,
                String::from_utf8_lossy(&output.stderr).trim()
            ));
        }
        String::from_utf8(output.stdout)
            .map_err(|error| format!("kanban-md emitted non-UTF-8 output: {error}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn claim_is_a_single_atomic_pick_command() {
        let adapter = KanbanMdAdapter::with_binary("kbmd", "kanban");
        assert_eq!(
            adapter.pick_spec("dock-worker", "backlog", "in-progress"),
            CommandSpec {
                program: "kbmd".into(),
                args: vec![
                    "--dir",
                    "kanban",
                    "pick",
                    "--claim",
                    "dock-worker",
                    "--status",
                    "backlog",
                    "--move",
                    "in-progress",
                    "--json",
                ]
                .into_iter()
                .map(str::to_owned)
                .collect(),
            }
        );
    }

    #[test]
    fn empty_claim_or_status_never_spawns_a_process() {
        let adapter = KanbanMdAdapter::with_binary("not-a-real-program", "kanban");
        assert_eq!(
            adapter.pick("", "backlog", "in-progress"),
            Err("claim and statuses must be non-empty".into())
        );
    }
}
