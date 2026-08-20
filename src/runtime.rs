use std::{
    collections::VecDeque,
    fs::File,
    io::{Error, Read},
    os::unix::process::CommandExt,
    process::{Child, Command, ExitStatus, Stdio},
    sync::{Arc, Mutex},
    thread,
};

use nix::{
    pty::openpty,
    sys::signal::{Signal, killpg},
    unistd::{Pid, setsid},
};

use crate::protocol::{ProcessState, RuntimeSnapshot};

const WORKSPACE_ID: &str = "fixture-workspace";
const PANE_ID: &str = "fixture-pane";

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
    command: Vec<String>,
    lifecycle: Arc<Mutex<LifecycleState>>,
    child: Arc<Mutex<Option<Child>>>,
    reaper: Mutex<Option<thread::JoinHandle<()>>>,
    pid: Option<u32>,
    owned_process_group: Option<OwnedProcessGroup>,
    scrollback: Arc<Mutex<Scrollback>>,
    launch_error: Option<String>,
}

/// A process group capability created only from a child successfully launched by Dock.
/// It is deliberately private so callers can never ask the runtime to signal an arbitrary PID.
#[derive(Debug)]
struct OwnedProcessGroup(Pid);

#[derive(Debug)]
enum LifecycleState {
    Running,
    Exited(ExitStatus),
    Unavailable(String),
}

impl OwnedRuntime {
    pub fn launch(command: Vec<String>, scrollback_capacity: usize) -> Self {
        let scrollback = Arc::new(Mutex::new(Scrollback {
            bytes: VecDeque::with_capacity(scrollback_capacity),
            capacity: scrollback_capacity,
            truncated: false,
        }));
        match launch_child(&command, Arc::clone(&scrollback)) {
            Ok((child, pid)) => {
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
                        *reaper_lifecycle
                            .lock()
                            .unwrap_or_else(|poisoned| poisoned.into_inner()) = state;
                    }) {
                    Ok(reaper) => Self {
                        command,
                        lifecycle,
                        child,
                        reaper: Mutex::new(Some(reaper)),
                        pid: Some(pid),
                        owned_process_group: i32::try_from(pid)
                            .ok()
                            .map(Pid::from_raw)
                            .map(OwnedProcessGroup),
                        scrollback,
                        launch_error: None,
                    },
                    Err(error) => Self {
                        command,
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
                        scrollback,
                        launch_error: None,
                    },
                }
            }
            Err(error) => Self {
                command,
                lifecycle: Arc::new(Mutex::new(LifecycleState::Unavailable(error.clone()))),
                child: Arc::new(Mutex::new(None)),
                reaper: Mutex::new(None),
                pid: None,
                owned_process_group: None,
                scrollback,
                launch_error: Some(error),
            },
        }
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
            workspace_id: WORKSPACE_ID.into(),
            pane_id: PANE_ID.into(),
            state,
            pid: self.pid,
            process_group_id: self
                .owned_process_group
                .as_ref()
                .map(|group| group.0.as_raw()),
            command: self.command.clone(),
            scrollback: String::from_utf8_lossy(&bytes).into_owned(),
            scrollback_bytes: bytes.len(),
            scrollback_capacity_bytes: scrollback.capacity,
            scrollback_truncated: scrollback.truncated,
            diagnostic: self.launch_error.clone().or(runtime_diagnostic),
        }
    }
}

impl Drop for OwnedRuntime {
    fn drop(&mut self) {
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

fn launch_child(
    command: &[String],
    scrollback: Arc<Mutex<Scrollback>>,
) -> Result<(Child, u32), String> {
    let Some(program) = command.first() else {
        return Err("fixture command is required".into());
    };
    let pty =
        openpty(None, None).map_err(|error| format!("could not allocate fixture PTY: {error}"))?;
    let master = File::from(pty.master);
    let slave = File::from(pty.slave);
    let stdin = slave
        .try_clone()
        .map_err(|error| format!("could not clone PTY slave: {error}"))?;
    let stdout = slave
        .try_clone()
        .map_err(|error| format!("could not clone PTY slave: {error}"))?;
    let mut process = Command::new(program);
    process
        .args(&command[1..])
        .stdin(Stdio::from(stdin))
        .stdout(Stdio::from(stdout))
        .stderr(Stdio::from(slave));
    // SAFETY: setsid(2) and ioctl(2) are async-signal-safe syscalls here. The PTY slave is already
    // fd 0, so the new session leader can safely make it its controlling terminal before exec.
    unsafe {
        process.pre_exec(|| {
            setsid().map_err(Error::other)?;
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
    let mut child = process
        .spawn()
        .map_err(|error| format!("could not launch fixture command {program:?}: {error}"))?;
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
    Ok((child, pid))
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
    use std::time::{Duration, Instant};

    fn process_exists(pid: u32) -> bool {
        // Signal 0 performs no mutation and is used only by tests to observe a known fixture PID.
        unsafe { nix::libc::kill(pid as i32, 0) == 0 }
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
        let runtime = OwnedRuntime::launch(
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
    }

    #[test]
    fn launch_failure_is_an_actionable_runtime_receipt() {
        let runtime = OwnedRuntime::launch(vec!["/definitely/not/a/program".into()], 64);
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
        let runtime = OwnedRuntime::launch(
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
        let runtime = OwnedRuntime::launch(
            vec!["sh".into(), "-c".into(), "sleep 30 & echo $!; wait".into()],
            128,
        );
        let snapshot = wait_for(&runtime, |snapshot| snapshot.scrollback.contains('\n'));
        let descendant: u32 = snapshot.scrollback.trim().parse().expect("descendant pid");
        assert!(process_exists(descendant));
        drop(runtime);
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
        let runtime = OwnedRuntime::launch(vec!["sh".into(), "-c".into(), "exit 7".into()], 64);
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
        let runtime = OwnedRuntime::launch(vec!["sh".into(), "-c".into(), "sleep 30".into()], 64);
        let _ = std::panic::catch_unwind(|| {
            let _guard = runtime.lifecycle.lock().expect("lock lifecycle");
            panic!("poison lifecycle lock");
        });
        let _ = runtime.snapshot();
        drop(runtime);
    }
}
