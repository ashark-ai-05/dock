mod kanban;

use std::{
    collections::VecDeque,
    error::Error,
    fs,
    io::{self, BufRead, BufReader, IsTerminal, Read, Write},
    os::unix::{
        fs::{FileTypeExt, PermissionsExt},
        io::{AsRawFd, RawFd},
        net::UnixStream,
    },
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    sync::mpsc,
    thread,
    time::{Duration, Instant},
};

use crossterm::{
    event::{
        self, DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture,
        Event, KeyCode, KeyEventKind, MouseEventKind,
    },
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use dock::{
    adapter::AdapterSelection,
    client::Client,
    client::{EventStream, StreamPoll},
    dashboard::{Dashboard, TaskDispatch, UiCommand},
    detect::{AgentKind, AgentState},
    git::GitAdapter,
    paths,
    protocol::{
        DashboardProfile, DispatchRequest, InspectRequest, LaunchIntoPaneRequest, PROTOCOL_VERSION,
        PaneInputRequest, PaneResizeRequest, ProcessState, QueueRequest, Request, Response,
        TerminalLaunchRequest, WorkspaceRequest,
    },
    storage::LocalStore,
};
use nix::libc;
use ratatui::{Terminal, backend::CrosstermBackend};

fn main() -> Result<(), Box<dyn Error>> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if run_noninteractive_legacy(&args)? {
        return Ok(());
    }
    let runtime_directory = std::env::current_dir()?;
    let (default_socket, default_state) = paths::runtime_paths_for(&runtime_directory)?;
    let socket = args
        .iter()
        .find_map(|arg| arg.strip_prefix("--socket=").map(PathBuf::from))
        .unwrap_or(default_socket);
    let state_dir = args
        .iter()
        .find_map(|arg| arg.strip_prefix("--state-dir=").map(PathBuf::from))
        .unwrap_or(default_state);
    let mut daemon = connect_or_start(&socket, &state_dir)?;
    let mut client = Client::connect(&socket)?;
    if args.iter().any(|arg| arg == "--headless-bootstrap") {
        let layout = request_layout(&mut client)?;
        println!(
            "{}",
            serde_json::json!({
                // Reported from the constant, not restated. It said 6 for four protocol
                // versions, which is exactly how long nobody would have noticed.
                "protocol": PROTOCOL_VERSION,
                "daemon": if daemon.spawned.is_some() { "started" } else { "reconnected" },
                "workspaces": layout.workspaces.len(),
                "socket_mode": socket_mode(&socket)?,
            })
        );
        daemon.keep_running();
        return Ok(());
    }
    if !io::stdin().is_terminal() || !io::stdout().is_terminal() {
        return Err(
            "interactive Dock requires a terminal (use --headless-bootstrap for smoke checks)"
                .into(),
        );
    }
    run_dashboard(
        &mut client,
        &socket,
        runtime_directory.to_string_lossy().into_owned(),
        &state_dir,
    )?;
    // A daemon started by Dock is intentionally left running for reconnect. This explicit policy
    // is observable through --headless-bootstrap; startup failures kill and reap it via Drop.
    daemon.keep_running();
    Ok(())
}

fn run_noninteractive_legacy(args: &[String]) -> Result<bool, Box<dyn Error>> {
    let dock_dir = args
        .iter()
        .find_map(|argument| argument.strip_prefix("--dock-dir=").map(str::to_owned))
        .unwrap_or_else(|| ".dock/local".into());
    if args.first().is_some_and(|first| first == "detect") {
        detect_command(&args[1..])?;
        return Ok(true);
    }
    if args.first().is_some_and(|first| first == "agent-state") {
        agent_state_command(&args[1..])?;
        return Ok(true);
    }
    if args.first().is_some_and(|first| first == "hooks") {
        hooks_command(&args[1..])?;
        return Ok(true);
    }
    if args.first().is_some_and(|first| first == "handoff") {
        handoff_command(&args[1..])?;
        return Ok(true);
    }
    if args.first().is_some_and(|first| first == "task") {
        task_command(&args[1..])?;
        return Ok(true);
    }
    // Unlike every other arm here, `dock queue` talks to the daemon rather than to files —
    // `dock task` writes the board, the queue lives in dockd. Parsing is pure and tested
    // (`parse_queue_command`); the socket call is a separate change and is not here yet.
    if args.first().is_some_and(|first| first == "queue") {
        // Printed and exited rather than returned: `main` renders an error with `{:?}`, and a
        // command whose whole job is to tell somebody what their queues are doing should not
        // sign off in Debug format. `dock hooks --check` does the same for the same reason.
        if let Err(error) = queue_command(&args[1..]) {
            eprintln!("{error}");
            std::process::exit(1);
        }
        return Ok(true);
    }
    if let Some(run_id) = args.iter().find_map(|a| a.strip_prefix("--load-handoff=")) {
        let packet = LocalStore::new(dock_dir)
            .load_handoff(run_id)
            .map_err(io::Error::other)?;
        println!("{}", serde_json::to_string_pretty(&packet)?);
        return Ok(true);
    }
    if let Some(packet_path) = args.iter().find_map(|a| a.strip_prefix("--save-handoff=")) {
        let packet = serde_json::from_slice(&fs::read(packet_path)?)?;
        let location = LocalStore::new(dock_dir)
            .save_handoff(&packet)
            .map_err(io::Error::other)?;
        println!("saved {}", location.display());
        return Ok(true);
    }
    if let Some(worktree) = args.iter().find_map(|a| a.strip_prefix("--git-dir=")) {
        let base = args
            .iter()
            .find_map(|a| a.strip_prefix("--base="))
            .unwrap_or("HEAD");
        let adapter = GitAdapter::new(worktree);
        let facts = adapter.facts(base).map_err(io::Error::other)?;
        let (diff, delta) = adapter.render_diff(base).map_err(io::Error::other)?;
        println!(
            "worktree={}\nbranch={}\nbase={}\nhead={}\nstatus={} files={} +{} -{}\ndelta={}\n\n{}",
            facts.worktree.display(),
            facts.branch,
            facts.base_sha,
            facts.head_sha,
            facts.status_entries,
            facts.changed_files,
            facts.insertions,
            facts.deletions,
            delta,
            diff
        );
        return Ok(true);
    }
    if let Some(board_dir) = args.iter().find_map(|a| a.strip_prefix("--kanban-dir=")) {
        let adapter = kanban::KanbanMdAdapter::new(board_dir);
        if let Some(claim) = args.iter().find_map(|a| a.strip_prefix("--claim=")) {
            let task = adapter
                .pick(claim, "backlog", "in-progress")
                .map_err(io::Error::other)?;
            println!("claimed {}\t{}\t{}", task.id, task.status, task.title);
        } else {
            for task in adapter.list().map_err(io::Error::other)? {
                println!(
                    "{}\t{}\t{}\t{}",
                    task.id,
                    task.status,
                    task.claimed_by.unwrap_or_else(|| "unclaimed".into()),
                    task.title
                );
            }
        }
        return Ok(true);
    }
    Ok(false)
}

struct DaemonChild {
    spawned: Option<Child>,
}

impl DaemonChild {
    fn keep_running(&mut self) {
        self.spawned.take();
    }
}

impl Drop for DaemonChild {
    fn drop(&mut self) {
        if let Some(child) = &mut self.spawned {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

fn connect_or_start(socket: &Path, state_dir: &Path) -> Result<DaemonChild, String> {
    if UnixStream::connect(socket).is_ok() {
        verify_socket(socket)?;
        return Ok(DaemonChild { spawned: None });
    }
    if socket == paths::default_socket_path()?.as_path() {
        let _ = paths::prepare_default_socket_path()?;
    } else if socket.exists() {
        return Err(format!(
            "refusing to replace socket override {}",
            socket.display()
        ));
    }
    let dockd = locate_or_build_dockd()?;
    let child = Command::new(dockd)
        .arg(format!("--socket={}", socket.display()))
        .arg(format!("--state-dir={}", state_dir.display()))
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("could not start dockd: {e}"))?;
    let mut guard = DaemonChild {
        spawned: Some(child),
    };
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if UnixStream::connect(socket).is_ok() {
            verify_socket(socket)?;
            return Ok(guard);
        }
        if let Some(status) = guard
            .spawned
            .as_mut()
            .and_then(|child| child.try_wait().ok())
            .flatten()
        {
            let detail = guard
                .spawned
                .as_mut()
                .and_then(|child| child.stderr.as_mut())
                .and_then(|stderr| {
                    let mut output = String::new();
                    stderr.read_to_string(&mut output).ok()?;
                    let output = output.trim();
                    (!output.is_empty()).then(|| output.to_owned())
                });
            return Err(match detail {
                Some(detail) => {
                    format!("dockd exited before socket readiness: {status}: {detail}")
                }
                None => format!("dockd exited before socket readiness: {status}"),
            });
        }
        if Instant::now() >= deadline {
            return Err("dockd did not create a ready socket within 5 seconds".into());
        }
        thread::sleep(Duration::from_millis(20));
    }
}

fn locate_or_build_dockd() -> Result<PathBuf, String> {
    let current = std::env::current_exe().map_err(|e| e.to_string())?;
    let sibling = current.with_file_name("dockd");
    if sibling.is_file() {
        return Ok(sibling);
    }

    // `cargo run --bin dock` builds only Dock. Development builds retain their trusted compile-
    // time manifest location, so bootstrap the sibling into the exact target/profile directory
    // containing the running executable. Installed binaries still require an installed sibling.
    let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("Cargo.toml");
    let Some(profile_dir) = current.parent() else {
        return Err(format!(
            "dockd must be installed beside dock: {}",
            sibling.display()
        ));
    };
    let Some(target_dir) = profile_dir.parent() else {
        return Err(format!(
            "dockd must be installed beside dock: {}",
            sibling.display()
        ));
    };
    let profile = profile_dir.file_name().and_then(|name| name.to_str());
    if !manifest.is_file() || !matches!(profile, Some("debug" | "release")) {
        return Err(format!(
            "dockd must be installed beside dock: {}",
            sibling.display()
        ));
    }
    let mut build = Command::new("cargo");
    build
        .arg("build")
        .arg("--quiet")
        .arg("--manifest-path")
        .arg(&manifest)
        .arg("--target-dir")
        .arg(target_dir)
        .arg("--bin")
        .arg("dockd")
        .stdin(Stdio::null());
    if profile == Some("release") {
        build.arg("--release");
    }
    let status = build
        .status()
        .map_err(|error| format!("could not invoke Cargo to build dockd: {error}"))?;
    if !status.success() || !sibling.is_file() {
        return Err(format!(
            "could not bootstrap dockd beside dock at {}",
            sibling.display()
        ));
    }
    Ok(sibling)
}

fn verify_socket(socket: &Path) -> Result<(), String> {
    let metadata = fs::symlink_metadata(socket)
        .map_err(|e| format!("could not inspect daemon socket: {e}"))?;
    if !metadata.file_type().is_socket() || metadata.permissions().mode() & 0o077 != 0 {
        return Err("daemon socket must be a restricted Unix socket".into());
    }
    Ok(())
}

fn socket_mode(socket: &Path) -> Result<u32, String> {
    Ok(fs::metadata(socket)
        .map_err(|e| e.to_string())?
        .permissions()
        .mode()
        & 0o777)
}

struct TerminalState {
    fd: RawFd,
    termios: libc::termios,
}

impl TerminalState {
    fn capture(fd: RawFd) -> io::Result<Self> {
        let mut termios = std::mem::MaybeUninit::uninit();
        // SAFETY: tcgetattr initializes the termios value when it reports success.
        if unsafe { libc::tcgetattr(fd, termios.as_mut_ptr()) } == -1 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: the successful tcgetattr above initialized this value.
        let termios = unsafe { termios.assume_init() };
        Ok(Self { fd, termios })
    }

    fn restore(&self, discard_pending_input: bool) -> io::Result<()> {
        if discard_pending_input {
            // Explicit quit abandons keystrokes that the dashboard has not consumed. In
            // particular, flushing before the final complete restore clears Darwin's PENDIN
            // kernel state instead of trying to edit that state bit in the saved termios.
            // SAFETY: self.fd is the terminal descriptor from which self.termios was captured.
            if unsafe { libc::tcflush(self.fd, libc::TCIFLUSH) } == -1 {
                return Err(io::Error::last_os_error());
            }
            // Darwin can retain a complete canonical line behind PENDIN even after TCIFLUSH.
            // Drain anything still readable while the dashboard's raw mode is active.
            let flags = unsafe { libc::fcntl(self.fd, libc::F_GETFL) };
            if flags == -1
                || unsafe { libc::fcntl(self.fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } == -1
            {
                return Err(io::Error::last_os_error());
            }
            let mut buffer = [0_u8; 256];
            loop {
                let read = unsafe {
                    libc::read(
                        self.fd,
                        buffer.as_mut_ptr().cast::<libc::c_void>(),
                        buffer.len(),
                    )
                };
                if read > 0 {
                    continue;
                }
                if read == 0 || io::Error::last_os_error().kind() == io::ErrorKind::WouldBlock {
                    break;
                }
                let error = io::Error::last_os_error();
                let _ = unsafe { libc::fcntl(self.fd, libc::F_SETFL, flags) };
                return Err(error);
            }
            if unsafe { libc::fcntl(self.fd, libc::F_SETFL, flags) } == -1 {
                return Err(io::Error::last_os_error());
            }
        }
        // Use libc directly here. Higher-level termios wrappers may normalize flags that they do
        // not know about, which would defeat the byte-for-byte terminal lifecycle guarantee.
        // SAFETY: self.termios came from tcgetattr for this file descriptor and remains valid.
        let action = if discard_pending_input {
            // Make the canonical-mode transition and pending-input discard one kernel operation;
            // Darwin may otherwise re-present a PENDIN line between separate calls.
            libc::TCSAFLUSH
        } else {
            libc::TCSANOW
        };
        if unsafe { libc::tcsetattr(self.fd, action, &self.termios) } == -1 {
            return Err(io::Error::last_os_error());
        }
        if discard_pending_input {
            // Restoring canonical mode can make Darwin re-present input that was pending while
            // raw mode was active. Flush once more after the exact snapshot is installed so an
            // explicit dashboard quit cannot leak abandoned input back to the caller's shell.
            if unsafe { libc::tcflush(self.fd, libc::TCIFLUSH) } == -1 {
                return Err(io::Error::last_os_error());
            }
        }
        Ok(())
    }
}

struct TerminalGuard {
    original: TerminalState,
    discard_pending_input: bool,
    restored: bool,
}

/// Narrows mouse reporting from any-event tracking to button-held motion.
///
/// `EnableMouseCapture` turns on `?1000h ?1002h ?1003h ?1015h ?1006h`, and `?1003h` is
/// *any-event* tracking: the terminal reports every pointer movement over the window, button
/// held or not. Dock has nothing to do with idle motion — it falls through to
/// `UiCommand::None` — but each report still woke the render loop for a complete repaint of
/// every pane, plus a deep clone of the workspace tree, which is why simply moving the mouse
/// across a dashboard made it feel heavy. `?1002` (report motion only while a button is down)
/// is all a drag selection needs.
///
/// The `?1002h` after the reset is not redundant, and leaving it out breaks the mouse
/// outright. xterm and the many emulators that copy it hold *one* mouse mode rather than a set
/// of independent flags, and reset the whole thing to off for any of `?1000l`/`?1002l`/`?1003l`
/// — so `?1003l` alone would have turned mouse reporting off completely and taken drag
/// selection, the wheel and every clickable control with it. Re-asserting `?1002h` lands that
/// single mode on button-event tracking; on a terminal that really does keep independent
/// flags, it is a set of a flag that is already set. Both end up in the same place.
///
/// Nothing extra is needed to restore this: `DisableMouseCapture` emits `?1002l` among its
/// inverses on both teardown paths below.
const BUTTON_MOTION_ONLY: &str = "\x1b[?1003l\x1b[?1002h";

impl TerminalGuard {
    fn enter() -> io::Result<Self> {
        let guard = Self {
            original: TerminalState::capture(io::stdin().as_raw_fd())?,
            discard_pending_input: false,
            restored: false,
        };
        enable_raw_mode()?;
        execute!(
            io::stdout(),
            EnterAlternateScreen,
            EnableMouseCapture,
            // Without this the host terminal delivers a paste as a burst of key events, so every
            // newline in it runs as a command the moment it arrives. Enabled here and disabled
            // on every restore path below, alongside mouse capture.
            EnableBracketedPaste
        )?;
        write!(io::stdout(), "{BUTTON_MOTION_ONLY}")?;
        io::stdout().flush()?;
        Ok(guard)
    }

