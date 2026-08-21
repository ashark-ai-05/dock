use std::{
    collections::VecDeque,
    fs::File,
    io::{Error, Read, Write},
    os::unix::{io::AsRawFd, net::UnixStream, process::CommandExt},
    path::{Path, PathBuf},
    process::{Child, Command, ExitStatus, Stdio},
    sync::{
        Arc, Mutex,
        mpsc::{SyncSender, TrySendError, sync_channel},
    },
    thread,
    time::{Duration, Instant},
};

use nix::{
    pty::openpty,
    sys::signal::{Signal, kill, killpg},
    unistd::{Pid, setsid},
};

use crate::{
    adapter::{AdapterCapabilities, AdapterId, ProcessCapabilities, ResolvedAdapter},
    protocol::{ProcessState, ProviderState, RuntimeSnapshot},
};

#[derive(Debug, Clone)]
pub struct RunBinding {
    pub repository_root: PathBuf,
    pub external_task_ref: String,
    pub run_id: String,
    pub worktree: PathBuf,
    pub branch: String,
    pub base_sha: String,
    pub workspace_id: String,
    pub pane_id: String,
}

#[derive(Debug)]
struct Scrollback {
    bytes: VecDeque<u8>,
    capacity: usize,
    truncated: bool,
}

impl Scrollback {
    fn push(&mut self, input: &[u8]) {
        if self.capacity == 0 {
            self.truncated |= !input.is_empty();
            return;
        }
        for byte in input {
            if self.bytes.len() == self.capacity {
                self.bytes.pop_front();
                self.truncated = true;
            }
            self.bytes.push_back(*byte);
        }
    }
}

pub struct OwnedRuntime {
    binding: RunBinding,
    command: Vec<String>,
    adapter: AdapterId,
    adapter_capabilities: AdapterCapabilities,
    lifecycle: Arc<Mutex<LifecycleState>>,
    child: Arc<Mutex<Option<Child>>>,
    reaper: Mutex<Option<thread::JoinHandle<()>>>,
    pid: Option<u32>,
    owned_process_group: Option<OwnedProcessGroup>,
    guardian_control: Option<UnixStream>,
    pty_input: Option<SyncSender<Vec<u8>>>,
    scrollback: Arc<Mutex<Scrollback>>,
    launch_error: Option<String>,
}

/// A process group capability created only from a child successfully launched by Dock.
/// It is deliberately private so callers can never ask the runtime to signal an arbitrary PID.
#[derive(Debug)]
struct OwnedProcessGroup(Pid);

type ChildLaunch = (Child, u32, UnixStream, SyncSender<Vec<u8>>);

#[derive(Debug)]
enum LifecycleState {
    Running,
    Exited(ExitStatus),
    Unavailable(String),
}

impl OwnedRuntime {
    pub fn launch(
        binding: RunBinding,
        adapter: ResolvedAdapter,
        scrollback_capacity: usize,
    ) -> Self {
        Self::launch_with_before_lifecycle_publish(binding, adapter, scrollback_capacity, || {})
    }

    fn launch_with_before_lifecycle_publish(
        binding: RunBinding,
        adapter: ResolvedAdapter,
        scrollback_capacity: usize,
        before_lifecycle_publish: impl FnOnce() + Send + 'static,
    ) -> Self {
        let command = adapter.command;
        let adapter_id = adapter.id;
        let adapter_capabilities = adapter.capabilities;
        let scrollback = Arc::new(Mutex::new(Scrollback {
            bytes: VecDeque::with_capacity(scrollback_capacity),
            capacity: scrollback_capacity,
            truncated: false,
        }));
        match launch_child(&command, &binding.worktree, Arc::clone(&scrollback)) {
            Ok((child, pid, guardian_control, pty_input_sender)) => {
                let lifecycle = Arc::new(Mutex::new(LifecycleState::Running));
                let reaper_lifecycle = Arc::clone(&lifecycle);
                let child = Arc::new(Mutex::new(Some(child)));
                let reaper_child = Arc::clone(&child);
                match thread::Builder::new()
                    .name("dock-child-reaper".into())
                    .spawn(move || {
                        let child = reaper_child
                            .lock()
                            .unwrap_or_else(|poisoned| poisoned.into_inner())
                            .take();
                        let state = match child.map(|mut child| child.wait()) {
                            None => LifecycleState::Unavailable(
                                "owned child handle is unavailable".into(),
                            ),
                            Some(Ok(status)) => LifecycleState::Exited(status),
                            Some(Err(error)) => LifecycleState::Unavailable(format!(
                                "could not reap owned child: {error}"
                            )),
                        };
                        before_lifecycle_publish();
                        *reaper_lifecycle
                            .lock()
                            .unwrap_or_else(|poisoned| poisoned.into_inner()) = state;
                    }) {
                    Ok(reaper) => Self {
                        binding,
                        command,
                        adapter: adapter_id,
                        adapter_capabilities,
                        lifecycle,
                        child,
                        reaper: Mutex::new(Some(reaper)),
                        pid: Some(pid),
                        owned_process_group: i32::try_from(pid)
                            .ok()
                            .map(Pid::from_raw)
                            .map(OwnedProcessGroup),
                        guardian_control: Some(guardian_control),
                        pty_input: Some(pty_input_sender),
                        scrollback,
                        launch_error: None,
                    },
                    Err(error) => Self {
                        binding,
                        command,
                        adapter: adapter_id,
                        adapter_capabilities,
                        lifecycle: Arc::new(Mutex::new(LifecycleState::Unavailable(format!(
                            "could not start child reaper: {error}"
                        )))),
                        child,
                        reaper: Mutex::new(None),
                        pid: Some(pid),
                        owned_process_group: i32::try_from(pid)
                            .ok()
                            .map(Pid::from_raw)
                            .map(OwnedProcessGroup),
                        guardian_control: Some(guardian_control),
                        pty_input: Some(pty_input_sender),
                        scrollback,
                        launch_error: None,
                    },
                }
            }
            Err(error) => Self {
                binding,
                command,
                adapter: adapter_id,
                adapter_capabilities,
                lifecycle: Arc::new(Mutex::new(LifecycleState::Unavailable(error.clone()))),
                child: Arc::new(Mutex::new(None)),
                reaper: Mutex::new(None),
                pid: None,
                owned_process_group: None,
                guardian_control: None,
                pty_input: None,
                scrollback,
                launch_error: Some(error),
            },
        }
    }

