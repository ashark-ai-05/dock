//! The conversation every non-interactive verb has with the daemon.
//!
//! Six binaries each carried a private copy of these twenty lines, which meant six places for
//! the framing to drift and six places a handshake could be forgotten. One copy, and a verb is
//! left with the only two things that are actually its own: what it parses, and what it prints.

use std::{
    io::{BufRead, BufReader, Write},
    os::unix::net::UnixStream,
    path::PathBuf,
};

use crate::{
    paths,
    protocol::{HelloRequest, PROTOCOL_VERSION, Request, Response},
};

/// One request, newline-framed. Generic over the sink so the framing is testable without a
/// socket, which is the whole reason a codec is worth separating from a connection.
pub fn encode(request: &Request, out: &mut impl Write) -> Result<(), String> {
    serde_json::to_writer(&mut *out, request).map_err(|error| error.to_string())?;
    out.write_all(b"\n").map_err(|error| error.to_string())
}

/// One response, read back from a line.
pub fn decode(reader: &mut impl BufRead) -> Result<Response, String> {
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

/// A connected, handshaken socket.
///
/// Opening one performs the `Hello` exchange, because a connection that has not agreed a
/// protocol version is not usable and every caller did it identically anyway.
pub struct Connection {
    stream: UnixStream,
    reader: BufReader<UnixStream>,
}

impl Connection {
    pub fn open(socket: Option<PathBuf>) -> Result<Self, String> {
        let socket = socket.map_or_else(paths::default_socket_path, Ok)?;
        let stream = UnixStream::connect(&socket)
            .map_err(|error| format!("could not connect to {}: {error}", socket.display()))?;
        let reader = BufReader::new(stream.try_clone().map_err(|error| error.to_string())?);
        let mut connection = Self { stream, reader };
        match connection.request(&Request::Hello(HelloRequest {
            version: PROTOCOL_VERSION,
        }))? {
            Response::Hello {
                version: PROTOCOL_VERSION,
            } => Ok(connection),
            Response::Error { message, .. } => Err(message),
            response => Err(format!("unexpected handshake response: {response:?}")),
        }
    }

    pub fn request(&mut self, request: &Request) -> Result<Response, String> {
        encode(request, &mut self.stream)?;
        decode(&mut self.reader)
    }
}

/// What every one of these verbs does with the answer it got.
pub fn print_json(value: &impl serde::Serialize) -> Result<(), String> {
    println!(
        "{}",
        serde_json::to_string_pretty(value).map_err(|error| error.to_string())?
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::InspectRequest;

    #[test]
    fn a_request_is_one_json_line_and_a_response_is_read_back_from_one() {
        let mut written = Vec::new();
        encode(
            &Request::Inspect(InspectRequest { run_id: None }),
            &mut written,
        )
        .expect("encode");
        assert!(
            written.ends_with(b"\n"),
            "the daemon reads by line, so the newline is the frame: {written:?}"
        );
        assert_eq!(
            written.iter().filter(|byte| **byte == b'\n').count(),
            1,
            "one request is one line"
        );

        let mut reader = &b"{\"type\":\"hello\",\"version\":13}\n"[..];
        assert!(matches!(
            decode(&mut reader).expect("decode"),
            Response::Hello { .. }
        ));
    }

    #[test]
    fn a_closed_connection_is_said_rather_than_parsed() {
        // An empty read is the daemon having gone away. Reporting that as a JSON error would
        // send whoever reads it looking for a malformed message that was never sent.
        let mut reader = &b""[..];
        assert_eq!(
            decode(&mut reader).unwrap_err(),
            "daemon closed the connection"
        );
    }
}