    fn discard_pending_input_on_exit(&mut self) {
        self.discard_pending_input = true;
    }

    fn restore(&mut self) -> io::Result<()> {
        let _ = execute!(
            io::stdout(),
            DisableBracketedPaste,
            DisableMouseCapture,
            LeaveAlternateScreen
        );
        let _ = disable_raw_mode();
        self.original.restore(self.discard_pending_input)?;
        self.restored = true;
        Ok(())
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        if self.restored {
            return;
        }
        let _ = execute!(
            io::stdout(),
            DisableBracketedPaste,
            DisableMouseCapture,
            LeaveAlternateScreen
        );
        // Crossterm's raw-mode snapshot uses its own termios representation. On macOS that
        // representation can change otherwise-unmodelled local flags while restoring. Let it
        // clear its global raw-mode bookkeeping first, then make our complete entry snapshot the
        // final terminal state. This also runs while unwinding from dashboard errors or panics.
        let _ = disable_raw_mode();
        let _ = self.original.restore(self.discard_pending_input);
    }
}

fn run_dashboard(
    client: &mut Client,
    socket: &Path,
    runtime_directory: String,
    state_dir: &Path,
) -> Result<(), String> {
    let mut guard = TerminalGuard::enter().map_err(|e| e.to_string())?;
    let mut terminal =
        Terminal::new(CrosstermBackend::new(io::stdout())).map_err(|e| e.to_string())?;
    let mut dashboard = Dashboard::default();
    dashboard.runtime_directory = runtime_directory.clone();
    let (catalog_tx, catalog_rx) = mpsc::channel();
    let mut catalog_loading = false;
    let mut test_events = test_events()?;
    // Events drained from the terminal in one burst and not yet handled. Handled one per
    // iteration like any other, so each still gets its own paint; the burst is only *read* in
    // one go, which is what lets the motion inside it be collapsed.
    let mut pending_events: VecDeque<Event> = VecDeque::new();
    // A running dashboard holds two daemon connections: `client` for requests, and this one,
    // which the daemon turns into a one-way push channel and never reads again. Pane content
    // now arrives on it, which is what lets the timed Inspect poll below go away entirely: an
    // idle dashboard sends nothing and the daemon sends nothing back.
    let mut events = EventStream::subscribe(socket)?;
    refresh(client, &mut dashboard)?;
    loop {
        if let Ok((root, launches)) = catalog_rx.try_recv() {
            dashboard.set_repository_catalog(root, launches);
            catalog_loading = false;
        }
        // Drained without blocking: the reader thread owns the socket, so a quiet daemon costs
        // this loop nothing and a busy one cannot stall the next paint.
        loop {
            match events.poll() {
                StreamPoll::Event(event) => dashboard.apply_event(event),
                StreamPoll::Idle => break,
                StreamPoll::Reconnected => {
                    // The replacement subscription re-attaches every live run from a full
                    // snapshot and re-announces its process and agent state, so the refresh
                    // this needs arrives through the normal event path on a later tick.
                    dashboard.detach_screens();
                    dashboard.error =
                        Some("event stream dropped; re-subscribed to the daemon".into());
                    break;
                }
                StreamPoll::Lost(error) => {
                    // Reported rather than swallowed: nothing polls any more, so a dashboard
                    // that ignored this would keep painting a frozen frame indefinitely.
                    dashboard.error = Some(format!("event stream lost, retrying: {error}"));
                    break;
                }
            }
        }
        if dashboard.take_refresh() {
            refresh(client, &mut dashboard)?;
        }
        // A Board pane draws from the client's own board load, and until now the only thing that
        // ever read the board was the overlay's key. A board pane restored from a previous
        // session would therefore have come back as an empty grid until somebody pressed
        // `Ctrl+B k`. Read once, when a board pane first exists and nothing has been read yet;
        // the check short-circuits on a field the moment it has.
        if dashboard.board_pane_needs_load()
            && let Some(directory) = dock::board::tasks_dir(
                &dashboard.repository_root,
                dashboard.workspace_id().unwrap_or_default(),
            )
        {
            let tasks = dock::board::load(&directory);
            dashboard.set_board_pane_tasks(tasks, directory);
        }
        terminal
            .draw(|frame| dashboard.render(frame))
            .map_err(|e| e.to_string())?;
        for (workspace_id, pane_id, rows, cols) in dashboard.take_pending_resizes() {
            // `send` rather than `request`: the reply was already discarded, and waiting for it
            // put a blocking daemon round trip inside the frame. A divider drag changes pane
            // geometry on every motion event, so that was one blocking round trip per frame for
            // as long as the pointer moved. Errors are not lost — the client counts the unread
            // reply and `take_deferred_error` below surfaces it on the next drain.
            let _ = client.send(&Request::PaneResize(PaneResizeRequest {
                workspace_id,
                pane_id,
                rows,
                cols,
            }));
        }
        for (workspace_id, pane_id, prompt) in dashboard.take_opening_prompts() {
            // Submitted, not merely typed. The user dispatched this card seconds ago and a Claude
            // pane dispatched the same way is already working on it; leaving Amp's task sitting
            // unsent in its box would make the same keypress mean two different things.
            let mut input = prompt.into_bytes();
            input.push(b'\r');
            let _ = client.send(&Request::PaneInput(PaneInputRequest {
                workspace_id,
                pane_id,
                input: PaneInputRequest::encode(&input),
            }));
        }
        if let Some(message) = client.take_deferred_error() {
            dashboard.error = Some(message);
        }
        let event = if let Some(event) = test_events.pop_front() {
            event
        } else if let Some(event) = pending_events.pop_front() {
            event
        } else {
            if !event::poll(Duration::from_millis(16)).map_err(|e| e.to_string())? {
                continue;
            }
            let mut batch = vec![event::read().map_err(|e| e.to_string())?];
            // Everything the terminal has already written is taken now, before the frame that
            // would otherwise be painted once per event. A pointer crossing a pane delivers a
            // report per cell, and each one used to cost a complete repaint of every pane plus
            // a deep clone of the workspace tree; collapsed here, a whole burst costs one.
            while batch.len() < MAX_COALESCED_EVENTS
                && event::poll(Duration::ZERO).map_err(|e| e.to_string())?
            {
                batch.push(event::read().map_err(|e| e.to_string())?);
            }
            let mut batch = coalesce_motion(batch);
            let first = batch.remove(0);
            pending_events.extend(batch);
            first
        };
        let command = match event {
            Event::Key(key) if key.kind == KeyEventKind::Press => dashboard.key(key),
            Event::Paste(text) => dashboard.paste(text),
            Event::Mouse(mouse) => dashboard.mouse(mouse),
            Event::Resize(_, _) => UiCommand::None,
            _ => UiCommand::None,
        };
        match command {
            UiCommand::Quit => {
                guard.discard_pending_input_on_exit();
                break;
            }
            UiCommand::Request(request) => {
                let test_launch = !test_events.is_empty()
                    && matches!(
                        request.as_ref(),
                        Request::TerminalLaunch(_) | Request::LaunchIntoPane(_)
                    );
                // Dashboard commands optimistically update focus, geometry, and form state.
                // Paint that local result before the bounded authority request so keyboard and
                // pointer interaction never waits for the daemon to become visible.
                terminal
                    .draw(|frame| dashboard.render(frame))
                    .map_err(|e| e.to_string())?;
                let response = client.request(&request)?;
                // A queue answer goes to the dashboard whole rather than being reduced to
                // "did the daemon object". Both halves matter: the success carries the full
                // listing, so an arming lands on the lane in the same round trip, and the
                // refusal is the product — the sentence naming `dock hooks --install` has to
                // reach the person who pressed the key, in the daemon's own words.
                if matches!(request.as_ref(), Request::Queue(_)) {
                    dashboard.apply_queue_response(response);
                } else if matches!(request.as_ref(), Request::PaneHistory(_)) {
                    // The same shape, for the same reason: the answer is the product. It
                    // carries the older output the wheel asked for, and the loop is
                    // synchronous — this response is applied before the next input event is
                    // read — so a request that fails simply leaves the cursor where it was and
                    // the next notch asks again.
                    dashboard.apply_pane_history_response(response);
                } else {
                    match response {
                        Response::Error { message, .. } => dashboard.error = Some(message),
                        _ => dashboard.error = None,
                    }
                }
                refresh(client, &mut dashboard)?;
                if test_launch {
                    let deadline = Instant::now() + Duration::from_secs(3);
                    while !dashboard
                        .runs
                        .iter()
                        .any(|run| run.state == ProcessState::Running)
                    {
                        if Instant::now() >= deadline {
                            return Err(
                                "deterministic launch did not become visibly running".into()
                            );
                        }
                        thread::sleep(Duration::from_millis(10));
                        refresh(client, &mut dashboard)?;
                    }
                    terminal
                        .draw(|frame| dashboard.render(frame))
                        .map_err(|e| e.to_string())?;
                }
            }
            UiCommand::Send(request) => {
                // Painted first, exactly as `Request` is, so the optimistic local change is on
                // screen before anything touches the socket. Then posted and forgotten: there
                // is no `refresh` here, because the daemon's own event stream is what
                // reconciles a change the dashboard has already made.
                terminal
                    .draw(|frame| dashboard.render(frame))
                    .map_err(|e| e.to_string())?;
                let _ = client.send(&request);
            }
            UiCommand::Requests(requests) => {
                // Painted before the batch for the same reason a single request is, then sent in
                // order. The loop keeps going after a refusal rather than stopping at the first
                // one: these are the panes of one workspace, and abandoning the rest would leave
                // a workspace that is neither closed nor whole. `refresh` below is what the
                // dashboard actually believes afterwards.
                terminal
                    .draw(|frame| dashboard.render(frame))
                    .map_err(|e| e.to_string())?;
                let mut failure = None;
                for request in &requests {
                    if let Response::Error { message, .. } = client.request(request)? {
                        failure = Some(message);
                    }
                }
                dashboard.error = failure;
                refresh(client, &mut dashboard)?;
            }
            UiCommand::PaneInput(bytes) => {
                let Some(workspace) = dashboard.workspace() else {
                    continue;
                };
                let request = Request::PaneInput(PaneInputRequest {
                    workspace_id: workspace.workspace_id.clone(),
                    pane_id: workspace.focused_pane_id.clone(),
                    input: PaneInputRequest::encode(&bytes),
                });
                // Deliberately not `request`: the keystroke's visible result is the pane echo
                // that arrives on the event stream, so waiting for an acknowledgement here
                // would add a daemon round trip to every keypress-to-paint.
                client.send(&request)?;
            }
            UiCommand::LoadReviewInbox => {
                // Read from the daemon's own store rather than asked for over the socket. The
                // pending queue has a request, but the answered ones do not, and adding a second
                // one would be another protocol version for records this client can already
                // reach: it computed this directory itself and handed it to the daemon at startup.
                let store = LocalStore::new(state_dir);
                match store.list_handoff_records() {
                    Ok(records) => {
                        let with_decisions = records
                            .into_iter()
                            .map(|record| {
                                let decision = store.load_decision(&record.packet.run_id).ok();
                                (record, decision)
                            })
                            .collect();
                        dashboard.set_review_inbox(with_decisions);
                    }
                    Err(message) => dashboard.error = Some(message),
                }
                continue;
            }
            UiCommand::LoadGit => {
                // The focused pane's own worktree, so a pane dispatched onto a task shows that
                // task's changes rather than the repository the dashboard was started in.
                let worktree = dashboard
                    .focused_run()
                    .map(|run| run.worktree.clone())
                    .filter(|worktree| !worktree.is_empty())
                    .unwrap_or_else(|| dashboard.repository_root.clone());
                if worktree.is_empty() {
                    dashboard.error = Some("no worktree here to inspect".into());
                    continue;
                }
                let adapter = GitAdapter::new(&worktree);
                match adapter
                    .facts("HEAD")
                    .and_then(|facts| adapter.diff("HEAD").map(|diff| (facts, diff)))
                {
                    Ok((facts, diff)) => dashboard.set_git(facts, diff),
                    Err(message) => dashboard.error = Some(message),
                }
            }
            UiCommand::LoadBoard => match dock::board::tasks_dir(
                &dashboard.repository_root,
                dashboard.workspace_id().unwrap_or_default(),
            ) {
                Some(directory) => {
                    let tasks = dock::board::load(&directory);
                    dashboard.set_board_tasks(tasks, directory);
                }
                None => {
                    dashboard.error = Some("no board: not in a repository and HOME is unset".into())
                }
            },
            UiCommand::DispatchTask(task) => {
                terminal
                    .draw(|frame| dashboard.render(frame))
                    .map_err(|e| e.to_string())?;
                match dispatch_task(client, &mut dashboard, &task) {
                    Ok(()) => refresh(client, &mut dashboard)?,
                    Err(message) => dashboard.error = Some(message),
                }
            }
            UiCommand::Refresh => refresh(client, &mut dashboard)?,
            UiCommand::LoadCatalog => {
                if !catalog_loading {
                    catalog_loading = true;
                    let directory = PathBuf::from(runtime_directory.clone());
                    let sender = catalog_tx.clone();
                    thread::spawn(move || {
                        let _ = sender.send(repository_catalog(&directory));
                    });
                }
            }
            UiCommand::None => {}
        }
    }
    terminal.show_cursor().map_err(|e| e.to_string())?;
    drop(terminal);
    guard.restore().map_err(|e| e.to_string())?;
    Ok(())
}

/// Gives a task somewhere isolated to be worked on, then launches an agent there.
///
/// The worktree is created by this client rather than by the daemon, which keeps the daemon's
/// authority exactly where it already was: it validates that a supplied worktree really is a Git
/// worktree top-level inside the repository before it will bind a run to it. So the client
/// proposes a worktree and the daemon still refuses anything that is not one — Dock did not gain
/// the power to dispatch into an arbitrary directory by gaining the power to make a worktree.
/// How much of a card's body is carried into the prompt.
///
/// A body is Markdown a person may write as much of as they like, and the prompt it goes into is
/// an argv entry for the adapters whose command line takes one and a sentence typed into a pane
/// for the rest. Neither wants a chapter. Cutting it here, with somewhere to read the remainder,
/// beats discovering the limit at exec time or watching Dock type for a minute and a half.
const MAX_PROMPT_BODY_BYTES: usize = 4096;

/// What a dispatched agent is told: the task, and how to record that it finished.
///
/// The task is the card's title *and its body*. The body is where a person writes the outcome,
/// the acceptance criteria and what was ruled out; sending only the title dispatched an agent
/// against a headline and left the description it was written under sitting on disk.
///
/// The instruction is part of the prompt rather than something Dock works out afterwards. Dock
/// could watch the pane and move the task when the agent looks done, but "looks done" is a regex
/// over a screen, and the board is the durable record of what happened — moving a real task on a
/// guess is how a board stops being trustworthy. Telling the agent costs one line and is true.
fn dispatch_prompt(task_id: u64, title: &str, body: &str) -> String {
    let body = body.trim();
    // An empty body is the common case on a personal board, where a card is a title and nothing
    // else, and it must leave no gap: a prompt opening on a blank paragraph reads like a card
    // that failed to load rather than one with nothing more to say.
    let card = if body.is_empty() {
        String::new()
    } else if body.len() <= MAX_PROMPT_BODY_BYTES {
        format!("\n\n{body}")
    } else {
        // Back off to a character boundary rather than slicing on the byte: prose puts multi-byte
        // characters wherever it likes and `&body[..n]` panics in the middle of one.
        let mut end = MAX_PROMPT_BODY_BYTES;
        while !body.is_char_boundary(end) {
            end -= 1;
        }
        format!(
            "\n\n{}\n\n[This card is longer than the prompt can carry. Read the rest with \
             `dock task show {task_id}`.]",
            &body[..end]
        )
    };
    format!(
        "{title}{card}\n\nThis is task #{task_id} on the Dock board. When you finish:\n    \
         dock task move {task_id} review\n    \
         dock handoff \"what you did\" --check=\"cargo test:pass\"\n\
         The handoff puts your result in front of the human with the evidence Dock measured itself."
    )
}

#[cfg(test)]
mod hook_check_tests {
    use super::{Verdict, hook_events, missing_hook_events, resolve_on_path, verdict};

