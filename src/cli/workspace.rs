//! `dock workspace` — create, split, focus, resize, rename and close panes non-interactively.

use std::path::PathBuf;

use crate::{
    cli::wire::{Connection, print_json},
    layout::{PaneKind, SplitAxis},
    protocol::{Request, Response, WorkspaceRequest},
};

const USAGE: &str = "usage: dock workspace [--socket=PATH] inspect | create ID NAME PANE | \
    split WORKSPACE PANE NEW_PANE horizontal|vertical [terminal|board] | focus WORKSPACE PANE | \
    resize WORKSPACE PANE RATIO_MILLI | rename-workspace WORKSPACE NAME | \
    rename-pane WORKSPACE PANE NAME | close WORKSPACE PANE | respawn WORKSPACE PANE";

pub fn parse_arguments(args: &[String]) -> Result<(Option<PathBuf>, Request), String> {
    // Taken out first: the operation is matched on position, so a flag left among the
    // positionals would be read as part of the operation's name.
    let mut rest: Vec<String> = Vec::with_capacity(args.len());
    let mut socket = None;
    for argument in args {
        match argument.strip_prefix("--socket=") {
            Some(value) => socket = Some(PathBuf::from(value)),
            None => rest.push(argument.clone()),
        }
    }
    let operation = match rest
        .iter()
        .map(String::as_str)
        .collect::<Vec<_>>()
        .as_slice()
    {
        ["inspect"] => WorkspaceRequest::Inspect,
        ["create", workspace, name, pane] => WorkspaceRequest::Create {
            workspace_id: (*workspace).into(),
            name: (*name).into(),
            pane_id: (*pane).into(),
        },
        // The kind is optional and trailing, so every existing invocation still means what it
        // meant: a split with no kind is the terminal split it always was.
        ["split", workspace, pane, new_pane, axis]
        | ["split", workspace, pane, new_pane, axis, _] => WorkspaceRequest::Split {
            workspace_id: (*workspace).into(),
            pane_id: (*pane).into(),
            new_pane_id: (*new_pane).into(),
            axis: match *axis {
                "horizontal" => SplitAxis::Horizontal,
                "vertical" => SplitAxis::Vertical,
                _ => return Err(USAGE.into()),
            },
            kind: match rest.get(5).map(String::as_str) {
                None | Some("terminal") => PaneKind::Terminal,
                Some("board") => PaneKind::Board,
                Some(_) => return Err(USAGE.into()),
            },
        },
        ["focus", workspace, pane] => WorkspaceRequest::Focus {
            workspace_id: (*workspace).into(),
            pane_id: (*pane).into(),
        },
        ["resize", workspace, pane, ratio] => WorkspaceRequest::Resize {
            workspace_id: (*workspace).into(),
            pane_id: (*pane).into(),
            ratio_milli: ratio.parse().map_err(|_| USAGE.to_owned())?,
        },
        ["rename-workspace", workspace, name] => WorkspaceRequest::Rename {
            workspace_id: (*workspace).into(),
            pane_id: None,
            name: (*name).into(),
        },
        ["rename-pane", workspace, pane, name] => WorkspaceRequest::Rename {
            workspace_id: (*workspace).into(),
            pane_id: Some((*pane).into()),
            name: (*name).into(),
        },
        ["close", workspace, pane] => WorkspaceRequest::Close {
            workspace_id: (*workspace).into(),
            pane_id: (*pane).into(),
        },
        ["respawn", workspace, pane] => WorkspaceRequest::Respawn {
            workspace_id: (*workspace).into(),
            pane_id: (*pane).into(),
        },
        _ => return Err(USAGE.into()),
    };
    Ok((socket, Request::Workspace(operation)))
}

pub fn render(response: Response) -> Result<(), String> {
    match response {
        Response::Layout { layout } => print_json(&layout),
        Response::WorkspaceChanged { workspace } => print_json(&workspace),
        Response::Error { message, .. } => Err(message),
        response => Err(format!("unexpected workspace response: {response:?}")),
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
    fn a_split_without_a_kind_is_still_the_terminal_split_it_always_was() {
        let (_, request) = parse_arguments(&[
            "split".to_owned(),
            "w".to_owned(),
            "a".to_owned(),
            "b".to_owned(),
            "vertical".to_owned(),
        ])
        .expect("parse");
        match request {
            Request::Workspace(WorkspaceRequest::Split { axis, kind, .. }) => {
                assert_eq!(axis, SplitAxis::Vertical);
                assert_eq!(kind, PaneKind::Terminal);
            }
            other => panic!("{other:?}"),
        }

        let (_, request) = parse_arguments(&[
            "split".to_owned(),
            "w".to_owned(),
            "a".to_owned(),
            "b".to_owned(),
            "horizontal".to_owned(),
            "board".to_owned(),
        ])
        .expect("parse");
        match request {
            Request::Workspace(WorkspaceRequest::Split { axis, kind, .. }) => {
                assert_eq!(axis, SplitAxis::Horizontal);
                assert_eq!(kind, PaneKind::Board);
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn the_socket_flag_is_removed_before_the_positionals_are_read() {
        // The flag may appear anywhere, and the operation is matched on position, so failing to
        // take it out first makes `--socket=… inspect` parse as an unknown two-word operation.
        let (socket, request) =
            parse_arguments(&["--socket=/tmp/x.sock".to_owned(), "inspect".to_owned()])
                .expect("parse");
        assert_eq!(socket, Some(PathBuf::from("/tmp/x.sock")));
        assert!(matches!(
            request,
            Request::Workspace(WorkspaceRequest::Inspect)
        ));
    }

    #[test]
    fn an_unrecognised_operation_answers_with_the_whole_usage() {
        let error = parse_arguments(&["teleport".to_owned()]).unwrap_err();
        assert!(error.contains("usage:"), "{error}");
        assert!(error.contains("rename-workspace"), "{error}");
    }
}
