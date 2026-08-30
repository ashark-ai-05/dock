//! `dock inspect` — what the daemon knows about one run, or about all of them.

use std::path::PathBuf;

use crate::{
    cli::wire::{Connection, print_json},
    protocol::{InspectRequest, Request, Response},
};

const USAGE: &str = "usage: dock inspect [--run-id=dock_ID] [--socket=PATH]";

pub fn parse_arguments(args: &[String]) -> Result<(Option<PathBuf>, Request), String> {
    let mut socket = None;
    let mut run_id = None;
    for argument in args {
        if let Some(value) = argument.strip_prefix("--socket=") {
            socket = Some(PathBuf::from(value));
        } else if let Some(value) = argument.strip_prefix("--run-id=") {
            run_id = Some(value.to_owned());
        } else {
            return Err(format!("unknown option {argument:?}; {USAGE}"));
        }
    }
    Ok((socket, Request::Inspect(InspectRequest { run_id })))
}

pub fn render(response: Response) -> Result<(), String> {
    match response {
        Response::Snapshot { snapshot } => print_json(&snapshot),
        Response::Snapshots { snapshots } => print_json(&snapshots),
        Response::Error { message, .. } => Err(message),
        response => Err(format!("unexpected inspect response: {response:?}")),
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
    fn a_run_id_is_optional_and_a_socket_can_be_named() {
        let (socket, request) = parse_arguments(&[]).expect("no arguments is the whole daemon");
        assert!(socket.is_none());
        assert!(matches!(
            request,
            Request::Inspect(InspectRequest { run_id: None })
        ));

        let (socket, request) = parse_arguments(&[
            "--socket=/tmp/x.sock".to_owned(),
            "--run-id=dock_7".to_owned(),
        ])
        .expect("both flags");
        assert_eq!(socket, Some(PathBuf::from("/tmp/x.sock")));
        assert!(matches!(
            request,
            Request::Inspect(InspectRequest { run_id: Some(ref id) }) if id == "dock_7"
        ));
    }

    #[test]
    fn an_unknown_flag_is_refused_rather_than_ignored() {
        // A typo'd flag that is silently dropped is a command that quietly did something other
        // than what was asked, which is the one outcome a scripting surface must not have.
        let error = parse_arguments(&["--run-ids=dock_7".to_owned()]).unwrap_err();
        assert!(error.contains("--run-ids"), "{error}");
        assert!(error.contains("usage:"), "{error}");
    }
}
