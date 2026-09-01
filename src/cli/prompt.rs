//! `dock prompt` — type text into a pane, using DOCK_WORKSPACE / DOCK_PANE when inside one.
//!
//! Distinct from `dock queue add`: this writes bytes now. The queue holds work until the pane
//! is ready to take it.

use std::io::{self, IsTerminal, Read};
use std::path::PathBuf;

use crate::{
    cli::wire::Connection,
    protocol::{PaneInputRequest, Request, Response},
};

const USAGE: &str = "usage: dock prompt [--workspace=ID] [--pane=ID] [--socket=PATH] [--no-newline] [--] TEXT\n\
                     (defaults: DOCK_WORKSPACE, DOCK_PANE; TEXT may be omitted to read stdin)";

#[derive(Debug)]
struct PromptArgs {
    socket: Option<PathBuf>,
    workspace_id: String,
    pane_id: String,
    text: String,
}

fn parse_arguments(args: &[String]) -> Result<PromptArgs, String> {
    let (head, tail) = match args.iter().position(|argument| argument == "--") {
        Some(index) => (&args[..index], &args[index + 1..]),
        None => (args, &args[args.len()..]),
    };
    let mut socket = None;
    let mut workspace = None;
    let mut pane = None;
    let mut newline = true;
    let mut positional: Vec<&str> = Vec::new();
    for argument in head {
        if let Some(value) = argument.strip_prefix("--socket=") {
            socket = Some(PathBuf::from(value));
        } else if let Some(value) = argument.strip_prefix("--workspace=") {
            workspace = Some(value.to_owned());
        } else if let Some(value) = argument.strip_prefix("--pane=") {
            pane = Some(value.to_owned());
        } else if argument == "--no-newline" {
            newline = false;
        } else if argument.starts_with("--") {
            return Err(format!("unknown option {argument:?}; {USAGE}"));
        } else {
            positional.push(argument);
        }
    }
    positional.extend(tail.iter().map(String::as_str));
    if socket.is_none() {
        socket = std::env::var_os("DOCK_SOCKET").map(PathBuf::from);
    }
    let workspace_id = workspace
        .or_else(|| {
            std::env::var("DOCK_WORKSPACE")
                .ok()
                .filter(|id| !id.is_empty())
        })
        .ok_or_else(|| {
            format!("--workspace is required (or set DOCK_WORKSPACE inside a Dock pane); {USAGE}")
        })?;
    let pane_id = pane
        .or_else(|| std::env::var("DOCK_PANE").ok().filter(|id| !id.is_empty()))
        .ok_or_else(|| {
            format!("--pane is required (or set DOCK_PANE inside a Dock pane); {USAGE}")
        })?;
    let mut text = if positional.is_empty() {
        String::new()
    } else {
        positional.join(" ")
    };
    if text.is_empty() && !io::stdin().is_terminal() {
        io::stdin()
            .read_to_string(&mut text)
            .map_err(|error| error.to_string())?;
        // stdin already carries its own newlines; do not add a second.
        newline = false;
    }
    if text.is_empty() {
        return Err(format!("a prompt needs text; {USAGE}"));
    }
    if newline && !text.ends_with('\n') {
        text.push('\n');
    }
    Ok(PromptArgs {
        socket,
        workspace_id,
        pane_id,
        text,
    })
}

pub fn run(args: &[String]) -> Result<(), String> {
    let args = parse_arguments(args)?;
    let request = Request::PaneInput(PaneInputRequest {
        workspace_id: args.workspace_id,
        pane_id: args.pane_id,
        input: PaneInputRequest::encode(args.text.as_bytes()),
    });
    match Connection::open(args.socket)?.request(&request)? {
        Response::PaneInputAccepted { bytes, pane_id, .. } => {
            println!("{pane_id}\t{bytes}");
            Ok(())
        }
        Response::Error { message, .. } => Err(message),
        response => Err(format!("unexpected prompt response: {response:?}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn flags_and_text_are_parsed() {
        let parsed = parse_arguments(&[
            "--workspace=w1".to_owned(),
            "--pane=p1".to_owned(),
            "keep going".to_owned(),
        ])
        .expect("parse");
        assert_eq!(parsed.workspace_id, "w1");
        assert_eq!(parsed.pane_id, "p1");
        assert_eq!(parsed.text, "keep going\n");
    }

    #[test]
    fn a_prompt_that_starts_with_dashes_goes_after_a_bare_double_dash() {
        let parsed = parse_arguments(&[
            "--workspace=w1".to_owned(),
            "--pane=p1".to_owned(),
            "--".to_owned(),
            "--task=7 is not a flag".to_owned(),
        ])
        .expect("parse");
        assert_eq!(parsed.text, "--task=7 is not a flag\n");
    }

    #[test]
    fn no_newline_keeps_the_text_as_typed() {
        let parsed = parse_arguments(&[
            "--workspace=w1".to_owned(),
            "--pane=p1".to_owned(),
            "--no-newline".to_owned(),
            "partial".to_owned(),
        ])
        .expect("parse");
        assert_eq!(parsed.text, "partial");
    }

    #[test]
    fn an_unknown_flag_is_refused() {
        let error = parse_arguments(&[
            "--workspace=w1".to_owned(),
            "--pane=p1".to_owned(),
            "--remote".to_owned(),
            "hi".to_owned(),
        ])
        .unwrap_err();
        assert!(error.contains("--remote"), "{error}");
    }
}
