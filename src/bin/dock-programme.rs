use std::{
    io::{BufRead, BufReader, Write},
    os::unix::net::UnixStream,
    path::PathBuf,
};

use dock::{
    adapter::{AdapterId, AdapterSelection},
    model::ReviewRoute,
    paths,
    protocol::{
        DispatchRequest, HelloRequest, InspectProgrammeRequest, PROTOCOL_VERSION,
        QueueGatedRequest, ReleaseGateRequest, Request, Response,
    },
};

fn main() -> Result<(), String> {
    let (socket, request) = parse_arguments(std::env::args().skip(1))?;
    run(socket, request)
}

fn parse_arguments(
    arguments: impl IntoIterator<Item = String>,
) -> Result<(Option<PathBuf>, Request), String> {
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
    let mut args = arguments.into_iter();
    while let Some(arg) = args.next() {
        if arg == "--" {
            queue_flag_seen = true;
            command.extend(args);
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

fn run(socket: Option<PathBuf>, request: Request) -> Result<(), String> {
    let socket = socket.map_or_else(paths::default_socket_path, Ok)?;
    let mut stream = UnixStream::connect(&socket)
        .map_err(|e| format!("could not connect to {}: {e}", socket.display()))?;
    let mut reader = BufReader::new(stream.try_clone().map_err(|e| e.to_string())?);
    send(
        &mut stream,
        &Request::Hello(HelloRequest {
            version: PROTOCOL_VERSION,
        }),
    )?;
    match receive(&mut reader)? {
        Response::Hello {
            version: PROTOCOL_VERSION,
        } => {}
        Response::Error { message, .. } => return Err(message),
        r => return Err(format!("unexpected handshake response: {r:?}")),
    }
    send(&mut stream, &request)?;
    match receive(&mut reader)? {
        Response::Programme { portfolio } => print_json(&portfolio),
        Response::GateQueued { gate } => print_json(&gate),
        Response::GateReleased { snapshot } => print_json(&snapshot),
        Response::Error { message, .. } => Err(message),
        response => Err(format!("unexpected programme response: {response:?}")),
    }
}

fn print_json(value: &impl serde::Serialize) -> Result<(), String> {
    println!(
        "{}",
        serde_json::to_string_pretty(value).map_err(|e| e.to_string())?
    );
    Ok(())
}
fn send(stream: &mut UnixStream, request: &Request) -> Result<(), String> {
    serde_json::to_writer(&mut *stream, request).map_err(|e| e.to_string())?;
    stream.write_all(b"\n").map_err(|e| e.to_string())
}
fn receive(reader: &mut impl BufRead) -> Result<Response, String> {
    let mut line = String::new();
    if reader.read_line(&mut line).map_err(|e| e.to_string())? == 0 {
        return Err("daemon closed the connection".into());
    }
    serde_json::from_str(&line).map_err(|e| format!("invalid daemon response: {e}"))
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
            let error = parse_arguments(["--release=dock_downstream".into(), queue_flag.into()])
                .unwrap_err();
            assert!(error.contains("mutually exclusive"));
        }
        assert!(
            parse_arguments([
                "--release=dock_downstream".into(),
                "--".into(),
                "unexpected-command".into(),
            ])
            .unwrap_err()
            .contains("mutually exclusive")
        );
    }

    #[test]
    fn release_alone_is_valid() {
        let (_, request) = parse_arguments(["--release=dock_downstream".into()]).unwrap();
        assert!(matches!(request, Request::ReleaseGate(_)));
    }
}
