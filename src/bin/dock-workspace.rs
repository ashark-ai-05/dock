use dock::{
    layout::SplitAxis,
    paths,
    protocol::{HelloRequest, PROTOCOL_VERSION, Request, Response, WorkspaceRequest},
};
use std::{
    io::{BufRead, BufReader, Write},
    os::unix::net::UnixStream,
    path::PathBuf,
};

fn main() -> Result<(), String> {
    let mut args: Vec<String> = std::env::args().skip(1).collect();
    let socket = args
        .iter()
        .position(|a| a.starts_with("--socket="))
        .map(|i| args.remove(i))
        .and_then(|a| a.strip_prefix("--socket=").map(PathBuf::from))
        .map_or_else(paths::default_socket_path, Ok)?;
    let operation = parse(&args)?;
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
        response => return Err(format!("unexpected handshake response: {response:?}")),
    }
    send(&mut stream, &Request::Workspace(operation))?;
    match receive(&mut reader)? {
        Response::Layout { layout } => {
            println!("{}", serde_json::to_string_pretty(&layout).unwrap())
        }
        Response::WorkspaceChanged { workspace } => {
            println!("{}", serde_json::to_string_pretty(&workspace).unwrap())
        }
        Response::Error { message, .. } => return Err(message),
        other => return Err(format!("unexpected response: {other:?}")),
    }
    Ok(())
}

fn parse(a: &[String]) -> Result<WorkspaceRequest, String> {
    let usage = "usage: dock-workspace [--socket=PATH] inspect | create ID NAME PANE | split WORKSPACE PANE NEW_PANE horizontal|vertical | focus WORKSPACE PANE | resize WORKSPACE PANE RATIO_MILLI | rename-workspace WORKSPACE NAME | rename-pane WORKSPACE PANE NAME | close WORKSPACE PANE | respawn WORKSPACE PANE";
    match a.iter().map(String::as_str).collect::<Vec<_>>().as_slice() {
        ["inspect"] => Ok(WorkspaceRequest::Inspect),
        ["create", w, n, p] => Ok(WorkspaceRequest::Create {
            workspace_id: (*w).into(),
            name: (*n).into(),
            pane_id: (*p).into(),
        }),
        ["split", w, p, n, axis] => Ok(WorkspaceRequest::Split {
            workspace_id: (*w).into(),
            pane_id: (*p).into(),
            new_pane_id: (*n).into(),
            axis: match *axis {
                "horizontal" => SplitAxis::Horizontal,
                "vertical" => SplitAxis::Vertical,
                _ => return Err(usage.into()),
            },
        }),
        ["focus", w, p] => Ok(WorkspaceRequest::Focus {
            workspace_id: (*w).into(),
            pane_id: (*p).into(),
        }),
        ["resize", w, p, r] => Ok(WorkspaceRequest::Resize {
            workspace_id: (*w).into(),
            pane_id: (*p).into(),
            ratio_milli: r.parse().map_err(|_| usage.to_owned())?,
        }),
        ["rename-workspace", w, n] => Ok(WorkspaceRequest::Rename {
            workspace_id: (*w).into(),
            pane_id: None,
            name: (*n).into(),
        }),
        ["rename-pane", w, p, n] => Ok(WorkspaceRequest::Rename {
            workspace_id: (*w).into(),
            pane_id: Some((*p).into()),
            name: (*n).into(),
        }),
        ["close", w, p] => Ok(WorkspaceRequest::Close {
            workspace_id: (*w).into(),
            pane_id: (*p).into(),
        }),
        ["respawn", w, p] => Ok(WorkspaceRequest::Respawn {
            workspace_id: (*w).into(),
            pane_id: (*p).into(),
        }),
        _ => Err(usage.into()),
    }
}
fn send(s: &mut UnixStream, r: &Request) -> Result<(), String> {
    serde_json::to_writer(&mut *s, r).map_err(|e| e.to_string())?;
    s.write_all(b"\n").map_err(|e| e.to_string())
}
fn receive(r: &mut impl BufRead) -> Result<Response, String> {
    let mut line = String::new();
    if r.read_line(&mut line).map_err(|e| e.to_string())? == 0 {
        return Err("daemon closed connection".into());
    }
    serde_json::from_str(&line).map_err(|e| e.to_string())
}
