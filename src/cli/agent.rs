//! `dock agent` — attach, focus, interrupt, stop or restart one run.

use std::path::PathBuf;

use crate::{
    cli::wire::{Connection, print_json},
    protocol::{LifecycleOperation, LifecycleRequest, Request, Response},
};

const USAGE: &str = "usage: dock agent --run-id=dock_ID \
                     --operation=attach|focus|interrupt|stop|restart [--socket=PATH]";

pub(crate) fn parse_arguments(args: &[String]) -> Result<(Option<PathBuf>, Request), String> {
    let mut socket = None;
    let mut run_id = None;
    let mut operation = None;
    for argument in args {
        if let Some(value) = argument.strip_prefix("--socket=") {
            socket = Some(PathBuf::from(value));
        } else if let Some(value) = argument.strip_prefix("--run-id=") {
            run_id = Some(value.to_owned());
        } else if let Some(value) = argument.strip_prefix("--operation=") {
            operation = Some(match value {
                "attach" => LifecycleOperation::Attach,
                "focus" => LifecycleOperation::Focus,
                "interrupt" => LifecycleOperation::Interrupt,
                "stop" => LifecycleOperation::Stop,
                "restart" => LifecycleOperation::Restart,
                _ => return Err(format!("unknown lifecycle operation {value:?}; {USAGE}")),
            });
        } else {
            return Err(format!("unknown option {argument:?}; {USAGE}"));
        }
    }
    Ok((
        socket,
        Request::Lifecycle(LifecycleRequest {
            run_id: run_id.ok_or(format!("--run-id is required; {USAGE}"))?,
            operation: operation.ok_or(format!("--operation is required; {USAGE}"))?,
        }),
    ))
}

pub(crate) fn render(response: Response) -> Result<(), String> {
    match response {
        Response::LifecycleApplied { snapshot, .. } => print_json(&snapshot),
        Response::Error { message, .. } => Err(message),
        response => Err(format!("unexpected agent response: {response:?}")),
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
    fn every_lifecycle_operation_is_spelled_the_way_the_protocol_spells_it() {
        for (spelled, expected) in [
            ("attach", LifecycleOperation::Attach),
            ("focus", LifecycleOperation::Focus),
            ("interrupt", LifecycleOperation::Interrupt),
            ("stop", LifecycleOperation::Stop),
            ("restart", LifecycleOperation::Restart),
        ] {
            let (_, request) = parse_arguments(&[
                "--run-id=dock_1".to_owned(),
                format!("--operation={spelled}"),
            ])
            .unwrap_or_else(|error| panic!("{spelled}: {error}"));
            match request {
                Request::Lifecycle(lifecycle) => assert_eq!(lifecycle.operation, expected),
                other => panic!("{spelled} produced {other:?}"),
            }
        }
    }

    #[test]
    fn both_halves_of_the_instruction_are_required() {
        assert!(
            parse_arguments(&["--operation=stop".to_owned()])
                .unwrap_err()
                .starts_with("--run-id")
        );
        assert!(
            parse_arguments(&["--run-id=dock_1".to_owned()])
                .unwrap_err()
                .starts_with("--operation")
        );
        assert!(
            parse_arguments(&[
                "--run-id=dock_1".to_owned(),
                "--operation=levitate".to_owned()
            ])
            .unwrap_err()
            .contains("levitate")
        );
    }
}
