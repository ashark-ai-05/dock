//! `dock wait` — block until a run is blocked, done or idle.
//!
//! Agents script against the same Unix socket the dashboard uses. There is no SSH attach: the
//! daemon is a local socket, and waiting is inspect-until-state rather than a second protocol.

use std::{
    path::PathBuf,
    thread,
    time::{Duration, Instant},
};

use crate::{
    cli::wire::Connection,
    detect::AgentState,
    protocol::{InspectRequest, Request, Response, RuntimeSnapshot},
};

const USAGE: &str =
    "usage: dock wait [--run-id=ID] [--until=blocked|done|idle] [--timeout=SECS] [--socket=PATH]";

#[derive(Debug)]
struct WaitArgs {
    socket: Option<PathBuf>,
    run_id: Option<String>,
    until: AgentState,
    timeout: Option<Duration>,
}

fn parse_arguments(args: &[String]) -> Result<WaitArgs, String> {
    let mut socket = None;
    let mut run_id = None;
    let mut until = AgentState::Blocked;
    let mut timeout = None;
    for argument in args {
        if let Some(value) = argument.strip_prefix("--socket=") {
            socket = Some(PathBuf::from(value));
        } else if let Some(value) = argument.strip_prefix("--run-id=") {
            run_id = Some(value.to_owned());
        } else if let Some(value) = argument.strip_prefix("--until=") {
            until = match value {
                "blocked" | "needs-you" => AgentState::Blocked,
                "done" => AgentState::Done,
                "idle" => AgentState::Idle,
                other => {
                    return Err(format!(
                        "unknown --until={other:?}; expected blocked, done or idle"
                    ));
                }
            };
        } else if let Some(value) = argument.strip_prefix("--timeout=") {
            let secs: u64 = value
                .parse()
                .map_err(|_| format!("--timeout needs seconds; {USAGE}"))?;
            timeout = Some(Duration::from_secs(secs));
        } else {
            return Err(format!("unknown option {argument:?}; {USAGE}"));
        }
    }
    if run_id.is_none() {
        run_id = std::env::var("DOCK_RUN").ok().filter(|id| !id.is_empty());
    }
    if socket.is_none() {
        socket = std::env::var_os("DOCK_SOCKET").map(PathBuf::from);
    }
    Ok(WaitArgs {
        socket,
        run_id,
        until,
        timeout,
    })
}

fn snapshot_matches(snapshot: &RuntimeSnapshot, run_id: Option<&str>, until: AgentState) -> bool {
    if let Some(want) = run_id
        && snapshot.run_id != want
    {
        return false;
    }
    snapshot.agent_state == until
}

const PROMPT_STALL: Duration = Duration::from_secs(5);

fn wait_satisfied(
    until: AgentState,
    matched: bool,
    held_since: Option<Instant>,
    now: Instant,
) -> bool {
    if !matched {
        return false;
    }
    if until == AgentState::Blocked {
        return true;
    }
    held_since.is_some_and(|since| now.saturating_duration_since(since) >= PROMPT_STALL)
}

pub fn run(args: &[String]) -> Result<(), String> {
    let args = parse_arguments(args)?;
    let run_id = args.run_id.ok_or_else(|| {
        format!("--run-id is required (or set DOCK_RUN inside a Dock pane); {USAGE}")
    })?;
    let until = args.until;
    let deadline = args.timeout.map(|d| Instant::now() + d);
    let mut connection = Connection::open(args.socket)?;
    let mut held_since = None;
    loop {
        if deadline.is_some_and(|at| Instant::now() >= at) {
            return Err(format!(
                "timed out waiting for {run_id} to become {until:?}"
            ));
        }
        let response = connection.request(&Request::Inspect(InspectRequest {
            run_id: Some(run_id.clone()),
        }))?;
        let hit = match response {
            Response::Snapshot { snapshot } => snapshot_matches(&snapshot, Some(&run_id), until),
            Response::Snapshots { snapshots } => snapshots
                .iter()
                .any(|snapshot| snapshot_matches(snapshot, Some(&run_id), until)),
            Response::Error { message, .. } => return Err(message),
            other => return Err(format!("unexpected wait response: {other:?}")),
        };
        let now = Instant::now();
        if hit {
            if held_since.is_none() {
                held_since = Some(now);
            }
        } else {
            held_since = None;
        }
        if wait_satisfied(until, hit, held_since, now) {
            println!("{run_id}\t{until:?}");
            return Ok(());
        }
        thread::sleep(Duration::from_millis(100));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn until_defaults_to_blocked_and_run_id_can_be_named() {
        let parsed = parse_arguments(&["--run-id=dock_1".to_owned()]).expect("parse");
        assert_eq!(parsed.run_id.as_deref(), Some("dock_1"));
        assert_eq!(parsed.until, AgentState::Blocked);
        assert!(parsed.timeout.is_none());
    }

    #[test]
    fn until_and_timeout_are_parsed() {
        let parsed = parse_arguments(&[
            "--run-id=dock_1".to_owned(),
            "--until=done".to_owned(),
            "--timeout=5".to_owned(),
        ])
        .expect("parse");
        assert_eq!(parsed.until, AgentState::Done);
        assert_eq!(parsed.timeout, Some(Duration::from_secs(5)));
    }

    #[test]
    fn already_blocked_is_immediate_and_done_stalls() {
        let now = Instant::now();
        assert!(wait_satisfied(AgentState::Blocked, true, Some(now), now));
        assert!(!wait_satisfied(AgentState::Done, true, Some(now), now));
        assert!(wait_satisfied(
            AgentState::Done,
            true,
            Some(now),
            now + Duration::from_secs(5)
        ));
    }

    #[test]
    fn an_unknown_until_is_refused() {
        let error = parse_arguments(&["--run-id=dock_1".to_owned(), "--until=working".to_owned()])
            .unwrap_err();
        assert!(error.contains("working"), "{error}");
    }
}