    #[test]
    fn a_check_that_could_not_reach_a_pane_has_not_thereby_failed() {
        // This command's own first bug: run from an ordinary shell, where there is no pane and so
        // no connection to test, it reported a correctly wired install as broken and exited 1.
        assert_eq!(verdict(0, true), Verdict::Reachable);
        assert_eq!(verdict(0, false), Verdict::Wired);
        // What it must not do is let the skip swallow a real failure found before it.
        assert_eq!(verdict(1, true), Verdict::Failed);
        assert_eq!(verdict(1, false), Verdict::Failed);
    }

    #[test]
    fn a_settings_file_with_no_hooks_at_all_is_missing_every_event() {
        // The state this bug actually presented in: hooks never installed, so nothing was ever
        // reported, so every pane sat on screen-scraped state and nothing said why.
        let missing = missing_hook_events(&serde_json::json!({}), &hook_events());
        assert_eq!(missing.len(), 6, "{missing:?}");
        assert!(missing.contains(&"Stop".to_owned()), "{missing:?}");
    }

    #[test]
    fn what_install_writes_is_what_check_reads_back_as_wired() {
        // The two halves have to agree about what "installed" means, or `--install` followed by
        // `--check` reports work it just did as missing.
        let expected = hook_events();
        let settings = serde_json::json!({ "hooks": expected["hooks"].clone() });
        assert!(missing_hook_events(&settings, &expected).is_empty());
    }

    #[test]
    fn somebody_elses_hook_on_the_same_event_does_not_count_as_ours() {
        // An event can carry several handlers. Ours being absent is what matters, not the slot
        // being empty — the earlier merge logic is careful about this and the check must match it.
        let expected = hook_events();
        let settings = serde_json::json!({
            "hooks": { "Stop": [{"hooks": [{"type": "command", "command": "make lint"}]}] }
        });
        let missing = missing_hook_events(&settings, &expected);
        assert!(missing.contains(&"Stop".to_owned()), "{missing:?}");
    }

    #[test]
    fn a_dock_that_is_not_on_path_is_found_to_be_missing_rather_than_assumed_present() {
        // Hooks invoke `dock` by bare name, so this is its own failure mode: a Dock that works
        // when you type its path and does nothing when a hook runs it.
        assert_eq!(resolve_on_path(None, "dock"), None);
        assert_eq!(resolve_on_path(Some(""), "dock"), None);
        assert_eq!(
            resolve_on_path(Some("/nonexistent:/also/nothing"), "dock"),
            None
        );
        // And an entry that does hold the file resolves to it, empty segments notwithstanding.
        let dir = std::env::current_dir().expect("cwd");
        let found = resolve_on_path(Some(&format!(":{}", dir.display())), "Cargo.toml");
        assert_eq!(found, Some(dir.join("Cargo.toml")));
    }
}

#[cfg(test)]
mod dispatch_prompt_tests {
    use super::{MAX_PROMPT_BODY_BYTES, dispatch_prompt};

    #[test]
    fn a_dispatched_agent_is_told_the_task_and_how_to_close_it() {
        let prompt = dispatch_prompt(7, "fix the retry path", "");
        assert!(prompt.starts_with("fix the retry path"), "{prompt}");
        // The instruction is explicit because the alternative is Dock watching the pane and
        // guessing when the agent is finished, which would move a durable record on a regex.
        assert!(prompt.contains("dock task move 7 review"), "{prompt}");
        // And how to put the result in front of a person, which is the step that was impossible:
        // the review queue could previously only be filled by hand-authoring a JSON packet.
        assert!(prompt.contains("dock handoff"), "{prompt}");
        assert!(prompt.contains("#7"), "{prompt}");
    }

    #[test]
    fn an_agent_is_sent_what_the_card_says_and_not_only_its_headline() {
        // A title is a filename-length summary. The acceptance criteria, the constraints and the
        // out-of-scope list are all in the body, and dispatching used to drop every word of it —
        // so the agent was asked to do work nobody had described to it.
        let prompt = dispatch_prompt(
            7,
            "fix the retry path",
            "# Outcome\n\nRetries stop after three attempts.\n",
        );
        assert!(
            prompt.starts_with("fix the retry path\n\n# Outcome"),
            "{prompt}"
        );
        assert!(
            prompt.contains("Retries stop after three attempts."),
            "{prompt}"
        );
        // The closing instruction still comes after the card rather than being buried in it.
        assert!(prompt.contains("dock task move 7 review"), "{prompt}");
    }

    #[test]
    fn a_card_with_no_body_leaves_no_hole_where_one_would_have_been() {
        // Most cards on a personal board are a title and nothing else; that prompt has to read
        // as a sentence rather than as a paragraph that failed to load.
        let prompt = dispatch_prompt(7, "fix the retry path", "\n\n   \n");
        assert!(
            prompt.starts_with("fix the retry path\n\nThis is task #7"),
            "{prompt}"
        );
    }

    #[test]
    fn a_card_longer_than_a_prompt_can_carry_is_cut_with_somewhere_to_read_the_rest() {
        // The prompt is an argv entry for the adapters whose command line takes one and typed
        // into a pane for the rest, and a body is Markdown a person may write as much of as they
        // like. Cutting it here beats finding out at exec time, and the agent is told where the
        // whole card is rather than left with a sentence that stops mid-word.
        let body = "x".repeat(MAX_PROMPT_BODY_BYTES * 2);
        let prompt = dispatch_prompt(7, "fix the retry path", &body);
        assert!(prompt.len() < body.len(), "{}", prompt.len());
        assert!(prompt.contains("dock task show 7"), "{prompt}");
        // And the instruction survives the cut, which is the reason the body is what gets cut.
        assert!(prompt.contains("dock task move 7 review"), "{prompt}");
    }

    #[test]
    fn cutting_a_card_never_splits_a_character_in_half() {
        // A body is prose, so the byte at the limit is as likely as not to sit inside a
        // multi-byte character, and slicing there panics rather than truncating.
        let body = "☃".repeat(MAX_PROMPT_BODY_BYTES);
        let prompt = dispatch_prompt(7, "snowed under", &body);
        assert!(prompt.contains("dock task show 7"), "{prompt}");
        assert!(prompt.contains('☃'), "{prompt}");
    }
}

#[cfg(test)]
mod claim_tests {
    use super::claim_task;
    use std::fs;

