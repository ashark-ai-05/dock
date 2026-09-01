//! `dock read` — a bounded snapshot of a pane's output, using DOCK_RUN when inside one.
//!
//! This is the log, not a second inspect JSON: agents need the bytes that were painted, capped
//! so a long-running pane cannot dump megabytes into a hook.

use std::io::Write;
use std::path::PathBuf;

use base64::{Engine as _, engine::general_purpose::STANDARD};

use crate::{
    cli::wire::Connection,
    protocol::{PaneHistoryRequest, Request, Response},
};

const USAGE: &str = "usage: dock read [--run-id=ID] [--max-bytes=N] [--socket=PATH]\n\
     (default run: DOCK_RUN; default --max-bytes=8192, capped at 32768)";

const DEFAULT_MAX: u32 = 8192;
const HARD_MAX: u32 = 32768;

#[derive(Debug)]
struct ReadArgs {
    socket: Option<PathBuf>,
    run_id: String,
    max_bytes: u32,
}

fn parse_arguments(args: &[String]) -> Result<ReadArgs, String> {
    let mut socket = None;
    let mut run_id = None;
    let mut max_bytes = DEFAULT_MAX;
    for argument in args {
        if let Some(value) = argument.strip_prefix("--socket=") {
            socket = Some(PathBuf::from(value));
        } else if let Some(value) = argument.strip_prefix("--run-id=") {
            run_id = Some(value.to_owned());
        } else if let Some(value) = argument.strip_prefix("--max-bytes=") {
            let parsed: u32 = value
                .parse()
                .map_err(|_| format!("--max-bytes needs a number; {USAGE}"))?;
            max_bytes = parsed.clamp(1, HARD_MAX);
        } else {
            return Err(format!("unknown option {argument:?}; {USAGE}"));
        }
    }
    if socket.is_none() {
        socket = std::env::var_os("DOCK_SOCKET").map(PathBuf::from);
    }
    let run_id = run_id
        .or_else(|| std::env::var("DOCK_RUN").ok().filter(|id| !id.is_empty()))
        .ok_or_else(|| {
            format!("--run-id is required (or set DOCK_RUN inside a Dock pane); {USAGE}")
        })?;
    Ok(ReadArgs {
        socket,
        run_id,
        max_bytes,
    })
}

pub fn run(args: &[String]) -> Result<(), String> {
    let args = parse_arguments(args)?;
    let request = Request::PaneHistory(PaneHistoryRequest {
        run_id: args.run_id,
        // End of the log: `before` is exclusive, so u64::MAX is "everything still retained,
        // newest last, capped at max_bytes".
        before: u64::MAX,
        max_bytes: args.max_bytes,
    });
    match Connection::open(args.socket)?.request(&request)? {
        Response::PaneHistory { bytes, .. } => {
            let decoded = STANDARD
                .decode(bytes)
                .map_err(|error| format!("pane history is not valid base64: {error}"))?;
            let mut stdout = io_stdout();
            stdout
                .write_all(&decoded)
                .map_err(|error| error.to_string())?;
            Ok(())
        }
        Response::Error { message, .. } => Err(message),
        response => Err(format!("unexpected read response: {response:?}")),
    }
}

fn io_stdout() -> std::io::Stdout {
    std::io::stdout()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn run_id_and_budget_are_parsed() {
        let parsed =
            parse_arguments(&["--run-id=dock_1".to_owned(), "--max-bytes=4096".to_owned()])
                .expect("parse");
        assert_eq!(parsed.run_id, "dock_1");
        assert_eq!(parsed.max_bytes, 4096);
    }

    #[test]
    fn an_oversize_budget_is_clamped_rather_than_refused() {
        let parsed = parse_arguments(&[
            "--run-id=dock_1".to_owned(),
            "--max-bytes=999999".to_owned(),
        ])
        .expect("parse");
        assert_eq!(parsed.max_bytes, HARD_MAX);
    }

    #[test]
    fn an_unknown_flag_is_refused() {
        let error =
            parse_arguments(&["--run-id=dock_1".to_owned(), "--remote".to_owned()]).unwrap_err();
        assert!(error.contains("--remote"), "{error}");
    }
}
