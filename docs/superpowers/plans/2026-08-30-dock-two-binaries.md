# Dock — Two Binaries Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Fold Dock's six scripting binaries into `dock <verb>` so that installing Dock puts exactly two executables on a user's `PATH`, and publish the crate under a name that is actually available.

**Architecture:** Each scripting binary today is a `main()` that parses flags, opens a Unix socket, performs a `Hello` handshake, sends one request, and pretty-prints one response — with its own private copy of `send`/`receive`. The fold extracts that shared middle into `dock::cli::wire`, leaving each verb as a pure `parse_arguments` plus a `render`, both unit-testable without a daemon. `main.rs` gains one `VERBS` table that both dispatch and `--help` read, so a verb cannot exist without being documented.

**Tech Stack:** Rust 2024, `serde_json`, `std::os::unix::net::UnixStream`. No new dependencies.

**Spec:** `docs/superpowers/specs/2026-08-30-dock-install-and-first-run-design.md`

## Global Constraints

- **Crate name is `dock-tui`; the binary is still `dock`.** `dock` is taken on crates.io (2022 squat, 1580 downloads all-time).
- **Exactly two `[[bin]]` targets ship:** `dock` (`src/main.rs`) and `dockd` (`src/bin/dockd.rs`). `Cargo.toml` must declare them explicitly — auto-discovery is what put eight binaries on `PATH`.
- **`dockd` stays a separate executable.** A supervisor execs it directly; burying it behind `dock daemon` adds an argument whose only job is to get back where a second binary already is.
- **Bare `dock` opens the dashboard.** Not `--help`.
- **Nothing has ever been released**, so old binary names may simply cease to exist. No deprecation shims, no argv[0] aliases.
- **The two older specs under `docs/superpowers/specs/` are not rewritten.** They record decisions taken at the time; editing them to match later code destroys their value as evidence.
- **Three gates must pass before every commit:** `cargo fmt --check`, `cargo clippy --all-targets --all-features -- -D warnings`, `cargo test --all-targets`.
- **Smoke scripts are updated in the same commit as the verb they exercise.** They are macOS-only and run in CI; a missed rename fails there rather than locally.

---

## File Structure

**Create:**
- `src/cli/mod.rs` — the `cli` module root; re-exports each verb module.
- `src/cli/wire.rs` — `Connection` (connect + handshake + request/response) and `print_json`. The one copy of what six binaries each had privately.
- `src/cli/inspect.rs`, `src/cli/agent.rs`, `src/cli/dispatch.rs`, `src/cli/workspace.rs`, `src/cli/programme.rs`, `src/cli/review.rs` — one module per verb, each a pure `parse_arguments` plus a `render` plus a four-line `run`.

**Modify:**
- `src/lib.rs` — add `pub mod cli;`
- `src/main.rs:97` (`run_noninteractive_legacy`) — dispatch through a `VERBS` table; add `--help`.
- `Cargo.toml` — crate rename, explicit `[[bin]]`.
- `README.md`, `docs/terminal-runtime-parity.md`, `docs/slice61-macos-walkthrough.md`, and `scripts/smoke-slice{3,4,5,6}-macos.sh`.

**Delete:** `src/bin/dock-agent.rs`, `dock-dispatch.rs`, `dock-handoff.rs`, `dock-inspect.rs`, `dock-programme.rs`, `dock-workspace.rs`.

Within `cli::`, the module named `dispatch` is the CLI verb; the runtime registry remains `crate::dispatch`. They are never imported into the same scope.

---

### Task 1: `cli::wire` — one copy of the conversation

**Files:**
- Create: `src/cli/mod.rs`, `src/cli/wire.rs`
- Modify: `src/lib.rs`

**Interfaces:**
- Consumes: `dock::protocol::{Request, Response, HelloRequest, PROTOCOL_VERSION}`, `dock::paths::default_socket_path`.
- Produces:
  - `wire::encode(request: &Request, out: &mut impl Write) -> Result<(), String>`
  - `wire::decode(reader: &mut impl BufRead) -> Result<Response, String>`
  - `wire::Connection::open(socket: Option<PathBuf>) -> Result<Connection, String>`
  - `wire::Connection::request(&mut self, request: &Request) -> Result<Response, String>`
  - `wire::print_json(value: &impl serde::Serialize) -> Result<(), String>`

- [ ] **Step 1: Write the failing test**

Create `src/cli/wire.rs` containing only the test module for now:

```rust
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

        let mut reader = &b"{\"kind\":\"hello\",\"version\":13}\n"[..];
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
```

- [ ] **Step 2: Run it to make sure it fails**

Run: `cargo test --lib -- cli::wire --nocapture`
Expected: FAIL to compile — `cannot find function encode in this scope`.

- [ ] **Step 3: Write the implementation**

Prepend to `src/cli/wire.rs`:

```rust
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
```

Create `src/cli/mod.rs`:

```rust
//! The non-interactive face of `dock`: one module per verb.
//!
//! Each verb is a pure `parse_arguments` and a `render`, joined by a four-line `run`. Splitting
//! them is what makes a verb testable at all — parsing needs no daemon, and neither does
//! deciding what to print.

pub mod wire;
```

Add to `src/lib.rs`, in alphabetical position (after `clipboard`, before `copy`):