    #[test]
    fn a_claim_is_never_written_to_a_board_that_is_not_docks_own() {
        // The claim moved to after the daemon accepts, and the rule it has to keep on the way is
        // this one: a repository's board belongs to kanban-md and to whoever commits to it, and
        // Dock moving a card there is that tool's business rather than this one's.
        let directory = std::env::temp_dir().join(format!("dock-claim-{}", std::process::id()));
        fs::create_dir_all(&directory).expect("a board that is not under ~/.dock/boards");
        fs::write(
            directory.join("001-a.md"),
            "---\nid: 1\ntitle: 'Theirs'\nstatus: backlog\n---\n",
        )
        .expect("seed a task");

        claim_task(Some(&directory), 1);

        assert_eq!(dock::board::load(&directory)[0].status, "backlog");
        let _ = fs::remove_dir_all(&directory);
    }
}

fn dispatch_task(
    client: &mut Client,
    dashboard: &mut Dashboard,
    task: &TaskDispatch,
) -> Result<(), String> {
    let TaskDispatch {
        workspace_id,
        pane_id,
        run_id,
        task_id,
        title,
        adapter,
    } = task;
    let board = dock::board::tasks_dir(
        &dashboard.repository_root,
        dashboard.workspace_id().unwrap_or_default(),
    );
    // Read off disk rather than out of the dashboard's copy: the board is files, and the card may
    // have been edited since the list in hand was loaded. What the agent is sent should be what
    // the file says at the moment it is dispatched.
    let body = board
        .as_ref()
        .map(|directory| dock::board::load(directory))
        .and_then(|tasks| tasks.into_iter().find(|task| task.id == *task_id))
        .map(|task| task.body)
        .unwrap_or_default();
    let repository_root = PathBuf::from(dashboard.repository_root.clone());
    // Without a repository there is no worktree to isolate the work in, and nothing to isolate it
    // from. The agent is launched where the dashboard already is, with the task as its opening
    // prompt — both `claude [prompt]` and `codex [PROMPT]` take one, read from their own --help.
    if repository_root.as_os_str().is_empty() {
        let profile = DashboardProfile::try_from(adapter.clone()).map_err(|()| {
            format!(
                "cannot dispatch: {} has no terminal profile",
                adapter.label()
            )
        })?;
        let request = Request::TerminalLaunch(TerminalLaunchRequest {
            workspace_id: workspace_id.to_owned(),
            pane_id: pane_id.to_owned(),
            run_id: run_id.to_owned(),
            profile,
            runtime_directory: dashboard.runtime_directory.clone(),
            arguments: adapter.prompt_arguments(&dispatch_prompt(*task_id, title, &body)),
            // Recorded on the run itself, so which card this pane is working stays known after
            // the dashboard that dispatched it has gone.
            external_task_ref: task_id.to_string(),
        });
        if let Response::Error { message, .. } = client.request(&request)? {
            return Err(message);
        }
        claim_task(board.as_deref(), *task_id);
        remember_opening_prompt(
            dashboard,
            adapter,
            run_id,
            workspace_id,
            pane_id,
            *task_id,
            title,
            &body,
        );
        dashboard.error = Some(format!("task {task_id} dispatched here, unbound: {title}"));
        return Ok(());
    }
    let branch = format!("dock/task-{task_id}");
    // Beside the repository rather than inside it, so the worktree is never a candidate for the
    // repository's own status, ignore rules, or a recursive walk.
    let path = repository_root
        .parent()
        .ok_or("repository root has no parent to place a worktree beside")?
        .join(format!(
            "{}-task-{task_id}",
            repository_root
                .file_name()
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or_else(|| "repo".into())
        ));
    let worktree = dock::git::ensure_worktree(&repository_root, &branch, &path, "HEAD")?;
    let request = Request::LaunchIntoPane(LaunchIntoPaneRequest {
        workspace_id: workspace_id.to_owned(),
        pane_id: pane_id.to_owned(),
        dispatch: DispatchRequest {
            repository_root: repository_root.display().to_string(),
            external_task_ref: task_id.to_string(),
            run_id: run_id.to_owned(),
            worktree: worktree.path.display().to_string(),
            adapter: AdapterSelection {
                id: adapter.clone(),
                executable: None,
                // The task, so the agent knows what it was dispatched for. Without this a
                // repository-bound dispatch built a branch and a worktree and then opened the
                // agent into silence, while the unbound path — the casual one — handed it
                // everything. That was backwards.
                arguments: adapter.prompt_arguments(&dispatch_prompt(*task_id, title, &body)),
            },
        },
    });
    match client.request(&request)? {
        Response::Error { message, .. } => return Err(message),
        _ => {
            dashboard.error = None;
        }
    }
    claim_task(board.as_deref(), *task_id);
    remember_opening_prompt(
        dashboard,
        adapter,
        run_id,
        workspace_id,
        pane_id,
        *task_id,
        title,
        &body,
    );
    // The footer carries one status line, which the codebase already uses for outcomes as well as
    // failures — a yank reports the same way.
    dashboard.error = Some(format!(
        "task {task_id} {} on {branch}: {title}",
        if worktree.created {
            "dispatched into a new worktree"
        } else {
            "dispatched into its existing worktree"
        }
    ));
    Ok(())
}

/// Marks a card as claimed, once the dispatch it was claimed for has actually been accepted.
///
/// This used to run first, before the profile check, before the worktree, and before the daemon
/// round trip — so a dispatch that failed at any of those three left a card sitting in
/// `in-progress` with nothing whatsoever working on it, and the board said a person was busy on
/// work that had never started. The claim is still best effort, because a board that will not
/// move a task is no reason to disown a run the daemon has already begun; it is just no longer
/// speculative.
///
/// Only on Dock's own board: a repository's belongs to `kanban-md` and to whoever commits to it,
/// and moving a task there is that tool's business, not this one's.
fn claim_task(board: Option<&Path>, task_id: u64) {
    if let Some(directory) = board
        && dock::board::is_personal(directory)
    {
        let _ = dock::board::set_status(directory, task_id, "in-progress");
    }
}

/// Notes a task that its agent's command line could not carry, to be typed in once it is up.
///
/// Only for the agents that have somewhere to type it. `prompt_arguments` returns nothing for five
/// adapters and only two of them are agents with an input box; the rest are shells and fixtures,
/// where a sentence is a command rather than a request.
#[allow(clippy::too_many_arguments)]
fn remember_opening_prompt(
    dashboard: &mut Dashboard,
    adapter: &dock::adapter::AdapterId,
    run_id: &str,
    workspace_id: &str,
    pane_id: &str,
    task_id: u64,
    title: &str,
    body: &str,
) {
    if !adapter.opening_prompt_is_typed() {
        return;
    }
    dashboard.expect_opening_prompt(
        run_id,
        workspace_id,
        pane_id,
        &dispatch_prompt(task_id, title, body),
    );
}

/// `dock detect <agent> [--explain]` — the rules in force, and what they make of a screen.
///
/// The reason this exists: every wrong classification so far has been invisible. The roster said a
/// word and there was no way to ask why, so the only way to find out was to read Dock's source.
/// With `--explain` a captured screen goes in on stdin and the matching rule comes out, which
/// turns "the status is wrong" into a line somebody can point at and edit.
fn detect_command(args: &[String]) -> io::Result<()> {
    let name = args
        .iter()
        .find(|argument| !argument.starts_with("--"))
        .ok_or_else(|| io::Error::other("dock detect <claude|codex|amp|copilot|…> [--explain]"))?;
    let agent = AgentKind::from_executable(name)
        .ok_or_else(|| io::Error::other(format!("unknown agent {name:?}")))?;

    // Reported here rather than swallowed: a manifest that will not parse is exactly the moment
    // somebody is staring at the answer their edit was meant to change.
    if let Err(message) = dock::detect::manifest::read_override(agent) {
        eprintln!("warning: ignoring a manifest that will not parse — {message}");
    }
    let rules = dock::detect::manifest::resolve(agent);
    match &rules.source {
        dock::detect::manifest::Source::BuiltIn => {
            println!("{} — built-in rules", agent.label());
            if let Some(directory) = dock::detect::manifest::override_dir() {
                println!(
                    "  override with {}/{}.json",
                    directory.display(),
                    agent.label()
                );
            }
        }
        dock::detect::manifest::Source::Override(path) => {
            println!("{} — {}", agent.label(), path.display());
        }
    }
    let (blocked, working, awaiting) = &rules.patterns;
    for (state, patterns) in [
        ("blocked ", blocked),
        ("working ", working),
        ("awaiting", awaiting),
    ] {
        if patterns.is_empty() {
            println!("  {state}  (none)");
        }
        for pattern in patterns {
            println!("  {state}  {pattern}");
        }
    }
    if !args.iter().any(|argument| argument == "--explain") {
        return Ok(());
    }

    let mut screen = String::new();
    io::stdin().read_to_string(&mut screen)?;
    println!("\nagainst {} bytes of screen:", screen.len());
    let mut said_something = false;
    for (state, set, patterns) in [
        ("blocked", &rules.blocked, blocked),
        ("working", &rules.working, working),
        ("awaiting", &rules.awaiting, awaiting),
    ] {
        for index in set.matches(&screen).into_iter() {
            println!("  {state} matched:  {}", patterns[index]);
            said_something = true;
        }
    }
    if !said_something {
        println!("  nothing matched — this screen falls through to output-based detection");
    }
    println!(
        "\nverdict from the screen alone: {:?}",
        dock::detect::classify_screen(agent, &screen)
    );
    println!(
        "(a hook report, where one is installed, overrides this; so does recent output for working)"
    );
    Ok(())
}

/// `dock agent-state <working|blocked|done|idle>` — what a hook reports.
///
/// Small on purpose: it is called from an agent's own event hooks, several times a turn, and any
/// latency here is latency the agent pays. Silent on failure for the same reason — a dashboard
/// that is not running, or a pane Dock did not launch, must never make an agent's hook fail and
/// interrupt the work it was reporting on.
fn agent_state_command(args: &[String]) -> io::Result<()> {
    let Some(word) = args.iter().find(|argument| !argument.starts_with("--")) else {
        return Err(io::Error::other(
            "dock agent-state <working|blocked|done|idle>",
        ));
    };
    let state = match word.as_str() {
        "working" => AgentState::Working,
        "blocked" | "needs-you" => AgentState::Blocked,
        "done" => AgentState::Done,
        "idle" => AgentState::Idle,
        other => {
            return Err(io::Error::other(format!(
                "unknown state {other:?}; expected working, blocked, done or idle"
            )));
        }
    };
    let (Ok(run_id), Ok(socket)) = (std::env::var("DOCK_RUN"), std::env::var("DOCK_SOCKET")) else {
        // Not in a Dock pane. Nothing to report to, and nothing worth failing over.
        hook_debug("DOCK_RUN and DOCK_SOCKET are not both set, so this is not a Dock pane");
        return Ok(());
    };
    let Ok(mut stream) = UnixStream::connect(&socket) else {
        hook_debug(&format!("could not connect to the daemon at {socket}"));
        return Ok(());
    };
    let mut reader = BufReader::new(match stream.try_clone() {
        Ok(clone) => clone,
        Err(error) => {
            hook_debug(&format!("could not read from the daemon socket: {error}"));
            return Ok(());
        }
    });
    for request in [
        serde_json::to_string(&Request::Hello(dock::protocol::HelloRequest {
            version: dock::protocol::PROTOCOL_VERSION,
        }))?,
        serde_json::to_string(&Request::ReportAgentState(
            dock::protocol::ReportAgentStateRequest { run_id, state },
        ))?,
    ] {
        if stream.write_all(request.as_bytes()).is_err() || stream.write_all(b"\n").is_err() {
            hook_debug("the daemon closed the connection mid-report");
            return Ok(());
        }
        let mut line = String::new();
        let _ = reader.read_line(&mut line);
        // The daemon's refusals were being read and dropped, so a report the daemon rejected
        // outright — a run it does not own, a protocol it does not speak — looked from here
        // exactly like one it accepted.
        if let Ok(Response::Error { code, message }) = serde_json::from_str::<Response>(&line) {
            hook_debug(&format!(
                "the daemon refused the report: {code:?}: {message}"
            ));
            return Ok(());
        }
    }
    Ok(())
}

/// Which of Dock's hook entries are absent from a settings document.
///
/// Pure, so the comparison can be tested without a settings file on disk. It looks for exactly what
/// `--install` writes and by the same rule `--install` skips on: an event is wired when Dock's own
/// entry is one of the values in that event's list. Anything else in the list belongs to somebody
/// else and is not our business either way.
fn missing_hook_events(settings: &serde_json::Value, expected: &serde_json::Value) -> Vec<String> {
    let installed = settings.get("hooks").and_then(|hooks| hooks.as_object());
    let mut missing = Vec::new();
    for (event, entry) in expected["hooks"].as_object().expect("hook events") {
        let ours = &entry.as_array().expect("hook entries")[0];
        let present = installed
            .and_then(|hooks| hooks.get(event))
            .and_then(|slot| slot.as_array())
            .is_some_and(|list| list.contains(ours));
        if !present {
            missing.push(event.clone());
        }
    }
    missing.sort();
    missing
}

/// Where a bare `dock` resolves on this `PATH`, if anywhere.
///
/// Pure over its inputs because the answer is worth testing and a real `PATH` is not reproducible.
/// The hooks invoke `dock` by bare name, so a Dock that works when you type its full path and does
/// nothing when a hook runs it is a `PATH` problem wearing a protocol problem's clothes.
fn resolve_on_path(path: Option<&str>, name: &str) -> Option<PathBuf> {
    path?
        .split(':')
        .filter(|entry| !entry.is_empty())
        .map(|entry| PathBuf::from(entry).join(name))
        .find(|candidate| candidate.is_file())
}

/// `dock hooks --check` — walks the road a hook report takes and says where it stops.
///
/// Every failure along that road is silent by design (see [`hook_debug`]), which is survivable
/// right up until you are looking at a pane whose state is wrong and have no way to ask why. This
/// asks on your behalf, in order, and reports the first thing that is not true.
///
/// The daemon leg deliberately sends `Inspect` rather than a state report: `report_agent_state`
/// refuses a run the daemon does not own, and `Inspect` refuses the same run for the same reason,
/// so it answers the identical question without leaving a sticky state behind on a pane whose owner
/// only asked a question.
fn hooks_check(expected: &serde_json::Value) -> io::Result<()> {
    let mut failures = 0usize;
    let mut report = |ok: bool, detail: String| {
        if !ok {
            failures += 1;
        }
        println!("{} {detail}", if ok { "ok  " } else { "FAIL" });
    };

    let settings_path = PathBuf::from(".claude").join("settings.json");
    match fs::read_to_string(&settings_path) {
        Ok(text) if !text.trim().is_empty() => match serde_json::from_str(&text) {
            Ok(settings) => {
                let missing = missing_hook_events(&settings, expected);
                report(
                    missing.is_empty(),
                    if missing.is_empty() {
                        format!("all {} hooks wired in {}", 6, settings_path.display())
                    } else {
                        format!(
                            "{} is missing Dock's entry for: {} — run `dock hooks --install`",
                            settings_path.display(),
                            missing.join(", ")
                        )
                    },
                );
            }
            Err(error) => report(
                false,
                format!("{} is not valid JSON: {error}", settings_path.display()),
            ),
        },
        _ => report(
            false,
            format!(
                "no hooks found at {} — run `dock hooks --install`",
                settings_path.display()
            ),
        ),
    }

    let path = std::env::var("PATH").ok();
    match resolve_on_path(path.as_deref(), "dock") {
        Some(found) => report(true, format!("hooks can run `dock`: {}", found.display())),
        None => report(
            false,
            "`dock` is not on PATH, so every hook runs a command that does not exist".into(),
        ),
    }

    let (run_id, socket) = match (std::env::var("DOCK_RUN"), std::env::var("DOCK_SOCKET")) {
        (Ok(run_id), Ok(socket)) => {
            report(true, format!("inside Dock pane run {run_id}"));
            (run_id, socket)
        }
        _ => {
            // Not a failure, and calling it one was this command's own bug: run from any ordinary
            // shell it condemned a correctly wired install, because the two checks it cannot make
            // from out here are about a pane's connection and there is no pane. What it can check
            // it has checked; the rest is unknown, which is a different answer from wrong.
            println!(
                "--   not inside a Dock pane, so the daemon connection was not checked. \
                 Run this again in a Dock pane to finish."
            );
            return finish_check(failures, true);
        }
    };

    let mut stream = match UnixStream::connect(&socket) {
        Ok(stream) => {
            report(true, format!("daemon answers at {socket}"));
            stream
        }
        Err(error) => {
            report(false, format!("no daemon at {socket}: {error}"));
            return finish_check(failures, false);
        }
    };
    let mut reader = BufReader::new(stream.try_clone()?);
    for request in [
        serde_json::to_string(&Request::Hello(dock::protocol::HelloRequest {
            version: dock::protocol::PROTOCOL_VERSION,
        }))?,
        serde_json::to_string(&Request::Inspect(dock::protocol::InspectRequest {
            run_id: Some(run_id.clone()),
        }))?,
    ] {
        stream.write_all(request.as_bytes())?;
        stream.write_all(b"\n")?;
        let mut line = String::new();
        reader.read_line(&mut line)?;
        match serde_json::from_str::<Response>(&line) {
            Ok(Response::Hello { version }) => report(
                version == dock::protocol::PROTOCOL_VERSION,
                format!(
                    "daemon speaks protocol {version} (this dock speaks {})",
                    dock::protocol::PROTOCOL_VERSION
                ),
            ),
            Ok(Response::Snapshot { .. }) => report(
                true,
                format!("daemon owns run {run_id}, so it will accept its reports"),
            ),
            Ok(Response::Error { code, message }) => {
                report(false, format!("daemon refused: {code:?}: {message}"))
            }
            Ok(other) => report(
                false,
                format!("unexpected answer from the daemon: {other:?}"),
            ),
            Err(error) => report(
                false,
                format!("could not read the daemon's answer: {error}"),
            ),
        }
    }
    finish_check(failures, false)
}

/// What a run of the checks amounts to.
///
/// Separated from the printing because the first version of this command got the classification
/// wrong rather than the wording: run from an ordinary shell it counted "there is no pane to ask
/// about" as a failure, and condemned an install that was in fact correctly wired. Not knowing is
/// its own answer and has to be able to succeed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Verdict {
    /// Everything asked, everything true.
    Wired,
    /// Everything askable from here is true; the pane-bound checks were not reachable.
    Reachable,
    /// Something asked was false.
    Failed,
}

fn verdict(failures: usize, skipped: bool) -> Verdict {
    match (failures, skipped) {
        (0, true) => Verdict::Reachable,
        (0, false) => Verdict::Wired,
        _ => Verdict::Failed,
    }
}

/// The verdict, and an exit status that a script can branch on.
fn finish_check(failures: usize, skipped: bool) -> io::Result<()> {
    if verdict(failures, skipped) == Verdict::Reachable {
        println!(
            "\nhooks are installed and Dock is reachable. Run this inside a Dock pane to check \
             the connection they report over."
        );
        return Ok(());
    }
    if verdict(failures, skipped) == Verdict::Wired {
        println!("\nhooks are wired: this pane's state is reported, not guessed.");
        return Ok(());
    }
    // Printed and exited rather than returned as an error: `main` renders one with `{:?}`, and a
    // diagnostic whose entire purpose is a legible answer should not sign off in Debug format. The
    // status is the part a script branches on, in the manner of `cargo fmt --check`.
    eprintln!(
        "\n{failures} check(s) failed. Until they pass, this pane's state is inferred from its \
         screen rather than reported — which is what makes it flicker between working and your \
         turn while nothing is happening."
    );
    std::process::exit(1);
}

/// Why a hook report went nowhere, said out loud only when asked.
///
/// A hook runs inside the agent's own terminal, so anything written here lands in the middle of the
/// transcript somebody is reading. Silence is the right default — and it was also the bug: a report
/// that never arrives looked exactly like one that did, so a pane whose hooks were never wired sat
/// on guessed-from-the-screen state forever with nothing to say so. `DOCK_HOOK_DEBUG` buys the
/// explanation back for one run, and `dock hooks --check` walks the same road end to end without
/// having to wait for a real turn boundary to fire.
fn hook_debug(reason: &str) {
    if std::env::var_os("DOCK_HOOK_DEBUG").is_some() {
        eprintln!("dock agent-state: {reason}");
    }
}

/// The turn boundaries Dock needs, in the vocabulary Claude Code and Codex both use.
///
/// The two agents converged on the same event names and the same handler shape, so one description
/// serves both and only the destination differs. Amp is not here: its lifecycle is a plugin system
/// with its own names (`agent.start`, `agent.end`), so it stays on the output-and-screen tier until
/// somebody writes that adapter. Copilot is not here because nothing of its interface has been
/// verified, and inventing one would produce a config that silently never fires.
fn hook_events() -> serde_json::Value {
    serde_json::json!({
        "hooks": {
            "UserPromptSubmit": [{"hooks": [{"type": "command", "command": "dock agent-state working"}]}],
            "PreToolUse": [{"hooks": [{"type": "command", "command": "dock agent-state working"}]}],
            "PermissionRequest": [{"hooks": [{"type": "command", "command": "dock agent-state blocked"}]}],
            "Notification": [{"hooks": [{"type": "command", "command": "dock agent-state blocked"}]}],
            "Stop": [{"hooks": [{"type": "command", "command": "dock agent-state done"}]}],
            "SessionEnd": [{"hooks": [{"type": "command", "command": "dock agent-state idle"}]}]
        }
    })
}

/// `dock hooks` — the hook configuration that makes agent state exact rather than inferred.
///
/// Printed rather than installed by default. This writes into files shared with every other tool
/// that reads them, and a program that edits your settings because you asked it a question is not
/// one you leave installed.
fn hooks_command(args: &[String]) -> io::Result<()> {
    let hooks = hook_events();
    let pretty = serde_json::to_string_pretty(&hooks)?;
    if args.iter().any(|argument| argument == "--check") {
        return hooks_check(&hooks);
    }
    if !args.iter().any(|argument| argument == "--install") {
        println!("{pretty}");
        eprintln!("\nClaude Code: .claude/settings.json — or `dock hooks --install` to merge it.");
        eprintln!(
            "Codex: the same events and shape, in hooks.json or an inline [hooks] table, and it \
             needs `hooks = true` under [features]. Added by hand: Dock has not verified where \
             your Codex build reads that file from, and a config written to the wrong path is one \
             that silently never fires."
        );
        eprintln!(
            "Amp: not supported — its lifecycle is a plugin system with different event names, so \
             it stays on Dock's output-and-screen detection."
        );
        return Ok(());
    }
    let path = PathBuf::from(".claude").join("settings.json");
    fs::create_dir_all(".claude")?;
    let mut settings: serde_json::Value = match fs::read_to_string(&path) {
        Ok(text) if !text.trim().is_empty() => serde_json::from_str(&text)?,
        _ => serde_json::json!({}),
    };
    // Merged per event rather than wholesale: whatever else is hooked to these events is somebody
    // else's and stays. Dock's entry is skipped where it is already present, so this is repeatable.
    let existing = settings
        .as_object_mut()
        .ok_or_else(|| io::Error::other("settings.json is not an object"))?
        .entry("hooks")
        .or_insert_with(|| serde_json::json!({}));
    let existing = existing
        .as_object_mut()
        .ok_or_else(|| io::Error::other("settings.json \"hooks\" is not an object"))?;
    for (event, entry) in hooks["hooks"].as_object().expect("hook events") {
        let slot = existing
            .entry(event.clone())
            .or_insert_with(|| serde_json::json!([]));
        let Some(list) = slot.as_array_mut() else {
            continue;
        };
        let ours = entry.as_array().expect("hook entries")[0].clone();
        if !list.contains(&ours) {
            list.push(ours);
        }
    }
    fs::write(&path, serde_json::to_string_pretty(&settings)? + "\n")?;
    println!("merged Dock's hooks into {}", path.display());
    println!("Codex uses the same events — run `dock hooks` to print them for it.");
    Ok(())
}

/// `dock handoff` — an agent filing a result a person can review.
///
/// The review queue could only be filled by hand-authoring an eleven-field JSON packet and passing
/// it to a separate binary, which meant nothing ever filled it. An agent will run one command with
/// a sentence in it; it will not assemble a schema. Everything else is already known: the run, the
/// task, the workspace and pane come from the environment Dock gave the pane, and the branch and
/// base come from the worktree it is sitting in.
///
/// The evidence is measured here rather than taken from the agent, which is the whole point of the
/// review queue: what it claims sits beside what was actually observed.
fn handoff_command(args: &[String]) -> io::Result<()> {
    let summary = args
        .iter()
        .find(|argument| !argument.starts_with("--"))
        .ok_or_else(|| io::Error::other("dock handoff \"<what you did>\" [--question \"...\"]"))?;
    let question = args
        .iter()
        .find_map(|argument| argument.strip_prefix("--question="))
        .map(str::to_owned);
    let checks = args
        .iter()
        .filter_map(|argument| argument.strip_prefix("--check="))
        .map(|check| {
            // `name:pass` / `name:fail`, so a check can be reported without another flag.
            let (name, verdict) = check.rsplit_once(':').unwrap_or((check, "pass"));
            dock::model::Check {
                name: name.to_owned(),
                passed: !verdict.eq_ignore_ascii_case("fail"),
            }
        })
        .collect();
    let variable = |name: &str| {
        std::env::var(name).map_err(|_| {
            io::Error::other(format!(
                "{name} is not set: run this inside a pane Dock launched"
            ))
        })
    };
    let worktree = std::env::current_dir()?;
    let facts = GitAdapter::new(&worktree)
        .facts("HEAD")
        .map_err(io::Error::other)?;
    let packet = dock::model::HandoffPacket {
        schema_version: 1,
        run_id: variable("DOCK_RUN")?,
        task_id: std::env::var("DOCK_TASK").unwrap_or_else(|_| "untracked".into()),
        workspace_id: variable("DOCK_WORKSPACE")?,
        pane_id: variable("DOCK_PANE")?,
        worktree: facts.worktree.display().to_string(),
        branch: facts.branch.clone(),
        base_sha: facts.base_sha.clone(),
        summary: summary.clone(),
        question,
        checks,
    };
    packet.validate().map_err(io::Error::other)?;
    let socket = PathBuf::from(variable("DOCK_SOCKET")?);
    let mut stream = UnixStream::connect(&socket)
        .map_err(|error| io::Error::other(format!("could not reach the daemon: {error}")))?;
    let mut reader = BufReader::new(
        stream
            .try_clone()
            .map_err(|error| io::Error::other(format!("could not read the daemon: {error}")))?,
    );
    for request in [
        serde_json::to_string(&Request::Hello(dock::protocol::HelloRequest {
            version: dock::protocol::PROTOCOL_VERSION,
        }))?,
        serde_json::to_string(&Request::SubmitHandoff(
            dock::protocol::SubmitHandoffRequest { packet },
        ))?,
    ] {
        stream.write_all(request.as_bytes())?;
        stream.write_all(b"\n")?;
        let mut line = String::new();
        reader.read_line(&mut line)?;
        if let Ok(Response::Error { message, .. }) = serde_json::from_str::<Response>(&line) {
            return Err(io::Error::other(message));
        }
    }
    println!("handed off for review");
    Ok(())
}

/// `dock task` — the board, from a shell or from an agent.
///
/// An agent Dock launches is handed `DOCK_BOARD` (and `DOCK_TASK` when it was dispatched onto
/// one), so it can record what it is doing without being told where anything lives. The same
/// command run by a person does the same thing, which is the point: there is one board and one way
/// to move a task across it, whether a human or an agent is doing the moving.
fn task_command(args: &[String]) -> io::Result<()> {
    let board = args
        .iter()
        .find_map(|argument| argument.strip_prefix("--board="))
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("DOCK_BOARD").map(PathBuf::from))
        .ok_or_else(|| {
            io::Error::other(
                "no board: pass --board=<dir>, or run this inside a pane Dock launched, where \
                 DOCK_BOARD is already set",
            )
        })?;
    let positional: Vec<&String> = args
        .iter()
        .filter(|argument| !argument.starts_with("--"))
        .collect();
    let verb = positional
        .first()
        .map(|verb| verb.as_str())
        .unwrap_or("list");
    match verb {
        "list" => {
            let tasks = dock::board::load(&board);
            if tasks.is_empty() {
                println!("no tasks in {}", board.display());
            }
            for task in tasks {
                println!("{}\t{}\t{}", task.id, task.status, task.title);
            }
        }
        "add" => {
            let title = positional
                .get(1)
                .ok_or_else(|| io::Error::other("dock task add \"<title>\""))?;
            let task = dock::board::create(&board, title).map_err(io::Error::other)?;
            println!("{}\t{}\t{}", task.id, task.status, task.title);
        }
        "move" => {
            let id: u64 = positional
                .get(1)
                .ok_or_else(|| io::Error::other("dock task move <id> <status>"))?
                .parse()
                .map_err(|_| io::Error::other("the task id must be a number"))?;
            let status = positional
                .get(2)
                .ok_or_else(|| io::Error::other("dock task move <id> <status>"))?;
            let task = dock::board::set_status(&board, id, status).map_err(io::Error::other)?;
            println!("{}\t{}\t{}", task.id, task.status, task.title);
        }
        "show" => {
            let id: u64 = positional
                .get(1)
                .ok_or_else(|| io::Error::other("dock task show <id>"))?
                .parse()
                .map_err(|_| io::Error::other("the task id must be a number"))?;
            let task = dock::board::load(&board)
                .into_iter()
                .find(|task| task.id == id)
                .ok_or_else(|| io::Error::other(format!("no task {id} on this board")))?;
            println!("{}", fs::read_to_string(&task.file)?);
        }
        other => {
            return Err(io::Error::other(format!(
                "unknown task command {other:?}; expected list, add, move or show"
            )));
        }
    }
    Ok(())
}