    #[cfg(test)]
    pub fn launch_fixture(command: Vec<String>, scrollback_capacity: usize) -> Self {
        let worktree = std::env::current_dir().expect("fixture current directory");
        Self::launch(
            RunBinding {
                repository_root: worktree.clone(),
                external_task_ref: "fixture-task".into(),
                run_id: "dock_fixture".into(),
                worktree,
                branch: "fixture".into(),
                base_sha: "fixture".into(),
                workspace_id: "workspace_fixture".into(),
                pane_id: "pane_fixture".into(),
            },
            ResolvedAdapter {
                id: AdapterId::Fixture,
                executable: PathBuf::from(&command[0]),
                command,
                capabilities: AdapterCapabilities {
                    ..AdapterCapabilities::default()
                },
            },
            scrollback_capacity,
        )
    }

    pub fn snapshot(&self) -> RuntimeSnapshot {
        let (state, runtime_diagnostic) = if self.launch_error.is_some() {
            (ProcessState::FailedToLaunch, None)
        } else {
            let lifecycle = self
                .lifecycle
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            match &*lifecycle {
                LifecycleState::Exited(status) => (
                    ProcessState::Exited {
                        code: status.code(),
                    },
                    None,
                ),
                LifecycleState::Running => (ProcessState::Running, None),
                LifecycleState::Unavailable(error) => {
                    (ProcessState::Unavailable, Some(error.clone()))
                }
            }
        };
        let scrollback = self
            .scrollback
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let bytes: Vec<u8> = scrollback.bytes.iter().copied().collect();
        RuntimeSnapshot {
            repository_root: self.binding.repository_root.display().to_string(),
            external_task_ref: self.binding.external_task_ref.clone(),
            run_id: self.binding.run_id.clone(),
            worktree: self.binding.worktree.display().to_string(),
            branch: self.binding.branch.clone(),
            base_sha: self.binding.base_sha.clone(),
            workspace_id: self.binding.workspace_id.clone(),
            pane_id: self.binding.pane_id.clone(),
            state: state.clone(),
            pid: self.pid,
            process_group_id: self
                .owned_process_group
                .as_ref()
                .map(|group| group.0.as_raw()),
            command: self.command.clone(),
            adapter: self.adapter.clone(),
            process_capabilities: ProcessCapabilities::OWNED_RUNTIME,
            adapter_capabilities: self.adapter_capabilities.clone(),
            provider_state: if !self.adapter_capabilities.provider_lifecycle {
                ProviderState::Unknown
            } else {
                match state {
                    ProcessState::Running => ProviderState::Running,
                    ProcessState::Exited { .. } => ProviderState::Exited,
                    _ => ProviderState::Unknown,
                }
            },
            scrollback: String::from_utf8_lossy(&bytes).into_owned(),
            scrollback_bytes: bytes.len(),
            scrollback_capacity_bytes: scrollback.capacity,
            scrollback_truncated: scrollback.truncated,
            diagnostic: self.launch_error.clone().or(runtime_diagnostic),
        }
    }