```rust
pub mod cli;
```

- [ ] **Step 4: Run the tests**

Run: `cargo test --lib -- cli::wire --nocapture`
Expected: PASS, 2 tests.

- [ ] **Step 5: Gates and commit**

```bash
cargo fmt && cargo clippy --all-targets --all-features -- -D warnings && cargo test --all-targets
git add src/cli/mod.rs src/cli/wire.rs src/lib.rs
git commit -m "refactor: give the daemon conversation one copy instead of six"
```

---

### Task 2: `dock inspect`, and the table that makes a verb exist

**Files:**
- Create: `src/cli/inspect.rs`
- Modify: `src/cli/mod.rs`, `src/main.rs:97-135`
- Delete: `src/bin/dock-inspect.rs`
- Modify: `scripts/smoke-slice3-macos.sh`, `scripts/smoke-slice4-macos.sh`, `scripts/smoke-slice5-macos.sh`, `scripts/smoke-slice6-macos.sh`, `README.md`

**Interfaces:**
- Consumes: `wire::{Connection, print_json}` from Task 1.
- Produces:
  - `cli::inspect::parse_arguments(args: &[String]) -> Result<(Option<PathBuf>, Request), String>`
  - `cli::inspect::render(response: Response) -> Result<(), String>`
  - `cli::inspect::run(args: &[String]) -> Result<(), String>`
  - In `main.rs`: `struct Verb { name: &'static str, summary: &'static str, run: fn(&[String]) -> Result<(), String> }`, `const VERBS: &[Verb]`, `fn help_text() -> String`.

- [ ] **Step 1: Write the failing tests**

Create `src/cli/inspect.rs` with only its test module:

```rust
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
```

- [ ] **Step 2: Run it to make sure it fails**

Run: `cargo test --lib -- cli::inspect --nocapture`
Expected: FAIL to compile — `cannot find function parse_arguments`.

- [ ] **Step 3: Write the implementation**

Prepend to `src/cli/inspect.rs`:

```rust
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
```

Add to `src/cli/mod.rs`, keeping the list alphabetical:

```rust
pub mod inspect;
pub mod wire;
```

- [ ] **Step 4: Run the tests**

Run: `cargo test --lib -- cli::inspect --nocapture`
Expected: PASS, 2 tests.

- [ ] **Step 5: Write the failing test for the verb table**

Add to `src/main.rs`, inside its existing `#[cfg(test)] mod tests`:

```rust
#[test]
fn every_verb_is_dispatchable_and_documented() {
    // Two lists of the same verbs is how a command comes to exist without appearing in
    // `--help`, or appear in `--help` without existing. There is one list, and this is what
    // holds it to being one.
    let help = help_text();
    for verb in VERBS {
        assert!(
            help.contains(verb.name),
            "{} is dispatchable but absent from --help:\n{help}",
            verb.name
        );
        assert!(
            !verb.summary.is_empty(),
            "{} has no summary to print",
            verb.name
        );
    }
    assert!(
        VERBS.windows(2).all(|pair| pair[0].name < pair[1].name),
        "VERBS is listed in the order --help prints, so it is kept sorted"
    );
}
```

- [ ] **Step 6: Run it to make sure it fails**

Run: `cargo test --bin dock -- every_verb_is_dispatchable --nocapture`
Expected: FAIL to compile — `cannot find value VERBS`.

- [ ] **Step 7: Add the table and route `inspect` through it**

In `src/main.rs`, above `run_noninteractive_legacy`:

```rust
/// One non-interactive verb: how it is spelled, what `--help` says about it, and what runs.
///
/// A single table read by both dispatch and `--help`, so the two cannot disagree about which
/// verbs exist. Kept sorted, because that is the order `--help` prints.
struct Verb {
    name: &'static str,
    summary: &'static str,
    run: fn(&[String]) -> Result<(), String>,
}

const VERBS: &[Verb] = &[Verb {
    name: "inspect",
    summary: "what the daemon knows about a run",
    run: dock::cli::inspect::run,
}];

fn help_text() -> String {
    let mut text = String::from("dock — a terminal multiplexer that understands coding agents\n\n");
    text.push_str("  dock                 open the dashboard here\n\n");
    for verb in VERBS {
        text.push_str(&format!("  dock {:<14} {}\n", verb.name, verb.summary));
    }
    text
}
```

Then, as the first arm inside `run_noninteractive_legacy` (before the existing `detect` arm at `main.rs:102`):

```rust
if args.first().is_some_and(|first| first == "--help" || first == "-h") {
    println!("{}", help_text());
    return Ok(true);
}
if let Some(verb) = args
    .first()
    .and_then(|name| VERBS.iter().find(|verb| &verb.name == name))
{
    // Printed and exited rather than returned, for the reason `dock queue` already is at
    // `main.rs:127`: `main` renders an error with `{:?}`, and a scripting surface should not
    // sign off in Debug format.
    if let Err(error) = (verb.run)(&args[1..]) {
        eprintln!("{error}");
        std::process::exit(1);
    }
    return Ok(true);
}
```

- [ ] **Step 8: Run the tests**

Run: `cargo test --bin dock -- every_verb_is_dispatchable --nocapture`
Expected: PASS.

