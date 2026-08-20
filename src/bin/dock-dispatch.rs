use std::{
    io::{BufRead, BufReader, Write},
    os::unix::net::UnixStream,
    path::PathBuf,
    time::{SystemTime, UNIX_EPOCH},
};

use dock::{
    paths,
    protocol::{DispatchRequest, HelloRequest, PROTOCOL_VERSION, Request, Response},
};

fn main() -> Result<(), String> {
    let mut socket = None;
    let mut repository_root = None;
    let mut task = None;
    let mut run_id = None;
    let mut worktree = None;
    let mut command = Vec::new();
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        if arg == "--" {
            command.extend(args);
            break;
        }
        if let Some(v) = arg.strip_prefix("--socket=") {
            socket = Some(PathBuf::from(v));
        } else if let Some(v) = arg.strip_prefix("--repo=") {
            repository_root = Some(v.to_owned());
        } else if let Some(v) = arg.strip_prefix("--task=") {
            task = Some(v.to_owned());
        } else if let Some(v) = arg.strip_prefix("--run-id=") {
            run_id = Some(v.to_owned());
        } else if let Some(v) = arg.strip_prefix("--worktree=") {
            worktree = Some(v.to_owned());
        } else {
            return Err(format!(
                "unknown option {arg:?}; usage: dock-dispatch --repo=PATH --task=REF --run-id=dock_ID --worktree=PATH [--socket=PATH] -- COMMAND [ARG ...]"
            ));
        }
    }
    let request = DispatchRequest {
        repository_root: repository_root.ok_or("--repo is required")?,
        external_task_ref: task.ok_or("--task is required")?,
        run_id: run_id.unwrap_or_else(generate_run_id),
        worktree: worktree.ok_or("--worktree is required")?,
        command,
    };
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
    send(&mut stream, &Request::Dispatch(request))?;
    match receive(&mut reader)? {
        Response::Dispatched { snapshot } => {
            println!(
                "{}",
                serde_json::to_string_pretty(&snapshot).map_err(|e| e.to_string())?
            );
            Ok(())
        }
        Response::Error { message, .. } => Err(message),
        r => Err(format!("unexpected dispatch response: {r:?}")),
    }
}
fn generate_run_id() -> String {
    format!(
        "dock_{}_{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock after epoch")
            .as_nanos()
    )
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