/// What `dock queue add` was asked to enqueue.
///
/// The parser records the *intent* and stops there: a card's title and body are read off the
/// board with `board::load`, which is I/O, and the whole point of splitting this out from the
/// socket call is that no usage error needs a daemon, a board or a filesystem to reproduce.
#[derive(Debug, Clone, PartialEq, Eq)]
enum QueuePrompt {
    /// Text typed on the command line, fed to the agent verbatim.
    Literal(String),
    /// A card id. The caller resolves it to the card's title plus body (§8.7) before sending.
    Task(u64),
}

/// One parsed `dock queue` invocation. Every variant is what crosses the socket, not how it was
/// typed, so the wiring never re-reads argv.
#[derive(Debug, Clone, PartialEq, Eq)]
enum QueueCommand {
    /// `None` means unfiltered: every queue the daemon holds.
    List {
        pane: Option<String>,
        workspace: Option<String>,
    },
    Add {
        pane: String,
        prompt: QueuePrompt,
    },
    Remove {
        pane: String,
        entry_id: u64,
    },
    Clear {
        pane: String,
    },
    /// Turn auto-feed on for one pane. Deliberately its own verb rather than a flag on `add`,
    /// because it is the act that lets Dock speak to an agent with nobody watching, and because
    /// the daemon can refuse it (§8.4 guard 4) where `add` cannot.
    Arm {
        pane: String,
    },
    Disarm {
        pane: String,
    },
    /// Daemon-wide kill switch, independent of which panes are armed.
    Pause,
    Resume,
}

/// Options belonging to other parts of the CLI that may legitimately appear after `queue`:
/// `--board=` is how `--task=<id>` is resolved, and the rest are read by `main` when it decides
/// which daemon to talk to. They are skipped here rather than rejected as unknown.
const QUEUE_FOREIGN_OPTIONS: [&str; 4] = ["--board=", "--socket=", "--state-dir=", "--dock-dir="];

const QUEUE_VERBS: &str = "expected list, add, remove, clear, arm, disarm, pause or resume";

