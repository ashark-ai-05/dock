use std::path::PathBuf;

use crate::{
    adapter::{AdapterId, AdapterSelection},
    cli::wire::{Connection, print_json},
    model::ReviewRoute,
    protocol::{
        DispatchRequest, InspectProgrammeRequest, QueueGatedRequest, ReleaseGateRequest, Request,
        Response,
    },
};

pub fn parse_arguments(args: &[String]) -> Result<(Option<PathBuf>, Request), String> {
    let mut socket = None;
    let mut release = None;
    let mut upstream = None;
    let mut required_route = ReviewRoute::AcceptScope;
    let mut repo = None;
    let mut task = None;
    let mut run_id = None;
    let mut worktree = None;
    let mut command = Vec::new();
    let mut queue_flag_seen = false;
    let mut arguments = args.iter();
    while let Some(arg) = arguments.next() {
        if arg == "--" {
            queue_flag_seen = true;
            command.extend(arguments.cloned());
            break;
        }
        if let Some(v) = arg.strip_prefix("--socket=") {
            socket = Some(PathBuf::from(v));
        } else if let Some(v) = arg.strip_prefix("--release=") {
            release = Some(v.to_owned());
        } else if let Some(v) = arg.strip_prefix("--upstream-run-id=") {
            queue_flag_seen = true;
            upstream = Some(v.to_owned());
        } else if let Some(v) = arg.strip_prefix("--required-route=") {
            queue_flag_seen = true;
            required_route = match v {
                "accept-scope" => ReviewRoute::AcceptScope,
                "request-change" => ReviewRoute::RequestChange,
                _ => return Err("--required-route must be accept-scope or request-change".into()),
            };
        } else if let Some(v) = arg.strip_prefix("--repo=") {
            queue_flag_seen = true;
            repo = Some(v.to_owned());
        } else if let Some(v) = arg.strip_prefix("--task=") {
            queue_flag_seen = true;
            task = Some(v.to_owned());
        } else if let Some(v) = arg.strip_prefix("--run-id=") {
            queue_flag_seen = true;
            run_id = Some(v.to_owned());
        } else if let Some(v) = arg.strip_prefix("--worktree=") {
            queue_flag_seen = true;
            worktree = Some(v.to_owned());
        } else {
            return Err(format!("unknown option {arg:?}"));
        }
    }
    if release.is_some() && queue_flag_seen {
        return Err("--release is mutually exclusive with queue flags".into());
    }
    let request = if let Some(downstream_run_id) = release {
        Request::ReleaseGate(ReleaseGateRequest { downstream_run_id })
    } else if let Some(upstream_run_id) = upstream {
        Request::QueueGated(QueueGatedRequest {
            dispatch: DispatchRequest {
                repository_root: repo.ok_or("--repo is required when queueing")?,
                external_task_ref: task.ok_or("--task is required when queueing")?,
                run_id: run_id.ok_or("--run-id is required when queueing")?,
                worktree: worktree.ok_or("--worktree is required when queueing")?,
                adapter: AdapterSelection {
                    id: AdapterId::Fixture,
                    executable: None,
                    arguments: command,
                },
            },
            upstream_run_id,
            required_route,
        })
    } else {
        Request::InspectProgramme(InspectProgrammeRequest {})
    };
    Ok((socket, request))
}

/// Render the daemon's response the way each of `programme`'s three requests wants it shown:
/// the portfolio, a freshly queued gate, or the snapshot a release just unblocked.
pub fn render(response: Response) -> Result<(), String> {
    match response {
        Response::Programme { portfolio } => print_json(&portfolio),
        Response::GateQueued { gate } => print_json(&gate),
        Response::GateReleased { snapshot } => print_json(&snapshot),
        Response::Error { message, .. } => Err(message),
        response => Err(format!("unexpected programme response: {response:?}")),
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
    fn release_rejects_every_queue_shape() {
        for queue_flag in [
            "--upstream-run-id=dock_upstream",
            "--repo=/repo",
            "--task=T-1",
            "--run-id=dock_downstream",
            "--worktree=/repo",
            "--required-route=request-change",
        ] {
            let error = parse_arguments(&[
                "--release=dock_downstream".to_owned(),
                queue_flag.to_owned(),
            ])
            .unwrap_err();
            assert!(error.contains("mutually exclusive"));
        }
        assert!(
            parse_arguments(&[
                "--release=dock_downstream".to_owned(),
                "--".to_owned(),
                "unexpected-command".to_owned(),
            ])
            .unwrap_err()
            .contains("mutually exclusive")
        );
    }

    #[test]
    fn release_alone_is_valid() {
        let (_, request) = parse_arguments(&["--release=dock_downstream".to_owned()]).unwrap();
        assert!(matches!(request, Request::ReleaseGate(_)));
    }
}
