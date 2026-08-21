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
    thread,
    time::{Duration, Instant},
};

use crossterm::{
    event::{self, DisableMouseCapture, EnableMouseCapture, Event, KeyEventKind},
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use dock::{
    client::Client,
    dashboard::{Dashboard, UiCommand},
    discovery::{AgentDiscovery, ProcessNameDiscovery},
    git::GitAdapter,
    paths,
    protocol::{InspectRequest, Request, Response, WorkspaceRequest},
    storage::LocalStore,
};
use nix::libc;
use ratatui::{Terminal, backend::CrosstermBackend};

fn main() -> Result<(), Box<dyn Error>> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if run_noninteractive_legacy(&args)? {
        return Ok(());
    }
    let socket = args
        .iter()
        .find_map(|arg| arg.strip_prefix("--socket=").map(PathBuf::from))
        .map_or_else(paths::default_socket_path, Ok)?;
    let state_dir = args
        .iter()
        .find_map(|arg| arg.strip_prefix("--state-dir=").map(PathBuf::from))
        .unwrap_or_else(|| PathBuf::from(".dock/local"));
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
    let repository = std::env::current_dir()?;
    let external = ProcessNameDiscovery.discover(&repository);
    run_dashboard(
        &mut client,
        external,
        repository.to_string_lossy().into_owned(),
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
        execute!(io::stdout(), EnterAlternateScreen, EnableMouseCapture)?;
        Ok(guard)
    }

    fn discard_pending_input_on_exit(&mut self) {
        self.discard_pending_input = true;
    }

    fn restore(&mut self) -> io::Result<()> {
        let _ = execute!(io::stdout(), DisableMouseCapture, LeaveAlternateScreen);
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
        let _ = execute!(io::stdout(), DisableMouseCapture, LeaveAlternateScreen);
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
    external: Vec<dock::discovery::ExternalAgentCandidate>,
    repository_root: String,
) -> Result<(), String> {
    let mut guard = TerminalGuard::enter().map_err(|e| e.to_string())?;
    let mut terminal =
        Terminal::new(CrosstermBackend::new(io::stdout())).map_err(|e| e.to_string())?;
    let mut dashboard = Dashboard::default();
    dashboard.external = external;
    dashboard.repository_root = repository_root;
    let mut test_events = test_events()?;
    refresh(client, &mut dashboard)?;
    loop {
        terminal
            .draw(|frame| dashboard.render(frame))
            .map_err(|e| e.to_string())?;
        let event = if let Some(event) = test_events.pop_front() {
            event
        } else {
            if !event::poll(Duration::from_millis(200)).map_err(|e| e.to_string())? {
                refresh(client, &mut dashboard)?;
                continue;
            }
            event::read().map_err(|e| e.to_string())?
        };
        let command = match event {
            Event::Key(key) if key.kind == KeyEventKind::Press => dashboard.key(key),
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
                match client.request(&request)? {
                    Response::Error { message, .. } => dashboard.error = Some(message),
                    _ => dashboard.error = None,
                }
                refresh(client, &mut dashboard)?;
            }
            UiCommand::Refresh => refresh(client, &mut dashboard)?,
            UiCommand::None => {}
        }
    }
    terminal.show_cursor().map_err(|e| e.to_string())?;
    drop(terminal);
    guard.restore().map_err(|e| e.to_string())?;
    Ok(())
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
    let mut events = VecDeque::new();
    for key in value.chars() {
        if key.is_control() {
            return Err("DOCK_TEST_KEY_EVENTS accepts printable keys only".into());
        }
        events.push_back(Event::Key(crossterm::event::KeyEvent::new(
            crossterm::event::KeyCode::Char(key),
            crossterm::event::KeyModifiers::NONE,
        )));
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
    dashboard.runs = match client.request(&Request::Inspect(InspectRequest { run_id: None }))? {
        Response::Snapshots { snapshots } => snapshots,
        Response::Error { message, .. } => return Err(message),
        response => return Err(format!("unexpected runtime response: {response:?}")),
    };
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