/// Parse `dock queue`'s arguments and nothing else — no socket, no board, no clock.
///
/// Split out from the sending half on purpose: `task_command` parses and performs I/O in one
/// function, so not one of its usage errors has a test, and this command is not repeating that.
///
/// Flag handling follows the hand-parsed house convention: `--flag=value`, positionals are the
/// arguments that do not start with `--`. That convention cannot express a prompt beginning with
/// `--`, so a bare `--` ends the options and everything after it is a positional. Without that,
/// `dock queue add p1 "--task=7 is not a flag here"` would quietly enqueue somebody's card
/// instead of their sentence; with it, that spelling is refused with the escape named, and
/// `dock queue add p1 -- "--task=7 is not a flag here"` says what was meant. Unknown options are
/// refused rather than ignored, for the same reason: silently dropping an argument that looks
/// like a flag is how a prompt goes missing.
fn parse_queue_command(args: &[String]) -> Result<QueueCommand, String> {
    let (head, tail) = match args.iter().position(|argument| argument == "--") {
        Some(index) => (&args[..index], &args[index + 1..]),
        None => (args, &args[args.len()..]),
    };
    let mut pane_flag: Option<&str> = None;
    let mut workspace_flag: Option<&str> = None;
    let mut task_flag: Option<&str> = None;
    let mut positional: Vec<&str> = Vec::new();
    for argument in head {
        if !argument.starts_with("--") {
            positional.push(argument);
            continue;
        }
        if argument == "--all" || argument.starts_with("--all=") {
            // Refused rather than ignored: §9 has no --all, and a person who typed one believes
            // they armed everything.
            return Err(
                "there is no --all: arm and disarm are per pane, so four agents is four commands"
                    .to_owned(),
            );
        }
        if let Some(value) = argument.strip_prefix("--pane=") {
            pane_flag = Some(value);
        } else if let Some(value) = argument.strip_prefix("--workspace=") {
            workspace_flag = Some(value);
        } else if let Some(value) = argument.strip_prefix("--task=") {
            task_flag = Some(value);
        } else if !QUEUE_FOREIGN_OPTIONS
            .iter()
            .any(|known| argument.starts_with(known))
        {
            return Err(format!(
                "unknown queue option {argument:?}; expected --pane=, --workspace= or --task= \
                 (a prompt that starts with -- goes after a bare --)"
            ));
        }
    }
    positional.extend(tail.iter().map(String::as_str));

    // Same default as `dock task`: the verb that only reads is the one you get for typing nothing.
    let verb = positional.first().copied().unwrap_or("list");
    let rest = &positional[positional.len().min(1)..];
    if !matches!(
        verb,
        "list" | "add" | "remove" | "clear" | "arm" | "disarm" | "pause" | "resume"
    ) {
        return Err(format!("unknown queue command {verb:?}; {QUEUE_VERBS}"));
    }
    if verb != "list" && (pane_flag.is_some() || workspace_flag.is_some()) {
        return Err(match verb {
            "pause" | "resume" => format!(
                "dock queue {verb} is daemon-wide and takes no pane; stop one pane with \
                 dock queue disarm <pane>"
            ),
            _ => format!("dock queue {verb} takes its pane positionally: dock queue {verb} <pane>"),
        });
    }
    if verb != "add" && task_flag.is_some() {
        return Err(format!(
            "--task=<id> is only for dock queue add; dock queue {verb} works on a pane"
        ));
    }
    match verb {
        "list" => {
            if let Some(extra) = rest.first() {
                return Err(format!(
                    "dock queue list filters with --pane=<id> and --workspace=<id>, so {extra:?} \
                     would have been silently ignored"
                ));
            }
            Ok(QueueCommand::List {
                pane: queue_filter("--pane", pane_flag)?,
                workspace: queue_filter("--workspace", workspace_flag)?,
            })
        }
        "add" => {
            let usage = "dock queue add <pane> \"<prompt>\", or dock queue add <pane> --task=<id>";
            let pane = queue_pane_id(rest.first().copied(), usage)?;
            if rest.len() > 2 {
                return Err(format!(
                    "{usage} — a prompt is one argument, so quote it; got {} instead",
                    rest.len() - 1
                ));
            }
            let prompt = match (rest.get(1), task_flag) {
                (Some(_), Some(_)) => {
                    return Err(
                        "dock queue add takes either a prompt or --task=<id>, not both".to_owned(),
                    );
                }
                (None, None) => return Err(usage.to_owned()),
                (Some(text), None) => {
                    if text.trim().is_empty() {
                        return Err(
                            "that prompt is empty; an agent fed a bare newline learns nothing"
                                .to_owned(),
                        );
                    }
                    QueuePrompt::Literal((*text).to_owned())
                }
                (None, Some(id)) => QueuePrompt::Task(id.parse().map_err(|_| {
                    "the task id must be a number; a prompt that starts with -- goes after a \
                     bare --"
                        .to_owned()
                })?),
            };
            Ok(QueueCommand::Add { pane, prompt })
        }
        "remove" => {
            let usage = "dock queue remove <pane> <entry-id>";
            let pane = queue_pane_id(rest.first().copied(), usage)?;
            let entry_id: u64 = rest
                .get(1)
                .ok_or_else(|| usage.to_owned())?
                .parse()
                .map_err(|_| {
                    "the entry id must be a number; dock queue list prints it".to_owned()
                })?;
            if rest.len() > 2 {
                return Err(format!("{usage} removes one entry at a time"));
            }
            Ok(QueueCommand::Remove { pane, entry_id })
        }
        "clear" => Ok(QueueCommand::Clear {
            pane: queue_one_pane("clear", rest)?,
        }),
        "arm" => Ok(QueueCommand::Arm {
            pane: queue_one_pane("arm", rest)?,
        }),
        "disarm" => Ok(QueueCommand::Disarm {
            pane: queue_one_pane("disarm", rest)?,
        }),
        _ => {
            if let Some(extra) = rest.first() {
                return Err(format!(
                    "dock queue {verb} is daemon-wide and takes no pane; {extra:?} would have \
                     suggested otherwise — stop one pane with dock queue disarm <pane>"
                ));
            }
            Ok(if verb == "pause" {
                QueueCommand::Pause
            } else {
                QueueCommand::Resume
            })
        }
    }
}

/// A verb whose whole argument list is one pane.
fn queue_one_pane(verb: &str, rest: &[&str]) -> Result<String, String> {
    let usage = format!("dock queue {verb} <pane>");
    let pane = queue_pane_id(rest.first().copied(), &usage)?;
    if rest.len() > 1 {
        return Err(format!(
            "{usage} takes one pane; quote the id if it has a space in it"
        ));
    }
    Ok(pane)
}

/// An absent pane and a blank one are different mistakes and get different sentences; both are
/// caught here so no verb can send an empty pane id to the daemon and be told "no such pane".
fn queue_pane_id(pane: Option<&str>, usage: &str) -> Result<String, String> {
    let pane = pane.ok_or_else(|| usage.to_owned())?;
    if pane.trim().is_empty() {
        return Err(format!("a pane id cannot be empty: {usage}"));
    }
    Ok(pane.to_owned())
}

/// `--pane=` with nothing after it is a filter that matches nothing; refusing beats listing
/// everything as though no filter had been asked for.
fn queue_filter(flag: &str, value: Option<&str>) -> Result<Option<String>, String> {
    match value {
        Some(value) if value.trim().is_empty() => Err(format!(
            "{flag}= needs an id after it, or leave it off to list every queue"
        )),
        other => Ok(other.map(str::to_owned)),
    }
}

/// PROVISIONAL. Parsing is done and tested; the socket call is a separate change, so this
/// reports what it understood and exits non-zero rather than pretending a prompt was queued.
fn queue_command(args: &[String]) -> io::Result<()> {
    let command = parse_queue_command(args).map_err(io::Error::other)?;
    // The first arm of `run_noninteractive_legacy` that talks to the daemon. Every other one
    // reads or writes files; a queue lives in the daemon because the daemon is what feeds it, so
    // this opens a client the way `dock-dispatch` does rather than touching the state directory.
    let runtime_directory = std::env::current_dir()?;
    let (default_socket, _) =
        paths::runtime_paths_for(&runtime_directory).map_err(io::Error::other)?;
    let socket = args
        .iter()
        .find_map(|argument| argument.strip_prefix("--socket=").map(PathBuf::from))
        .unwrap_or(default_socket);
    let mut client = Client::connect(&socket).map_err(|error| {
        io::Error::other(format!(
            "{error}; a queue lives in the daemon, so Dock has to be running to hold one"
        ))
    })?;

    let request = match command {
        QueueCommand::Pause => Request::Queue(QueueRequest::SetPaused { paused: true }),
        QueueCommand::Resume => Request::Queue(QueueRequest::SetPaused { paused: false }),
        QueueCommand::List { pane, workspace } => {
            return print_queues(&mut client, pane.as_deref(), workspace.as_deref());
        }
        QueueCommand::Add { pane, prompt } => {
            // Resolved here rather than in the daemon: the board is a directory this client was
            // given and the daemon has never been told about, and only the resulting text needs
            // to cross the socket.
            let (label, prompt) = match prompt {
                QueuePrompt::Literal(text) => (queue_label(&text), text),
                QueuePrompt::Task(id) => {
                    let board = queue_board(args)?;
                    let task = dock::board::load(&board)
                        .into_iter()
                        .find(|task| task.id == id)
                        .ok_or_else(|| io::Error::other(format!("no task {id} on this board")))?;
                    (
                        format!("#{id} {}", task.title),
                        dispatch_prompt(id, &task.title, &task.body),
                    )
                }
            };
            let (workspace_id, pane_id) = resolve_pane(&mut client, &pane)?;
            Request::Queue(QueueRequest::Add {
                workspace_id,
                pane_id,
                prompt,
                label,
            })
        }
        QueueCommand::Remove { pane, entry_id } => {
            let (workspace_id, pane_id) = resolve_pane(&mut client, &pane)?;
            Request::Queue(QueueRequest::Remove {
                workspace_id,
                pane_id,
                entry_id,
            })
        }
        QueueCommand::Clear { pane } => {
            let (workspace_id, pane_id) = resolve_pane(&mut client, &pane)?;
            Request::Queue(QueueRequest::Clear {
                workspace_id,
                pane_id,
            })
        }
        QueueCommand::Arm { pane } => {
            let (workspace_id, pane_id) = resolve_pane(&mut client, &pane)?;
            Request::Queue(QueueRequest::SetAuto {
                workspace_id,
                pane_id,
                enabled: true,
            })
        }
        QueueCommand::Disarm { pane } => {
            let (workspace_id, pane_id) = resolve_pane(&mut client, &pane)?;
            Request::Queue(QueueRequest::SetAuto {
                workspace_id,
                pane_id,
                enabled: false,
            })
        }
    };
    match client.request(&request).map_err(io::Error::other)? {
        Response::Queues { queues, paused } => {
            render_queues(&queues, paused, None, None);
            Ok(())
        }
        Response::Error { message, .. } => Err(io::Error::other(message)),
        other => Err(io::Error::other(format!("unexpected answer: {other:?}"))),
    }
}

/// The board a `--task=` is read from, on the same terms `dock task` uses.
fn queue_board(args: &[String]) -> io::Result<PathBuf> {
    args.iter()
        .find_map(|argument| argument.strip_prefix("--board="))
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("DOCK_BOARD").map(PathBuf::from))
        .ok_or_else(|| {
            io::Error::other(
                "no board: pass --board=<dir>, or run this inside a pane Dock launched, where \
                 DOCK_BOARD is already set",
            )
        })
}

/// A short name for a literal prompt, so a listing says something other than its first line.
fn queue_label(prompt: &str) -> String {
    let first = prompt.lines().next().unwrap_or("").trim();
    ellipsised_label(first)
}

fn ellipsised_label(text: &str) -> String {
    const WIDTH: usize = 48;
    if text.chars().count() <= WIDTH {
        return text.to_owned();
    }
    let kept: String = text.chars().take(WIDTH.saturating_sub(1)).collect();
    format!("{kept}\u{2026}")
}

/// Which workspace holds this pane.
///
/// The command names one pane because that is what a person knows; the daemon keys queues by the
/// pair, because a pane id is only unique inside its workspace. Asked of the layout rather than
/// guessed, and an id in two workspaces is refused by name rather than resolved to whichever came
/// first — feeding the wrong pane is the whole class of mistake this feature has to avoid.
fn resolve_pane(client: &mut Client, pane: &str) -> io::Result<(String, String)> {
    let layout = match client
        .request(&Request::Workspace(WorkspaceRequest::Inspect))
        .map_err(io::Error::other)?
    {
        Response::Layout { layout } => layout,
        Response::Error { message, .. } => return Err(io::Error::other(message)),
        other => return Err(io::Error::other(format!("unexpected answer: {other:?}"))),
    };
    let found: Vec<&str> = layout
        .workspaces
        .iter()
        .filter(|workspace| workspace.panes.contains_key(pane))
        .map(|workspace| workspace.workspace_id.as_str())
        .collect();
    match found.as_slice() {
        [one] => Ok(((*one).to_owned(), pane.to_owned())),
        [] => Err(io::Error::other(format!(
            "no pane {pane:?}; `dock queue list` names the panes that have queues"
        ))),
        many => Err(io::Error::other(format!(
            "pane {pane:?} is in {} workspaces ({}); name one with --workspace=",
            many.len(),
            many.join(", ")
        ))),
    }
}

fn print_queues(
    client: &mut Client,
    pane: Option<&str>,
    workspace: Option<&str>,
) -> io::Result<()> {
    match client
        .request(&Request::Queue(QueueRequest::Inspect))
        .map_err(io::Error::other)?
    {
        Response::Queues { queues, paused } => {
            render_queues(&queues, paused, pane, workspace);
            Ok(())
        }
        Response::Error { message, .. } => Err(io::Error::other(message)),
        other => Err(io::Error::other(format!("unexpected answer: {other:?}"))),
    }
}

/// Prints the queues, narrowed to what was asked for.
fn render_queues(
    queues: &[dock::protocol::PaneQueueSnapshot],
    paused: bool,
    pane: Option<&str>,
    workspace: Option<&str>,
) {
    if paused {
        println!("every queue is paused — `dock queue resume` starts them again");
    }
    let shown: Vec<_> = queues
        .iter()
        .filter(|queue| pane.is_none_or(|wanted| queue.pane_id == wanted))
        .filter(|queue| workspace.is_none_or(|wanted| queue.workspace_id == wanted))
        .collect();
    if shown.is_empty() {
        println!("no queues");
        return;
    }
    for queue in shown {
        let armed = if queue.auto_feed {
            "armed"
        } else {
            "not armed"
        };
        let count = queue.entries.len();
        let entries = if count == 1 { "entry" } else { "entries" };
        println!(
            "{}/{}  {count} {entries}  {armed}",
            queue.workspace_id, queue.pane_id
        );
        if let Some(reason) = &queue.holding_because {
            println!("    holding: {reason}");
        }
        for entry in &queue.entries {
            println!("    {:>4}  {}", entry.entry_id, entry.label);
        }
    }
}

