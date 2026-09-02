//! Executes a resolved check's argv — the only place in Dock that runs a command Dock did not
//! write.
//!
//! Everything here exists to make one column of a receipt trustworthy. The agent named a check;
//! `declaration` turned that name into an argv the repository committed; this module runs that
//! argv and writes down what it saw. The agent cannot reach any of it, which is why the
//! containment has to hold at this boundary rather than anywhere upstream of it.
//!
//! Three properties are load-bearing, and each is a way this could hang or leak instead of
//! finishing:
//!
//! * **Both pipes are drained while the child runs.** A check that fills the kernel's pipe
//!   buffer while Dock sits in `wait()` deadlocks both sides forever, and 64 KB is not much
//!   output at all.
//! * **The timeout kills the process *group*.** `sh -c "sleep 30 & sleep 30"` leaves a child
//!   that outlives its parent, so signalling the process Dock spawned would leave the rest of it
//!   running. Dock owns the group, exactly as it does for a pane, and retires the group.
//! * **Nothing waits without a deadline.** Every wait in here — for the child, for the group to
//!   leave, for the readers to see end-of-file — is bounded, because a daemon that stops
//!   answering is worse than a check that reports nothing.

use std::io::Read;
use std::os::unix::process::CommandExt as _;
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::{Arc, Condvar, Mutex, OnceLock};
use std::time::{Duration, Instant};

use nix::sys::signal::{Signal, killpg};
use nix::unistd::Pid;

use crate::declaration::Check;
use dock_model::env::environment_is_allowed;
use dock_model::receipt::{CheckOutcome, CheckRun};

/// How many lines of a check's output a receipt keeps. A receipt is evidence a person reads,
/// not a log file, and it is the *last* lines that survive, because that is where a failing
/// check says why it failed.
pub const TAIL_LINES: usize = 200;

/// The same cap counted in bytes, for a check whose lines are long. Whichever of the two binds
/// first decides how much is kept.
pub const TAIL_BYTES: usize = 64 * 1024;

/// How long an overrunning group is given to leave on SIGTERM before SIGKILL, and how long Dock
/// then waits for the output readers to see end-of-file.
///
/// The same number serves both because both are the same judgement: long enough that an orderly
/// exit is never cut short, short enough that a check which refuses to leave cannot hold a lane
/// for a noticeable part of a handoff.
const GRACE: Duration = Duration::from_secs(5);

/// Runs one declared check in `worktree` and reports what was witnessed.
///
/// Never returns an error: a check that could not run is a `CheckRun` saying so, because
/// "unwitnessed, and here is the sentence why" is itself evidence the receipt must carry. The
/// SHA is pinned either side so a reader can see whether the tree moved under the check —
/// `check_stale` and `check_mutated_worktree` are decided from those two pins alone.
pub fn run(check: &Check, worktree: &Path, permitted_env: &[String], run_id: &str) -> CheckRun {
    let unwitnessed = |reason: String, before: Option<&dock_git::GitFacts>| CheckRun {
        name: check.name.clone(),
        command: check.run.clone(),
        outcome: CheckOutcome::Unwitnessed,
        exit_code: None,
        duration_ms: 0,
        sha_before: before.map(|f| f.head_sha.clone()).unwrap_or_default(),
        sha_after: String::new(),
        dirty_before: before.is_some_and(|f| f.status_entries > 0),
        dirty_after: false,
        tail: String::new(),
        reason: Some(reason),
    };

    // A declaration with no argv is rejected when the file is parsed, but `Check`'s fields are
    // public, so the one place Dock indexes into someone else's command refuses rather than
    // panics.
    if check.run.is_empty() {
        return unwitnessed(format!("check `{}` declares no command", check.name), None);
    }

    // Before the pin, not after it. A lane can park for as long as four other checks take, and
    // a pin taken on the far side of that wait describes a worktree the check never ran against
    // — `check_stale` and `check_mutated_worktree` are decided from these two SHAs, so a queued
    // check would report a mutation that happened minutes before it was spawned. Holding a lane
    // across one `git rev-parse` costs about 13ms against a ten-minute default timeout.
    let _lane = Lane::acquire();

    // A worktree whose facts cannot be read is unwitnessed with that error as the reason: a
    // check with no SHA witnesses nothing, however cleanly it exits.
    let adapter = dock_git::GitAdapter::new(worktree);
    let before = match adapter.facts("HEAD") {
        Ok(facts) => facts,
        Err(error) => return unwitnessed(error, None),
    };

    let started = Instant::now();
    let Watched {
        mut outcome,
        exit_code,
        tail,
        mut reason,
    } = match spawn_and_watch(check, worktree, permitted_env, run_id) {
        Ok(watched) => watched,
        Err(error) => return unwitnessed(error, Some(&before)),
    };
    let duration_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);

    // The same rule as the pin before, but written out rather than folded into `unwitnessed`,
    // because by now there is a duration and a tail worth keeping even though the run witnesses
    // nothing. A reason already present wins: "it timed out" is *why* there is nothing to
    // witness, and an unreadable worktree afterwards is a second symptom, not the cause.
    let (sha_after, dirty_after) = match adapter.facts("HEAD") {
        Ok(after) => (after.head_sha, after.status_entries > 0),
        Err(error) => {
            outcome = CheckOutcome::Unwitnessed;
            reason = Some(reason.take().unwrap_or(error));
            (String::new(), false)
        }
    };

    CheckRun {
        name: check.name.clone(),
        command: check.run.clone(),
        outcome,
        exit_code,
        duration_ms,
        sha_before: before.head_sha,
        sha_after,
        dirty_before: before.status_entries > 0,
        dirty_after,
        tail,
        reason,
    }
}