    pub fn binding(&self) -> RunBinding {
        self.binding.clone()
    }
    pub fn resolved_adapter(&self) -> ResolvedAdapter {
        ResolvedAdapter {
            id: self.adapter.clone(),
            executable: PathBuf::from(&self.command[0]),
            command: self.command.clone(),
            capabilities: self.adapter_capabilities.clone(),
        }
    }
    pub fn interrupt(&self) -> Result<(), String> {
        self.signal(Signal::SIGINT)
    }
    pub fn input(&self, input: &[u8]) -> Result<(), String> {
        if input.is_empty() {
            return Ok(());
        }
        if input.len() > 4096 {
            return Err("pane input is limited to 4096 bytes per request".into());
        }
        if !matches!(
            *self.lifecycle.lock().unwrap_or_else(|p| p.into_inner()),
            LifecycleState::Running
        ) {
            return Err("pane input requires a running Dock-owned runtime".into());
        }
        self.pty_input
            .as_ref()
            .ok_or("run has no Dock-owned PTY input capability")?
            .try_send(input.to_vec())
            .map_err(|error| match error {
                TrySendError::Full(_) => "Dock-owned PTY input queue is full".into(),
                TrySendError::Disconnected(_) => "Dock-owned PTY input capability is closed".into(),
            })
    }
    pub fn stop(&self) -> Result<(), String> {
        if let Some(group) = self.owned_process_group.as_ref() {
            // The leader watcher is not group authority: descendants can keep the exact owned
            // PGID alive after the leader is terminal. Probe and retire that group independently.
            if owned_group_exists(group) {
                signal_owned_group_checked(group, Signal::SIGTERM)?;
                if !wait_for_owned_group_exit(group, Duration::from_millis(1500)) {
                    signal_owned_group_checked(group, Signal::SIGKILL)?;
                    if !wait_for_owned_group_exit(group, Duration::from_millis(1500)) {
                        return Err("Dock-owned process group did not exit after SIGKILL".into());
                    }
                }
            }
        } else if !self.lifecycle_is_terminal() {
            return Err("run has no Dock-owned process group".into());
        }
        self.wait_for_lifecycle_and_join(Duration::from_millis(1500))?;
        Ok(())
    }
    fn wait_for_lifecycle_and_join(&self, timeout: Duration) -> Result<(), String> {
        let deadline = Instant::now() + timeout;
        while !self.lifecycle_is_terminal() && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(10));
        }
        if !self.lifecycle_is_terminal() {
            return Err(
                "Dock-owned process group is absent, but the leader reaper did not publish a terminal lifecycle within the stop timeout; runtime authority retained"
                    .into(),
            );
        }
        self.join_reaper_if_terminal()
    }
    fn lifecycle_is_terminal(&self) -> bool {
        !matches!(
            *self.lifecycle.lock().unwrap_or_else(|p| p.into_inner()),
            LifecycleState::Running
        )
    }
    fn join_reaper_if_terminal(&self) -> Result<(), String> {
        if !self.lifecycle_is_terminal() {
            return Ok(());
        }
        if let Some(reaper) = self.reaper.lock().unwrap_or_else(|p| p.into_inner()).take() {
            reaper
                .join()
                .map_err(|_| "owned child reaper panicked while stopping the run".to_owned())?;
        }
        Ok(())
    }
    fn signal(&self, signal: Signal) -> Result<(), String> {
        if !matches!(
            *self
                .lifecycle
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner()),
            LifecycleState::Running
        ) {
            // The reaper has revoked operational ownership of an exited group. In particular,
            // never signal a stale numeric PGID which the OS could eventually reuse.
            return Ok(());
        }
        let group = self
            .owned_process_group
            .as_ref()
            .ok_or("run has no Dock-owned process group")?;
        checked_signal_result(killpg(group.0, signal), || probe_owned_group(group))
    }
}

impl Drop for OwnedRuntime {
    fn drop(&mut self) {
        // Closing this private channel makes the separately executing guardian terminate the
        // exact session/process group it created, including when dockd itself dies abruptly.
        self.guardian_control.take();
        let Some(group) = self.owned_process_group.take() else {
            return;
        };
        // The group token can only originate from our successful launch above. Signal the whole
        // group even if its leader already exited, so Dock-owned descendants cannot be orphaned.
        signal_owned_group(&group, Signal::SIGTERM);
        signal_owned_group(&group, Signal::SIGKILL);
        let mut reaper = self
            .reaper
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Some(reaper) = reaper.take() {
            let _ = reaper.join();
        }
        let mut child = self
            .child
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Some(mut child) = child.take() {
            let _ = child.wait();
        }
    }
}

fn signal_owned_group(group: &OwnedProcessGroup, signal: Signal) {
    // Drop is best-effort and must remain panic-free; there is no durable transcript or error sink
    // in Slice 1. A future lifecycle receipt can surface errors other than an already-gone group.
    let _ = killpg(group.0, signal);
}

