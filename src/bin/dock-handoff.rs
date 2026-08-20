use std::{
    fs,
    io::{BufRead, BufReader, Write},
    os::unix::net::UnixStream,
    path::PathBuf,
};

use dock::{
    model::{HandoffPacket, ReviewRoute},
    paths,
    protocol::{
        DecideRequest, HelloRequest, PROTOCOL_VERSION, Request, Response, ReviewInboxRequest,
        SubmitHandoffRequest,
    },
};

fn main() -> Result<(), String> {
    let mut socket = None;
    let mut packet = None;
    let mut run_id = None;
    let mut route = None;
    let mut note = None;
    let mut inbox = false;
    for argument in std::env::args().skip(1) {
        if let Some(value) = argument.strip_prefix("--socket=") {
            socket = Some(PathBuf::from(value));
        } else if let Some(value) = argument.strip_prefix("--submit=") {
            packet = Some(PathBuf::from(value));
        } else if argument == "--inbox" {
            inbox = true;
        } else if let Some(value) = argument.strip_prefix("--run-id=") {
            run_id = Some(value.to_owned());
        } else if let Some(value) = argument.strip_prefix("--route=") {
            route = Some(match value {
                "accept-scope" => ReviewRoute::AcceptScope,
                "request-change" => ReviewRoute::RequestChange,
                _ => return Err("--route must be accept-scope or request-change".into()),
            });
        } else if let Some(value) = argument.strip_prefix("--note=") {
            note = Some(value.to_owned());
        } else {
            return Err(format!("unknown option {argument:?}"));
        }
    }
    let (request, expected) = match (packet, inbox, route) {
        (Some(path), false, None) => {
            let packet: HandoffPacket = serde_json::from_slice(
                &fs::read(path).map_err(|error| format!("could not read packet: {error}"))?,
            )
            .map_err(|error| format!("invalid handoff packet: {error}"))?;
            (
                Request::SubmitHandoff(SubmitHandoffRequest { packet }),
                ExpectedResponse::Submit,
            )
        }
        (None, true, None) => (
            Request::ReviewInbox(ReviewInboxRequest {}),
            ExpectedResponse::Inbox,
        ),
        (None, false, Some(route)) => (
            Request::Decide(DecideRequest {
                run_id: run_id.ok_or("--run-id is required for a decision")?,
                route,
                note: note.ok_or("--note is required for a decision")?,
            }),
            ExpectedResponse::Decision,
        ),
        _ => return Err("usage: dock-handoff [--socket=PATH] (--submit=PACKET.json | --inbox | --run-id=dock_ID --route=accept-scope|request-change --note=TEXT)".into()),
    };
    let socket = socket.map_or_else(paths::default_socket_path, Ok)?;
    let mut stream = UnixStream::connect(&socket)
        .map_err(|error| format!("could not connect to {}: {error}", socket.display()))?;
    let mut reader = BufReader::new(stream.try_clone().map_err(|error| error.to_string())?);
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
    send(&mut stream, &request)?;
    let response = receive(&mut reader)?;
    require_expected_response(expected, &response)?;
    println!(
        "{}",
        serde_json::to_string_pretty(&response).map_err(|error| error.to_string())?
    );
    Ok(())
}

#[derive(Clone, Copy)]
enum ExpectedResponse {
    Submit,
    Inbox,
    Decision,
}

fn require_expected_response(
    expected: ExpectedResponse,
    response: &Response,
) -> Result<(), String> {
    if let Response::Error { message, .. } = response {
        return Err(message.clone());
    }
    let matches = matches!(
        (expected, response),
        (ExpectedResponse::Submit, Response::HandoffSubmitted { .. })
            | (ExpectedResponse::Inbox, Response::ReviewInbox { .. })
            | (
                ExpectedResponse::Decision,
                Response::DecisionRecorded { .. }
            )
    );
    if matches {
        Ok(())
    } else {
        Err(format!("unexpected operation response: {response:?}"))
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn each_operation_rejects_an_unexpected_success_variant() {
        let unrelated = Response::Hello {
            version: PROTOCOL_VERSION,
        };
        for expected in [
            ExpectedResponse::Submit,
            ExpectedResponse::Inbox,
            ExpectedResponse::Decision,
        ] {
            assert!(require_expected_response(expected, &unrelated).is_err());
        }
    }
}