#[cfg(test)]
mod queue_parse_tests {
    use super::{QueueCommand, QueuePrompt, parse_queue_command};

    fn parse(args: &[&str]) -> Result<QueueCommand, String> {
        let owned: Vec<String> = args.iter().map(|arg| (*arg).to_owned()).collect();
        parse_queue_command(&owned)
    }

    fn refusal(args: &[&str]) -> String {
        parse(args).expect_err("this spelling has to be refused")
    }

    #[test]
    fn every_verb_the_command_offers_parses_into_the_thing_it_names() {
        // The whole surface in one place, because a verb that parses as a neighbouring verb is
        // the failure this enum exists to make impossible: `disarm` read as `arm` would arm a
        // pane somebody was explicitly turning off.
        assert_eq!(
            parse(&["add", "p1", "look at the retry path"]),
            Ok(QueueCommand::Add {
                pane: "p1".to_owned(),
                prompt: QueuePrompt::Literal("look at the retry path".to_owned()),
            })
        );
        assert_eq!(
            parse(&["remove", "p1", "3"]),
            Ok(QueueCommand::Remove {
                pane: "p1".to_owned(),
                entry_id: 3
            })
        );
        assert_eq!(
            parse(&["clear", "p1"]),
            Ok(QueueCommand::Clear {
                pane: "p1".to_owned()
            })
        );
        assert_eq!(
            parse(&["arm", "p1"]),
            Ok(QueueCommand::Arm {
                pane: "p1".to_owned()
            })
        );
        assert_eq!(
            parse(&["disarm", "p1"]),
            Ok(QueueCommand::Disarm {
                pane: "p1".to_owned()
            })
        );
        assert_eq!(parse(&["pause"]), Ok(QueueCommand::Pause));
        assert_eq!(parse(&["resume"]), Ok(QueueCommand::Resume));
    }

    #[test]
    fn a_bare_queue_lists_every_queue_rather_than_complaining() {
        // `dock task` with no verb lists, and the verb you get for typing nothing should be the
        // one that only reads.
        assert_eq!(
            parse(&[]),
            Ok(QueueCommand::List {
                pane: None,
                workspace: None
            })
        );
    }

    #[test]
    fn a_list_narrows_to_the_pane_or_workspace_it_was_given_and_to_everything_when_it_was_not() {
        assert_eq!(
            parse(&["list", "--pane=p1"]),
            Ok(QueueCommand::List {
                pane: Some("p1".to_owned()),
                workspace: None
            })
        );
        assert_eq!(
            parse(&["list", "--workspace=w1", "--pane=p1"]),
            Ok(QueueCommand::List {
                pane: Some("p1".to_owned()),
                workspace: Some("w1".to_owned())
            })
        );
        assert_eq!(
            parse(&["list"]),
            Ok(QueueCommand::List {
                pane: None,
                workspace: None
            })
        );
    }

    #[test]
    fn a_card_is_queued_by_id_and_left_for_the_caller_to_read_off_the_board() {
        // The parser must not try to resolve the card: title and body come from `board::load`
        // against --board= / $DOCK_BOARD, which is I/O, and dragging it in here is exactly what
        // makes `task_command` untestable.
        assert_eq!(
            parse(&["add", "p1", "--task=7"]),
            Ok(QueueCommand::Add {
                pane: "p1".to_owned(),
                prompt: QueuePrompt::Task(7),
            })
        );
    }

    #[test]
    fn a_verb_the_queue_does_not_have_is_named_back_with_the_ones_it_does() {
        // Including `dispatch`, which §9 cut deliberately and which is the first thing anybody
        // will try, so the message has to list what exists rather than only say no.
        let message = refusal(&["dispatch", "p1"]);
        assert!(message.contains("\"dispatch\""), "{message}");
        assert!(message.contains("arm"), "{message}");
        assert!(message.contains("pause"), "{message}");
    }

    #[test]
    fn a_verb_that_needs_a_pane_and_was_given_none_prints_the_shape_of_the_command() {
        // Every one of these, because the pane argument is positional and a person who forgets
        // it gets the same silence from all five otherwise.
        assert_eq!(refusal(&["clear"]), "dock queue clear <pane>");
        assert_eq!(refusal(&["arm"]), "dock queue arm <pane>");
        assert_eq!(refusal(&["disarm"]), "dock queue disarm <pane>");
        assert_eq!(refusal(&["remove"]), "dock queue remove <pane> <entry-id>");
        assert!(refusal(&["add"]).starts_with("dock queue add <pane>"));
    }

    #[test]
    fn dock_queue_add_without_a_prompt_explains_the_shape_of_the_command() {
        // A pane and nothing else is the commonest mistake — the prompt was left off, or the
        // shell ate it — and the answer has to name both spellings, since --task= is the one
        // that is not obvious from the failure.
        let message = refusal(&["add", "p1"]);
        assert!(message.contains("\"<prompt>\""), "{message}");
        assert!(message.contains("--task=<id>"), "{message}");
    }

    #[test]
    fn a_prompt_and_a_card_id_together_are_refused_rather_than_one_quietly_winning() {
        // Whichever won, half the invocations would feed an agent something the person did not
        // type, and they would not find out until they read the pane.
        let message = refusal(&["add", "p1", "look here", "--task=7"]);
        assert!(message.contains("not both"), "{message}");
    }

    #[test]
    fn there_is_no_way_to_arm_every_pane_at_once_and_asking_for_one_says_so() {
        // §9: arming is the act that lets Dock speak with nobody watching, and it is per pane.
        // Ignoring --all would leave somebody believing they had armed four agents.
        let message = refusal(&["arm", "p1", "--all"]);
        assert!(message.contains("no --all"), "{message}");
        assert!(message.contains("per pane"), "{message}");
        // Anywhere it appears, not only on arm, since --all means the same wrong thing on each.
        assert!(refusal(&["disarm", "--all"]).contains("no --all"));
        assert!(refusal(&["clear", "--all"]).contains("no --all"));
    }

    #[test]
    fn an_entry_id_that_is_not_a_number_is_refused_here_rather_than_by_the_daemon() {
        // And the message says where entry ids come from, because they are the daemon's
        // invention and there is nowhere else to learn them.
        let message = refusal(&["remove", "p1", "second"]);
        assert!(message.contains("must be a number"), "{message}");
        assert!(message.contains("dock queue list"), "{message}");
        assert_eq!(
            refusal(&["remove", "p1"]),
            "dock queue remove <pane> <entry-id>"
        );
    }

    #[test]
    fn a_card_id_that_is_not_a_number_is_refused_and_names_the_way_to_mean_it_literally() {
        // This is where `dock queue add p1 "--task=7 is not a flag here"` lands: the house
        // convention has already claimed the argument as a flag, so the only honest outcome is
        // a refusal that says how to spell the prompt instead.
        let message = refusal(&["add", "p1", "--task=7 is not a flag here"]);
        assert!(message.contains("must be a number"), "{message}");
        assert!(message.contains("bare --"), "{message}");
        assert!(refusal(&["add", "p1", "--task=seven"]).contains("must be a number"));
    }

    #[test]
    fn pause_is_daemon_wide_and_a_pane_given_to_it_is_a_misunderstanding_worth_naming() {
        // Accepting and ignoring the pane would leave every other agent stopped too, which is
        // the opposite of what the person meant; the message points at the per-pane verb.
        let message = refusal(&["pause", "p1"]);
        assert!(message.contains("daemon-wide"), "{message}");
        assert!(message.contains("dock queue disarm <pane>"), "{message}");
        assert!(refusal(&["resume", "p1"]).contains("daemon-wide"));
        assert!(refusal(&["pause", "--pane=p1"]).contains("daemon-wide"));
    }

    #[test]
    fn a_prompt_keeps_its_spaces_its_newlines_and_its_punctuation_exactly_as_typed() {
        // The prompt is fed to an agent verbatim (§8.7), so anything this function does to it —
        // trimming, splitting, collapsing whitespace — is damage nobody asked for.
        let prompt = "first line\n\n  second, indented — with an em dash\n";
        assert_eq!(
            parse(&["add", "p1", prompt]),
            Ok(QueueCommand::Add {
                pane: "p1".to_owned(),
                prompt: QueuePrompt::Literal(prompt.to_owned()),
            })
        );
    }

    #[test]
    fn a_prompt_that_looks_like_a_flag_is_a_prompt_when_it_is_given_after_a_bare_dash_dash() {
        // The escape hatch the refusal above advertises has to actually work, and it has to
        // work for text that would otherwise be read as any flag, not only as --task=.
        assert_eq!(
            parse(&["add", "p1", "--", "--task=7 is not a flag here"]),
            Ok(QueueCommand::Add {
                pane: "p1".to_owned(),
                prompt: QueuePrompt::Literal("--task=7 is not a flag here".to_owned()),
            })
        );
        assert_eq!(
            parse(&["add", "p1", "--", "--all of the above"]),
            Ok(QueueCommand::Add {
                pane: "p1".to_owned(),
                prompt: QueuePrompt::Literal("--all of the above".to_owned()),
            })
        );
    }

    #[test]
    fn an_empty_prompt_is_refused_rather_than_feeding_an_agent_a_bare_newline() {
        // `dock queue add p1 "$PROMPT"` with an unset variable is the way this arrives, and the
        // queue entry it would make is one an agent cannot act on and a person cannot read.
        assert!(refusal(&["add", "p1", ""]).contains("empty"));
        assert!(refusal(&["add", "p1", "   \n "]).contains("empty"));
    }

    #[test]
    fn an_unquoted_prompt_that_arrived_as_several_words_is_refused_rather_than_joined() {
        // Joining would work often enough to be trusted and would silently normalise the
        // whitespace the shell had already eaten; naming the quoting is the honest answer.
        let message = refusal(&["add", "p1", "look", "at", "the", "retry", "path"]);
        assert!(message.contains("quote it"), "{message}");
    }

    #[test]
    fn a_filter_flag_on_a_verb_that_takes_its_pane_positionally_is_refused() {
        // --pane= belongs to `list`. On `arm` it would parse as a pane-less arm, so accepting
        // it as a synonym is one more way to arm the wrong thing.
        let message = refusal(&["arm", "--pane=p1"]);
        assert!(message.contains("positionally"), "{message}");
        assert!(message.contains("dock queue arm <pane>"), "{message}");
        assert!(refusal(&["clear", "--workspace=w1"]).contains("positionally"));
    }

    #[test]
    fn a_list_does_not_take_its_pane_positionally_either() {
        // The mirror of the rule above: `dock queue list p1` would otherwise list every queue
        // on the daemon and look like it had filtered.
        let message = refusal(&["list", "p1"]);
        assert!(message.contains("--pane=<id>"), "{message}");
        assert!(message.contains("silently ignored"), "{message}");
    }

    #[test]
    fn a_card_id_on_a_verb_that_cannot_use_one_is_refused() {
        // `dock queue arm --task=7` is somebody expecting arm to enqueue as well as arm, which
        // §9 split apart on purpose.
        let message = refusal(&["arm", "p1", "--task=7"]);
        assert!(message.contains("only for dock queue add"), "{message}");
    }

    #[test]
    fn an_option_the_queue_does_not_know_is_named_back_rather_than_dropped() {
        // The convention filters positionals by a leading --, so an unknown option vanishes
        // silently; for a command that carries a prompt, a vanished argument is a lost prompt.
        let message = refusal(&["add", "p1", "--tasks=7"]);
        assert!(message.contains("\"--tasks=7\""), "{message}");
        assert!(message.contains("bare --"), "{message}");
    }

    #[test]
    fn the_options_the_rest_of_the_cli_owns_are_not_mistaken_for_queue_options() {
        // --board= is how --task= gets resolved and --socket= chooses the daemon, so both reach
        // this parser on perfectly ordinary invocations and must pass straight through.
        assert_eq!(
            parse(&["--board=/tmp/board", "add", "p1", "--task=7"]),
            Ok(QueueCommand::Add {
                pane: "p1".to_owned(),
                prompt: QueuePrompt::Task(7),
            })
        );
        assert_eq!(
            parse(&["--socket=/tmp/dock.sock", "--state-dir=/tmp/state", "pause"]),
            Ok(QueueCommand::Pause)
        );
    }

    #[test]
    fn an_empty_pane_id_is_refused_wherever_a_pane_is_required() {
        // `dock queue arm "$PANE"` with nothing in the variable, again. The daemon would answer
        // "no such pane", which describes the pane rather than the mistake.
        assert!(refusal(&["arm", ""]).contains("cannot be empty"));
        assert!(refusal(&["add", "  ", "look here"]).contains("cannot be empty"));
        assert!(refusal(&["list", "--pane="]).contains("needs an id after it"));
    }

    #[test]
    fn a_verb_that_wants_one_pane_is_not_quietly_given_two() {
        // Two panes means either an unquoted id or a person expecting one command to arm both,
        // and both deserve to hear about it rather than have the second argument dropped.
        assert!(refusal(&["arm", "p1", "p2"]).contains("takes one pane"));
        assert!(refusal(&["clear", "p1", "p2"]).contains("takes one pane"));
        assert!(refusal(&["remove", "p1", "3", "4"]).contains("one entry at a time"));
    }
}

fn repository_catalog(directory: &Path) -> (String, Vec<dock::dashboard::RepositoryLaunchOption>) {
    let marker = directory
        .ancestors()
        .find(|candidate| candidate.join(".git").exists());
    let Some(marker) = marker else {
        return (String::new(), vec![]);
    };
    let output = Command::new("git")
        .args([
            "-C",
            &marker.to_string_lossy(),
            "rev-parse",
            "--show-toplevel",
        ])
        .output();
    let Ok(output) = output else {
        return (String::new(), vec![]);
    };
    if !output.status.success() {
        return (String::new(), vec![]);
    }
    let root = String::from_utf8_lossy(&output.stdout).trim().to_owned();
    let tasks = Path::new(&root).join("kanban/tasks");
    let task_ref = fs::read_dir(tasks)
        .ok()
        .into_iter()
        .flatten()
        .filter_map(Result::ok)
        .filter(|entry| entry.path().extension().is_some_and(|ext| ext == "md"))
        .filter_map(|entry| fs::read_to_string(entry.path()).ok())
        .flat_map(|text| text.lines().map(str::to_owned).collect::<Vec<_>>())
        .find_map(|line| {
            line.strip_prefix("id:")
                .map(|id| id.trim().trim_matches('\'').to_owned())
        });
    let Some(task_ref) = task_ref else {
        return (root, vec![]);
    };
    let worktrees = Command::new("git")
        .args(["-C", &root, "worktree", "list", "--porcelain"])
        .output();
    let launches = worktrees
        .ok()
        .filter(|o| o.status.success())
        .into_iter()
        .flat_map(|o| {
            String::from_utf8_lossy(&o.stdout)
                .lines()
                .filter_map(|line| line.strip_prefix("worktree ").map(str::to_owned))
                .collect::<Vec<_>>()
        })
        .map(|worktree| dock::dashboard::RepositoryLaunchOption {
            task_ref: task_ref.clone(),
            worktree,
        })
        .collect();
    (root, launches)
}

