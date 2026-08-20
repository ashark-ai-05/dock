use dock::{
    paths,
    protocol::{
        HelloRequest, LifecycleOperation, LifecycleRequest, PROTOCOL_VERSION, Request, Response,
    },
};
use std::{
    io::{BufRead, BufReader, Write},
    os::unix::net::UnixStream,
    path::PathBuf,
};

fn main() -> Result<(), String> {
    let mut socket: Option<PathBuf> = None;
    let mut run_id = None;
    let mut operation = None;
    for arg in std::env::args().skip(1) {
        if let Some(v) = arg.strip_prefix("--socket=") {
            socket = Some(v.into());
        } else if let Some(v) = arg.strip_prefix("--run-id=") {
            run_id = Some(v.to_owned());
        } else if let Some(v) = arg.strip_prefix("--operation=") {
            operation = Some(match v {
                "attach" => LifecycleOperation::Attach,
                "focus" => LifecycleOperation::Focus,
                "interrupt" => LifecycleOperation::Interrupt,
                "stop" => LifecycleOperation::Stop,
                "restart" => LifecycleOperation::Restart,
                _ => return Err(format!("unknown lifecycle operation {v:?}")),
            });
        } else {
            return Err(format!(
                "unknown option {arg:?}; usage: dock-agent --run-id=dock_ID --operation=attach|focus|interrupt|stop|restart [--socket=PATH]"
            ));
        }
    }
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
    send(
        &mut stream,
        &Request::Lifecycle(LifecycleRequest {
            run_id: run_id.ok_or("--run-id is required")?,
            operation: operation.ok_or("--operation is required")?,
        }),
    )?;
    match receive(&mut reader)? {
        Response::LifecycleApplied { snapshot, .. } => {
            println!(
                "{}",
                serde_json::to_string_pretty(&snapshot).map_err(|e| e.to_string())?
            );
            Ok(())
        }
        Response::Error { message, .. } => Err(message),
        r => Err(format!("unexpected lifecycle response: {r:?}")),
    }
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
