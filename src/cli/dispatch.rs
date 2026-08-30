//! `dock dispatch` — start one agent run without the dashboard.

use std::{
    path::PathBuf,
    time::{SystemTime, UNIX_EPOCH},
};

use crate::{
    adapter::{AdapterId, AdapterSelection},
    cli::wire::{Connection, print_json},
    protocol::{DispatchRequest, Request, Response},
};

const USAGE: &str = "usage: dock dispatch --repo=PATH --task=REF --worktree=PATH \
                     [--run-id=dock_ID] \
                     [--adapter=fixture|amp|claude-code|codex-cli|github-copilot-cli|generic] \
                     [--executable=PATH] [--socket=PATH] -- [ARG ...]";

pub fn parse_arguments(args: &[String]) -> Result<(Option<PathBuf>, Request), String> {
    let mut socket = None;
    let mut repository_root = None;
    let mut task = None;
    let mut run_id = None;
    let mut worktree = None;
    let mut command = Vec::new();
    let mut adapter = AdapterId::Fixture;
    let mut executable = None;
    let mut arguments = args.iter();
    while let Some(argument) = arguments.next() {
        // Everything past `--` belongs to the agent, including anything spelled like one of
        // ours. This is the only place in the parser where a flag is not a flag.
        if argument == "--" {
            command.extend(arguments.cloned());
            break;
        }
        if let Some(value) = argument.strip_prefix("--socket=") {
            socket = Some(PathBuf::from(value));
        } else if let Some(value) = argument.strip_prefix("--repo=") {
            repository_root = Some(value.to_owned());
        } else if let Some(value) = argument.strip_prefix("--task=") {
            task = Some(value.to_owned());
        } else if let Some(value) = argument.strip_prefix("--run-id=") {
            run_id = Some(value.to_owned());
        } else if let Some(value) = argument.strip_prefix("--worktree=") {
            worktree = Some(value.to_owned());
        } else if let Some(value) = argument.strip_prefix("--adapter=") {
            adapter = match value {
                "fixture" => AdapterId::Fixture,
                "amp" => AdapterId::Amp,
                "claude-code" => AdapterId::ClaudeCode,
                "codex-cli" => AdapterId::CodexCli,
                "github-copilot-cli" => AdapterId::GithubCopilotCli,
                "generic" => AdapterId::Generic,
                _ => return Err(format!("unknown adapter {value:?}; {USAGE}")),
            };
        } else if let Some(value) = argument.strip_prefix("--executable=") {
            executable = Some(value.to_owned());
        } else {
            return Err(format!("unknown option {argument:?}; {USAGE}"));
        }
    }
    Ok((
        socket,
        Request::Dispatch(DispatchRequest {
            repository_root: repository_root.ok_or(format!("--repo is required; {USAGE}"))?,
            external_task_ref: task.ok_or(format!("--task is required; {USAGE}"))?,
            run_id: run_id.unwrap_or_else(generate_run_id),
            worktree: worktree.ok_or(format!("--worktree is required; {USAGE}"))?,
            adapter: AdapterSelection {
                id: adapter,
                executable,
                arguments: command,
            },
        }),
    ))
}

/// Unique without coordinating with anything: this process, and the moment it asked.
pub fn generate_run_id() -> String {
    format!(
        "dock_{}_{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock after epoch")
            .as_nanos()
    )
}

pub fn render(response: Response) -> Result<(), String> {
    match response {
        Response::Dispatched { snapshot } => print_json(&snapshot),
        Response::Error { message, .. } => Err(message),
        response => Err(format!("unexpected dispatch response: {response:?}")),
    }
}

pub fn run(args: &[String]) -> Result<(), String> {
    let (socket, request) = parse_arguments(args)?;
    let response = Connection::open(socket)?.request(&request)?;
    render(response)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn everything_after_the_separator_is_the_agents_command() {
        let (_, request) = parse_arguments(&[
            "--repo=/repo".to_owned(),
            "--task=T-1".to_owned(),
            "--worktree=/repo".to_owned(),
            "--".to_owned(),
            "-c".to_owned(),
            "--task=not-a-flag-of-ours".to_owned(),
        ])
        .expect("parse");
        match request {
            Request::Dispatch(dispatch) => assert_eq!(
                dispatch.adapter.arguments,
                vec!["-c".to_owned(), "--task=not-a-flag-of-ours".to_owned()],
                "past `--`, an argument that looks like ours is still the agent's"
            ),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn a_run_id_is_generated_when_none_is_given_and_kept_when_one_is() {
        let required = [
            "--repo=/repo".to_owned(),
            "--task=T-1".to_owned(),
            "--worktree=/repo".to_owned(),
        ];
        let (_, request) = parse_arguments(&required).expect("parse");
        match request {
            Request::Dispatch(dispatch) => {
                assert!(dispatch.run_id.starts_with("dock_"), "{}", dispatch.run_id)
            }
            other => panic!("{other:?}"),
        }
        let mut named = required.to_vec();
        named.push("--run-id=dock_mine".to_owned());
        let (_, request) = parse_arguments(&named).expect("parse");
        match request {
            Request::Dispatch(dispatch) => assert_eq!(dispatch.run_id, "dock_mine"),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn each_required_flag_names_itself_when_missing() {
        for (omitted, rest) in [
            ("--repo", vec!["--task=T-1", "--worktree=/repo"]),
            ("--task", vec!["--repo=/repo", "--worktree=/repo"]),
            ("--worktree", vec!["--repo=/repo", "--task=T-1"]),
        ] {
            let args: Vec<String> = rest.into_iter().map(str::to_owned).collect();
            let error = parse_arguments(&args).unwrap_err();
            assert!(error.starts_with(omitted), "{omitted}: {error}");
        }
    }
}