fn test_events() -> Result<VecDeque<Event>, String> {
    let Some(value) = std::env::var_os("DOCK_TEST_KEY_EVENTS") else {
        return Ok(VecDeque::new());
    };
    if !cfg!(debug_assertions) {
        return Err("DOCK_TEST_KEY_EVENTS is available only in debug/test builds".into());
    }
    let value = value
        .into_string()
        .map_err(|_| "DOCK_TEST_KEY_EVENTS must be UTF-8".to_string())?;
    parse_test_events(&value)
}

fn parse_test_events(value: &str) -> Result<VecDeque<Event>, String> {
    let mut events = VecDeque::new();
    let mut input = value;
    while !input.is_empty() {
        let (code, modifiers, consumed) = if input.starts_with('<') {
            let end = input.find('>').ok_or_else(|| {
                "DOCK_TEST_KEY_EVENTS contains an unterminated named key".to_string()
            })?;
            let name = &input[1..end];
            let (code, modifiers) = match name {
                "Enter" => (KeyCode::Enter, crossterm::event::KeyModifiers::NONE),
                "Tab" => (KeyCode::Tab, crossterm::event::KeyModifiers::NONE),
                "Esc" => (KeyCode::Esc, crossterm::event::KeyModifiers::NONE),
                "Up" => (KeyCode::Up, crossterm::event::KeyModifiers::NONE),
                "Down" => (KeyCode::Down, crossterm::event::KeyModifiers::NONE),
                "Left" => (KeyCode::Left, crossterm::event::KeyModifiers::NONE),
                "Right" => (KeyCode::Right, crossterm::event::KeyModifiers::NONE),
                "Backspace" => (KeyCode::Backspace, crossterm::event::KeyModifiers::NONE),
                // `<C-b>` sends the Dock prefix (Ctrl+B) as a real Ctrl-modified key event,
                // the same shape crossterm hands the dashboard from a live terminal — plain
                // `b` in DOCK_TEST_KEY_EVENTS cannot express a held modifier.
                _ if name.len() == 3 && name.starts_with("C-") => (
                    KeyCode::Char(name[2..].chars().next().expect("one char after C-")),
                    crossterm::event::KeyModifiers::CONTROL,
                ),
                _ => {
                    return Err(format!(
                        "DOCK_TEST_KEY_EVENTS contains unknown named key <{name}>"
                    ));
                }
            };
            (code, modifiers, end + 1)
        } else {
            let character = input.chars().next().expect("non-empty event input");
            if character.is_control() {
                return Err(
                    "DOCK_TEST_KEY_EVENTS accepts printable characters and named keys only".into(),
                );
            }
            (
                KeyCode::Char(character),
                crossterm::event::KeyModifiers::NONE,
                character.len_utf8(),
            )
        };
        events.push_back(Event::Key(crossterm::event::KeyEvent::new(code, modifiers)));
        input = &input[consumed..];
    }
    Ok(events)
}

/// How many events one drain takes before handing control back to the loop.
///
/// A bound rather than "everything pending" so a terminal producing reports faster than Dock
/// consumes them cannot hold the loop away from painting indefinitely. 512 is far above any
/// real burst — a pointer swept across a full-screen dashboard produces a few hundred at most.
const MAX_COALESCED_EVENTS: usize = 512;

/// Whether an event is pure pointer motion, and so safe to throw away when a newer one of the
/// same kind is already in hand.
fn motion_kind(event: &Event) -> Option<MouseEventKind> {
    match event {
        Event::Mouse(mouse) => match mouse.kind {
            kind @ (MouseEventKind::Moved | MouseEventKind::Drag(_)) => Some(kind),
            _ => None,
        },
        _ => None,
    }
}

/// Collapses each run of consecutive same-kind pointer motion down to its most recent report.
///
/// Motion is the one event class where only the latest matters: the pointer's position is a
/// state, not a message, and every intermediate cell it crossed is already implied by where it
/// ended up. Nothing else may be touched, and this is deliberately conservative about it:
///
/// * Keys, pastes and resizes are messages — each one means something the next cannot replace —
///   so they pass through untouched and in order.
/// * Presses and releases bracket a gesture. Dropping either would leave a drag that never
///   started or never ended, and reordering one past a motion event would anchor a selection on
///   the wrong cell.
/// * Only *consecutive* motion collapses, and only when the two reports are the same kind. A
///   `Moved` and a `Drag` are different gestures, and collapsing a `Drag` into a following
///   `Moved` would throw away the last position of a selection in progress.
fn coalesce_motion(events: Vec<Event>) -> Vec<Event> {
    let mut kept: Vec<Event> = Vec::with_capacity(events.len());
    for event in events {
        if let Some(kind) = motion_kind(&event)
            && kept.last().and_then(motion_kind) == Some(kind)
        {
            kept.pop();
        }
        kept.push(event);
    }
    kept
}

fn request_layout(client: &mut Client) -> Result<dock::layout::LayoutSnapshot, String> {
    match client.request(&Request::Workspace(WorkspaceRequest::Inspect))? {
        Response::Layout { layout } => Ok(layout),
        Response::Error { message, .. } => Err(message),
        response => Err(format!("unexpected layout response: {response:?}")),
    }
}

fn refresh(client: &mut Client, dashboard: &mut Dashboard) -> Result<(), String> {
    dashboard.layout = request_layout(client)?;
    // Through `set_runs` rather than the field, so the agent roster sheds entries for runs that
    // no longer exist instead of accumulating them for the session's lifetime.
    dashboard.set_runs(
        match client.request(&Request::Inspect(InspectRequest { run_id: None }))? {
            Response::Snapshots { snapshots } => snapshots,
            Response::Error { message, .. } => return Err(message),
            response => return Err(format!("unexpected runtime response: {response:?}")),
        },
    );
    // On the same refresh as the layout and the runs, not on a timer of its own. Queue depth is
    // the third thing the runs lane is assembled from, and `Event::QueueChanged` already marks
    // the client dirty — so a `dock queue add` from another terminal lands in the open board pane
    // through exactly the path an agent state change does, with no keypress and no second poll.
    match client.request(&Request::Queue(QueueRequest::Inspect))? {
        Response::Queues { queues, paused } => dashboard.set_queues(queues, paused),
        Response::Error { message, .. } => return Err(message),
        response => return Err(format!("unexpected queue response: {response:?}")),
    }
    if dashboard.workspace_index >= dashboard.layout.workspaces.len() {
        dashboard.workspace_index = dashboard.layout.workspaces.len().saturating_sub(1);
    }
    Ok(())
}

#[cfg(test)]
mod terminal_tests {
    use super::*;
    use nix::pty::openpty;

    /// The dominant cost of pointer input was never the work Dock does with a motion event —
    /// it does none — but the complete repaint each one triggered on its way to being ignored.
    /// A pointer crossing a pane delivers one report per cell, so this is the difference
    /// between a burst costing one frame and costing forty.
    #[test]
    fn a_burst_of_pointer_motion_collapses_to_its_most_recent_report() {
        let moved = |column: u16| {
            Event::Mouse(crossterm::event::MouseEvent {
                kind: MouseEventKind::Moved,
                column,
                row: 4,
                modifiers: crossterm::event::KeyModifiers::NONE,
            })
        };
        let dragged = |column: u16| {
            Event::Mouse(crossterm::event::MouseEvent {
                kind: MouseEventKind::Drag(crossterm::event::MouseButton::Left),
                column,
                row: 4,
                modifiers: crossterm::event::KeyModifiers::NONE,
            })
        };

        assert_eq!(
            coalesce_motion(vec![moved(1), moved(2), moved(3)]),
            vec![moved(3)],
            "only where the pointer ended up matters"
        );
        assert_eq!(
            coalesce_motion(vec![dragged(1), dragged(2), dragged(9)]),
            vec![dragged(9)],
            "a drag selects to the newest cell, so the ones it swept through are implied"
        );
        // A move and a drag are different gestures. Fusing them would let a trailing `Moved`
        // throw away the last position of a selection in progress.
        assert_eq!(
            coalesce_motion(vec![dragged(1), dragged(2), moved(7), moved(8)]),
            vec![dragged(2), moved(8)],
            "runs collapse within a kind, never across two"
        );
    }

    /// The other half of the contract, and the one that matters: motion is the *only* thing
    /// safe to throw away. A dropped key is a keystroke the user typed and never saw, and a
    /// dropped press or release is a selection that never starts or never ends.
    #[test]
    fn coalescing_motion_never_drops_or_reorders_a_key_paste_press_or_release() {
        let mouse = |kind| {
            Event::Mouse(crossterm::event::MouseEvent {
                kind,
                column: 3,
                row: 4,
                modifiers: crossterm::event::KeyModifiers::NONE,
            })
        };
        let key = |character| {
            Event::Key(crossterm::event::KeyEvent::new(
                KeyCode::Char(character),
                crossterm::event::KeyModifiers::NONE,
            ))
        };
        let batch = vec![
            key('a'),
            mouse(MouseEventKind::Down(crossterm::event::MouseButton::Left)),
            mouse(MouseEventKind::Drag(crossterm::event::MouseButton::Left)),
            mouse(MouseEventKind::Drag(crossterm::event::MouseButton::Left)),
            mouse(MouseEventKind::Up(crossterm::event::MouseButton::Left)),
            Event::Paste("pasted".into()),
            Event::Resize(80, 24),
            key('b'),
            Event::Key(crossterm::event::KeyEvent::new(
                KeyCode::Enter,
                crossterm::event::KeyModifiers::NONE,
            )),
        ];
        let kept = coalesce_motion(batch.clone());
        // Exactly one event is gone: the first of the two consecutive drags.
        assert_eq!(kept.len(), batch.len() - 1, "got {kept:?}");
        let survivors: Vec<&Event> = kept.iter().collect();
        let expected: Vec<&Event> = batch
            .iter()
            .enumerate()
            .filter(|(index, _)| *index != 2)
            .map(|(_, event)| event)
            .collect();
        assert_eq!(survivors, expected, "order and content are untouched");
    }

    /// Ordering matters more than it looks: xterm keeps one mouse mode rather than independent
    /// flags and resets it to off for any of `?1000l`/`?1002l`/`?1003l`, so a sequence that
    /// ended at `?1003l` would leave a dashboard with no mouse at all.
    #[test]
    fn narrowing_mouse_reporting_ends_by_re_asserting_button_event_tracking() {
        assert!(
            BUTTON_MOTION_ONLY.starts_with("\x1b[?1003l"),
            "any-event tracking is what costs a frame per pointer movement"
        );
        assert!(
            BUTTON_MOTION_ONLY.ends_with("\x1b[?1002h"),
            "a terminal with a single mouse mode is left with drags, not with nothing"
        );
    }

    #[test]
    fn deterministic_events_represent_real_printable_and_named_keys() {
        let events = parse_test_events("nlfix<Enter><Enter>q").unwrap();
        let codes = events
            .into_iter()
            .map(|event| match event {
                Event::Key(key) => key.code,
                _ => panic!("expected a key event"),
            })
            .collect::<Vec<_>>();
        assert_eq!(
            codes,
            vec![
                KeyCode::Char('n'),
                KeyCode::Char('l'),
                KeyCode::Char('f'),
                KeyCode::Char('i'),
                KeyCode::Char('x'),
                KeyCode::Enter,
                KeyCode::Enter,
                KeyCode::Char('q'),
            ]
        );
        assert!(parse_test_events("n<Return>").is_err());
        assert!(parse_test_events("n<Enter").is_err());
    }

    #[test]
    fn named_control_key_carries_the_control_modifier() {
        let events = parse_test_events("<C-b>q").unwrap();
        let keys = events
            .into_iter()
            .map(|event| match event {
                Event::Key(key) => key,
                _ => panic!("expected a key event"),
            })
            .collect::<Vec<_>>();
        assert_eq!(keys[0].code, KeyCode::Char('b'));
        assert_eq!(keys[0].modifiers, crossterm::event::KeyModifiers::CONTROL);
        assert_eq!(keys[1].code, KeyCode::Char('q'));
        assert_eq!(keys[1].modifiers, crossterm::event::KeyModifiers::NONE);
        assert!(parse_test_events("<C-bb>").is_err());
    }

    #[test]
    fn terminal_state_restores_all_captured_termios_fields() {
        let pty = openpty(None, None).expect("open test PTY");
        let fd = pty.slave.as_raw_fd();
        let original = TerminalState::capture(fd).expect("capture terminal state");
        let mut changed = original.termios;
        changed.c_lflag ^= libc::ECHO;
        changed.c_cc[libc::VMIN] = changed.c_cc[libc::VMIN].wrapping_add(1);

        // SAFETY: changed was captured from this PTY and only valid termios fields were changed.
        assert_eq!(unsafe { libc::tcsetattr(fd, libc::TCSANOW, &changed) }, 0);
        original.restore(false).expect("restore terminal state");
        let restored = TerminalState::capture(fd).expect("recapture terminal state");

        assert_eq!(restored.termios.c_iflag, original.termios.c_iflag);
        assert_eq!(restored.termios.c_oflag, original.termios.c_oflag);
        assert_eq!(restored.termios.c_cflag, original.termios.c_cflag);
        assert_eq!(restored.termios.c_lflag, original.termios.c_lflag);
        assert_eq!(restored.termios.c_cc, original.termios.c_cc);
        assert_eq!(unsafe { libc::cfgetispeed(&restored.termios) }, unsafe {
            libc::cfgetispeed(&original.termios)
        });
        assert_eq!(unsafe { libc::cfgetospeed(&restored.termios) }, unsafe {
            libc::cfgetospeed(&original.termios)
        });
    }
}