fn signal_owned_group_checked(group: &OwnedProcessGroup, signal: Signal) -> Result<(), String> {
    checked_signal_result(killpg(group.0, signal), || probe_owned_group(group))
}

fn checked_signal_result(
    result: Result<(), nix::errno::Errno>,
    inspect_group: impl FnOnce() -> Result<(), nix::errno::Errno>,
) -> Result<(), String> {
    match result {
        Ok(()) | Err(nix::errno::Errno::ESRCH) => Ok(()),
        Err(nix::errno::Errno::EPERM) => match inspect_group() {
            Err(nix::errno::Errno::ESRCH) => Ok(()),
            Ok(()) | Err(nix::errno::Errno::EPERM) => Err(
                "could not signal Dock-owned process group: EPERM; exact group still exists".into(),
            ),
            Err(error) => Err(format!(
                "could not signal Dock-owned process group: EPERM; could not inspect exact group: {error}"
            )),
        },
        Err(error) => Err(format!(
            "could not signal Dock-owned process group: {error}"
        )),
    }
}

fn probe_owned_group(group: &OwnedProcessGroup) -> Result<(), nix::errno::Errno> {
    kill(Pid::from_raw(-group.0.as_raw()), None)
}

fn owned_group_exists(group: &OwnedProcessGroup) -> bool {
    match probe_owned_group(group) {
        Ok(()) | Err(nix::errno::Errno::EPERM) => true,
        Err(nix::errno::Errno::ESRCH) => false,
        // Unknown inspection failures cannot safely prove that the exact group is gone.
        Err(_) => true,
    }
}

fn wait_for_owned_group_exit(group: &OwnedProcessGroup, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if !owned_group_exists(group) {
            return true;
        }
        thread::sleep(Duration::from_millis(10));
    }
    !owned_group_exists(group)
}

fn launch_child(
    command: &[String],
    worktree: &Path,
    scrollback: Arc<Mutex<Scrollback>>,
) -> Result<ChildLaunch, String> {
    launch_child_with_before_spawn(command, worktree, scrollback, || {})
}

fn launch_child_with_before_spawn(
    command: &[String],
    worktree: &Path,
    scrollback: Arc<Mutex<Scrollback>>,
    before_spawn: impl FnOnce(),
) -> Result<ChildLaunch, String> {
    let Some(program) = command.first() else {
        return Err("fixture command is required".into());
    };
    let pty =
        openpty(None, None).map_err(|error| format!("could not allocate fixture PTY: {error}"))?;
    let master = File::from(pty.master);
    let pty_input = master
        .try_clone()
        .map_err(|error| format!("could not clone PTY master for input: {error}"))?;
    let slave = File::from(pty.slave);
    let stdin = slave
        .try_clone()
        .map_err(|error| format!("could not clone PTY slave: {error}"))?;
    let stdout = slave
        .try_clone()
        .map_err(|error| format!("could not clone PTY slave: {error}"))?;
    if program.contains('/') && !Path::new(program).exists() {
        return Err(format!(
            "could not launch fixture command {program:?}: executable does not exist"
        ));
    }
    let (guardian_control, guardian_end) = UnixStream::pair()
        .map_err(|error| format!("could not create launch guardian channel: {error}"))?;
    let guardian_fd = guardian_end.as_raw_fd();
    // Keep CLOEXEC set in the multi-threaded parent. Only the post-fork child makes fd 3
    // inheritable, so an unrelated concurrent child cannot retain this live control endpoint.
    let mut process = Command::new("/bin/sh");
    apply_child_environment(&mut process, std::env::vars_os());
    process
        .arg("-c")
        .arg(
            r#"(dock_cleanup() { trap '' INT TERM; kill -TERM -$$; sleep 1; kill -KILL -$$; }
trap dock_cleanup TERM
trap 'exit 0' USR1
IFS= read -r dock_guard <&3
dock_cleanup) &
dock_watcher=$!
"$@" 3<&- </dev/tty &
dock_child=$!
# The supervisor must survive group lifecycle signals long enough to reap the worker. Install
# these only after spawn so the worker receives SIGINT/SIGTERM with its own dispositions.
trap '' INT TERM
wait "$dock_child"
dock_status=$?
kill -USR1 "$dock_watcher" 2>/dev/null || true
wait "$dock_watcher" 2>/dev/null || true
exit "$dock_status""#,
        )
        .arg("dock-launch-guardian")
        .args(command)
        .current_dir(worktree)
        .stdin(Stdio::from(stdin))
        .stdout(Stdio::from(stdout))
        .stderr(Stdio::from(slave));
    process.env("DOCK_WORKTREE", worktree);
    // SAFETY: setsid(2) and ioctl(2) are async-signal-safe syscalls here. The PTY slave is already
    // fd 0, so the new session leader can safely make it its controlling terminal before exec.
    unsafe {
        process.pre_exec(move || {
            setsid().map_err(Error::other)?;
            if guardian_fd == 3 {
                if nix::libc::fcntl(guardian_fd, nix::libc::F_SETFD, 0) == -1 {
                    return Err(Error::last_os_error());
                }
            } else if nix::libc::dup2(guardian_fd, 3) == -1 {
                return Err(Error::last_os_error());
            }
            if nix::libc::ioctl(
                nix::libc::STDIN_FILENO,
                nix::libc::TIOCSCTTY as nix::libc::c_ulong,
                0,
            ) == -1
            {
                return Err(Error::last_os_error());
            }
            Ok(())
        });
    }
    before_spawn();
    let mut child = process
        .spawn()
        .map_err(|error| format!("could not launch guardian for {program:?}: {error}"))?;
    drop(guardian_end);
    let pid = child.id();
    if let Err(error) = thread::Builder::new()
        .name("dock-pty-reader".into())
        .spawn(move || read_pty(master, scrollback))
    {
        if let Ok(raw_pid) = i32::try_from(pid) {
            signal_owned_group(&OwnedProcessGroup(Pid::from_raw(raw_pid)), Signal::SIGKILL);
        }
        let _ = child.wait();
        return Err(format!("could not start PTY reader: {error}"));
    }
    let (pty_input_sender, pty_input_receiver) = sync_channel::<Vec<u8>>(64);
    if let Err(error) = thread::Builder::new()
        .name("dock-pty-writer".into())
        .spawn(move || {
            let mut pty_input = pty_input;
            while let Ok(input) = pty_input_receiver.recv() {
                if pty_input.write_all(&input).is_err() {
                    break;
                }
            }
        })
    {
        if let Ok(raw_pid) = i32::try_from(pid) {
            signal_owned_group(&OwnedProcessGroup(Pid::from_raw(raw_pid)), Signal::SIGKILL);
        }
        let _ = child.wait();
        return Err(format!("could not start PTY writer: {error}"));
    }
    Ok((child, pid, guardian_control, pty_input_sender))
}