/// What the child itself told us, before it is joined to the SHA pins around it.
struct Watched {
    outcome: CheckOutcome,
    exit_code: Option<i32>,
    tail: String,
    reason: Option<String>,
}

/// Spawns the check and watches it to a conclusion. `Err` means it never started at all.
fn spawn_and_watch(
    check: &Check,
    worktree: &Path,
    permitted_env: &[String],
    run_id: &str,
) -> Result<Watched, String> {
    let mut command = Command::new(&check.run[0]);
    command
        .args(&check.run[1..])
        // The bound worktree, never the primary checkout and never wherever the daemon happens
        // to have been started.
        .current_dir(worktree)
        // A check does not get the keyboard. One that prompts reads end-of-file and finishes,
        // or hangs and is timed out; either way nobody is waiting at a terminal to answer it.
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        // Dock owns the group, as it does for a pane, so a timeout can retire everything the
        // check started rather than only the process it spawned.
        .process_group(0);
    apply_check_environment(&mut command, check, permitted_env, run_id);

    let mut child = command
        .spawn()
        .map_err(|error| format!("could not start `{}`: {error}", check.run[0]))?;
    // `cast_signed` rather than a fallible conversion with a default, because every candidate
    // default is a loaded gun: `killpg(0)` signals *Dock's own* process group and `killpg(-1)`
    // signals every process Dock can reach. A pid is a `pid_t`, so this is exact.
    let group = Pid::from_raw(child.id().cast_signed());

    // Drain both pipes on their own threads. This is not optional: a check that fills the pipe
    // buffer while Dock waits on `wait()` deadlocks. Measured rather than assumed: `seq 1 5000`
    // is 24 KB and fits, `seq 1 50000` is 288 KB and blocks a `wait()` that has not read first.
    let tail = Arc::new(Mutex::new(Tail::default()));
    let (drained, drain_finished) = mpsc::channel::<()>();
    for stream in [
        child.stdout.take().map(Readable::Out),
        child.stderr.take().map(Readable::Err),
    ]
    .into_iter()
    .flatten()
    {
        let tail = Arc::clone(&tail);
        let drained = drained.clone();
        std::thread::spawn(move || {
            stream.drain_into(&tail);
            drop(drained);
        });
    }
    // The parent's own sender must go, or the disconnect that says "both readers are done"
    // could never arrive.
    drop(drained);

    // `Child` has no timed wait, so one thread does the blocking wait and the deadline is
    // applied to the channel instead.
    let (reaped, exited) = mpsc::channel();
    std::thread::spawn(move || {
        let _ = reaped.send(child.wait());
    });

    let (outcome, exit_code, reason) = match exited.recv_timeout(check.timeout) {
        Ok(Ok(status)) if status.success() => (CheckOutcome::Passed, status.code(), None),
        Ok(Ok(status)) => (CheckOutcome::Failed, status.code(), None),
        Ok(Err(error)) => (
            CheckOutcome::Unwitnessed,
            None,
            Some(format!("could not wait for `{}`: {error}", check.run[0])),
        ),
        Err(RecvTimeoutError::Timeout) => {
            retire_group(group, &exited);
            (
                CheckOutcome::Unwitnessed,
                None,
                Some(format!("timed out after {}s", check.timeout.as_secs())),
            )
        }
        // The waiting thread can only vanish without sending by panicking, which `Child::wait`
        // has no way to do; recording it beats treating a silent channel as success.
        Err(RecvTimeoutError::Disconnected) => (
            CheckOutcome::Unwitnessed,
            None,
            Some(format!("lost track of `{}` while it ran", check.run[0])),
        ),
    };

    // Wait for end-of-file on both pipes rather than joining the readers, because a check may
    // exit while leaving a descendant holding the write end — `sh -c "daemon & exit"` does
    // exactly that — and joining would then park Dock for as long as that descendant lives.
    let _ = drain_finished.recv_timeout(GRACE);
    let tail = tail
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .render();

    Ok(Watched {
        outcome,
        exit_code,
        tail,
        reason,
    })
}

