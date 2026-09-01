//! `dock split` — split the current pane, using DOCK_WORKSPACE / DOCK_PANE when inside one.

use std::path::PathBuf;

use crate::{
    cli::wire::{Connection, print_json},
    layout::{PaneKind, SplitAxis},
    protocol::{Request, Response, WorkspaceRequest},
};

const USAGE: &str = "usage: dock split horizontal|vertical [NEW_PANE] [--socket=PATH]\n\
                     (defaults: DOCK_WORKSPACE, DOCK_PANE; NEW_PANE is generated if omitted)";

pub(crate) fn parse_arguments(args: &[String]) -> Result<(Option<PathBuf>, Request), String> {
    let mut socket = None;
    let mut rest = Vec::new();
    for argument in args {
        match argument.strip_prefix("--socket=") {
            Some(value) => socket = Some(PathBuf::from(value)),
            None => rest.push(argument.clone()),
        }
    }
    if socket.is_none() {
        socket = std::env::var_os("DOCK_SOCKET").map(PathBuf::from);
    }
    let axis = match rest.first().map(String::as_str) {
        Some("horizontal") => SplitAxis::Horizontal,
        Some("vertical") => SplitAxis::Vertical,
        _ => return Err(USAGE.into()),
    };
    let new_pane_id = rest.get(1).cloned().unwrap_or_else(|| {
        format!(
            "pane_{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_millis())
                .unwrap_or(0)
        )
    });
    let workspace_id = std::env::var("DOCK_WORKSPACE").map_err(|_| {
        format!("DOCK_WORKSPACE is not set; run this inside a Dock pane, or use `dock workspace split`; {USAGE}")
    })?;
    let pane_id = std::env::var("DOCK_PANE").map_err(|_| {
        format!("DOCK_PANE is not set; run this inside a Dock pane, or use `dock workspace split`; {USAGE}")
    })?;
    Ok((
        socket,
        Request::Workspace(WorkspaceRequest::Split {
            workspace_id,
            pane_id,
            new_pane_id,
            axis,
            kind: PaneKind::Terminal,
        }),
    ))
}

pub fn run(args: &[String]) -> Result<(), String> {
    let (socket, request) = parse_arguments(args)?;
    let response = Connection::open(socket)?.request(&request)?;
    match response {
        Response::Layout { layout } => print_json(&layout),
        Response::WorkspaceChanged { workspace } => print_json(&workspace),
        Response::Error { message, .. } => Err(message),
        response => Err(format!("unexpected split response: {response:?}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn axis_is_required() {
        let error = parse_arguments(&[]).unwrap_err();
        assert!(error.contains("usage:"), "{error}");
    }
}
