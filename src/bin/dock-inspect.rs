use std::{
    io::{BufRead, BufReader, Write},
    os::unix::net::UnixStream,
    path::PathBuf,
};

use dock::{
    paths,
    protocol::{HelloRequest, InspectRequest, PROTOCOL_VERSION, Request, Response},
};

fn main() -> Result<(), String> {
    let args: Vec<_> = std::env::args().skip(1).collect();
    let socket = args
        .iter()
        .find_map(|argument| argument.strip_prefix("--socket=").map(PathBuf::from))
        .map_or_else(paths::default_socket_path, Ok)?;
    let run_id = args
        .iter()
        .find_map(|argument| argument.strip_prefix("--run-id=").map(str::to_owned));
    let mut stream = UnixStream::connect(&socket)
        .map_err(|error| format!("could not connect to {}: {error}", socket.display()))?;
    let reader_stream = stream.try_clone().map_err(|error| error.to_string())?;
    let mut reader = BufReader::new(reader_stream);
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
    send(&mut stream, &Request::Inspect(InspectRequest { run_id }))?;
    match receive(&mut reader)? {
        Response::Snapshot { snapshot } => {
            println!(
                "{}",
                serde_json::to_string_pretty(&snapshot).map_err(|error| error.to_string())?
            );
            Ok(())
        }
        Response::Snapshots { snapshots } => {
            println!(
                "{}",
                serde_json::to_string_pretty(&snapshots).map_err(|error| error.to_string())?
            );
            Ok(())
        }
        Response::Error { message, .. } => Err(message),
        response => Err(format!("unexpected inspect response: {response:?}")),
    }
}

fn send(stream: &mut UnixStream, request: &Request) -> Result<(), String> {
    serde_json::to_writer(&mut *stream, request).map_err(|error| error.to_string())?;
    stream.write_all(b"\n").map_err(|error| error.to_string())
}

fn receive(reader: &mut impl BufRead) -> Result<Response, String> {
    let mut line = String::new();
    if reader
        .read_line(&mut line)
        .map_err(|error| error.to_string())?
        == 0
    {
        return Err("daemon closed the connection".into());
    }
    serde_json::from_str(&line).map_err(|error| format!("invalid daemon response: {error}"))
}