/// SIGTERM the group, give it `GRACE` to leave, then SIGKILL it — the same escalation a pane
/// gets, for the same reason: the polite signal is what lets a check's own cleanup run, and the
/// second one is what makes "gone" a fact rather than a hope.
fn retire_group(group: Pid, exited: &mpsc::Receiver<std::io::Result<std::process::ExitStatus>>) {
    for signal in [Signal::SIGTERM, Signal::SIGKILL] {
        // An already-empty group answers ESRCH, which is the outcome being asked for.
        let _ = killpg(group, signal);
        if exited.recv_timeout(GRACE).is_ok() {
            return;
        }
    }
}

/// Builds the check's environment from nothing.
///
/// `env_clear` first, so this is a list of what a check may see rather than a list of what it
/// may not — an allowlist cannot be defeated by a variable nobody thought to name. On top of the
/// ambient values `dock_model::env` permits, a check gets the variables its declaration asked
/// for *and* the user's config permitted, and the id of the run it belongs to.
fn apply_check_environment(
    command: &mut Command,
    check: &Check,
    permitted_env: &[String],
    run_id: &str,
) {
    command.env_clear();
    command.envs(std::env::vars_os().filter(|(key, _)| environment_is_allowed(key)));
    for name in &check.needs_env {
        // Both halves must agree: the repository declared the need, and the user permitted it.
        // `resolve` already refuses a check whose need is unpermitted, so this is the second of
        // two locks rather than the only one.
        if permitted_env.iter().any(|permitted| permitted == name)
            && let Some(value) = std::env::var_os(name)
        {
            command.env(name, value);
        }
    }
    // Last, because this is Dock describing the run rather than anything inherited.
    command.env("DOCK_RUN_ID", run_id);
}

/// Which pipe a reader is draining. The two are merged into one tail because a reader wants the
/// end of what the check said, not the end of one of its two streams.
enum Readable {
    Out(std::process::ChildStdout),
    Err(std::process::ChildStderr),
}

impl Readable {
    fn drain_into(self, tail: &Mutex<Tail>) {
        match self {
            Self::Out(stream) => drain(stream, tail),
            Self::Err(stream) => drain(stream, tail),
        }
    }
}

/// Reads a pipe to end-of-file, feeding whole lines into the bounded tail as they arrive.
///
/// Lines rather than raw bytes, so the two streams cannot interleave halfway through a word, and
/// bounded per line, so a check that emits a gigabyte with no newline in it cannot be a memory
/// exhaustion attack on the daemon that ran it.
fn drain(mut stream: impl Read, tail: &Mutex<Tail>) {
    let mut buffer = [0_u8; 8192];
    let mut line: Vec<u8> = Vec::new();
    loop {
        let count = match stream.read(&mut buffer) {
            Ok(0) => break,
            Ok(count) => count,
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(_) => break,
        };
        for &byte in &buffer[..count] {
            if byte == b'\n' {
                push(tail, std::mem::take(&mut line));
            } else if line.len() + 1 < TAIL_BYTES {
                line.push(byte);
            }
        }
    }
    if !line.is_empty() {
        push(tail, line);
    }
}

fn push(tail: &Mutex<Tail>, line: Vec<u8>) {
    // Lossy, because a check is free to print bytes that are not text and losing the receipt
    // over it would be the wrong trade.
    let line = String::from_utf8_lossy(&line).into_owned();
    tail.lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .push(line);
}