- [ ] **Step 9: Delete the old binary and update every reference**

```bash
git rm src/bin/dock-inspect.rs
grep -rn "dock-inspect" scripts/ README.md docs/terminal-runtime-parity.md docs/slice61-macos-walkthrough.md
```

Replace each hit: `dock-inspect` → `dock inspect`, and `cargo run --bin dock-inspect --` → `cargo run --bin dock -- inspect`. Do **not** touch the two files under `docs/superpowers/specs/`.

- [ ] **Step 10: Prove the smoke scripts still pass**

Run: `scripts/smoke-slice5-macos.sh`
Expected: exits 0, and leaves no daemon behind.

- [ ] **Step 11: Gates and commit**

```bash
cargo fmt && cargo clippy --all-targets --all-features -- -D warnings && cargo test --all-targets
git add -A
git commit -m "refactor: dock inspect becomes a verb, and verbs become a table"
```

---

### Task 3: `dock agent`

**Files:**
- Create: `src/cli/agent.rs`
- Modify: `src/cli/mod.rs`, `src/main.rs` (`VERBS`)
- Delete: `src/bin/dock-agent.rs`
- Modify: whichever of `scripts/smoke-slice{3,4,5,6}-macos.sh`, `README.md`, `docs/terminal-runtime-parity.md`, `docs/slice61-macos-walkthrough.md` mention `dock-agent`

**Interfaces:**
- Consumes: `wire::{Connection, print_json}`.
- Produces: `cli::agent::{parse_arguments, render, run}` with the signatures given in Task 2.

- [ ] **Step 1: Write the failing test**

Create `src/cli/agent.rs` with only its test module:

```rust
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
                .contains("--run-id")
        );
        assert!(
            parse_arguments(&["--run-id=dock_1".to_owned()])
                .unwrap_err()
                .contains("--operation")
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
```

- [ ] **Step 2: Run it to make sure it fails**

Run: `cargo test --lib -- cli::agent --nocapture`
Expected: FAIL to compile.

- [ ] **Step 3: Write the implementation**

Prepend to `src/cli/agent.rs`:

```rust
//! `dock agent` — attach, focus, interrupt, stop or restart one run.

use std::path::PathBuf;

use crate::{
    cli::wire::{Connection, print_json},
    protocol::{LifecycleOperation, LifecycleRequest, Request, Response},
};

const USAGE: &str = "usage: dock agent --run-id=dock_ID \
                     --operation=attach|focus|interrupt|stop|restart [--socket=PATH]";

pub fn parse_arguments(args: &[String]) -> Result<(Option<PathBuf>, Request), String> {
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

pub fn render(response: Response) -> Result<(), String> {
    match response {
        Response::LifecycleApplied { snapshot, .. } => print_json(&snapshot),
        Response::Error { message, .. } => Err(message),
        response => Err(format!("unexpected lifecycle response: {response:?}")),
    }
}

pub fn run(args: &[String]) -> Result<(), String> {
    let (socket, request) = parse_arguments(args)?;
    let response = Connection::open(socket)?.request(&request)?;
    render(response)
}
```

`LifecycleOperation` already derives `Debug, Clone, Copy, PartialEq, Eq` at `src/protocol.rs:360`, so the comparison and the panic message in the test above both compile as written.

Add `pub mod agent;` to `src/cli/mod.rs` (first, alphabetically).

Add to `VERBS` in `src/main.rs`, keeping it sorted — `agent` goes before `inspect`:

```rust
Verb {
    name: "agent",
    summary: "attach, interrupt, stop or restart a run",
    run: dock::cli::agent::run,
},
```

- [ ] **Step 4: Run the tests**

Run: `cargo test --lib -- cli::agent --nocapture`
Expected: PASS, 2 tests.

- [ ] **Step 5: Delete the old binary and update references**

```bash
git rm src/bin/dock-agent.rs
grep -rn "dock-agent" scripts/ README.md docs/terminal-runtime-parity.md docs/slice61-macos-walkthrough.md
```

Replace `dock-agent` → `dock agent`. Note `src/dispatch.rs:8617` contains the string `"/definitely/not/a/dock-agent"` — that is a test fixture path asserting a *non-existent executable* and must be left exactly as it is.

- [ ] **Step 6: Gates and commit**

```bash
scripts/smoke-slice5-macos.sh
cargo fmt && cargo clippy --all-targets --all-features -- -D warnings && cargo test --all-targets
git add -A
git commit -m "refactor: dock agent becomes a verb"
```

---

### Task 4: `dock dispatch`

**Files:**
- Create: `src/cli/dispatch.rs`
- Modify: `src/cli/mod.rs`, `src/main.rs` (`VERBS`)
- Delete: `src/bin/dock-dispatch.rs`
- Modify: `scripts/smoke-slice{3,4,5,6}-macos.sh` and `README.md` where they mention `dock-dispatch`

**Interfaces:**
- Consumes: `wire::{Connection, print_json}`.
- Produces: `cli::dispatch::{parse_arguments, render, run}`, plus `cli::dispatch::generate_run_id() -> String`.

- [ ] **Step 1: Write the failing test**