fn apply_child_environment(
    process: &mut Command,
    variables: impl IntoIterator<Item = (std::ffi::OsString, std::ffi::OsString)>,
) {
    process.env_clear();
    process.envs(
        variables
            .into_iter()
            .filter(|(key, _)| environment_is_allowed(key)),
    );
}

fn environment_is_allowed(key: &std::ffi::OsStr) -> bool {
    let key = key.to_string_lossy();
    matches!(
        key.as_ref(),
        "HOME" | "LANG" | "LOGNAME" | "PATH" | "SHELL" | "TERM" | "TMPDIR" | "USER"
    ) || key.starts_with("LC_")
}

fn read_pty(mut master: File, scrollback: Arc<Mutex<Scrollback>>) {
    let mut buffer = [0_u8; 4096];
    while let Ok(count) = master.read(&mut buffer) {
        if count == 0 {
            break;
        }
        if let Ok(mut bounded) = scrollback.lock() {
            bounded.push(&buffer[..count]);
        } else {
            break;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;
    use std::time::{Duration, Instant};

    fn process_exists(pid: u32) -> bool {
        // Signal 0 performs no mutation and is used only by tests to observe a known fixture PID.
        unsafe { nix::libc::kill(pid as i32, 0) == 0 }
    }

    fn process_group_exists(process_group_id: i32) -> bool {
        unsafe { nix::libc::kill(-process_group_id, 0) == 0 }
    }

    fn wait_for_group_exit(process_group_id: i32) {
        let deadline = Instant::now() + Duration::from_secs(3);
        while process_group_exists(process_group_id) && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(10));
        }
        assert!(
            !process_group_exists(process_group_id),
            "Dock-owned process group {process_group_id} survived lifecycle completion"
        );
    }

    fn wait_for(
        runtime: &OwnedRuntime,
        predicate: impl Fn(&RuntimeSnapshot) -> bool,
    ) -> RuntimeSnapshot {
        let deadline = Instant::now() + Duration::from_secs(3);
        loop {
            let snapshot = runtime.snapshot();
            if predicate(&snapshot) || Instant::now() >= deadline {
                return snapshot;
            }
            thread::sleep(Duration::from_millis(10));
        }
    }

    #[test]
    fn captures_bounded_pty_output_and_exit_state() {
        let runtime = OwnedRuntime::launch_fixture(
            vec!["sh".into(), "-c".into(), "printf 1234567890".into()],
            5,
        );
        let snapshot = wait_for(&runtime, |snapshot| {
            matches!(snapshot.state, ProcessState::Exited { .. }) && snapshot.scrollback_bytes == 5
        });
        assert_eq!(snapshot.scrollback, "67890");
        assert!(snapshot.scrollback_truncated);
        assert_eq!(
            snapshot.pid.map(|pid| pid as i32),
            snapshot.process_group_id
        );
        wait_for_group_exit(snapshot.process_group_id.expect("owned process group"));
    }

    #[test]
    fn input_uses_only_live_owned_pty_and_is_rejected_after_exit() {
        let runtime = OwnedRuntime::launch_fixture(
            vec![
                "sh".into(),
                "-c".into(),
                "read value; printf 'got:%s' \"$value\"".into(),
            ],
            128,
        );
        runtime.input(b"hello\n").expect("owned PTY input");
        let snapshot = wait_for(&runtime, |snapshot| {
            snapshot.scrollback.contains("got:hello")
        });
        assert!(snapshot.scrollback.contains("got:hello"));
        let terminal = wait_for(&runtime, |snapshot| {
            matches!(snapshot.state, ProcessState::Exited { .. })
        });
        assert!(matches!(terminal.state, ProcessState::Exited { .. }));
        assert!(runtime.input(b"again\n").unwrap_err().contains("running"));
    }

    #[test]
    fn stop_cleans_term_ignoring_owned_group_and_guardian() {
        let runtime = OwnedRuntime::launch_fixture(
            vec![
                "sh".into(),
                "-c".into(),
                "trap '' TERM; echo ready; while :; do sleep 1; done".into(),
            ],
            128,
        );
        let snapshot = wait_for(&runtime, |snapshot| snapshot.scrollback.contains("ready"));
        let process_group_id = snapshot.process_group_id.expect("owned process group");

        runtime.stop().expect("stop owned runtime");
        wait_for_group_exit(process_group_id);
        let snapshot = wait_for(&runtime, |snapshot| {
            matches!(snapshot.state, ProcessState::Exited { .. })
        });
        assert!(matches!(snapshot.state, ProcessState::Exited { .. }));
    }

    #[test]
    fn interrupt_then_stop_boundedly_kills_term_ignoring_owned_group() {
        let runtime = OwnedRuntime::launch_fixture(
            vec![
                "sh".into(),
                "-c".into(),
                "trap '' INT TERM; echo ready; while :; do sleep 1; done".into(),
            ],
            128,
        );
        let snapshot = wait_for(&runtime, |snapshot| snapshot.scrollback.contains("ready"));
        let process_group_id = snapshot.process_group_id.expect("owned process group");

        runtime.interrupt().expect("interrupt owned runtime");
        assert!(process_group_exists(process_group_id));
        let started = Instant::now();
        runtime.stop().expect("bounded stop owned runtime");

        assert!(started.elapsed() < Duration::from_secs(4));
        assert!(!process_group_exists(process_group_id));
        assert!(matches!(
            runtime.snapshot().state,
            ProcessState::Exited { .. }
        ));
    }

    #[test]
    fn interrupt_capable_process_keeps_running_until_stop() {
        let runtime = OwnedRuntime::launch_fixture(
            vec![
                "sh".into(),
                "-c".into(),
                "trap 'echo interrupted' INT; echo ready; while :; do sleep 1; done".into(),
            ],
            128,
        );
        let ready = wait_for(&runtime, |snapshot| snapshot.scrollback.contains("ready"));
        let process_group_id = ready.process_group_id.expect("owned process group");

        runtime.interrupt().expect("interrupt owned runtime");
        let interrupted = wait_for(&runtime, |snapshot| {
            snapshot.scrollback.contains("interrupted")
        });
        assert_eq!(interrupted.state, ProcessState::Running);
        assert!(process_group_exists(process_group_id));

        runtime.stop().expect("stop owned runtime");
        wait_for_group_exit(process_group_id);
    }

    #[test]
    fn stop_retires_owned_descendant_after_leader_is_already_reaped() {
        let runtime = OwnedRuntime::launch_fixture(
            vec![
                "sh".into(),
                "-c".into(),
                "trap '' HUP; sleep 30 & echo $!; exit 0".into(),
            ],
            128,
        );
        let terminal = wait_for(&runtime, |snapshot| {
            matches!(snapshot.state, ProcessState::Exited { .. })
                && snapshot.scrollback.contains('\n')
        });
        let process_group_id = terminal.process_group_id.expect("owned process group");
        let descendant: u32 = terminal.scrollback.trim().parse().expect("descendant pid");
        assert!(
            process_exists(descendant),
            "descendant must survive its leader"
        );
        assert!(process_group_exists(process_group_id));

        runtime.stop().expect("stop terminal-leader owned group");

        wait_for_group_exit(process_group_id);
        assert!(!process_exists(descendant), "owned descendant escaped stop");
    }

    #[test]
    fn stop_never_succeeds_while_esrch_precedes_lifecycle_publication() {
        let worktree = std::env::current_dir().expect("fixture current directory");
        let (reaped_tx, reaped_rx) = mpsc::channel();
        let (publish_tx, publish_rx) = mpsc::channel();
        let runtime = OwnedRuntime::launch_with_before_lifecycle_publish(
            RunBinding {
                repository_root: worktree.clone(),
                external_task_ref: "fixture-task".into(),
                run_id: "dock_esrch_before_lifecycle".into(),
                worktree,
                branch: "fixture".into(),
                base_sha: "fixture".into(),
                workspace_id: "workspace_fixture".into(),
                pane_id: "pane_fixture".into(),
            },
            ResolvedAdapter {
                id: AdapterId::Fixture,
                executable: PathBuf::from("sh"),
                command: vec!["sh".into(), "-c".into(), "exit 0".into()],
                capabilities: AdapterCapabilities::default(),
            },
            64,
            move || {
                reaped_tx.send(()).expect("report child reaped");
                publish_rx.recv().expect("release lifecycle publication");
            },
        );
        reaped_rx
            .recv_timeout(Duration::from_secs(3))
            .expect("leader must be reaped before the test probe");
        let group = runtime
            .owned_process_group
            .as_ref()
            .expect("owned process group");
        assert_eq!(probe_owned_group(group), Err(nix::errno::Errno::ESRCH));
        assert_eq!(runtime.snapshot().state, ProcessState::Running);

        let error = runtime
            .stop()
            .expect_err("stop cannot succeed before terminal lifecycle publication");
        assert!(error.contains("leader reaper did not publish a terminal lifecycle"));
        assert!(error.contains("authority retained"));
        assert_eq!(runtime.snapshot().state, ProcessState::Running);
        assert!(runtime.reaper.lock().unwrap().is_some());

        publish_tx.send(()).expect("publish terminal lifecycle");
        runtime.stop().expect("retry reconciles and joins reaper");
        assert!(matches!(
            runtime.snapshot().state,
            ProcessState::Exited { .. }
        ));
        assert!(runtime.reaper.lock().unwrap().is_none());
    }

    #[test]
    fn launch_failure_is_an_actionable_runtime_receipt() {
        let runtime = OwnedRuntime::launch_fixture(vec!["/definitely/not/a/program".into()], 64);
        let snapshot = runtime.snapshot();
        assert_eq!(snapshot.state, ProcessState::FailedToLaunch);
        assert!(
            snapshot
                .diagnostic
                .as_deref()
                .is_some_and(|message| message.contains("could not launch"))
        );
        assert!(snapshot.pid.is_none());
    }

    #[test]
    fn child_is_session_leader_with_the_pty_as_controlling_terminal() {
        let runtime = OwnedRuntime::launch_fixture(
            vec![
                "sh".into(),
                "-c".into(),
                "if test -t 0; then echo controlling-tty; else echo no-tty; fi; sleep 1".into(),
            ],
            128,
        );
        let snapshot = wait_for(&runtime, |snapshot| snapshot.scrollback.contains("tty"));
        let pid = snapshot.pid.expect("launched child") as i32;
        assert_eq!(
            unsafe { nix::libc::getsid(pid) },
            pid,
            "child must lead its session"
        );
        assert!(
            snapshot.scrollback.contains("controlling-tty"),
            "PTY slave must be the child's terminal: {:?}",
            snapshot.scrollback
        );
    }

    #[test]
    fn dropping_runtime_cleans_its_entire_group_but_not_an_unowned_process() {
        let mut unrelated = Command::new("sleep")
            .arg("30")
            .spawn()
            .expect("unrelated fixture");
        let runtime = OwnedRuntime::launch_fixture(
            vec!["sh".into(), "-c".into(), "sleep 30 & echo $!; wait".into()],
            128,
        );
        let snapshot = wait_for(&runtime, |snapshot| snapshot.scrollback.contains('\n'));
        let process_group_id = snapshot.process_group_id.expect("owned process group");
        let descendant: u32 = snapshot.scrollback.trim().parse().expect("descendant pid");
        assert!(process_exists(descendant));
        drop(runtime);
        wait_for_group_exit(process_group_id);
        let deadline = Instant::now() + Duration::from_secs(2);
        while process_exists(descendant) && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(10));
        }
        assert!(
            !process_exists(descendant),
            "Dock-owned descendant survived runtime drop"
        );
        assert!(
            process_exists(unrelated.id()),
            "unowned process was targeted"
        );
        unrelated.kill().expect("clean unrelated fixture");
        unrelated.wait().expect("reap unrelated fixture");
    }

    #[test]
    fn exited_child_is_reaped_without_snapshot_polling() {
        let runtime =
            OwnedRuntime::launch_fixture(vec!["sh".into(), "-c".into(), "exit 7".into()], 64);
        let pid = runtime.pid.expect("launched child") as i32;
        let deadline = Instant::now() + Duration::from_secs(3);
        loop {
            if matches!(
                *runtime
                    .lifecycle
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner()),
                LifecycleState::Exited(_)
            ) {
                break;
            }
            assert!(Instant::now() < deadline, "fixture was not reaped");
            thread::sleep(Duration::from_millis(10));
        }
        let result = unsafe { nix::libc::waitpid(pid, std::ptr::null_mut(), nix::libc::WNOHANG) };
        assert_eq!(result, -1, "fixture should already have been reaped");
        assert_eq!(
            std::io::Error::last_os_error().raw_os_error(),
            Some(nix::libc::ECHILD),
            "child waiter should own and reap the fixture"
        );
        drop(runtime);
    }

    #[test]
    fn poisoned_runtime_locks_do_not_panic_in_snapshot_or_drop() {
        let runtime =
            OwnedRuntime::launch_fixture(vec!["sh".into(), "-c".into(), "sleep 30".into()], 64);
        let _ = std::panic::catch_unwind(|| {
            let _guard = runtime.lifecycle.lock().expect("lock lifecycle");
            panic!("poison lifecycle lock");
        });
        let _ = runtime.snapshot();
        drop(runtime);
    }

    #[test]
    fn child_environment_allowlist_excludes_credential_shaped_ambient_values() {
        for poisoned in [
            "OPENAI_API_KEY",
            "ANTHROPIC_API_KEY",
            "AWS_SECRET_ACCESS_KEY",
            "GITHUB_TOKEN",
            "SSH_AUTH_SOCK",
            "CODEX_API_KEY",
        ] {
            assert!(!environment_is_allowed(std::ffi::OsStr::new(poisoned)));
        }
        for safe in ["HOME", "LANG", "LC_ALL", "PATH", "TERM", "TMPDIR"] {
            assert!(environment_is_allowed(std::ffi::OsStr::new(safe)));
        }
        let mut child = Command::new("env");
        apply_child_environment(
            &mut child,
            [
                ("PATH".into(), "/usr/bin:/bin".into()),
                ("OPENAI_API_KEY".into(), "poison-openai".into()),
                ("ANTHROPIC_API_KEY".into(), "poison-anthropic".into()),
                ("GITHUB_TOKEN".into(), "poison-github".into()),
            ],
        );
        let output = String::from_utf8(child.output().unwrap().stdout).unwrap();
        assert!(output.contains("PATH=/usr/bin:/bin"));
        assert!(!output.contains("poison-"));
    }

    #[test]
    fn checked_owned_group_signal_only_treats_a_definitively_absent_group_as_terminal() {
        let permission_denied =
            checked_signal_result(Err(nix::errno::Errno::EPERM), || Ok(())).unwrap_err();
        assert!(permission_denied.contains("EPERM"));
        assert!(permission_denied.contains("still exists"));
        assert_eq!(
            checked_signal_result(Err(nix::errno::Errno::EPERM), || {
                Err(nix::errno::Errno::EPERM)
            })
            .unwrap_err(),
            "could not signal Dock-owned process group: EPERM; exact group still exists"
        );
        assert_eq!(
            checked_signal_result(Err(nix::errno::Errno::EPERM), || {
                Err(nix::errno::Errno::ESRCH)
            }),
            Ok(())
        );
        assert_eq!(
            checked_signal_result(Err(nix::errno::Errno::ESRCH), || {
                panic!("ESRCH must not require a second probe")
            }),
            Ok(())
        );
        assert!(
            checked_signal_result(Err(nix::errno::Errno::EINVAL), || Ok(()))
                .unwrap_err()
                .contains("EINVAL")
        );
    }

    #[test]
    fn unrelated_concurrent_exec_cannot_inherit_guardian_control() {
        let scrollback = Arc::new(Mutex::new(Scrollback {
            bytes: VecDeque::with_capacity(128),
            capacity: 128,
            truncated: false,
        }));
        let unrelated = Mutex::new(None);
        let (mut guardian, pid, control, _pty_input) = launch_child_with_before_spawn(
            &["sh".into(), "-c".into(), "sleep 30".into()],
            &std::env::current_dir().unwrap(),
            scrollback,
            || {
                *unrelated.lock().unwrap() =
                    Some(Command::new("sh").args(["-c", "sleep 30"]).spawn().unwrap());
            },
        )
        .unwrap();
        drop(control);
        wait_for_group_exit(pid as i32);
        let _ = guardian.wait();

        let mut unrelated = unrelated.into_inner().unwrap().unwrap();
        assert!(unrelated.try_wait().unwrap().is_none());
        unrelated.kill().unwrap();
        unrelated.wait().unwrap();
    }
}