/// The last `TAIL_LINES` lines and `TAIL_BYTES` bytes of what a check said, kept as it arrives
/// so that a loud check costs a bounded amount of memory rather than all of it.
#[derive(Default)]
struct Tail {
    lines: std::collections::VecDeque<String>,
    bytes: usize,
}

impl Tail {
    fn push(&mut self, mut line: String) {
        // `drain` caps what it accumulates, but the lossy conversion between there and here can
        // still expand a line — every invalid byte becomes a three-byte U+FFFD — so the cap is
        // re-applied to the string that is actually stored. Without it the arithmetic below is
        // asked to free more than the whole deque holds, and a single enormous line erases every
        // line before it: an empty tail on exactly the noisy failure a reader needs to see.
        let mut limit = TAIL_BYTES - 1;
        if line.len() > limit {
            while !line.is_char_boundary(limit) {
                limit -= 1;
            }
            line.truncate(limit);
        }
        // The newline this line will be rendered with is counted here, so the byte budget is
        // never an underestimate of what `render` produces.
        self.bytes += line.len() + 1;
        self.lines.push_back(line);
        while self.lines.len() > TAIL_LINES || self.bytes > TAIL_BYTES {
            let Some(dropped) = self.lines.pop_front() else {
                break;
            };
            self.bytes -= dropped.len() + 1;
        }
    }

    fn render(&self) -> String {
        let mut rendered = String::with_capacity(self.bytes);
        for (index, line) in self.lines.iter().enumerate() {
            if index > 0 {
                rendered.push('\n');
            }
            rendered.push_str(line);
        }
        rendered
    }
}

/// Permission to have a check running. Released on drop, including while a panic unwinds, so a
/// lane can never be lost by a path that returns early.
struct Lane;

impl Lane {
    fn acquire() -> Self {
        let (free, ready) = lanes();
        let mut free = free.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        while *free == 0 {
            free = ready
                .wait(free)
                .unwrap_or_else(|poisoned| poisoned.into_inner());
        }
        *free -= 1;
        Self
    }
}

impl Drop for Lane {
    fn drop(&mut self) {
        let (free, ready) = lanes();
        *free.lock().unwrap_or_else(|poisoned| poisoned.into_inner()) += 1;
        ready.notify_one();
    }
}

fn lanes() -> &'static (Mutex<usize>, Condvar) {
    static LANES: OnceLock<(Mutex<usize>, Condvar)> = OnceLock::new();
    LANES.get_or_init(|| (Mutex::new(lane_limit()), Condvar::new()))
}

