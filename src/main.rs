mod kanban;

use std::{
    collections::VecDeque,
    error::Error,
    fs,
    io::{self, IsTerminal, Read},
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
        Event, KeyCode, KeyEventKind,
    },
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use dock::{
    adapter::AdapterSelection,
    client::Client,
    client::{EventStream, StreamPoll},
    dashboard::{Dashboard, TaskDispatch, UiCommand},
    git::GitAdapter,
    paths,
    protocol::{
        DashboardProfile, DispatchRequest, InspectRequest, LaunchIntoPaneRequest, PaneInputRequest,
        PaneResizeRequest, ProcessState, Request, Response, ReviewInboxRequest,
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
                "protocol": 6,
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
) -> Result<(), String> {
    let mut guard = TerminalGuard::enter().map_err(|e| e.to_string())?;
    let mut terminal =
        Terminal::new(CrosstermBackend::new(io::stdout())).map_err(|e| e.to_string())?;
    let mut dashboard = Dashboard::default();
    dashboard.runtime_directory = runtime_directory.clone();
    let (catalog_tx, catalog_rx) = mpsc::channel();
    let mut catalog_loading = false;
    let mut test_events = test_events()?;
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
        terminal
            .draw(|frame| dashboard.render(frame))
            .map_err(|e| e.to_string())?;
        for (workspace_id, pane_id, rows, cols) in dashboard.take_pending_resizes() {
            let _ = client.request(&Request::PaneResize(PaneResizeRequest {
                workspace_id,
                pane_id,
                rows,
                cols,
            }));
        }
        if let Some(message) = client.take_deferred_error() {
            dashboard.error = Some(message);
        }
        let event = if let Some(event) = test_events.pop_front() {
            event
        } else {
            if !event::poll(Duration::from_millis(16)).map_err(|e| e.to_string())? {
                continue;
            }
            event::read().map_err(|e| e.to_string())?
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
                match client.request(&request)? {
                    Response::Error { message, .. } => dashboard.error = Some(message),
                    _ => dashboard.error = None,
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
                // Painted before the round trip like every other command, so the keypress is
                // visibly acknowledged even when the daemon is slow to answer.
                terminal
                    .draw(|frame| dashboard.render(frame))
                    .map_err(|e| e.to_string())?;
                match client.request(&Request::ReviewInbox(ReviewInboxRequest {}))? {
                    Response::ReviewInbox { items } => dashboard.set_review_inbox(items),
                    Response::Error { message, .. } => dashboard.error = Some(message),
                    other => {
                        dashboard.error =
                            Some(format!("unexpected review inbox response: {other:?}"))
                    }
                }
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
            UiCommand::LoadBoard => match dock::board::tasks_dir(&dashboard.repository_root) {
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
            arguments: vec![title.to_owned()],
        });
        if let Response::Error { message, .. } = client.request(&request)? {
            return Err(message);
        }
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
                arguments: Vec::new(),
            },
        },
    });
    match client.request(&request)? {
        Response::Error { message, .. } => return Err(message),
        _ => {
            dashboard.error = None;
        }
    }
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
    if dashboard.workspace_index >= dashboard.layout.workspaces.len() {
        dashboard.workspace_index = dashboard.layout.workspaces.len().saturating_sub(1);
    }
    Ok(())
}

#[cfg(test)]
mod terminal_tests {
    use super::*;
    use nix::pty::openpty;

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