Create `src/cli/dispatch.rs` with only its test module:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn everything_after_the_separator_is_the_agents_command() {
        let (_, request) = parse_arguments(&[
            "--repo=/repo".to_owned(),
            "--task=T-1".to_owned(),
            "--worktree=/repo".to_owned(),
            "--".to_owned(),
            "-c".to_owned(),
            "--task=not-a-flag-of-ours".to_owned(),
        ])
        .expect("parse");
        match request {
            Request::Dispatch(dispatch) => assert_eq!(
                dispatch.adapter.arguments,
                vec!["-c".to_owned(), "--task=not-a-flag-of-ours".to_owned()],
                "past `--`, an argument that looks like ours is still the agent's"
            ),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn a_run_id_is_generated_when_none_is_given_and_kept_when_one_is() {
        let required = [
            "--repo=/repo".to_owned(),
            "--task=T-1".to_owned(),
            "--worktree=/repo".to_owned(),
        ];
        let (_, request) = parse_arguments(&required).expect("parse");
        match request {
            Request::Dispatch(dispatch) => {
                assert!(dispatch.run_id.starts_with("dock_"), "{}", dispatch.run_id)
            }
            other => panic!("{other:?}"),
        }
        let mut named = required.to_vec();
        named.push("--run-id=dock_mine".to_owned());
        let (_, request) = parse_arguments(&named).expect("parse");
        match request {
            Request::Dispatch(dispatch) => assert_eq!(dispatch.run_id, "dock_mine"),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn each_required_flag_names_itself_when_missing() {
        for (omitted, rest) in [
            ("--repo", vec!["--task=T-1", "--worktree=/repo"]),
            ("--task", vec!["--repo=/repo", "--worktree=/repo"]),
            ("--worktree", vec!["--repo=/repo", "--task=T-1"]),
        ] {
            let args: Vec<String> = rest.into_iter().map(str::to_owned).collect();
            let error = parse_arguments(&args).unwrap_err();
            assert!(error.contains(omitted), "{omitted}: {error}");
        }
    }
}
```

- [ ] **Step 2: Run it to make sure it fails**

Run: `cargo test --lib -- cli::dispatch --nocapture`
Expected: FAIL to compile.

- [ ] **Step 3: Write the implementation**

Prepend to `src/cli/dispatch.rs`:

```rust
//! `dock dispatch` — start one agent run without the dashboard.

use std::{
    path::PathBuf,
    time::{SystemTime, UNIX_EPOCH},
};

use crate::{
    adapter::{AdapterId, AdapterSelection},
    cli::wire::{Connection, print_json},
    protocol::{DispatchRequest, Request, Response},
};

const USAGE: &str = "usage: dock dispatch --repo=PATH --task=REF --worktree=PATH \
                     [--run-id=dock_ID] \
                     [--adapter=fixture|amp|claude-code|codex-cli|github-copilot-cli|generic] \
                     [--executable=PATH] [--socket=PATH] -- [ARG ...]";

pub fn parse_arguments(args: &[String]) -> Result<(Option<PathBuf>, Request), String> {
    let mut socket = None;
    let mut repository_root = None;
    let mut task = None;
    let mut run_id = None;
    let mut worktree = None;
    let mut command = Vec::new();
    let mut adapter = AdapterId::Fixture;
    let mut executable = None;
    let mut arguments = args.iter();
    while let Some(argument) = arguments.next() {
        // Everything past `--` belongs to the agent, including anything spelled like one of
        // ours. This is the only place in the parser where a flag is not a flag.
        if argument == "--" {
            command.extend(arguments.cloned());
            break;
        }
        if let Some(value) = argument.strip_prefix("--socket=") {
            socket = Some(PathBuf::from(value));
        } else if let Some(value) = argument.strip_prefix("--repo=") {
            repository_root = Some(value.to_owned());
        } else if let Some(value) = argument.strip_prefix("--task=") {
            task = Some(value.to_owned());
        } else if let Some(value) = argument.strip_prefix("--run-id=") {
            run_id = Some(value.to_owned());
        } else if let Some(value) = argument.strip_prefix("--worktree=") {
            worktree = Some(value.to_owned());
        } else if let Some(value) = argument.strip_prefix("--adapter=") {
            adapter = match value {
                "fixture" => AdapterId::Fixture,
                "amp" => AdapterId::Amp,
                "claude-code" => AdapterId::ClaudeCode,
                "codex-cli" => AdapterId::CodexCli,
                "github-copilot-cli" => AdapterId::GithubCopilotCli,
                "generic" => AdapterId::Generic,
                _ => return Err(format!("unknown adapter {value:?}; {USAGE}")),
            };
        } else if let Some(value) = argument.strip_prefix("--executable=") {
            executable = Some(value.to_owned());
        } else {
            return Err(format!("unknown option {argument:?}; {USAGE}"));
        }
    }
    Ok((
        socket,
        Request::Dispatch(DispatchRequest {
            repository_root: repository_root.ok_or(format!("--repo is required; {USAGE}"))?,
            external_task_ref: task.ok_or(format!("--task is required; {USAGE}"))?,
            run_id: run_id.unwrap_or_else(generate_run_id),
            worktree: worktree.ok_or(format!("--worktree is required; {USAGE}"))?,
            adapter: AdapterSelection {
                id: adapter,
                executable,
                arguments: command,
            },
        }),
    ))
}

/// Unique without coordinating with anything: this process, and the moment it asked.
pub fn generate_run_id() -> String {
    format!(
        "dock_{}_{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock after epoch")
            .as_nanos()
    )
}

pub fn render(response: Response) -> Result<(), String> {
    match response {
        Response::Dispatched { snapshot } => print_json(&snapshot),
        Response::Error { message, .. } => Err(message),
        response => Err(format!("unexpected dispatch response: {response:?}")),
    }
}

pub fn run(args: &[String]) -> Result<(), String> {
    let (socket, request) = parse_arguments(args)?;
    let response = Connection::open(socket)?.request(&request)?;
    render(response)
}
```

Add `pub mod dispatch;` to `src/cli/mod.rs` (after `agent`), and to `VERBS` between `agent` and `inspect`:

```rust
Verb {
    name: "dispatch",
    summary: "start an agent run without the dashboard",
    run: dock::cli::dispatch::run,
},
```

- [ ] **Step 4: Run the tests**

Run: `cargo test --lib -- cli::dispatch --nocapture`
Expected: PASS, 3 tests.

- [ ] **Step 5: Delete the old binary and update references**

```bash
git rm src/bin/dock-dispatch.rs
grep -rn "dock-dispatch" scripts/ README.md docs/terminal-runtime-parity.md docs/slice61-macos-walkthrough.md
```

Replace `dock-dispatch` → `dock dispatch`, and in the README's scripting section
`cargo run --bin dock-dispatch -- \` → `cargo run --bin dock -- dispatch \`.
Leave `src/dispatch.rs:6488` alone: `"dock-dispatch-{label}-{}-{}"` is a run-id label, not a command name.

- [ ] **Step 6: Gates and commit**

```bash
scripts/smoke-slice5-macos.sh
cargo fmt && cargo clippy --all-targets --all-features -- -D warnings && cargo test --all-targets
git add -A
git commit -m "refactor: dock dispatch becomes a verb"
```

---

### Task 5: `dock workspace`

**Files:**
- Create: `src/cli/workspace.rs`
- Modify: `src/cli/mod.rs`, `src/main.rs` (`VERBS`)
- Delete: `src/bin/dock-workspace.rs`
- Modify: `scripts/smoke-slice{3,4,5,6}-macos.sh`, `README.md` where they mention `dock-workspace`

**Interfaces:**
- Consumes: `wire::{Connection, print_json}`.
- Produces: `cli::workspace::{parse_arguments, render, run}`.

`dock-workspace.rs` already has a pure `parse(&[String]) -> Result<WorkspaceRequest, String>` at line 52. This task moves it, renames it to `parse_arguments` returning the `(Option<PathBuf>, Request)` shape every other verb uses, and gives it the tests it never had.

- [ ] **Step 1: Write the failing test**

Create `src/cli/workspace.rs` with only its test module:

```rust
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
```

- [ ] **Step 2: Run it to make sure it fails**

Run: `cargo test --lib -- cli::workspace --nocapture`
Expected: FAIL to compile.

- [ ] **Step 3: Write the implementation**

Prepend to `src/cli/workspace.rs`:

```rust
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
    let operation = match rest.iter().map(String::as_str).collect::<Vec<_>>().as_slice() {
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
```

Note the `rest.get(5)` index is unchanged from the original because `--socket=` has already been removed from `rest`, so positions are the same as the original parser saw them.

Add `pub mod workspace;` to `src/cli/mod.rs` (last, alphabetically), and to `VERBS` after `review`:

```rust
Verb {
    name: "workspace",
    summary: "create, split, focus, rename and close panes",
    run: dock::cli::workspace::run,
},
```

- [ ] **Step 4: Run the tests**

Run: `cargo test --lib -- cli::workspace --nocapture`
Expected: PASS, 3 tests.

- [ ] **Step 5: Delete the old binary and update references**

```bash
git rm src/bin/dock-workspace.rs
grep -rn "dock-workspace" scripts/ README.md docs/terminal-runtime-parity.md docs/slice61-macos-walkthrough.md
```

`scripts/smoke-slice5-macos.sh` is the heaviest user of this one — check every hit.

- [ ] **Step 6: Gates and commit**

```bash
scripts/smoke-slice5-macos.sh && scripts/smoke-slice6-macos.sh
cargo fmt && cargo clippy --all-targets --all-features -- -D warnings && cargo test --all-targets
git add -A
git commit -m "refactor: dock workspace becomes a verb"
```

---

### Task 6: `dock programme`

**Files:**
- Create: `src/cli/programme.rs`
- Modify: `src/cli/mod.rs`, `src/main.rs` (`VERBS`), `src/protocol.rs:632` (doc comment)
- Delete: `src/bin/dock-programme.rs`
- Modify: `README.md` where it mentions `dock-programme`

**Interfaces:**
- Consumes: `wire::{Connection, print_json}`.
- Produces: `cli::programme::{parse_arguments, render, run}`.

This binary already has the target shape and two passing tests. The move is mechanical; the tests come with it.

- [ ] **Step 1: Move the file and adapt it**

```bash
git mv src/bin/dock-programme.rs src/cli/programme.rs
```

In `src/cli/programme.rs`, make exactly these changes and no others:

1. Delete the `fn main()` at the top (lines 17-20 of the original).
2. Change the `use dock::{...}` to `use crate::{...}`, and add `cli::wire::{Connection, print_json}` to it.
3. Delete the private `send`, `receive` and `print_json` functions at the bottom, and the now-unused `std::io` / `UnixStream` imports.
4. Change `parse_arguments` to take `&[String]` and be `pub`, so it matches every other verb:

```rust
pub fn parse_arguments(args: &[String]) -> Result<(Option<PathBuf>, Request), String> {
    let mut socket = None;
    // …body unchanged from the original, except the loop header below…
    let mut arguments = args.iter();
    while let Some(arg) = arguments.next() {
        if arg == "--" {
            queue_flag_seen = true;
            command.extend(arguments.cloned());
            break;
        }
        // …every `else if let Some(v) = arg.strip_prefix(…)` arm unchanged…
    }
    // …the request-building tail unchanged…
}
```

5. Replace the whole `fn run(socket, request)` with the four-line shape and a `render`:

```rust
pub fn render(response: Response) -> Result<(), String> {
    match response {
        Response::Programme { portfolio } => print_json(&portfolio),
        Response::GateQueued { gate } => print_json(&gate),
        Response::GateReleased { snapshot } => print_json(&snapshot),
        Response::Error { message, .. } => Err(message),
        response => Err(format!("unexpected programme response: {response:?}")),
    }
}

pub fn run(args: &[String]) -> Result<(), String> {
    let (socket, request) = parse_arguments(args)?;
    let response = Connection::open(socket)?.request(&request)?;
    render(response)
}
```

6. In the existing `mod tests`, change the two `parse_arguments([...])` calls to take slices, e.g.
   `parse_arguments(&["--release=dock_downstream".to_owned(), queue_flag.to_owned()])`.

- [ ] **Step 2: Wire it in**

Add `pub mod programme;` to `src/cli/mod.rs` (after `inspect`), and to `VERBS` between `inspect` and `review`:

```rust
Verb {
    name: "programme",
    summary: "multi-repository capacity and dependency gates",
    run: dock::cli::programme::run,
},
```

- [ ] **Step 3: Run the tests**

Run: `cargo test --lib -- cli::programme --nocapture`
Expected: PASS, 2 tests — `release_rejects_every_queue_shape` and `release_alone_is_valid`, both carried over unchanged in substance.

- [ ] **Step 4: Update references**

```bash
grep -rn "dock-programme" src/ scripts/ README.md docs/terminal-runtime-parity.md docs/slice61-macos-walkthrough.md
```

`src/protocol.rs:632` is a doc comment reading "so `dock-programme` shows both" — update it to `dock programme`.

- [ ] **Step 5: Gates and commit**

```bash
cargo fmt && cargo clippy --all-targets --all-features -- -D warnings && cargo test --all-targets
git add -A
git commit -m "refactor: dock programme becomes a verb"
```

---

### Task 7: `dock review` — the half of `handoff` that is not the agent's

**Files:**
- Create: `src/cli/review.rs`
- Modify: `src/cli/mod.rs`, `src/main.rs` (`VERBS`), `README.md`
- Delete: `src/bin/dock-handoff.rs`

**Interfaces:**
- Consumes: `wire::{Connection, print_json}`.
- Produces: `cli::review::run(&[String]) -> Result<(), String>`, plus
  `parse_arguments(&[String]) -> Result<(Option<PathBuf>, Request, ExpectedResponse), String>`
  and `render(ExpectedResponse, Response) -> Result<(), String>` — the one verb whose
  signatures differ from the other five, for the reason given in Step 3.

`dock handoff` stays exactly as it is — `main.rs:1691`, agent-facing, positional summary. This task takes only the operator-facing binary and gives it the name the dashboard already uses for that surface.

- [ ] **Step 1: Write the failing test**

Create `src/cli/review.rs` with only its test module:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_three_things_a_reviewer_can_do_are_each_reachable() {
        let (_, request) = parse_arguments(&["--inbox".to_owned()]).expect("inbox");
        assert!(matches!(request, Request::ReviewInbox(_)));

        let (_, request) = parse_arguments(&[
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
        assert!(error.contains("--note"), "{error}");
    }

    #[test]
    fn asking_for_two_things_at_once_is_refused_with_the_usage() {
        let error =
            parse_arguments(&["--inbox".to_owned(), "--submit=packet.json".to_owned()]).unwrap_err();
        assert!(error.contains("usage:"), "{error}");
    }

    #[test]
    fn an_unknown_route_names_the_two_that_exist() {
        let error = parse_arguments(&["--route=maybe".to_owned()]).unwrap_err();
        assert!(error.contains("accept-scope"), "{error}");
        assert!(error.contains("request-change"), "{error}");
    }
}
```

- [ ] **Step 2: Run it to make sure it fails**

Run: `cargo test --lib -- cli::review --nocapture`
Expected: FAIL to compile.

- [ ] **Step 3: Write the implementation**

Prepend to `src/cli/review.rs`:

```rust
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

pub fn parse_arguments(args: &[String]) -> Result<(Option<PathBuf>, Request), String> {
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
    let request = match (packet, inbox, route) {
        (Some(path), false, None) => {
            let packet: HandoffPacket = serde_json::from_slice(
                &fs::read(path).map_err(|error| format!("could not read packet: {error}"))?,
            )
            .map_err(|error| format!("invalid handoff packet: {error}"))?;
            Request::SubmitHandoff(SubmitHandoffRequest { packet })
        }
        (None, true, None) => Request::ReviewInbox(ReviewInboxRequest {}),
        (None, false, Some(route)) => Request::Decide(DecideRequest {
            run_id: run_id.ok_or(format!("--run-id is required for a decision; {USAGE}"))?,
            route,
            note: note.ok_or(format!("--note is required for a decision; {USAGE}"))?,
        }),
        _ => return Err(USAGE.into()),
    };
    Ok((socket, request))
}
```

Note the `match (packet, inbox, route)` above must also produce the response this request
expects, exactly as the original did. Change the two lines so it returns a pair, and carry
`ExpectedResponse` and `require_expected_response` over from
`src/bin/dock-handoff.rs:101-125` **unchanged**, along with their test:

```rust
/// Which success the daemon owes for the request that was sent.
///
/// Kept rather than collapsed into `render`'s match: this is the only verb whose three
/// requests have three different right answers, and pairing them is a claim worth asserting.
/// Deleting it would take its test with it.
#[derive(Clone, Copy)]
pub enum ExpectedResponse {
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

/// The whole response, not a field of it. Each of the three carries a different payload and
/// the original printed the envelope; narrowing to an inner field here would quietly change
/// what every existing script reads.
pub fn render(expected: ExpectedResponse, response: Response) -> Result<(), String> {
    require_expected_response(expected, &response)?;
    print_json(&response)
}

pub fn run(args: &[String]) -> Result<(), String> {
    let (socket, request, expected) = parse_arguments(args)?;
    let response = Connection::open(socket)?.request(&request)?;
    render(expected, response)
}
```

So `parse_arguments` here returns `Result<(Option<PathBuf>, Request, ExpectedResponse), String>` —
the one verb whose signature differs from the other five, because it is the one verb with three
different right answers. Its three arms return
`(Request::SubmitHandoff(…), ExpectedResponse::Submit)`,
`(Request::ReviewInbox(…), ExpectedResponse::Inbox)` and
`(Request::Decide(…), ExpectedResponse::Decision)` respectively, as the original does.

Carry the original's test over as well, renamed only where it says `dock-handoff`:

```rust
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
```

Add `pub mod review;` to `src/cli/mod.rs` (after `programme`), and to `VERBS` between `programme` and `workspace`:

```rust
Verb {
    name: "review",
    summary: "read the handoff inbox and record a decision",
    run: dock::cli::review::run,
},
```

- [ ] **Step 4: Run the tests**

Run: `cargo test --lib -- cli::review --nocapture`
Expected: PASS, 5 tests — the four above plus `each_operation_rejects_an_unexpected_success_variant` carried over from the deleted binary.

- [ ] **Step 5: Delete the old binary and rewrite the README's sentence**

```bash
git rm src/bin/dock-handoff.rs
grep -n "dock-handoff" README.md docs/terminal-runtime-parity.md docs/slice61-macos-walkthrough.md scripts/*.sh
```

The README's review-queue paragraph currently reads "the handoffs agents submitted with `dock-handoff --submit`". It becomes:

> `Ctrl+B i` opens the review queue: the handoffs agents submitted with `dock handoff`,
> waiting on a person. The same queue is readable from a shell with `dock review --inbox`,
> and a decision recorded with `dock review --run-id=… --route=… --note=…`.

- [ ] **Step 6: Gates and commit**

```bash
cargo fmt && cargo clippy --all-targets --all-features -- -D warnings && cargo test --all-targets
git add -A
git commit -m "feat: the operator half of handoff becomes dock review"
```

---

### Task 8: Two binaries, declared — and a crate name that is free

**Files:**
- Modify: `Cargo.toml`, `README.md`
- Verify: `src/bin/` contains only `dockd.rs`

**Interfaces:**
- Consumes: every verb from Tasks 2-7 being reachable through `VERBS`.
- Produces: a crate named `dock-tui` building exactly two executables, `dock` and `dockd`.

- [ ] **Step 1: Confirm there is nothing left to fold**

```bash
ls src/bin/
```
Expected: `dockd.rs` only. If anything else remains, the task that owned it is incomplete — stop and finish that one first.

- [ ] **Step 2: Declare the crate and its binaries**

In `Cargo.toml`, replace the `[package]` block's `name` and add explicit binaries:

```toml
[package]
name = "dock-tui"
version = "0.1.0"
edition = "2024"
description = "A terminal multiplexer that understands coding agents"
license = "MIT"
repository = "https://github.com/ashark-ai-05/dock"
readme = "README.md"
keywords = ["tmux", "terminal", "multiplexer", "tui", "agents"]
categories = ["command-line-utilities", "development-tools"]

[lib]
name = "dock"
path = "src/lib.rs"

[[bin]]
name = "dock"
path = "src/main.rs"

[[bin]]
name = "dockd"
path = "src/bin/dockd.rs"
```

The `[lib] name = "dock"` line is what keeps every `use dock::…` in the tree compiling: the package is renamed, the library is not. `keywords` is capped at five by crates.io.

- [ ] **Step 3: Prove the binary set**

Run:
```bash
cargo build 2>&1 | tail -3
ls target/debug/dock target/debug/dockd
ls target/debug/dock-* 2>/dev/null && echo "UNEXPECTED extra binary" || echo "exactly two, as intended"
```
Expected: both binaries exist; no `dock-*` executables.

- [ ] **Step 4: Update the README's quick start**

Replace the quick-start block so it leads with the built binary rather than a checkout:

````markdown
## Quick start

```bash
cargo install dock-tui     # the crate is dock-tui; the binary is dock
dock
```

Run it from any directory, Git or not. Dock connects to that directory's private
daemon, or starts one for you.

From a checkout, `cargo run --bin dock` does the same thing.
````

Also update the development section's `cargo run --bin dock` references if any now name a
folded binary.

- [ ] **Step 5: Gates, smoke, and commit**

```bash
scripts/smoke-slice5-macos.sh && scripts/smoke-slice6-macos.sh && scripts/smoke-slice62-nongit-macos.sh
cargo fmt && cargo clippy --all-targets --all-features -- -D warnings && cargo test --all-targets
git add -A
git commit -m "build: two declared binaries, and a crate name that is available"
```

---

### Task 9: The sweep

**Files:**
- Modify: any file still naming a folded binary, except the two under `docs/superpowers/specs/`

**Interfaces:**
- Consumes: everything above.
- Produces: no behaviour; a tree with no stale names.

- [ ] **Step 1: Find every survivor**

```bash
grep -rn "dock-agent\|dock-dispatch\|dock-handoff\|dock-inspect\|dock-programme\|dock-workspace" \
  --include='*.rs' --include='*.sh' --include='*.md' --include='*.py' --include='*.yml' --include='*.toml' . \
  | grep -v '^./target' \
  | grep -v '^./docs/superpowers/specs/'
```

Expected survivors, all of which are correct and must be left alone:
- `src/dispatch.rs:6488` — `"dock-dispatch-{label}-{}-{}"`, a run-id label.
- `src/dispatch.rs:8617` — `"/definitely/not/a/dock-agent"`, a fixture asserting a missing executable.

Anything else is a stale reference: fix it.

- [ ] **Step 2: Prove `--help` lists all six new verbs**

Run:
```bash
cargo run --bin dock -- --help
```
Expected: one line each for `agent`, `dispatch`, `inspect`, `programme`, `review`, `workspace`, under a line for bare `dock`.

- [ ] **Step 3: Prove bare `dock` still opens the dashboard**

Run:
```bash
cargo run --bin dock -- --headless-bootstrap
```
Expected: a JSON line with `protocol`, `daemon`, `workspaces` and `socket_mode` — i.e. the dashboard path was taken, not the help path.

- [ ] **Step 4: Run every smoke script**

```bash
for script in scripts/smoke-slice*-macos.sh; do echo "== $script"; "$script" || break; done
```
Expected: each exits 0, and no daemon is left behind.

- [ ] **Step 5: Gates and commit**

```bash
cargo fmt && cargo clippy --all-targets --all-features -- -D warnings && cargo test --all-targets
git add -A
git commit -m "docs: no reference survives to a binary that no longer exists"
```

---

## Self-review

**Spec coverage.** §1.1 (module shape, `[[bin]]`, `dockd` separate, bare `dock`) → Tasks 1-8. §1.2 (the `handoff`/`review` fork) → Task 7. §1.3 (the verb table) → Tasks 2-7, one verb each; `dock doctor` is the one row of that table this plan does **not** deliver, because §3 is its own plan. §1.4 (smoke scripts, README, the two docs, and specs left alone) → each verb task plus Task 9. §2's crate rename → Task 8; §2's `dist` pipeline, §3 `doctor`, §4 first-run and §5 positioning are out of this plan by the scope check at the top.

**Placeholder scan.** No "TBD", no "add error handling", no "similar to Task N". Task 6 and Task 7 direct the implementer to move named code from a named file at a named line rather than reprinting a hundred lines verbatim; both name exactly what changes and show every new line in full.

**Type consistency.** Five of the six verbs expose the same three items — `parse_arguments(&[String]) -> Result<(Option<PathBuf>, Request), String>`, `render(Response) -> Result<(), String>`, `run(&[String]) -> Result<(), String>`. `review` is the stated exception: its `parse_arguments` returns a third value and its `render` takes it, because it is the only verb whose three requests have three different correct answers. What every verb does share is `run`, which is what `Verb.run: fn(&[String]) -> Result<(), String>` requires — so the table is uniform even though the innards are not. `Connection::open` takes the `Option<PathBuf>` that each `parse_arguments` returns. `wire::print_json` is the only printer; the per-binary copies are deleted in Tasks 6 and 7.

**Two claims checked against the tree rather than assumed.** The review path's variants really are `HandoffSubmitted`, `ReviewInbox` and `DecisionRecorded` (`src/bin/dock-handoff.rs:112-118`) — an earlier draft of this plan guessed `Decided`, and would also have deleted `require_expected_response` along with the test that covers it, so Task 7 now carries both over intact. `LifecycleOperation` already derives `PartialEq`/`Debug` (`src/protocol.rs:360`), so Task 3 needs no change to `protocol.rs`.