/// Half the machine, capped at four, never zero.
///
/// A check is usually a build or a test run that will use every core it is given, so running
/// several at once is not free parallelism — it is the same work, contending. Half leaves the
/// machine usable for the person watching, and the cap keeps a big build server from starting
/// dozens of `cargo test`s at once.
fn lane_limit() -> usize {
    std::thread::available_parallelism()
        .map_or(1, |parallelism| parallelism.get() / 2)
        .clamp(1, 4)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::declaration::Check;
    use dock_model::receipt::CheckOutcome;
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::Duration;

    static SEQ: AtomicU64 = AtomicU64::new(0);

    fn git(dir: &Path, args: &[&str]) {
        assert!(
            std::process::Command::new("git")
                .arg("-C")
                .arg(dir)
                .args(args)
                .status()
                .unwrap()
                .success()
        );
    }

    /// A temporary repository with one commit, shaped exactly like `dispatch.rs`'s `Repo::new`.
    /// The path is canonicalized because one test compares it with what `pwd` prints.
    fn fixture_repo(label: &str) -> PathBuf {
        let root = std::env::current_dir()
            .unwrap()
            .join("target")
            .join(format!(
                "dock-runner-{label}-{}-{}",
                std::process::id(),
                SEQ.fetch_add(1, Ordering::Relaxed)
            ));
        std::fs::create_dir_all(&root).unwrap();
        git(&root, &["init", "-q"]);
        git(&root, &["config", "user.email", "dock@example.invalid"]);
        git(&root, &["config", "user.name", "Dock Fixture"]);
        std::fs::write(root.join("tracked"), "fixture\n").unwrap();
        git(&root, &["add", "tracked"]);
        git(&root, &["commit", "-qm", "fixture"]);
        std::fs::canonicalize(&root).unwrap()
    }

    fn check(name: &str, run: &[&str], timeout: Duration) -> Check {
        Check {
            name: name.into(),
            run: run.iter().map(|a| (*a).to_owned()).collect(),
            timeout,
            needs_env: Vec::new(),
        }
    }

    #[test]
    fn a_check_that_passes_is_witnessed_green_with_the_sha_it_ran_at() {
        let repo = fixture_repo("runner-pass");
        let outcome = run(
            &check("ok", &["true"], Duration::from_secs(5)),
            &repo,
            &[],
            "run_1",
        );
        assert_eq!(outcome.outcome, CheckOutcome::Passed);
        assert_eq!(outcome.exit_code, Some(0));
        assert_eq!(outcome.sha_before, outcome.sha_after);
        assert!(
            !outcome.sha_before.is_empty(),
            "a check with no SHA witnesses nothing"
        );
    }

    #[test]
    fn a_check_that_fails_carries_its_code_and_the_tail_of_what_it_said() {
        let repo = fixture_repo("runner-fail");
        let outcome = run(
            &check(
                "no",
                &["sh", "-c", "echo boom >&2; exit 3"],
                Duration::from_secs(5),
            ),
            &repo,
            &[],
            "run_1",
        );
        assert_eq!(outcome.outcome, CheckOutcome::Failed);
        assert_eq!(outcome.exit_code, Some(3));
        assert!(outcome.tail.contains("boom"), "{:?}", outcome.tail);
    }

    /// The tail is capped so a receipt cannot become a log file.
    #[test]
    fn a_loud_check_is_cut_to_the_last_lines_rather_than_stored_whole() {
        let repo = fixture_repo("runner-loud");
        let outcome = run(
            &check("loud", &["sh", "-c", "seq 1 5000"], Duration::from_secs(20)),
            &repo,
            &[],
            "run_1",
        );
        assert!(
            outcome.tail.len() <= TAIL_BYTES,
            "{} bytes",
            outcome.tail.len()
        );
        assert!(outcome.tail.lines().count() <= TAIL_LINES);
        // The *last* lines, because that is where a failure says why.
        assert!(outcome.tail.contains("5000"), "{:?}", outcome.tail);
    }

    /// A check that outlives its timeout is unwitnessed, and its whole process group is gone —
    /// not just the process Dock spawned.
    ///
    /// The descendant proves its own death, because the outcome alone cannot. A runner that
    /// signalled the process instead of the group would still record `Unwitnessed` promptly:
    /// `sh` dies, `wait()` returns, and the backgrounded child is merely orphaned and runs on.
    /// So the orphan is given a job — sleep, then touch a file in the worktree — and the test
    /// waits well past the moment it would have finished to assert the file never appeared.
    #[test]
    fn a_check_that_overruns_is_killed_by_the_group_and_recorded_unwitnessed() {
        let repo = fixture_repo("runner-timeout");
        let survivor = repo.join("survivor");
        let outcome = run(
            &check(
                "slow",
                &["sh", "-c", r#"sh -c "sleep 3; touch survivor" & sleep 30"#],
                Duration::from_millis(300),
            ),
            &repo,
            &[],
            "run_1",
        );
        assert_eq!(outcome.outcome, CheckOutcome::Unwitnessed);
        assert!(
            outcome
                .reason
                .as_deref()
                .is_some_and(|r| r.contains("timed out"))
        );
        assert!(
            outcome.duration_ms < 10_000,
            "the kill did not happen promptly"
        );
        // Comfortably past the orphan's three seconds, so a machine under load cannot make this
        // pass by being slow. The wait is only ever spent on the passing path.
        let deadline = Instant::now() + dock_testing::budget(9);
        while Instant::now() < deadline {
            assert!(
                !survivor.exists(),
                "a descendant outlived the timeout and kept working: the group was not retired"
            );
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    /// A check may not see a credential-shaped variable it was not permitted. This extends the
    /// allowlist test that moved to `dock-model` in step 1 to the process that actually runs.
    ///
    /// The unpermitted variable is one cargo already put in this process rather than one the
    /// test sets, because `set_var` here would be a live data race: the other tests in this
    /// binary walk `environ` inside `apply_check_environment` on parallel harness threads, which
    /// is exactly the pattern Rust 2024 made `unsafe` for. The filter is by name and does not
    /// care that this particular value is not literally a token, so the property under test is
    /// identical — a variable outside the allowlist does not reach the child.
    #[test]
    fn a_check_cannot_see_a_credential_the_user_did_not_permit() {
        let repo = fixture_repo("runner-env");
        // Without this the assertion below could pass by the variable simply being absent,
        // which would prove nothing about the filter.
        let unpermitted = std::env::var("CARGO_PKG_NAME").expect("cargo sets this for its tests");
        assert!(!unpermitted.is_empty());
        let outcome = run(
            &check(
                "env",
                &["sh", "-c", "echo [$CARGO_PKG_NAME][$PATH]"],
                Duration::from_secs(5),
            ),
            &repo,
            &[],
            "run_1",
        );
        assert!(!outcome.tail.contains(&unpermitted), "{:?}", outcome.tail);
        assert!(
            outcome.tail.contains("[]["),
            "PATH must survive: {:?}",
            outcome.tail
        );
    }

    /// A check that asks for the keyboard hangs, times out, and is recorded — it does not get
    /// the terminal, and it does not block the daemon waiting for someone to type.
    #[test]
    fn a_check_that_reads_stdin_gets_end_of_file_rather_than_the_keyboard() {
        let repo = fixture_repo("runner-stdin");
        let outcome = run(
            &check(
                "ask",
                &["sh", "-c", "read answer; echo [$answer]"],
                Duration::from_secs(5),
            ),
            &repo,
            &[],
            "run_1",
        );
        // `read` sees end-of-file immediately and `answer` stays empty, so the check completes
        // instead of waiting for a keystroke that is never coming.
        assert_ne!(outcome.outcome, CheckOutcome::Unwitnessed);
        assert!(outcome.tail.contains("[]"), "{:?}", outcome.tail);
    }

    /// `cwd` is the bound worktree, never wherever the daemon happens to be.
    #[test]
    fn a_check_runs_in_the_bound_worktree() {
        let repo = fixture_repo("runner-cwd");
        let outcome = run(
            &check("where", &["pwd"], Duration::from_secs(5)),
            &repo,
            &[],
            "run_1",
        );
        assert!(
            outcome.tail.contains(&repo.display().to_string()),
            "{:?}",
            outcome.tail
        );
    }

    /// The run id is the thread that ties a check's own output back to the receipt it belongs
    /// to, so a check that logs somewhere else can still be matched to the run that caused it.
    #[test]
    fn a_check_is_told_which_run_it_belongs_to() {
        let repo = fixture_repo("runner-run-id");
        let outcome = run(
            &check(
                "id",
                &["sh", "-c", "echo [$DOCK_RUN_ID]"],
                Duration::from_secs(5),
            ),
            &repo,
            &[],
            "dock_01J9",
        );
        assert!(outcome.tail.contains("[dock_01J9]"), "{:?}", outcome.tail);
    }

    /// The test that actually pins the concurrent drain, as opposed to the cap.
    ///
    /// `seq 1 5000` above is about 24 KB, which fits inside a pipe buffer — so it exercises the
    /// tail's arithmetic but would still pass against a runner that read the pipes only after
    /// `wait()` returned. This one writes roughly 288 KB down *each* pipe, several times any
    /// pipe buffer on any platform Dock runs on, so a runner that waited before reading would
    /// have both sides blocked on each other forever.
    ///
    /// It reports that as a failure rather than a hang because the check's own timeout is the
    /// backstop: a deadlocked runner returns `Unwitnessed`, and this asserts `Passed`.
    #[test]
    fn a_check_that_outruns_the_pipe_buffer_is_still_witnessed() {
        let repo = fixture_repo("runner-flood");
        let outcome = run(
            &check(
                "flood",
                &["sh", "-c", "seq 1 50000; seq 1 50000 >&2"],
                dock_testing::budget(30),
            ),
            &repo,
            &[],
            "run_1",
        );
        assert_eq!(
            outcome.outcome,
            CheckOutcome::Passed,
            "{:?}",
            outcome.reason
        );
        assert_eq!(outcome.exit_code, Some(0));
        assert!(!outcome.tail.is_empty(), "both pipes were read to the end");
        assert!(
            outcome.tail.len() <= TAIL_BYTES,
            "{} bytes",
            outcome.tail.len()
        );
        assert!(outcome.tail.lines().count() <= TAIL_LINES);
    }
}
