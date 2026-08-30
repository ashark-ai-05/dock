//! `dock review` — the command-line face of the review queue `Ctrl+B i` opens.
//!
//! Named for the surface rather than for the packet. `dock handoff` is what an *agent* runs to
//! say what it did; this is what a *person* runs to read those and decide. They were one word
//! with two meanings, and the word belongs to the agent because that is the one with a
//! positional argument and the one the README teaches.

use std::{fs, path::PathBuf};

use crate::{
    cli::wire::{Connection, print_json},
    model::{HandoffPacket, ReviewRoute},
    protocol::{DecideRequest, Request, Response, ReviewInboxRequest, SubmitHandoffRequest},
};

const USAGE: &str = "usage: dock review [--socket=PATH] (--inbox | --submit=PACKET.json | \
                     --run-id=dock_ID --route=accept-scope|request-change --note=TEXT)";

/// Which success the daemon owes for the request that was sent.
///
/// Kept rather than collapsed into `render`'s match: this is the only verb whose three
/// requests have three different right answers, and pairing them is a claim worth asserting.
/// Deleting it would take its test with it.
#[derive(Debug, Clone, Copy)]
pub(crate) enum ExpectedResponse {
    Submit,
    Inbox,
    Decision,
}

pub(crate) fn parse_arguments(
    args: &[String],
) -> Result<(Option<PathBuf>, Request, ExpectedResponse), String> {
    let mut socket = None;
    let mut packet = None;
    let mut run_id = None;
    let mut route = None;
    let mut note = None;
    let mut inbox = false;
    for argument in args {
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
            return Err(format!("unknown option {argument:?}; {USAGE}"));
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
                run_id: run_id.ok_or(format!("--run-id is required for a decision; {USAGE}"))?,
                route,
                note: note.ok_or(format!("--note is required for a decision; {USAGE}"))?,
            }),
            ExpectedResponse::Decision,
        ),
        _ => return Err(USAGE.into()),
    };
    Ok((socket, request, expected))
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
        Err(format!("unexpected review response: {response:?}"))
    }
}

/// The whole response, not a field of it. Each of the three carries a different payload and
/// the original printed the envelope; narrowing to an inner field here would quietly change
/// what every existing script reads.
pub(crate) fn render(expected: ExpectedResponse, response: Response) -> Result<(), String> {
    require_expected_response(expected, &response)?;
    print_json(&response)
}

pub fn run(args: &[String]) -> Result<(), String> {
    let (socket, request, expected) = parse_arguments(args)?;
    let response = Connection::open(socket)?.request(&request)?;
    render(expected, response)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_three_things_a_reviewer_can_do_are_each_reachable() {
        let (_, request, _) = parse_arguments(&["--inbox".to_owned()]).expect("inbox");
        assert!(matches!(request, Request::ReviewInbox(_)));

        let (_, request, _) = parse_arguments(&[
            "--run-id=dock_7".to_owned(),
            "--route=accept-scope".to_owned(),
            "--note=looks right".to_owned(),
        ])
        .expect("decision");
        match request {
            Request::Decide(decide) => {
                assert_eq!(decide.run_id, "dock_7");
                assert_eq!(decide.route, ReviewRoute::AcceptScope);
                assert_eq!(decide.note, "looks right");
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn a_decision_cannot_be_recorded_without_saying_why() {
        // A route with no note is a verdict nobody can act on, and the review queue's whole
        // claim is that a decision is recorded rather than merely reached.
        let error = parse_arguments(&[
            "--run-id=dock_7".to_owned(),
            "--route=request-change".to_owned(),
        ])
        .unwrap_err();
        assert!(error.starts_with("--note"), "{error}");
    }

    #[test]
    fn asking_for_two_things_at_once_is_refused_with_the_usage() {
        let error = parse_arguments(&["--inbox".to_owned(), "--submit=packet.json".to_owned()])
            .unwrap_err();
        assert!(error.contains("usage:"), "{error}");
    }

    #[test]
    fn an_unknown_route_names_the_two_that_exist() {
        let error = parse_arguments(&["--route=maybe".to_owned()]).unwrap_err();
        assert!(error.contains("accept-scope"), "{error}");
        assert!(error.contains("request-change"), "{error}");
    }

    #[test]
    fn each_operation_rejects_an_unexpected_success_variant() {
        let unrelated = Response::Hello {
            version: crate::protocol::PROTOCOL_VERSION,
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
