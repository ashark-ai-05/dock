use std::{
    collections::{HashMap, HashSet},
    ffi::CString,
    fs::{self, File, OpenOptions},
    io::Write,
    os::unix::{
        ffi::OsStrExt,
        fs::{MetadataExt, OpenOptionsExt, PermissionsExt},
        io::{AsRawFd, FromRawFd},
    },
    path::{Component, Path, PathBuf},
    process::Command,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    thread,
    time::{Duration, Instant},
};

use regex::Regex;
use serde::{Deserialize, Serialize};

use crate::{
    adapter::AdapterSelection,
    detect::{AgentKind, AgentState, ScreenRead, process::ProcessTree, read_screen},
    git::GitAdapter,
    layout::{LayoutRegistry, LayoutSnapshot, PaneKind, PaneRuntime, WorkspaceLayout},
    model::{HandoffEvidence, HandoffPacket, HandoffRecord, ReviewDecision, ReviewRoute},
    protocol::{
        BindingKind, DependencyGateSnapshot, DispatchRequest, DurablePaneQueue,
        DurableProgrammeGate, DurableQueueEntry, ErrorCode, GateState, LifecycleOperation,
        PaneQueueSnapshot, ProgrammeSnapshot, QueueEntrySnapshot, RepositoryPortfolioSnapshot,
        RuntimeSnapshot, WorkspaceRequest,
    },
    queue::{AutoFeedTrust, MAX_PROMPT_BYTES, MAX_QUEUED_TOTAL, PaneQueue, QueueEntry},
    runtime::{OwnedRuntime, PtySize, RunBinding, RunPulse},
    storage::LocalStore,
    terminal::{PaneOutput, PaneScreen},
};

/// What one poll has already established about a run before its agent is resolved.
///
/// Gathered by the caller rather than read again inside [`RuntimeRegistry::resolve_agent`], because
/// the process table now needs the same output marks to decide whether taking a fresh one could
/// say anything new. Read twice they could disagree, and a mark that disagreed with itself would
/// look exactly like a stale classification.
struct RunObservation<'a> {
    run_id: &'a str,
    mark: OutputMark,
    /// The pane's geometry. A resize reflows the screen without appending a byte, so it belongs
    /// beside the mark rather than being inferred from it.
    size: (u16, u16),
    process_group_id: Option<i32>,
}

/// How far one run's output log had got, and how many bytes of it there are.
///
/// The pair, rather than the length alone, because the log rotates: a run whose history has been
/// trimmed can have the same end offset it had before and hold entirely different bytes.
type OutputMark = (u64, u64);

/// One memoised classification, and the exact inputs each half of it was computed from.
///
/// Two keys rather than one, because the two halves answer to different things and used to be
/// invalidated together. Which agent runs under a pane can only change when a fresh process table
/// says so; what that agent's screen says can only change when new bytes arrive or the pane is
/// resized. Sharing one key meant every pane on the screen re-read its whole screen and ran three
/// regex sets over it every time a new process table landed — twice a second, with nothing on any
/// of those screens having moved.
#[derive(Debug, Clone, Copy)]
struct ClassifiedAgent {
    /// Which process-table snapshot `agent` was read from.
    generation: u64,
    agent: Option<AgentKind>,
    /// The output mark and pane geometry the screen was read at. A resize reflows the screen
    /// without appending a byte, so the mark alone would not notice it.
    screen: (OutputMark, (u16, u16)),
    from_screen: ScreenRead,
}

/// Everything one run's state inference remembers between polls.
///
/// Deliberately free of the registry and of the clock: every method is handed the `now` it should
/// reason from. Reaching this judgement through a `RuntimeRegistry` needs a real agent process in
/// the process table, which a unit test cannot conjure — an earlier attempt at testing it drove a
/// pane shell instead, where detection finds no agent and the decision is never reached, so the
/// test passed against the very logic it was written to catch. Keeping the reasoning here means a
/// test can replay six seconds of a real polling rhythm in no time at all and read every answer.
#[derive(Debug, Clone, Copy)]
struct StateTracker {
    /// How far the run's output log had got, and when that last moved.
    mark: (u64, u64),
    changed_at: Instant,
    /// When the burst of output that `changed_at` belongs to began.
    growing_since: Instant,
    /// What the roster is currently showing, once anything has been positively established.
    /// `None` until then: a run nobody has been able to say anything about yet.
    resolved: Option<AgentState>,
    /// A different answer waiting out [`STATE_DWELL`], and when it first appeared.
    pending: Option<(AgentState, Instant)>,
    /// Whether this run has ever been seen with a spinner in its terminal title.
    ///
    /// Latching this is what turns the title from evidence of work into evidence of its end. A
    /// title with no spinner means nothing on its own — most panes never have one. A title with no
    /// spinner *on an agent that was spinning it a moment ago* is that agent saying its turn is
    /// over, which is the positive claim [`SILENT_HANDOVER`] exists to guess at.
    title_spoke: bool,
}

impl StateTracker {
    fn new(mark: (u64, u64), now: Instant) -> Self {
        Self {
            mark,
            changed_at: now,
            growing_since: now,
            resolved: None,
            pending: None,
            title_spoke: false,
        }
    }

    /// Folds one poll's view of the output log into the record.
    fn observe(&mut self, mark: (u64, u64), now: Instant) {
        if self.mark != mark {
            // A burst that begins after a gap is a new burst; one that continues an existing burst
            // leaves its start where it was, so the burst's age keeps growing. The gap is short on
            // purpose: measured against [`WORKING_SILENCE`] instead, a footer clock ticking once a
            // second never registered a gap at all, so one burst ran from the moment the pane
            // opened and "sustained" came to mean "has written at least twice, ever".
            if now.saturating_duration_since(self.changed_at) >= BURST_GAP {
                self.growing_since = now;
            }
            self.mark = mark;
            self.changed_at = now;
        }
    }

    /// How long the pane has been silent.
    fn quiet_for(&self, now: Instant) -> Duration {
        now.saturating_duration_since(self.changed_at)
    }

    /// How long output had been arriving without a break, as of the last byte seen.
    fn growing_for(&self) -> Duration {
        self.changed_at
            .saturating_duration_since(self.growing_since)
    }

    /// What the roster shows for this run right now.
    ///
    /// A run whose state has never been established reads as working: the only way to reach that
    /// is to be looking at a pane that has just started, and a pane that has just started is
    /// starting up.
    fn shown(&self) -> AgentState {
        self.resolved.unwrap_or(AgentState::Working)
    }

    /// Commits a state without waiting, and forgets any transition part-way through its dwell.
    ///
    /// For the two answers that are not inferences: what the agent reported about itself, and the
    /// absence of an agent altogether. Delaying either would be holding a guess over a fact.
    fn commit(&mut self, state: AgentState) -> AgentState {
        self.resolved = Some(state);
        self.pending = None;
        state
    }

    /// Commits a candidate only once it has held for [`STATE_DWELL`].
    ///
    /// The first answer for a run commits immediately: hysteresis exists to resist *changes*, and
    /// at the start there is nothing yet to change from.
    fn settle(&mut self, candidate: AgentState, now: Instant) -> AgentState {
        if self.resolved.is_none() || candidate == AgentState::Blocked {
            // Blocked is the one state that costs the user throughput while it waits. Making an
            // agent that is stuck sit out a dwell before it can say so is the one delay this
            // roster cannot afford.
            return self.commit(candidate);
        }
        if Some(candidate) == self.resolved {
            self.pending = None;
            return candidate;
        }
        match self.pending {
            // A candidate that changes while pending is not a candidate that held, so its clock
            // starts over.
            Some((waiting, since)) if waiting == candidate => {
                if now.saturating_duration_since(since) >= STATE_DWELL {
                    return self.commit(candidate);
                }
            }
            _ => self.pending = Some((candidate, now)),
        }
        self.shown()
    }

    /// What to show for this run, given what its screen says and what the agent said about itself.
    fn decide(
        &mut self,
        now: Instant,
        agent: Option<AgentKind>,
        screen: ScreenRead,
        reported: Option<AgentState>,
    ) -> AgentState {
        // What the agent said about itself beats anything read off its screen. A hook fires on the
        // agent's own turn boundaries, so it knows; everything below is inference from bytes. It
        // is committed rather than merely returned so that if the hook later stops reporting —
        // an agent restarted without it, a wrapper that dropped away — inference resumes from
        // where the agent actually was rather than from whatever it had guessed beforehand.
        if let Some(reported) = reported {
            return self.commit(reported);
        }
        // A question outranks everything: an agent asking one has stopped, however recently it
        // printed the question itself.
        if screen.state == AgentState::Blocked {
            return self.commit(AgentState::Blocked);
        }
        if agent.is_none() {
            return self.commit(AgentState::Idle);
        }
        // Latches for the rest of the run: an agent that has spun its title once is an agent that
        // maintains it, and that is what makes its later stillness mean something.
        self.title_spoke |= screen.title_working;
        // Every arm below has to be a positive claim, because `classify_screen` returning `Idle`
        // means "no rule matched", which is "no idea" and not "not working". Letting a shrug fall
        // through to a state was the whole defect: an idle pane that failed to match its own
        // chrome for one frame was reported as working on that frame.
        let candidate = if screen.title_working {
            // The agent's own title says it is mid-turn. This is the only "is going" claim on the
            // screen that is trustworthy, and the reason is that the agent rewrites the title
            // itself on every state change — unlike body chrome, which simply stays where it was
            // printed. It outranks the output clock below because it is a statement rather than an
            // inference: an agent waiting on a slow first token is silent and still working, and
            // this is the case the clock cannot see.
            Some(AgentState::Working)
        } else if screen.state == AgentState::Done {
            // The agent is painting its own input chrome, which is it saying it is between turns.
            Some(AgentState::Done)
        } else if output_looks_like_work(self.quiet_for(now), self.growing_for()) {
            // Output has been streaming rather than twitching, which is generation.
            Some(AgentState::Working)
        } else if self.title_spoke && self.quiet_for(now) >= WORKING_SILENCE {
            // An agent that spins its title has stopped spinning it, and the pane has stopped
            // writing. Two independent signals agreeing is far better evidence than either alone,
            // which is why this settles in [`WORKING_SILENCE`] rather than waiting out the whole
            // of [`SILENT_HANDOVER`] — that number is the price of having no evidence at all, and
            // here there is some.
            Some(AgentState::Done)
        } else if self.quiet_for(now) >= SILENT_HANDOVER {
            // Nothing has been written for long enough that nothing is being written. Measured
            // against `SILENT_HANDOVER` rather than `WORKING_SILENCE`: ceasing to stream and
            // having finished are different claims, and the second needs far more evidence.
            Some(AgentState::Done)
        } else {
            // The ambiguous middle: something wrote recently, but not for long enough to be work,
            // and no chrome matched. Hold — including any transition already part-way through its
            // dwell, since a frame that says nothing is not a frame that argues against it.
            None
        };
        // Deliberately absent: an arm reading `screen.state == Working` as Working.
        // `WORKING_PATTERNS` is matched against the whole visible screen and the terminal title,
        // both of which keep whatever the last turn left there — a "Running…" line scrolled up but
        // still on screen would pin a finished agent to working forever. Screen text is trusted to
        // say an agent has *stopped* (that chrome is only painted when it has) and never to say it
        // is going.
        match candidate {
            Some(candidate) => self.settle(candidate, now),
            None => self.shown(),
        }
    }
}

pub struct RuntimeRegistry {
    runs: Mutex<HashMap<String, RuntimeSlot>>,
    receipts: PathBuf,
    scrollback_rows: usize,
    /// Bytes of raw output each pane retains, and therefore how far back a person can scroll.
    /// Separate from `scrollback_rows`, which is only what the daemon's own parser keeps: the
    /// daemon renders nothing, so its parser depth serves detection, and this serves people.
    pane_history_bytes: usize,
    store: LocalStore,
    programme: Mutex<ProgrammeState>,
    capacity: CapacityPolicy,
    layout: Mutex<LayoutRegistry>,
    /// Last geometry reported for each `workspace/pane`, so a run launched into an already
    /// measured pane starts at the size the client is drawing rather than the fallback. This is
    /// a leaf lock: it is never taken while `runs` or `layout` is held.
    pane_sizes: Mutex<HashMap<String, PtySize>>,
    /// The last process-table snapshot and whether another is already being taken.
    ///
    /// Behind its own `Arc` because a refresh runs on its own thread and installs the result under
    /// this same lock; see [`RuntimeRegistry::process_table`] for why it is not taken inline.
    process_table: Arc<Mutex<ProcessTableCache>>,
    /// Agent state per run, keyed by the exact output the screen was built from. Classification is
    /// a pure function of that screen, so nothing but new bytes can change its answer.
    agent_states: Mutex<HashMap<String, ClassifiedAgent>>,
    /// What each agent has said about itself, where a hook is wired to report it. Preferred over
    /// anything read from the screen: an agent firing its own turn-start and turn-end events knows
    /// what a pattern can only infer.
    reported_states: Mutex<HashMap<String, AgentState>>,
    /// When each run's output last grew. Not memoisable the way classification is: the answer it
    /// feeds changes with the passage of time rather than with new bytes, so it is read afresh.
    output_marks: Mutex<HashMap<String, StateTracker>>,
    /// Every pane's queue of prompts, keyed by `(workspace_id, pane_id)`.
    ///
    /// The pane, not the run: a run dies and is replaced by a resume, a respawn or a restart,
    /// while the pane is the identity `layout.json` persists and the one the user thinks in. A
    /// queue keyed by run would be lost every time the thing it was queued for was restarted.
    ///
    /// A leaf lock. Nothing else is taken while it is held — in particular not `layout` or `runs`,
    /// which is why `queue_tick` resolves every pane *before* it touches this map.
    queues: Mutex<HashMap<(String, String), PaneQueue>>,
    /// The daemon-wide kill switch, suppressing every feed regardless of any pane's own arming.
    ///
    /// An atomic rather than a field on the map because the 250ms tick reads it on every pass and
    /// the answer must not depend on the queue lock being free.
    queue_paused: AtomicBool,
    /// Which "the agent finished" signal an *already-armed* pane is willing to act on.
    ///
    /// Chooses a signal; it arms nothing. There is no setting anywhere that makes arming the
    /// default, and this is not a back door into one.
    auto_feed_trust: Mutex<AutoFeedTrust>,
    /// Bumped on every queue change, and the only thing the 16ms subscriber loop reads on a pass
    /// where nothing happened. Without it that loop would have to lock and walk the revision map
    /// sixty times a second to discover, almost always, that no queue had moved.
    queue_generation: AtomicU64,
    /// The generation at which each pane's queue last changed, so a subscriber can work out which
    /// panes to tell its client about rather than being told to refresh all of them.
    queue_revisions: Mutex<HashMap<(String, String), u64>>,
    #[cfg(test)]
    /// Auto-launched pane shells put a live run in every pane, which is exactly what the
    /// dispatch-authority tests below assert cannot happen. Those tests suppress the shell so
    /// they keep measuring dispatch rollback rather than the pane placeholder.
    suppress_pane_shells: Mutex<bool>,
    #[cfg(test)]
    retire_pane_shell_stop_failure: Mutex<Option<String>>,
    #[cfg(test)]
    restart_hook: Mutex<Option<Arc<dyn Fn() + Send + Sync>>>,
    #[cfg(test)]
    restart_after_stop_hook: Mutex<Option<Arc<dyn Fn() + Send + Sync>>>,
    #[cfg(test)]
    release_cleanup_hook: Mutex<Option<Arc<dyn Fn() + Send + Sync>>>,
    #[cfg(test)]
    release_restore_hook: Mutex<Option<Arc<dyn Fn() + Send + Sync>>>,
    #[cfg(test)]
    after_launch_before_receipt_hook: Mutex<Option<Arc<dyn Fn() + Send + Sync>>>,
    #[cfg(test)]
    before_runtime_launch_hook: Mutex<Option<Arc<dyn Fn() + Send + Sync>>>,
    #[cfg(test)]
    before_save_receipt_hook: Mutex<Option<Arc<dyn Fn() + Send + Sync>>>,
    #[cfg(test)]
    release_commit_hook: Mutex<Option<Arc<dyn Fn() + Send + Sync>>>,
    #[cfg(test)]
    portfolio_capture_hook: Mutex<Option<Arc<dyn Fn() + Send + Sync>>>,
    #[cfg(test)]
    pane_input_before_final_validation_hook: Mutex<Option<Arc<dyn Fn() + Send + Sync>>>,
    #[cfg(test)]
    workspace_stop_failure: Mutex<Option<String>>,
    #[cfg(test)]
    restart_stop_failure: Mutex<Option<String>>,
    #[cfg(test)]
    receipt_stop_failure: Mutex<Option<String>>,
}

#[derive(Clone)]
struct RuntimeEntry {
    runtime: Arc<OwnedRuntime>,
    selection: AdapterSelection,
}

#[derive(Clone)]
struct RuntimeSlot {
    transition: Arc<Mutex<()>>,
    state: RuntimeSlotState,
    /// A pane's auto-launched shell is infrastructure the user opened, not agent work, so it is
    /// excluded from every capacity count (Ruling R20). Decided once at admission from an
    /// explicit caller flag and carried here, never re-derived from the run id at a counting
    /// site.
    pane_shell: bool,
}

#[derive(Clone)]
enum RuntimeSlotState {
    /// Reserves identity, capacity, and pane ownership while launch runs without registry locks.
    Launching {
        repository_root: PathBuf,
    },
    Active(RuntimeEntry),
    /// Keeps one capacity and identity reservation while the exact old group is retired and its
    /// replacement is launched. The entry remains inspectable, but admission must not derive
    /// capacity from its lifecycle after retirement.
    Restarting {
        repository_root: PathBuf,
        entry: RuntimeEntry,
    },
    /// Receipt persistence failed, but the exact launched process group has not yet been proven
    /// stopped. Retain every authority and reservation until a later reconciliation can stop it.
    ReceiptRollbackStopping {
        repository_root: PathBuf,
        entry: RuntimeEntry,
        layout: crate::layout::BindRollback,
        receipt: PathBuf,
    },
    /// The launched process has been retired, but its exact pane binding and receipt reservation
    /// remain authoritative until their independently tracked rollback steps are persisted.
    RollbackPending {
        repository_root: PathBuf,
        layout: Option<crate::layout::BindRollback>,
        receipt: PathBuf,
    },
}

impl RuntimeSlot {
    fn active(&self) -> Option<&RuntimeEntry> {
        match &self.state {
            RuntimeSlotState::Active(entry) => Some(entry),
            RuntimeSlotState::Launching { .. } => None,
            RuntimeSlotState::Restarting { entry, .. } => Some(entry),
            RuntimeSlotState::ReceiptRollbackStopping { entry, .. } => Some(entry),
            RuntimeSlotState::RollbackPending { .. } => None,
        }
    }

    fn belongs_to_repository(&self, repository: &Path) -> bool {
        match &self.state {
            RuntimeSlotState::Launching { repository_root } => repository_root == repository,
            RuntimeSlotState::Restarting {
                repository_root, ..
            } => repository_root == repository,
            RuntimeSlotState::RollbackPending {
                repository_root, ..
            } => repository_root == repository,
            RuntimeSlotState::ReceiptRollbackStopping {
                repository_root, ..
            } => repository_root == repository,
            RuntimeSlotState::Active(entry) => {
                entry.runtime.binding().repository_root == repository
            }
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub struct CapacityPolicy {
    pub global_run_capacity: usize,
    pub per_repository_run_capacity: usize,
    pub human_review_reserved: usize,
}

impl CapacityPolicy {
    pub fn validate(self) -> Result<Self, String> {
        if self.global_run_capacity == 0 || self.per_repository_run_capacity == 0 {
            return Err("run capacities must be greater than zero".into());
        }
        if self.human_review_reserved >= self.global_run_capacity {
            return Err("human review reserve must leave at least one global run slot".into());
        }
        Ok(self)
    }
    fn agent_capacity(self) -> usize {
        self.global_run_capacity - self.human_review_reserved
    }
}

impl Default for CapacityPolicy {
    fn default() -> Self {
        Self {
            global_run_capacity: usize::MAX,
            per_repository_run_capacity: usize::MAX,
            human_review_reserved: 0,
        }
    }
}

type QueuedGate = DurableProgrammeGate;

#[derive(Default)]
struct ProgrammeState {
    gates: HashMap<String, QueuedGate>,
    releasing: HashSet<String>,
    terminal_gates: HashSet<String>,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct DispatchReceipt {
    protocol_version: u16,
    repository_id: String,
    worktree_relative: String,
    repository_root_canonical: bool,
    worktree_canonical: bool,
    shared_git_common_directory: bool,
    external_task_ref: String,
    run_id: String,
    branch: String,
    base_sha: String,
    workspace_id: String,
    pane_id: String,
    pid: Option<u32>,
    process_group_id: Option<i32>,
    state: crate::protocol::ProcessState,
    diagnostic: Option<String>,
    adapter: crate::adapter::AdapterId,
    process_capabilities: crate::adapter::ProcessCapabilities,
    adapter_capabilities: crate::adapter::AdapterCapabilities,
    provider_state: crate::protocol::ProviderState,
}

impl RuntimeRegistry {
    pub fn new(state_dir: impl Into<PathBuf>, scrollback_rows: usize) -> Result<Self, String> {
        Self::with_capacity(state_dir, scrollback_rows, CapacityPolicy::default())
    }

    pub fn with_capacity(
        state_dir: impl Into<PathBuf>,
        scrollback_rows: usize,
        capacity: CapacityPolicy,
    ) -> Result<Self, String> {
        let capacity = capacity.validate()?;
        let state_dir = state_dir.into();
        ensure_private_directory(&state_dir, "state")?;
        let state_dir = fs::canonicalize(&state_dir)
            .map_err(|error| format!("could not canonicalize state directory: {error}"))?;
        let receipts = state_dir.join("dispatches");
        ensure_private_directory(&receipts, "dispatch receipt")?;
        let store = LocalStore::new(&state_dir);
        let mut programme = ProgrammeState::default();
        let mut terminal_gates: HashSet<_> = store
            .list_quarantined_programme_gate_ids()?
            .into_iter()
            .collect();
        for stored_record in store.list_releasing_programme_gates()? {
            let run_id = stored_record.run_id;
            match stored_record.gate {
                Ok(_) => {}
                Err(_) => {
                    store.quarantine_programme_gate("programme-releases", &run_id)?;
                    terminal_gates.insert(run_id);
                    continue;
                }
            }
            let receipt = receipts.join(format!("{run_id}.json"));
            if dispatch_receipt_is_committed(&receipt, &run_id) {
                store.remove_releasing_programme_gate(&run_id)?;
            } else if receipt.exists() {
                // An uncommitted reservation may have crossed the spawn boundary. Its identity is
                // terminal, and the launch guardian kills any process that was actually spawned.
                store.remove_releasing_programme_gate(&run_id)?;
            } else {
                // Claiming alone cannot launch. With no reservation or receipt the exact gate is
                // safely retryable, so put it back in the durable queue.
                store.restore_programme_gate(&run_id)?;
            }
        }
        for stored_record in store.list_programme_gates()? {
            let run_id = stored_record.run_id;
            let gate = match stored_record
                .gate
                .and_then(|gate| restore_durable_gate(&state_dir, gate))
                .and_then(|gate| validate_durable_gate(&gate).map(|()| gate))
                .and_then(|gate| {
                    validate_upstream_dispatch_receipt(&receipts, &gate).map(|()| gate)
                }) {
                Ok(gate) => gate,
                Err(_) => {
                    store.quarantine_programme_gate("programme-gates", &run_id)?;
                    terminal_gates.insert(run_id);
                    continue;
                }
            };
            if programme
                .gates
                .insert(gate.dispatch.run_id.clone(), gate)
                .is_some()
            {
                return Err("duplicate durable programme gate run id".into());
            }
        }
        programme.terminal_gates = terminal_gates;
        let layout = LayoutRegistry::load(&state_dir)?;
        let queues = restore_pane_queues(&store, &layout);
        // Read once, here, rather than consulted on disk from the tick: the flag changes only
        // when somebody asks it to, and the tick must not do file I/O sixty times a minute to
        // learn something that almost never moves.
        let paused = store.queue_paused();
        Ok(Self {
            runs: Mutex::new(HashMap::new()),
            receipts,
            scrollback_rows,
            pane_history_bytes: crate::terminal::PANE_HISTORY_BYTES,
            store,
            programme: Mutex::new(programme),
            capacity,
            layout: Mutex::new(layout),
            pane_sizes: Mutex::new(HashMap::new()),
            process_table: Arc::new(Mutex::new(ProcessTableCache::default())),
            agent_states: Mutex::new(HashMap::new()),
            output_marks: Mutex::new(HashMap::new()),
            reported_states: Mutex::new(HashMap::new()),
            queues: Mutex::new(queues),
            queue_paused: AtomicBool::new(paused),
            auto_feed_trust: Mutex::new(AutoFeedTrust::default()),
            queue_generation: AtomicU64::new(0),
            queue_revisions: Mutex::new(HashMap::new()),
            #[cfg(test)]
            suppress_pane_shells: Mutex::new(false),
            #[cfg(test)]
            retire_pane_shell_stop_failure: Mutex::new(None),
            #[cfg(test)]
            restart_hook: Mutex::new(None),
            #[cfg(test)]
            restart_after_stop_hook: Mutex::new(None),
            #[cfg(test)]
            release_cleanup_hook: Mutex::new(None),
            #[cfg(test)]
            release_restore_hook: Mutex::new(None),
            #[cfg(test)]
            after_launch_before_receipt_hook: Mutex::new(None),
            #[cfg(test)]
            before_runtime_launch_hook: Mutex::new(None),
            #[cfg(test)]
            before_save_receipt_hook: Mutex::new(None),
            #[cfg(test)]
            release_commit_hook: Mutex::new(None),
            #[cfg(test)]
            portfolio_capture_hook: Mutex::new(None),
            #[cfg(test)]
            pane_input_before_final_validation_hook: Mutex::new(None),
            #[cfg(test)]
            workspace_stop_failure: Mutex::new(None),
            #[cfg(test)]
            restart_stop_failure: Mutex::new(None),
            #[cfg(test)]
            receipt_stop_failure: Mutex::new(None),
        })
    }

    pub fn dispatch(
        &self,
        request: DispatchRequest,
    ) -> Result<RuntimeSnapshot, (ErrorCode, String)> {
        self.dispatch_with_gate_authorization(request, false, None)
    }

    pub fn launch_into_pane(
        &self,
        request: DispatchRequest,
        workspace_id: String,
        pane_id: String,
    ) -> Result<RuntimeSnapshot, (ErrorCode, String)> {
        self.dispatch_with_gate_authorization(request, false, Some((workspace_id, pane_id)))
    }

    /// Takes the request whole rather than seven loose strings, so the closed shape the protocol
    /// defines is the shape this reasons about, and a field added there does not reshuffle a
    /// positional argument list here.
    pub fn terminal_launch(
        &self,
        launch: crate::protocol::TerminalLaunchRequest,
    ) -> Result<RuntimeSnapshot, (ErrorCode, String)> {
        let crate::protocol::TerminalLaunchRequest {
            workspace_id,
            pane_id,
            run_id,
            profile,
            runtime_directory,
            arguments: supplied_arguments,
            external_task_ref,
        } = launch;
        validate_external_run_id(&run_id).map_err(|m| (ErrorCode::InvalidBinding, m))?;
        // Bounded before it reaches the binding, so the one field this deliberately closed shape
        // gained can never be walked into a path by anything downstream.
        crate::protocol::validate_external_task_ref(&external_task_ref)
            .map_err(|m| (ErrorCode::InvalidBinding, m))?;
        let directory = canonical_terminal_directory(Path::new(&runtime_directory))
            .map_err(|m| (ErrorCode::InvalidBinding, m))?;
        let adapter_id = crate::adapter::AdapterId::from(profile);
        // Supplied arguments win, which is how a resume reaches the agent. The fixture keeps its
        // built-in script when nothing is supplied, so an ordinary launch is unchanged.
        let arguments = if !supplied_arguments.is_empty() {
            supplied_arguments
        } else if adapter_id == crate::adapter::AdapterId::Fixture {
            vec![
                "-c".into(),
                "printf 'Dock-owned fixture ready\\n'; sleep 30".into(),
            ]
        } else {
            vec![]
        };
        let request = DispatchRequest {
            repository_root: directory.display().to_string(),
            external_task_ref: external_task_ref.clone(),
            run_id,
            worktree: directory.display().to_string(),
            adapter: AdapterSelection {
                id: adapter_id,
                executable: None,
                arguments,
            },
        };
        let binding = RunBinding {
            binding_kind: BindingKind::Terminal,
            repository_root: directory.clone(),
            external_task_ref: String::new(),
            run_id: request.run_id.clone(),
            worktree: directory,
            branch: String::new(),
            base_sha: String::new(),
            workspace_id: workspace_id.clone(),
            pane_id: pane_id.clone(),
        };
        self.dispatch_with_binding(
            request,
            false,
            Some((workspace_id, pane_id)),
            Some(binding),
            false,
        )
    }

    /// Every Dock *terminal* pane is a working terminal from the moment it exists. This is a
    /// Dock-created PTY in a Dock-created process group like any other owned run, so the
    /// no-adoption invariant is untouched.
    ///
    /// A Board pane gets nothing. Not a PTY that is ignored — no run at all, which is why the
    /// refusal is here rather than at the three places that call this. Pane create, pane split
    /// and `revive_restored_panes` each reach this line, and a guard written three times is a
    /// guard the fourth caller forgets; the property being protected is the absence of a
    /// process, which is exactly the kind of thing nobody notices has stopped being true.
    fn launch_pane_shell(&self, workspace_id: &str, pane_id: &str) {
        // Bound rather than tested inline so the layout lock is released on this line: every
        // launch path below takes `runs` and then `layout`, and holding it across the dispatch
        // would invert that order.
        let kind = self
            .layout
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .pane_kind(workspace_id, pane_id);
        if kind == Some(PaneKind::Board) {
            return;
        }
        #[cfg(test)]
        if *self
            .suppress_pane_shells
            .lock()
            .unwrap_or_else(|p| p.into_inner())
        {
            return;
        }
        let Some(directory) = std::env::current_dir()
            .ok()
            .and_then(|directory| canonical_terminal_directory(&directory).ok())
        else {
            return;
        };
        self.reclaim_pane_shell_identity(workspace_id, pane_id);
        let run_id = pane_shell_run_id(workspace_id, pane_id);
        let request = DispatchRequest {
            repository_root: directory.display().to_string(),
            external_task_ref: String::new(),
            run_id: run_id.clone(),
            worktree: directory.display().to_string(),
            adapter: AdapterSelection {
                id: crate::adapter::AdapterId::Shell,
                executable: None,
                arguments: vec!["-l".into()],
            },
        };
        let binding = RunBinding {
            binding_kind: BindingKind::Terminal,
            repository_root: directory.clone(),
            external_task_ref: String::new(),
            run_id,
            worktree: directory,
            branch: String::new(),
            base_sha: String::new(),
            workspace_id: workspace_id.to_owned(),
            pane_id: pane_id.to_owned(),
        };
        // A shell that fails to launch must not fail workspace creation; the pane still exists
        // and stays operable for close and for a later explicit launch into it.
        let _ = self.dispatch_with_binding(
            request,
            false,
            Some((workspace_id.to_owned(), pane_id.to_owned())),
            Some(binding),
            true,
        );
    }

    /// Gives every pane restored from durable layout a working shell, so a pane that survives a
    /// daemon restart is a pane the user can type into.
    ///
    /// This is not adoption and cannot become adoption. The durable layout deliberately records
    /// no PIDs, PGIDs, or screen content, so there is no old process to reattach to and none is
    /// consulted: each restored pane gets a brand-new Dock-created PTY in a brand-new
    /// Dock-created process group, exactly as `Create` and `Split` do. Adoption would mean
    /// binding a pane to a process Dock did not spawn; nothing here ever names such a process.
    ///
    /// Called once at daemon start-up rather than from the constructor, because constructing a
    /// registry is not by itself a statement that panes should start running.
    pub fn revive_restored_panes(&self) {
        let targets: Vec<(String, String)> = self
            .layout
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .snapshot()
            .workspaces
            .into_iter()
            .flat_map(|workspace| {
                let workspace_id = workspace.workspace_id;
                workspace
                    .panes
                    .into_values()
                    .filter(|pane| pane.run_id.is_none())
                    .map(move |pane| (workspace_id.clone(), pane.pane_id))
            })
            .collect();
        for (workspace_id, pane_id) in targets {
            self.launch_pane_shell(&workspace_id, &pane_id);
        }
    }

    /// Retires the placeholder shell a committed dispatch has just displaced from a pane. Called
    /// only after that dispatch is irrevocable, so a refused or rolled-back launch always leaves
    /// the pane's working shell running and bound.
    fn retire_pane_shell(&self, workspace_id: &str, pane_id: &str) {
        let run_id = pane_shell_run_id(workspace_id, pane_id);
        let slot = self
            .runs
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .get(&run_id)
            .cloned();
        if let Some(slot) = slot {
            // Never block in process shutdown while holding a registry or layout lock.
            let _transition = slot.transition.lock().unwrap_or_else(|p| p.into_inner());
            let entry = self
                .runs
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .get(&run_id)
                .filter(|current| Arc::ptr_eq(&current.transition, &slot.transition))
                .and_then(RuntimeSlot::active)
                .cloned();
            #[cfg(test)]
            let stop = match self
                .retire_pane_shell_stop_failure
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .take()
            {
                Some(message) => Err(message),
                None => entry.as_ref().map_or(Ok(()), |entry| entry.runtime.stop()),
            };
            #[cfg(not(test))]
            let stop = entry.as_ref().map_or(Ok(()), |entry| entry.runtime.stop());
            // The exact group staying live means its authority and pane binding stay put too.
            // `reclaim_pane_shell_identity` is what stops this becoming permanent.
            if stop.is_err() {
                return;
            }
            let mut runs = self.runs.lock().unwrap_or_else(|p| p.into_inner());
            let mut layout = self.layout.lock().unwrap_or_else(|p| p.into_inner());
            if !runs
                .get(&run_id)
                .is_some_and(|current| Arc::ptr_eq(&current.transition, &slot.transition))
            {
                return;
            }
            // A no-op unless the pane still points at the shell, which is the case only when
            // this displaced it rather than a rollback having already restored something else.
            layout.unbind_run(workspace_id, pane_id, &run_id);
            runs.remove(&run_id);
        }
        self.clear_pane_shell_reservation(&run_id);
    }

    /// A pane's shell slot is always recoverable. `validate_external_run_id` reserves the
    /// `dock_sh_` namespace, so any run wearing this pane's shell identity is definitionally
    /// Dock's own earlier shell for this pane; if it is no longer the pane's binding it is stale
    /// and is reclaimed here rather than being allowed to hold the identity forever.
    ///
    /// This closes a class, not two paths. A shell whose `stop` failed during retirement, one
    /// skipped because the commit path errored after the replacement went `Active`, and any third
    /// route that leaves the same residue all end identically: the pane's next shell refused as a
    /// duplicate run id, leaving a recreated pane permanently inert — the exact failure
    /// auto-launch exists to prevent.
    fn reclaim_pane_shell_identity(&self, workspace_id: &str, pane_id: &str) {
        let run_id = pane_shell_run_id(workspace_id, pane_id);
        let slot = self
            .runs
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .get(&run_id)
            .cloned();
        if let Some(slot) = slot {
            // Serialises against an in-flight launch of this same identity, and is never held
            // while a registry or layout lock is, so process shutdown cannot block either.
            let _transition = slot.transition.lock().unwrap_or_else(|p| p.into_inner());
            let entry = self
                .runs
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .get(&run_id)
                .filter(|current| Arc::ptr_eq(&current.transition, &slot.transition))
                .and_then(RuntimeSlot::active)
                .cloned();
            // Best-effort by design. A group that refuses to die is already unreachable — it has
            // no pane and no caller can name it — so keeping the identity pinned to it would
            // trade a survivable leak for a permanently unusable pane.
            if let Some(entry) = &entry {
                let _ = entry.runtime.stop();
            }
            let mut runs = self.runs.lock().unwrap_or_else(|p| p.into_inner());
            let mut layout = self.layout.lock().unwrap_or_else(|p| p.into_inner());
            if runs
                .get(&run_id)
                .is_some_and(|current| Arc::ptr_eq(&current.transition, &slot.transition))
            {
                layout.unbind_run(workspace_id, pane_id, &run_id);
                runs.remove(&run_id);
            }
        }
        self.clear_pane_shell_reservation(&run_id);
    }

    /// A pane shell's identity belongs to the pane, not to one launch, so its durable reservation
    /// must not outlive the run holding it: otherwise the pane's next shell is refused as a
    /// duplicate run id and the pane is left silently inert. Only ever clears an identity that no
    /// live run holds.
    fn clear_pane_shell_reservation(&self, run_id: &str) {
        if self
            .runs
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .contains_key(run_id)
        {
            return;
        }
        // Guarded by `exists` so the common case costs no directory fsync.
        if let Ok(receipt) = self.receipt_path(run_id)
            && receipt.exists()
        {
            let _ = rollback_run_id_reservation(&receipt);
        }
    }

    pub fn layout(&self) -> LayoutSnapshot {
        self.reconcile_failed_dispatches();
        let states: Vec<_> = self
            .runs
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .iter()
            .filter_map(|(id, slot)| {
                let entry = slot.active()?;
                let state = if matches!(
                    entry.runtime.snapshot().state,
                    crate::protocol::ProcessState::Running
                ) {
                    PaneRuntime::Running
                } else {
                    PaneRuntime::Exited
                };
                Some((id.clone(), state))
            })
            .collect();
        let mut layout = self.layout.lock().unwrap_or_else(|p| p.into_inner());
        for (run_id, state) in states {
            layout.set_runtime(&run_id, state);
        }
        layout.snapshot()
    }

    /// Retry terminal dispatch rollback without ever reviving process authority. The exact slot
    /// remains reserved until both durable cleanup operations have succeeded.
    fn reconcile_failed_dispatches(&self) {
        let stopping: Vec<_> = self
            .runs
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .iter()
            .filter_map(|(run_id, slot)| match &slot.state {
                RuntimeSlotState::ReceiptRollbackStopping {
                    entry,
                    layout,
                    receipt,
                    repository_root,
                } => Some((
                    run_id.clone(),
                    Arc::clone(&slot.transition),
                    entry.clone(),
                    layout.clone(),
                    receipt.clone(),
                    repository_root.clone(),
                    slot.pane_shell,
                )),
                _ => None,
            })
            .collect();
        for (run_id, transition, entry, layout, receipt, repository_root, pane_shell) in stopping {
            // Never block in process shutdown while holding a registry or layout lock.
            let _transition = transition.lock().unwrap_or_else(|p| p.into_inner());
            if entry.runtime.stop().is_err() {
                continue;
            }
            let mut runs = self.runs.lock().unwrap_or_else(|p| p.into_inner());
            let exact = runs.get(&run_id).is_some_and(|slot| {
                Arc::ptr_eq(&slot.transition, &transition)
                    && matches!(
                        &slot.state,
                        RuntimeSlotState::ReceiptRollbackStopping { entry: current, .. }
                            if Arc::ptr_eq(&current.runtime, &entry.runtime)
                    )
            });
            if exact {
                runs.insert(
                    run_id,
                    RuntimeSlot {
                        transition: Arc::clone(&transition),
                        state: RuntimeSlotState::RollbackPending {
                            repository_root,
                            layout: Some(layout),
                            receipt,
                        },
                        pane_shell,
                    },
                );
            }
        }
        let mut runs = self.runs.lock().unwrap_or_else(|p| p.into_inner());
        let pending: Vec<_> = runs
            .iter()
            .filter_map(|(run_id, slot)| match &slot.state {
                RuntimeSlotState::RollbackPending {
                    layout, receipt, ..
                } => Some((run_id.clone(), layout.clone(), receipt.clone())),
                _ => None,
            })
            .collect();
        if pending.is_empty() {
            return;
        }
        let mut layout_registry = self.layout.lock().unwrap_or_else(|p| p.into_inner());
        for (run_id, rollback, receipt) in pending {
            if let Some(rollback) = rollback {
                if layout_registry.rollback_bound_pane(rollback).is_err() {
                    continue;
                }
                if let Some(RuntimeSlot {
                    state: RuntimeSlotState::RollbackPending { layout, .. },
                    ..
                }) = runs.get_mut(&run_id)
                {
                    // Advance this state before attempting the independent receipt cleanup. A
                    // later retry must never reapply an already durable layout inverse.
                    *layout = None;
                }
            }
            if rollback_run_id_reservation(&receipt).is_ok() {
                runs.remove(&run_id);
            }
        }
    }

    pub fn workspace(
        &self,
        request: WorkspaceRequest,
    ) -> Result<Option<WorkspaceLayout>, (ErrorCode, String)> {
        if let WorkspaceRequest::Respawn {
            workspace_id,
            pane_id,
        } = &request
        {
            let existing = {
                let layout = self.layout.lock().unwrap_or_else(|p| p.into_inner());
                if !layout.pane_exists(workspace_id, pane_id) {
                    return Err((
                        ErrorCode::InvalidLayout,
                        "respawn target pane does not exist".into(),
                    ));
                }
                // Refused rather than quietly doing nothing. `launch_pane_shell` would walk past
                // a board and this request would answer with an unchanged layout, which reads as
                // Dock having failed at something it in fact declined to do.
                if layout.pane_kind(workspace_id, pane_id) == Some(PaneKind::Board) {
                    return Err((
                        ErrorCode::UnsupportedOperation,
                        "that pane is a board; there is no process in it to restart".into(),
                    ));
                }
                layout.pane_run(workspace_id, pane_id)
            };
            // Respawn is a recovery path, never a way to displace something that is alive: a
            // running agent must not be killable by a stray keystroke.
            let alive = existing.as_deref().is_some_and(|run_id| {
                self.runs
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .get(run_id)
                    .and_then(RuntimeSlot::active)
                    .is_some_and(|entry| {
                        matches!(
                            entry.runtime.snapshot().state,
                            crate::protocol::ProcessState::Running
                        )
                    })
            });
            if alive {
                return Err((
                    ErrorCode::UnsupportedOperation,
                    "pane already has a running process; close it before respawning".into(),
                ));
            }
            self.launch_pane_shell(workspace_id, pane_id);
            return Ok(self
                .layout()
                .workspaces
                .into_iter()
                .find(|workspace| &workspace.workspace_id == workspace_id));
        }
        if let WorkspaceRequest::Close {
            workspace_id,
            pane_id,
        } = &request
        {
            let run_id = self
                .layout
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .pane_run(workspace_id, pane_id);
            let slot = run_id.as_ref().and_then(|run_id| {
                self.runs
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .get(run_id)
                    .cloned()
            });
            // The identity-scoped transition lock can wait, but no registry or layout lock is
            // held. Once acquired, revalidate both the pane binding and exact runtime capability.
            let _transition = slot
                .as_ref()
                .map(|slot| slot.transition.lock().unwrap_or_else(|p| p.into_inner()));
            let entry = match (&run_id, &slot) {
                (Some(run_id), Some(slot)) => {
                    let runs = self.runs.lock().unwrap_or_else(|p| p.into_inner());
                    let current = runs
                        .get(run_id)
                        .filter(|current| Arc::ptr_eq(&current.transition, &slot.transition));
                    current.and_then(RuntimeSlot::active).cloned()
                }
                _ => None,
            };
            let mut closed_pane_queue: Option<QueueKey> = None;
            if run_id.is_some() && slot.is_some() && entry.is_none() {
                return Err((
                    ErrorCode::Internal,
                    "pane run is transitioning; retry close".into(),
                ));
            }
            if self
                .layout
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .pane_run(workspace_id, pane_id)
                != run_id
            {
                return Err((
                    ErrorCode::InvalidLayout,
                    "pane binding changed during close".into(),
                ));
            }

            let stop_result = if let Some(entry) = &entry {
                #[cfg(test)]
                if let Some(message) = self
                    .workspace_stop_failure
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .take()
                {
                    Err(message)
                } else {
                    entry.runtime.stop()
                }
                #[cfg(not(test))]
                {
                    entry.runtime.stop()
                }
            } else {
                Ok(())
            };

            if let Err(message) = stop_result {
                return Err((ErrorCode::Internal, message));
            }
            let mut runs = self.runs.lock().unwrap_or_else(|p| p.into_inner());
            let mut layout = self.layout.lock().unwrap_or_else(|p| p.into_inner());
            if layout.pane_run(workspace_id, pane_id) != run_id {
                return Err((
                    ErrorCode::InvalidLayout,
                    "pane binding changed during close".into(),
                ));
            }
            if let (Some(run_id), Some(slot), Some(entry)) = (&run_id, &slot, &entry) {
                let exact = runs.get(run_id).is_some_and(|current| {
                    Arc::ptr_eq(&current.transition, &slot.transition)
                        && current
                            .active()
                            .is_some_and(|current| Arc::ptr_eq(&current.runtime, &entry.runtime))
                });
                if !exact {
                    return Err((
                        ErrorCode::Internal,
                        "run identity changed during close".into(),
                    ));
                }
            }
            let result = layout.close(workspace_id, pane_id);
            if result.is_ok() {
                // Queued for a pane that no longer exists, so the entries have nowhere to go and
                // the file would otherwise outlive the pane forever.
                closed_pane_queue = Some((workspace_id.clone(), pane_id.clone()));
            }
            if let Some(run_id) = &run_id {
                // Stop irrevocably retired this capability. A persistence failure retains an
                // Exited pane marker for a safe Close retry, but never dead Active authority.
                runs.remove(run_id);
                if result.is_err() {
                    layout.set_runtime(run_id, PaneRuntime::Exited);
                }
            }
            drop(layout);
            drop(runs);
            if let Some((workspace_id, pane_id)) = closed_pane_queue {
                self.forget_pane_queue(&workspace_id, &pane_id);
            }
            return result.map_err(layout_error);
        }
        // Panes that gained a terminal in this request, launched below once every registry lock
        // is released: launching takes `runs` then `layout`, and holding `layout` here would
        // invert that order.
        let mut new_panes = Vec::new();
        let mut layout = self.layout.lock().unwrap_or_else(|p| p.into_inner());
        let result = match request {
            WorkspaceRequest::Inspect => {
                return Err((
                    ErrorCode::UnsupportedOperation,
                    "inspect returns the complete layout".into(),
                ));
            }
            WorkspaceRequest::Create {
                workspace_id,
                name,
                pane_id,
            } => layout
                .create_workspace(workspace_id.clone(), name, pane_id.clone())
                .inspect(|_| new_panes.push((workspace_id, pane_id)))
                .map(Some),
            WorkspaceRequest::Split {
                workspace_id,
                pane_id,
                new_pane_id,
                axis,
                kind,
            } => layout
                .split(&workspace_id, &pane_id, new_pane_id.clone(), axis, kind)
                // Pushed whatever the kind is, because the decision about a shell belongs inside
                // `launch_pane_shell` rather than at each of its callers. A caller that filtered
                // here would be a caller that could forget to.
                .inspect(|_| new_panes.push((workspace_id, new_pane_id)))
                .map(Some),
            WorkspaceRequest::Focus {
                workspace_id,
                pane_id,
            } => layout.focus(&workspace_id, &pane_id).map(Some),
            WorkspaceRequest::Resize {
                workspace_id,
                pane_id,
                ratio_milli,
            } => layout
                .resize(&workspace_id, &pane_id, ratio_milli)
                .map(Some),
            WorkspaceRequest::Rename {
                workspace_id,
                pane_id,
                name,
            } => layout
                .rename(&workspace_id, pane_id.as_deref(), name)
                .map(Some),
            WorkspaceRequest::Close { .. } => {
                unreachable!("close requests are handled by the ownership-safe path above")
            }
            WorkspaceRequest::Respawn { .. } => {
                unreachable!("respawn requests are handled by the ownership-safe path above")
            }
        };
        drop(layout);
        let result = result.map_err(layout_error)?;
        for (workspace_id, pane_id) in &new_panes {
            self.launch_pane_shell(workspace_id, pane_id);
        }
        // The shell binds after the topology change, so re-read rather than returning the
        // pre-launch snapshot the caller would otherwise see as an empty pane.
        if let Some((workspace_id, _)) = new_panes.first() {
            let refreshed = self
                .layout()
                .workspaces
                .into_iter()
                .find(|workspace| &workspace.workspace_id == workspace_id);
            if refreshed.is_some() {
                return Ok(refreshed);
            }
        }
        Ok(result)
    }

    fn dispatch_with_gate_authorization(
        &self,
        request: DispatchRequest,
        gate_release_authorized: bool,
        launch_target: Option<(String, String)>,
    ) -> Result<RuntimeSnapshot, (ErrorCode, String)> {
        self.dispatch_with_binding(request, gate_release_authorized, launch_target, None, false)
    }

    fn dispatch_with_binding(
        &self,
        request: DispatchRequest,
        gate_release_authorized: bool,
        launch_target: Option<(String, String)>,
        binding: Option<RunBinding>,
        pane_shell: bool,
    ) -> Result<RuntimeSnapshot, (ErrorCode, String)> {
        self.reconcile_failed_dispatches();
        let mut binding = match binding {
            Some(binding) => binding,
            None => validate_binding(&request).map_err(|m| (ErrorCode::InvalidBinding, m))?,
        };
        if let Some((workspace_id, pane_id)) = &launch_target {
            binding.workspace_id = workspace_id.clone();
            binding.pane_id = pane_id.clone();
        }
        // Reject an already gated identity before adapter discovery. This is repeated under the
        // run lock below so a concurrent queue cannot cross the dispatch admission boundary.
        {
            let programme = self.programme.lock().unwrap_or_else(|p| p.into_inner());
            self.authorize_programme_dispatch(
                &programme,
                &request.run_id,
                gate_release_authorized,
            )?;
        }
        // Adapter discovery is intentionally before the registry lock, receipt reservation, and
        // runtime construction: a missing binary must leave no run, pane, or durable receipt.
        let adapter = request
            .adapter
            .resolve()
            .map_err(|m| (ErrorCode::AdapterUnavailable, m))?;
        let mut runs = self.runs.lock().unwrap_or_else(|p| p.into_inner());
        // All operations needing both registries take runs before programme. Queueing uses the
        // same order, so the identity check and subsequent run reservation cannot deadlock or be
        // bypassed by a concurrent direct dispatch.
        let programme = self.programme.lock().unwrap_or_else(|p| p.into_inner());
        self.authorize_programme_dispatch(&programme, &request.run_id, gate_release_authorized)?;
        let receipt = self
            .receipt_path(&request.run_id)
            .map_err(|m| (ErrorCode::InvalidBinding, m))?;
        if runs.contains_key(&request.run_id) || receipt.exists() {
            return Err((
                ErrorCode::DuplicateRunId,
                format!("run id {:?} already exists", request.run_id),
            ));
        }
        self.check_capacity(&runs, &binding.repository_root)?;
        let mut layout = self.layout.lock().unwrap_or_else(|p| p.into_inner());
        if let Some((workspace_id, pane_id)) = &launch_target {
            // A pane's auto-launched shell is a placeholder, not an occupant: it must not make
            // the pane permanently un-launchable-into. Every other refusal — missing workspace,
            // missing pane, a real run already bound — still applies, and the shell itself is
            // only retired once this dispatch is irrevocably committed far below.
            let placeholder = layout.pane_run(workspace_id, pane_id).as_deref()
                == Some(pane_shell_run_id(workspace_id, pane_id).as_str());
            if let Err(message) = layout.check_launch_target(workspace_id, pane_id)
                && !placeholder
            {
                return Err(layout_error(message));
            }
        }
        layout
            .check_bind_capacity(&binding.workspace_id, &binding.pane_id)
            .map_err(|message| (ErrorCode::CapacityExceeded, message))?;
        reserve_run_id(&receipt).map_err(|m| (ErrorCode::Internal, m))?;
        let transition = Arc::new(Mutex::new(()));
        let transition_guard = transition.lock().unwrap_or_else(|p| p.into_inner());
        runs.insert(
            request.run_id.clone(),
            RuntimeSlot {
                transition: Arc::clone(&transition),
                state: RuntimeSlotState::Launching {
                    repository_root: binding.repository_root.clone(),
                },
                pane_shell,
            },
        );
        let layout_rollback = match layout.ensure_bound_pane(
            binding.workspace_id.clone(),
            binding.pane_id.clone(),
            binding.run_id.clone(),
        ) {
            Ok(rollback) => rollback,
            Err(message) => {
                runs.remove(&request.run_id);
                drop(transition_guard);
                rollback_run_id_reservation(&receipt).map_err(|rollback| {
                    (
                        ErrorCode::Internal,
                        format!("{message}; could not roll back dispatch reservation: {rollback}"),
                    )
                })?;
                return Err((ErrorCode::InvalidLayout, message));
            }
        };
        drop(programme);
        drop(layout);
        drop(runs);
        #[cfg(test)]
        if let Some(hook) = self
            .before_runtime_launch_hook
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .take()
        {
            hook();
        }
        let size = self.pane_size(&binding.workspace_id, &binding.pane_id);
        let bound_pane = (binding.workspace_id.clone(), binding.pane_id.clone());
        let runtime = Arc::new(OwnedRuntime::launch(
            binding,
            adapter,
            self.scrollback_rows,
            self.pane_history_bytes,
            size,
        ));
        let snapshot = runtime.snapshot();
        #[cfg(test)]
        if let Some(hook) = self
            .after_launch_before_receipt_hook
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .clone()
        {
            hook();
            return Err((
                ErrorCode::Internal,
                "injected crash after spawn before receipt commit".into(),
            ));
        }
        #[cfg(test)]
        if let Some(hook) = self
            .before_save_receipt_hook
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .take()
        {
            hook();
        }
        let commit_error = if snapshot.state == crate::protocol::ProcessState::FailedToLaunch {
            Some(
                snapshot
                    .diagnostic
                    .clone()
                    .unwrap_or_else(|| "adapter process failed to launch".into()),
            )
        } else {
            save_receipt(&receipt, &snapshot).err()
        };
        if let Some(message) = commit_error {
            #[cfg(test)]
            let stop = if let Some(message) = self
                .receipt_stop_failure
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .take()
            {
                Err(message)
            } else {
                runtime.stop()
            };
            #[cfg(not(test))]
            let stop = runtime.stop();
            let mut failures = Vec::new();
            let mut runs = self.runs.lock().unwrap_or_else(|p| p.into_inner());
            let reserved = runs.get(&request.run_id).is_some_and(|slot| {
                Arc::ptr_eq(&slot.transition, &transition)
                    && matches!(slot.state, RuntimeSlotState::Launching { .. })
            });
            if reserved {
                if let Err(error) = stop {
                    runs.insert(
                        request.run_id.clone(),
                        RuntimeSlot {
                            transition: Arc::clone(&transition),
                            state: RuntimeSlotState::ReceiptRollbackStopping {
                                repository_root: runtime.binding().repository_root.clone(),
                                entry: RuntimeEntry {
                                    runtime,
                                    selection: request.adapter,
                                },
                                layout: layout_rollback,
                                receipt,
                            },
                            pane_shell,
                        },
                    );
                    drop(runs);
                    drop(transition_guard);
                    return Err((
                        ErrorCode::Internal,
                        format!("{message}; could not stop rolled-back launch: {error}"),
                    ));
                }
                let layout_restore = self
                    .layout
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .rollback_bound_pane(layout_rollback.clone());
                if let Err(error) = layout_restore {
                    failures.push(format!("could not restore dispatch layout: {error}"));
                    // Runtime display state is process-local and safe to retire even when the
                    // durable topology inverse cannot yet be written.
                    self.layout
                        .lock()
                        .unwrap_or_else(|p| p.into_inner())
                        .set_runtime(&request.run_id, PaneRuntime::Exited);
                    runs.insert(
                        request.run_id.clone(),
                        RuntimeSlot {
                            transition: Arc::clone(&transition),
                            state: RuntimeSlotState::RollbackPending {
                                repository_root: runtime.binding().repository_root.clone(),
                                layout: Some(layout_rollback),
                                receipt: receipt.clone(),
                            },
                            pane_shell,
                        },
                    );
                } else if let Err(error) = rollback_run_id_reservation(&receipt) {
                    failures.push(format!("could not roll back dispatch reservation: {error}"));
                    runs.insert(
                        request.run_id.clone(),
                        RuntimeSlot {
                            transition: Arc::clone(&transition),
                            state: RuntimeSlotState::RollbackPending {
                                repository_root: runtime.binding().repository_root.clone(),
                                layout: None,
                                receipt: receipt.clone(),
                            },
                            pane_shell,
                        },
                    );
                } else {
                    runs.remove(&request.run_id);
                }
            }
            drop(runs);
            drop(transition_guard);
            if !reserved {
                failures.push("exact launch reservation changed during rollback".into());
            }
            let detail = if failures.is_empty() {
                message
            } else {
                format!("{message}; {}", failures.join("; "))
            };
            let code = if snapshot.state == crate::protocol::ProcessState::FailedToLaunch {
                ErrorCode::AdapterUnavailable
            } else {
                ErrorCode::Internal
            };
            return Err((code, detail));
        }
        let run_id = snapshot.run_id.clone();
        let mut runs = self.runs.lock().unwrap_or_else(|p| p.into_inner());
        let reserved = runs.get(&run_id).is_some_and(|slot| {
            Arc::ptr_eq(&slot.transition, &transition)
                && matches!(slot.state, RuntimeSlotState::Launching { .. })
        });
        if !reserved {
            return Err((ErrorCode::Internal, "run launch reservation changed".into()));
        }
        drop(transition_guard);
        runs.insert(
            run_id.clone(),
            RuntimeSlot {
                transition: Arc::clone(&transition),
                state: RuntimeSlotState::Active(RuntimeEntry {
                    runtime,
                    selection: request.adapter,
                }),
                pane_shell,
            },
        );
        if gate_release_authorized {
            let mut programme = self.programme.lock().unwrap_or_else(|p| p.into_inner());
            self.authorize_programme_dispatch(&programme, &run_id, true)?;
            #[cfg(test)]
            if let Some(hook) = self
                .release_commit_hook
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .take()
            {
                hook();
            }
            programme.gates.remove(&run_id);
            programme.releasing.remove(&run_id);
        }
        drop(runs);
        if !pane_shell {
            // Every refusal and every rollback above leaves the pane's shell running and bound —
            // `ensure_bound_pane` records it as the prior binding and `rollback_bound_pane`
            // restores it. Only here, with the new run irrevocably owning the pane, is the
            // shell it replaced unreachable, so only here is it safe to retire. Retiring any
            // earlier would leave a pane permanently inert after a refused dispatch.
            self.retire_pane_shell(&bound_pane.0, &bound_pane.1);
        }
        Ok(snapshot)
    }

    fn authorize_programme_dispatch(
        &self,
        programme: &ProgrammeState,
        run_id: &str,
        gate_release_authorized: bool,
    ) -> Result<(), (ErrorCode, String)> {
        let queued = programme.gates.contains_key(run_id);
        let releasing = programme.releasing.contains(run_id);
        if programme.terminal_gates.contains(run_id) {
            return Err((
                ErrorCode::GateBlocked,
                format!("run id {run_id:?} is sealed by an invalid durable programme gate"),
            ));
        }
        if gate_release_authorized && queued && releasing {
            return Ok(());
        }
        if queued || releasing {
            let state = if releasing { "releasing" } else { "queued" };
            return Err((
                ErrorCode::GateBlocked,
                format!(
                    "run id {run_id:?} is {state} in programme state; direct dispatch is forbidden and the dependency gate must be released explicitly"
                ),
            ));
        }
        if gate_release_authorized {
            return Err((
                ErrorCode::GateBlocked,
                format!("run id {run_id:?} is not an authorized programme gate release"),
            ));
        }
        Ok(())
    }

    pub fn queue_gated(
        &self,
        request: DispatchRequest,
        upstream_run_id: String,
        required_route: ReviewRoute,
    ) -> Result<DependencyGateSnapshot, (ErrorCode, String)> {
        let binding = validate_binding(&request).map_err(|m| (ErrorCode::InvalidBinding, m))?;
        validate_durable_adapter(&request).map_err(|m| (ErrorCode::InvalidBinding, m))?;
        request
            .adapter
            .resolve()
            .map_err(|m| (ErrorCode::AdapterUnavailable, m))?;
        let runs = self.runs.lock().unwrap_or_else(|p| p.into_inner());
        let upstream = runs.get(&upstream_run_id).ok_or_else(|| {
            (
                ErrorCode::RunNotFound,
                format!("upstream run id {upstream_run_id:?} is not active in this daemon"),
            )
        })?;
        let upstream_snapshot = upstream
            .active()
            .ok_or_else(|| {
                (
                    ErrorCode::RunNotFound,
                    "upstream run is still launching".into(),
                )
            })?
            .runtime
            .snapshot();
        if runs.contains_key(&request.run_id)
            || self
                .receipt_path(&request.run_id)
                .map_err(|m| (ErrorCode::InvalidBinding, m))?
                .exists()
        {
            return Err((
                ErrorCode::DuplicateRunId,
                format!("run id {:?} already exists", request.run_id),
            ));
        }
        let mut programme = self.programme.lock().unwrap_or_else(|p| p.into_inner());
        if programme.gates.contains_key(&request.run_id)
            || programme.terminal_gates.contains(&request.run_id)
        {
            return Err((
                ErrorCode::DuplicateGate,
                format!(
                    "a queued gate for downstream run {:?} already exists",
                    request.run_id
                ),
            ));
        }
        let gate = QueuedGate {
            schema_version: 2,
            upstream_run_id,
            upstream_repository_id: repository_id(Path::new(&upstream_snapshot.repository_root)),
            downstream_repository_id: repository_id(&binding.repository_root),
            dispatch: request,
            required_route,
        };
        let snapshot = self.gate_snapshot(&gate);
        let stored_gate = gate_for_storage(&self.receipts, &gate)
            .map_err(|message| (ErrorCode::Internal, message))?;
        self.store
            .save_programme_gate(&stored_gate)
            .map_err(|m| (ErrorCode::Internal, m))?;
        programme.gates.insert(gate.dispatch.run_id.clone(), gate);
        Ok(snapshot)
    }

    pub fn release_gate(
        &self,
        downstream_run_id: &str,
    ) -> Result<RuntimeSnapshot, (ErrorCode, String)> {
        let request = {
            let mut programme = self.programme.lock().unwrap_or_else(|p| p.into_inner());
            let gate = programme.gates.get(downstream_run_id).ok_or_else(|| {
                (
                    ErrorCode::GateNotFound,
                    format!("no dependency gate exists for downstream run {downstream_run_id:?}"),
                )
            })?;
            let status = self.gate_snapshot(gate);
            if status.state != GateState::Ready {
                return Err((
                    ErrorCode::GateBlocked,
                    status
                        .validation_reason
                        .unwrap_or_else(|| "dependency gate is not ready".into()),
                ));
            }
            let request = gate.dispatch.clone();
            if !programme.releasing.insert(downstream_run_id.to_owned()) {
                return Err((
                    ErrorCode::GateBlocked,
                    format!("dependency gate {downstream_run_id:?} is already being released"),
                ));
            }
            request
        };
        if let Err(message) = self.store.claim_programme_gate(downstream_run_id) {
            self.programme
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .releasing
                .remove(downstream_run_id);
            return Err((ErrorCode::Internal, message));
        }
        match self.dispatch_with_gate_authorization(request, true, None) {
            Ok(snapshot) => {
                // Dispatch and its durable receipt are the commit point. Failure to remove this
                // claim must not turn a launched run into a retryable release; startup reconciles
                // a leftover claim against the receipt without launching again.
                #[cfg(test)]
                if let Some(hook) = self
                    .release_cleanup_hook
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .take()
                {
                    hook();
                }
                let _ = self
                    .store
                    .remove_releasing_programme_gate(downstream_run_id);
                Ok(snapshot)
            }
            Err(error) => {
                #[cfg(test)]
                if let Some(hook) = self
                    .release_restore_hook
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .take()
                {
                    hook();
                }
                let restore = self.store.restore_programme_gate(downstream_run_id);
                self.programme
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .releasing
                    .remove(downstream_run_id);
                match restore {
                    Ok(()) => Err(error),
                    Err(message) => Err((
                        ErrorCode::Internal,
                        format!(
                            "release failed and its durable gate could not be restored: {message}"
                        ),
                    )),
                }
            }
        }
    }

    pub fn inspect_programme(&self) -> ProgrammeSnapshot {
        let runs = self.runs.lock().unwrap_or_else(|p| p.into_inner());
        let programme = self.programme.lock().unwrap_or_else(|p| p.into_inner());
        let capacity_snapshots: Vec<_> = runs
            .iter()
            .map(|(run_id, slot)| {
                let (repository_root, reported_run_id) = match &slot.state {
                    RuntimeSlotState::Launching { repository_root } => {
                        (repository_root.clone(), Some(run_id.clone()))
                    }
                    RuntimeSlotState::RollbackPending {
                        repository_root, ..
                    } => (repository_root.clone(), None),
                    RuntimeSlotState::Active(entry)
                    | RuntimeSlotState::Restarting { entry, .. }
                    | RuntimeSlotState::ReceiptRollbackStopping { entry, .. } => {
                        let snapshot = entry.runtime.snapshot();
                        (
                            PathBuf::from(snapshot.repository_root),
                            Some(snapshot.run_id),
                        )
                    }
                };
                (
                    reported_run_id,
                    repository_root,
                    slot_reserves_capacity(slot),
                )
            })
            .collect();
        #[cfg(test)]
        if let Some(hook) = self
            .portfolio_capture_hook
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .take()
        {
            hook();
        }
        let mut repositories: HashMap<String, RepositoryPortfolioSnapshot> = HashMap::new();
        for (run_id, repository_root, reserves_capacity) in &capacity_snapshots {
            if !reserves_capacity {
                continue;
            }
            let id = repository_id(repository_root);
            let repo =
                repositories
                    .entry(id.clone())
                    .or_insert_with(|| RepositoryPortfolioSnapshot {
                        repository_id: id,
                        active_run_ids: vec![],
                        queued_run_ids: vec![],
                        active_capacity: 0,
                        run_capacity: self.capacity.per_repository_run_capacity,
                    });
            if let Some(run_id) = run_id {
                repo.active_run_ids.push(run_id.clone());
            }
        }
        for gate in programme.gates.values() {
            let repo = repositories
                .entry(gate.downstream_repository_id.clone())
                .or_insert_with(|| RepositoryPortfolioSnapshot {
                    repository_id: gate.downstream_repository_id.clone(),
                    active_run_ids: vec![],
                    queued_run_ids: vec![],
                    active_capacity: 0,
                    run_capacity: self.capacity.per_repository_run_capacity,
                });
            repo.queued_run_ids.push(gate.dispatch.run_id.clone());
        }
        for repo in repositories.values_mut() {
            repo.active_run_ids.sort();
            repo.queued_run_ids.sort();
            repo.active_capacity = repo.active_run_ids.len();
        }
        let mut repositories: Vec<_> = repositories.into_values().collect();
        repositories.sort_by(|a, b| a.repository_id.cmp(&b.repository_id));
        let mut gates: Vec<_> = programme
            .gates
            .values()
            .map(|g| self.gate_snapshot(g))
            .collect();
        gates.sort_by(|a, b| a.downstream_run_id.cmp(&b.downstream_run_id));
        // `runs` is still held here, and `queue_snapshots` takes `layout` and then `queues`.
        // That is the order every binding mutation uses — runs, then layout — so this adds no new
        // edge to the lock graph.
        let queues = self.queue_snapshots();
        ProgrammeSnapshot {
            global_active: capacity_snapshots
                .iter()
                .filter(|(_, _, reserves_capacity)| *reserves_capacity)
                .count(),
            global_run_capacity: self.capacity.agent_capacity(),
            human_review_reserved: self.capacity.human_review_reserved,
            repositories,
            gates,
            queues,
        }
    }

    fn check_capacity(
        &self,
        runs: &HashMap<String, RuntimeSlot>,
        repository: &Path,
    ) -> Result<(), (ErrorCode, String)> {
        let active = runs
            .values()
            .filter(|slot| slot_reserves_capacity(slot))
            .count();
        if active >= self.capacity.agent_capacity() {
            return Err((
                ErrorCode::CapacityExceeded,
                format!(
                    "global run capacity {} is in use ({} total slots, {} reserved for human review)",
                    self.capacity.agent_capacity(),
                    self.capacity.global_run_capacity,
                    self.capacity.human_review_reserved
                ),
            ));
        }
        let repo_active = runs
            .values()
            .filter(|slot| slot.belongs_to_repository(repository))
            .filter(|slot| slot_reserves_capacity(slot))
            .count();
        if repo_active >= self.capacity.per_repository_run_capacity {
            return Err((
                ErrorCode::CapacityExceeded,
                format!(
                    "repository run capacity {} is in use; stop an active run or dispatch in another repository",
                    self.capacity.per_repository_run_capacity
                ),
            ));
        }
        Ok(())
    }

    fn gate_snapshot(&self, gate: &QueuedGate) -> DependencyGateSnapshot {
        let (state, validation_reason) = match self.store.load_handoff_record(&gate.upstream_run_id)
        {
            Err(_) => (
                GateState::AwaitingHandoff,
                Some(format!(
                    "waiting for a valid handoff from exact upstream run {:?}",
                    gate.upstream_run_id
                )),
            ),
            Ok(record) if record.packet.run_id != gate.upstream_run_id => (
                GateState::AwaitingHandoff,
                Some("stored handoff does not match the exact upstream run identity".into()),
            ),
            Ok(_) => match self.store.load_decision(&gate.upstream_run_id) {
                Err(_) => (
                    GateState::AwaitingDecision,
                    Some(format!(
                        "waiting for an explicit human {:?} decision on upstream run {:?}",
                        gate.required_route, gate.upstream_run_id
                    )),
                ),
                Ok(decision) if decision.route != gate.required_route => (
                    GateState::DecisionMismatch,
                    Some(format!(
                        "human decision {:?} does not satisfy required route {:?}",
                        decision.route, gate.required_route
                    )),
                ),
                Ok(_) => (GateState::Ready, None),
            },
        };
        DependencyGateSnapshot {
            upstream_run_id: gate.upstream_run_id.clone(),
            downstream_run_id: gate.dispatch.run_id.clone(),
            upstream_repository_id: gate.upstream_repository_id.clone(),
            downstream_repository_id: gate.downstream_repository_id.clone(),
            required_route: gate.required_route,
            state,
            validation_reason,
        }
    }

    pub fn lifecycle(
        &self,
        run_id: &str,
        operation: LifecycleOperation,
    ) -> Result<RuntimeSnapshot, (ErrorCode, String)> {
        let slot = self
            .runs
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .get(run_id)
            .cloned()
            .ok_or_else(|| {
                (
                    ErrorCode::RunNotFound,
                    format!("run id {run_id:?} is not active in this daemon"),
                )
            })?;
        let _transition = slot.transition.lock().unwrap_or_else(|p| p.into_inner());
        let entry = {
            let runs = self.runs.lock().unwrap_or_else(|p| p.into_inner());
            let current = runs.get(run_id).ok_or_else(|| {
                (
                    ErrorCode::RunNotFound,
                    format!("run id {run_id:?} disappeared"),
                )
            })?;
            if !Arc::ptr_eq(&current.transition, &slot.transition) {
                return Err((
                    ErrorCode::Internal,
                    "run identity changed during lifecycle operation".into(),
                ));
            }
            current.active().cloned().ok_or_else(|| {
                (
                    ErrorCode::RunNotFound,
                    format!("run id {run_id:?} is still launching"),
                )
            })?
        };
        let runtime = Arc::clone(&entry.runtime);
        let capabilities = &runtime.snapshot().process_capabilities;
        let supported = match operation {
            LifecycleOperation::Attach => capabilities.attach,
            LifecycleOperation::Focus => capabilities.focus,
            LifecycleOperation::Interrupt => capabilities.interrupt,
            LifecycleOperation::Stop => capabilities.stop,
            LifecycleOperation::Restart => capabilities.restart,
        };
        if !supported {
            return Err((
                ErrorCode::UnsupportedOperation,
                format!("adapter does not support {operation:?}"),
            ));
        }
        match operation {
            LifecycleOperation::Attach | LifecycleOperation::Focus => Ok(runtime.snapshot()),
            LifecycleOperation::Interrupt => {
                runtime.interrupt().map_err(|m| (ErrorCode::Internal, m))?;
                Ok(runtime.snapshot())
            }
            LifecycleOperation::Stop => {
                runtime.stop().map_err(|m| (ErrorCode::Internal, m))?;
                Ok(runtime.snapshot())
            }
            LifecycleOperation::Restart => {
                // Rediscovery can block without changing authority. Once it succeeds, retire and
                // reap the exact registered group before any replacement is spawned. Thus a stop
                // failure leaves the old capability solely registered, and no failed retire can
                // ever leave two live Dock-owned groups for one run.
                let adapter = entry
                    .selection
                    .resolve()
                    .map_err(|m| (ErrorCode::AdapterUnavailable, m))?;
                #[cfg(test)]
                if let Some(hook) = self
                    .restart_hook
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .clone()
                {
                    hook();
                }
                let runs = self.runs.lock().unwrap_or_else(|p| p.into_inner());
                let current = runs
                    .get(run_id)
                    .and_then(RuntimeSlot::active)
                    .ok_or_else(|| {
                        (
                            ErrorCode::RunNotFound,
                            format!("run id {run_id:?} disappeared during restart"),
                        )
                    })?;
                if !Arc::ptr_eq(&current.runtime, &runtime) {
                    return Err((
                        ErrorCode::Internal,
                        "run changed during concurrent restart; retry against the current runtime"
                            .into(),
                    ));
                }
                drop(runs);
                {
                    let mut runs = self.runs.lock().unwrap_or_else(|p| p.into_inner());
                    let current = runs.get(run_id).and_then(RuntimeSlot::active);
                    if !current.is_some_and(|current| Arc::ptr_eq(&current.runtime, &runtime)) {
                        return Err((
                            ErrorCode::Internal,
                            "run identity changed during restart".into(),
                        ));
                    }
                    runs.insert(
                        run_id.to_owned(),
                        RuntimeSlot {
                            transition: Arc::clone(&slot.transition),
                            state: RuntimeSlotState::Restarting {
                                repository_root: runtime.binding().repository_root,
                                entry: entry.clone(),
                            },
                            pane_shell: slot.pane_shell,
                        },
                    );
                }
                #[cfg(test)]
                if let Some(message) = self
                    .restart_stop_failure
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .take()
                {
                    let mut runs = self.runs.lock().unwrap_or_else(|p| p.into_inner());
                    if runs.get(run_id).is_some_and(|current| {
                        Arc::ptr_eq(&current.transition, &slot.transition)
                            && matches!(current.state, RuntimeSlotState::Restarting { .. })
                    }) {
                        runs.insert(
                            run_id.to_owned(),
                            RuntimeSlot {
                                transition: Arc::clone(&slot.transition),
                                state: RuntimeSlotState::Active(entry.clone()),
                                pane_shell: slot.pane_shell,
                            },
                        );
                    }
                    return Err((ErrorCode::Internal, message));
                }
                if let Err(message) = runtime.stop() {
                    let mut runs = self.runs.lock().unwrap_or_else(|p| p.into_inner());
                    if runs.get(run_id).is_some_and(|current| {
                        Arc::ptr_eq(&current.transition, &slot.transition)
                            && matches!(current.state, RuntimeSlotState::Restarting { .. })
                    }) {
                        runs.insert(
                            run_id.to_owned(),
                            RuntimeSlot {
                                transition: Arc::clone(&slot.transition),
                                state: RuntimeSlotState::Active(entry.clone()),
                                pane_shell: slot.pane_shell,
                            },
                        );
                    }
                    return Err((ErrorCode::Internal, message));
                }
                #[cfg(test)]
                if let Some(hook) = self
                    .restart_after_stop_hook
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .clone()
                {
                    hook();
                }
                let binding = runtime.binding();
                let size = self.pane_size(&binding.workspace_id, &binding.pane_id);
                let replacement = Arc::new(OwnedRuntime::launch(
                    binding,
                    adapter,
                    self.scrollback_rows,
                    self.pane_history_bytes,
                    size,
                ));
                let snapshot = replacement.snapshot();
                if snapshot.pid.is_none() {
                    let mut runs = self.runs.lock().unwrap_or_else(|p| p.into_inner());
                    let reserved = runs.get(run_id).is_some_and(|current| {
                        Arc::ptr_eq(&current.transition, &slot.transition)
                            && matches!(current.state, RuntimeSlotState::Restarting { .. })
                    });
                    if reserved {
                        let binding = runtime.binding();
                        self.layout
                            .lock()
                            .unwrap_or_else(|p| p.into_inner())
                            .unbind_run(&binding.workspace_id, &binding.pane_id, run_id);
                        runs.remove(run_id);
                    }
                    return Err((
                        ErrorCode::AdapterUnavailable,
                        snapshot
                            .diagnostic
                            .clone()
                            .unwrap_or_else(|| "replacement adapter failed to launch".into()),
                    ));
                }
                let mut runs = self.runs.lock().unwrap_or_else(|p| p.into_inner());
                let current = runs.get(run_id);
                if !current.is_some_and(|current| {
                    Arc::ptr_eq(&current.transition, &slot.transition)
                        && matches!(current.state, RuntimeSlotState::Restarting { .. })
                }) {
                    return Err((
                        ErrorCode::Internal,
                        "run identity changed during restart".into(),
                    ));
                }
                runs.insert(
                    run_id.to_owned(),
                    RuntimeSlot {
                        transition: Arc::clone(&slot.transition),
                        state: RuntimeSlotState::Active(RuntimeEntry {
                            runtime: Arc::clone(&replacement),
                            selection: entry.selection,
                        }),
                        pane_shell: slot.pane_shell,
                    },
                );
                Ok(snapshot)
            }
        }
    }

    #[cfg(test)]
    /// Lengthens how long a run's stop waits before escalating SIGTERM to SIGKILL.
    ///
    /// Several tests park a reap on a fixture that ignores SIGTERM and then check that unrelated
    /// work still gets through. The escalation is what eventually unparks it, so with the
    /// production window those tests have about three seconds to do everything they check —
    /// including dispatching a real process — and on a loaded machine they lose that race and fail
    /// for a reason that has nothing to do with the property under test.
    pub(crate) fn set_stop_escalation(&self, run_id: &str, escalation: Duration) {
        let runs = self.runs.lock().unwrap_or_else(|p| p.into_inner());
        if let Some(run) = runs.get(run_id).and_then(RuntimeSlot::active) {
            *run.runtime
                .stop_escalation
                .lock()
                .unwrap_or_else(|p| p.into_inner()) = Some(escalation);
        }
    }

    /// Which agent is under a run and what it is doing, memoised on everything that can change it.
    ///
    /// Shared by `inspect` and `pulse` so the two cannot drift: they answer the same question and
    /// a difference between them would show as a state that changed when nothing did.
    /// Which agent a poll should report for one pane, given what the last poll concluded.
    ///
    /// Memoised against the process-table generation: an answer read from the snapshot this poll
    /// is already looking at is still that snapshot's answer, so a quiet pane costs a hash lookup
    /// rather than a walk. Detection itself walks *down* from the pane's process-group leader by
    /// parentage, because a job-control shell puts every command the user starts into a new
    /// process group of its own.
    ///
    /// The arm worth naming is the one with no table at all. A poll that could not read the
    /// process table has not looked, which is a different claim from having looked and found
    /// nothing — and the difference is not cosmetic, because [`StateTracker::decide`] treats the
    /// absence of an agent as a *fact* and commits it with no dwell at all, on the reasoning that
    /// delaying a fact would be holding a guess over it. Answering "no agent" out of the daemon's
    /// own blindness therefore did not merely report one pane wrongly for one poll: it reset
    /// every agent on the canvas to idle at once, instantly, with none of the hysteresis that
    /// protects every other transition. Holding the previous answer keeps the claim honest —
    /// this pane last had that agent in it and nothing has been seen since to say otherwise.
    fn resolved_agent(
        previous: Option<ClassifiedAgent>,
        generation: u64,
        table: Option<&ProcessTree>,
        process_group_id: Option<i32>,
    ) -> Option<AgentKind> {
        if let Some(previous) = previous
            && previous.generation == generation
        {
            return previous.agent;
        }
        let Some(tree) = table else {
            return previous.and_then(|previous| previous.agent);
        };
        process_group_id.and_then(|leader_pid| tree.agent_under(leader_pid))
    }

    fn resolve_agent(
        &self,
        runtime: &OwnedRuntime,
        observed: RunObservation<'_>,
        generation: u64,
        table: Option<&ProcessTree>,
    ) -> (Option<AgentKind>, AgentState) {
        let RunObservation {
            run_id,
            mark,
            size,
            process_group_id,
        } = observed;
        // The two halves are memoised against different keys because different things change
        // them, and keying both on both is what made a quiet pane pay for a new process table:
        // every one bumped the generation, and every generation bump re-read every pane's whole
        // screen and ran three regex sets over it to reach the answer it already had.
        let mut cached = self
            .agent_states
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let previous = cached.get(run_id).copied();
        let agent = Self::resolved_agent(previous, generation, table, process_group_id);
        let from_screen = match previous {
            Some(previous) if previous.agent == agent && previous.screen == (mark, size) => {
                previous.from_screen
            }
            // The whole screen, not a tail. An agent's chooser leaves the cursor on the
            // highlighted option and prints its instructions underneath, so a cursor-anchored tail
            // cannot contain the very chrome that says the agent is waiting — every pattern
            // matched against one was unreachable.
            _ => match agent {
                Some(kind) => {
                    runtime.with_screen(|screen| read_screen(kind, &screen.classifiable_text()))
                }
                None => AgentState::Idle.into(),
            },
        };
        cached.insert(
            run_id.to_owned(),
            ClassifiedAgent {
                generation,
                agent,
                screen: (mark, size),
                from_screen,
            },
        );
        drop(cached);
        (agent, self.resolve_state(run_id, agent, from_screen, mark))
    }

    /// Combines what the screen says with whether the pane is still writing.
    ///
    /// The screen is only asked one narrow question — is this agent blocked on something it needs
    /// an answer to — because that is the one a pattern can answer reliably: a permission prompt
    /// and a chooser both paint fixed chrome that does not move between releases. Everything else
    /// comes from output. A pane that wrote in the last moment is working, whatever its spinner
    /// happens to be called this month; a pane that has an agent and has fallen silent has handed
    /// the turn back.
    fn resolve_state(
        &self,
        run_id: &str,
        agent: Option<AgentKind>,
        from_screen: ScreenRead,
        mark: OutputMark,
    ) -> AgentState {
        // Read before the output marks are locked: this is a leaf lock and taking the two in one
        // order everywhere is what keeps it one.
        let reported = self
            .reported_states
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get(run_id)
            .copied();
        let mut marks = self
            .output_marks
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let now = Instant::now();
        let tracker = marks
            .entry(run_id.to_owned())
            .or_insert_with(|| StateTracker::new(mark, now));
        tracker.observe(mark, now);
        tracker.decide(now, agent, from_screen, reported)
    }

    /// Records what an agent says it is doing.
    ///
    /// Sticky by design: it holds until the agent reports something else. "Finished" stays true
    /// until the next turn begins, and expiring it after some interval would invent a transition
    /// nobody observed — which is the whole failing of guessing from a screen.
    pub fn report_agent_state(
        &self,
        run_id: &str,
        state: AgentState,
    ) -> Result<(), (ErrorCode, String)> {
        if !self
            .runs
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .contains_key(run_id)
        {
            return Err((
                ErrorCode::RunNotFound,
                format!("no Dock-owned run {run_id}"),
            ));
        }
        self.reported_states
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .insert(run_id.to_owned(), state);
        Ok(())
    }

    /// Forgets everything remembered about runs that have ended.
    ///
    /// None of the three maps is keyed to anything but a run id, and run ids come back: a pane
    /// shell wears the same identity every time its pane opens one. A hook report is sticky on
    /// purpose — it holds until the agent says otherwise — so one left behind by a dead run would
    /// be inherited by the next run wearing its name and outrank everything that run's own screen
    /// said, for as long as it lived. The rest is a slow leak of one entry per run the daemon has
    /// ever hosted.
    ///
    /// Runs leave through a dozen paths — a stop, a rollback, a pane closing, a shell reclaimed —
    /// and sweeping here rather than at each of them keeps the cleanup out of code whose whole job
    /// is undoing things carefully. It costs three lengths compared and does nothing else at all
    /// until a run has actually departed.
    fn forget_departed_runs(&self, live_runs: usize) {
        // Each length in a statement of its own: taking two of these locks at once, in an order
        // no other caller uses, is how a leaf lock stops being one.
        let marks = self
            .output_marks
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .len();
        let classified = self
            .agent_states
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .len();
        let reported = self
            .reported_states
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .len();
        if marks.max(classified).max(reported) <= live_runs {
            return;
        }
        let live: Vec<String> = self
            .runs
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .keys()
            .cloned()
            .collect();
        // Every slot, not only the active ones: a run part-way through a transition is still a run
        // whose agent has not gone anywhere.
        self.output_marks
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .retain(|run_id, _| live.contains(run_id));
        self.agent_states
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .retain(|run_id, _| live.contains(run_id));
        self.reported_states
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .retain(|run_id, _| live.contains(run_id));
    }

    /// The process table, taken afresh only when a fresh one could say something new, and never
    /// on the thread that is polling.
    ///
    /// Both halves of that came out of measurement. `ps -axo pid=,ppid=,pgid=,comm=` on an
    /// ordinary machine — 949 processes, 92KB of output — costs about 35ms of CPU and 60ms of
    /// wall time, and it is a *subprocess*, so that CPU is charged to `ps` rather than to the
    /// daemon. A daemon with one subscriber and nothing at all happening was taking one twice a
    /// second: ten percent of a core, spent where no tool shows it against the daemon's own name.
    /// That is precisely the shape of the complaint that forgotten daemons burn CPU while looking
    /// idle — measured here at 11.2% total against 1.0% for the daemon process itself.
    ///
    /// So the table is now reused, however old it is, on any poll where no run has written a byte
    /// since it was taken. The question it answers is which agent runs beneath a pane, and both
    /// ways that answer can change announce themselves in the pane's own output: an agent starting
    /// prints its banner, an agent exiting hands back a shell that prints its prompt. A run
    /// appearing or departing counts as a change too, since the marks are compared as a whole.
    /// [`PROCESS_TABLE_QUIET_TTL`] is the backstop for the case the inference cannot cover.
    ///
    /// And the refresh itself runs on its own thread. Taken inline it stalled a sixteen-
    /// millisecond poll loop for sixty milliseconds twice a second — four frames dropped, which
    /// measured as a p99 of 69ms against a p50 of 0.07ms. The cost of that is an answer at most
    /// one refresh older, which is far inside the dwell the roster already applies to every state
    /// change it shows.
    fn process_table(
        &self,
        marks: &HashMap<String, OutputMark>,
    ) -> Option<(u64, Arc<ProcessTree>)> {
        let mut cache = self
            .process_table
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let Some(latest) = cache.latest.as_ref() else {
            // Nothing to answer from at all, so this caller pays for the table it needs. Only the
            // first poll of a daemon's life reaches here; deferring it would mean reporting every
            // pane as agentless until a refresh landed.
            let taken = ProcessTableSnapshot::take(0, marks.clone())?;
            let answer = (taken.generation, Arc::clone(&taken.tree));
            cache.latest = Some(taken);
            return Some(answer);
        };
        let answer = (latest.generation, Arc::clone(&latest.tree));
        let age = latest.taken.elapsed();
        let due =
            (age >= PROCESS_TABLE_TTL && latest.marks != *marks) || age >= PROCESS_TABLE_QUIET_TTL;
        let generation = latest.generation + 1;
        if due && !cache.refreshing {
            cache.refreshing = true;
            let marks = marks.clone();
            let cache_handle = Arc::clone(&self.process_table);
            // Detached deliberately: it owns everything it touches and publishes its result under
            // the same lock every reader takes, so there is nothing for anyone to join.
            let spawned = thread::Builder::new()
                .name("dock-process-table".into())
                .spawn(move || {
                    let taken = ProcessTableSnapshot::take(generation, marks);
                    let mut cache = cache_handle
                        .lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner());
                    match taken {
                        Some(taken) => cache.latest = Some(taken),
                        // `ps` failed. The floor still applies, or a machine that cannot run it
                        // would be asked to sixty times a second.
                        None => {
                            if let Some(latest) = cache.latest.as_mut() {
                                latest.taken = Instant::now();
                            }
                        }
                    }
                    cache.refreshing = false;
                });
            if spawned.is_err() {
                cache.refreshing = false;
            }
        }
        Some(answer)
    }

    /// The little the event stream needs from every run, sixty times a second.
    ///
    /// `inspect` returns a full snapshot, which allocates around ten strings per run — the
    /// repository root and worktree are formatted from paths every time — and the stream loop uses
    /// six fields, none of them those. Those fields are also fixed for a run's whole life, so
    /// rebuilding them at frame rate is work whose answer cannot have changed.
    pub fn pulse(&self) -> Vec<RunPulse> {
        let (runtimes, live_runs): (Vec<Arc<OwnedRuntime>>, usize) = {
            let runs = self.runs.lock().unwrap_or_else(|p| p.into_inner());
            (
                runs.values()
                    .filter_map(RuntimeSlot::active)
                    .map(|run| Arc::clone(&run.runtime))
                    .collect(),
                runs.len(),
            )
        };
        self.forget_departed_runs(live_runs);
        // Read here rather than inside `resolve_agent` because the process table now needs them
        // too: whether a fresh one could say anything new is answered by whether any pane has
        // written since the last one was taken.
        let mut pulses: Vec<(Arc<OwnedRuntime>, RunPulse, OutputMark)> = runtimes
            .into_iter()
            .map(|runtime| {
                let pulse = runtime.pulse();
                let mark = runtime.with_output(output_mark);
                (runtime, pulse, mark)
            })
            .collect();
        pulses.sort_by(|a, b| a.1.run_id.cmp(&b.1.run_id));
        let marks: HashMap<String, OutputMark> = pulses
            .iter()
            .map(|(_, pulse, mark)| (pulse.run_id.clone(), *mark))
            .collect();
        let table = pulses
            .iter()
            .any(|(_, pulse, _)| pulse.process_group_id.is_some())
            .then(|| self.process_table(&marks))
            .flatten();
        let generation = table
            .as_ref()
            .map_or(u64::MAX, |(generation, _)| *generation);
        pulses
            .into_iter()
            .map(|(runtime, mut pulse, mark)| {
                let (agent, state) = self.resolve_agent(
                    &runtime,
                    RunObservation {
                        run_id: &pulse.run_id,
                        mark,
                        size: (pulse.rows, pulse.cols),
                        process_group_id: pulse.process_group_id,
                    },
                    generation,
                    table.as_ref().map(|(_, tree)| tree.as_ref()),
                );
                pulse.agent = agent;
                pulse.agent_state = state;
                pulse
            })
            .collect()
    }

    pub fn inspect(
        &self,
        run_id: Option<&str>,
    ) -> Result<Vec<RuntimeSnapshot>, (ErrorCode, String)> {
        // Snapshots are taken outside the registry lock: agent classification reads each run's
        // emulated screen, and holding `runs` across that would serialise every dispatch behind
        // the event stream's continuous polling.
        let live_runs = self.runs.lock().unwrap_or_else(|p| p.into_inner()).len();
        self.forget_departed_runs(live_runs);
        let runtimes: Vec<Arc<OwnedRuntime>> = {
            let runs = self.runs.lock().unwrap_or_else(|p| p.into_inner());
            match run_id {
                Some(run_id) => vec![
                    runs.get(run_id)
                        .and_then(RuntimeSlot::active)
                        .map(|run| Arc::clone(&run.runtime))
                        .ok_or_else(|| {
                            (
                                ErrorCode::RunNotFound,
                                format!("run id {run_id:?} is not active in this daemon"),
                            )
                        })?,
                ],
                None => runs
                    .values()
                    .filter_map(RuntimeSlot::active)
                    .map(|run| Arc::clone(&run.runtime))
                    .collect(),
            }
        };
        let mut runs: Vec<_> = runtimes
            .into_iter()
            .map(|runtime| {
                let snapshot = runtime.snapshot();
                let mark = runtime.with_output(output_mark);
                (runtime, snapshot, mark)
            })
            .collect();
        runs.sort_by(|a, b| a.1.run_id.cmp(&b.1.run_id));
        let marks: HashMap<String, OutputMark> = runs
            .iter()
            .map(|(_, snapshot, mark)| (snapshot.run_id.clone(), *mark))
            .collect();
        // Exactly one `ps` per call, shared by every run: one per run would make this hot path
        // cost a subprocess spawn for each pane on the screen. Often none at all — see
        // `process_table` for when a poll can reuse the table it already has.
        let table = runs
            .iter()
            .any(|(_, snapshot, _)| snapshot.process_group_id.is_some())
            .then(|| self.process_table(&marks))
            .flatten();
        let generation = table
            .as_ref()
            .map_or(u64::MAX, |(generation, _)| *generation);
        Ok(runs
            .into_iter()
            .map(|(runtime, mut snapshot, mark)| {
                // The pane's process-group leader pid. Dock's pane children call `setsid` before
                // `exec`, so the group id and the leader's pid are the same number by
                // construction, and it is a pid Dock's own spawn produced. Detection walks
                // *down* from there by parentage, because a job-control shell puts every command
                // the user starts into a new process group of its own.
                // Both inputs to the answer, so a pane that has written nothing since the last
                // poll and is looking at the same process-table snapshot costs one hash lookup —
                // no table parsing and no screen scan. That is most panes, most of the time, and
                // parsing a 79KB table per pane per poll was the cost that made this hot.
                let (agent, state) = self.resolve_agent(
                    &runtime,
                    RunObservation {
                        run_id: &snapshot.run_id,
                        mark,
                        size: (snapshot.rows, snapshot.cols),
                        process_group_id: snapshot.process_group_id,
                    },
                    generation,
                    table.as_ref().map(|(_, tree)| tree.as_ref()),
                );
                snapshot.agent = agent;
                snapshot.agent_state = state;
                snapshot
            })
            .collect())
    }

    pub fn pane_input(
        &self,
        workspace_id: &str,
        pane_id: &str,
        input: &[u8],
    ) -> Result<usize, (ErrorCode, String)> {
        let run_id = self
            .layout
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .pane_run(workspace_id, pane_id)
            .ok_or_else(|| {
                (
                    ErrorCode::InvalidBinding,
                    "pane is not bound to a live Dock-owned run".into(),
                )
            })?;
        let entry = self
            .runs
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .get(&run_id)
            .and_then(RuntimeSlot::active)
            .cloned()
            .ok_or_else(|| {
                (
                    ErrorCode::InvalidBinding,
                    "pane binding has no live authority in this daemon".into(),
                )
            })?;
        let binding = entry.runtime.binding();
        if binding.workspace_id != workspace_id || binding.pane_id != pane_id {
            return Err((
                ErrorCode::InvalidBinding,
                "pane binding does not match the exact Dock-owned runtime".into(),
            ));
        }
        #[cfg(test)]
        if let Some(hook) = self
            .pane_input_before_final_validation_hook
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .take()
        {
            hook();
        }
        // Input is enqueued, rather than written here, so this exact-authority critical section
        // cannot block on a PTY. All binding mutations use runs -> layout in the same order.
        let runs = self.runs.lock().unwrap_or_else(|p| p.into_inner());
        let layout = self.layout.lock().unwrap_or_else(|p| p.into_inner());
        let exact_runtime = runs
            .get(&run_id)
            .and_then(RuntimeSlot::active)
            .is_some_and(|current| Arc::ptr_eq(&current.runtime, &entry.runtime));
        if !exact_runtime || layout.pane_run(workspace_id, pane_id).as_deref() != Some(&run_id) {
            return Err((
                ErrorCode::InvalidBinding,
                "pane binding changed before input reached the exact Dock-owned runtime".into(),
            ));
        }
        entry
            .runtime
            .input(input)
            .map_err(|message| (ErrorCode::UnsupportedOperation, message))?;
        Ok(input.len())
    }

    /// Which "the agent finished" signal an already-armed pane believes.
    ///
    /// Set once at startup from `--auto-feed-trust`. It chooses a *signal*; there is deliberately
    /// no setting here or anywhere else that arms a pane, because arming is the one deliberate act
    /// that lets Dock work while nobody is watching.
    pub fn set_auto_feed_trust(&self, trust: AutoFeedTrust) {
        *self
            .auto_feed_trust
            .lock()
            .unwrap_or_else(|p| p.into_inner()) = trust;
    }

    pub fn auto_feed_trust(&self) -> AutoFeedTrust {
        *self
            .auto_feed_trust
            .lock()
            .unwrap_or_else(|p| p.into_inner())
    }

    pub fn queue_paused(&self) -> bool {
        self.queue_paused.load(Ordering::Acquire)
    }

    /// How many times any queue has changed since this daemon started.
    ///
    /// One atomic load is the whole cost of "did anything happen" on the 16ms subscriber loop,
    /// which is why it exists: the alternative is walking a map sixty times a second to find out
    /// that nothing did.
    pub fn queue_generation(&self) -> u64 {
        self.queue_generation.load(Ordering::Acquire)
    }

    /// The generation at which each pane's queue last changed. Read only when
    /// [`Self::queue_generation`] says something moved.
    pub fn queue_revisions(&self) -> HashMap<(String, String), u64> {
        self.queue_revisions
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .clone()
    }

    /// Adds one prompt to a pane's queue.
    ///
    /// Two caps are checked here rather than in `PaneQueue`, for the same reason: a queue cannot
    /// see anything but itself. `MAX_QUEUED_TOTAL` is a property of the daemon, and the byte limit
    /// is checked before the entry is built so an over-long prompt is refused in the words §11
    /// gives rather than in the queue's own.
    pub fn queue_add(
        &self,
        workspace_id: &str,
        pane_id: &str,
        label: String,
        prompt: String,
    ) -> Result<u64, (ErrorCode, String)> {
        self.require_pane(workspace_id, pane_id)?;
        if prompt.len() > MAX_PROMPT_BYTES {
            return Err((
                ErrorCode::QueueRefused,
                format!(
                    "that prompt is {} bytes; the limit is {MAX_PROMPT_BYTES}",
                    prompt.len()
                ),
            ));
        }
        let key = (workspace_id.to_owned(), pane_id.to_owned());
        let (entry_id, durable) = {
            let mut queues = self.queues.lock().unwrap_or_else(|p| p.into_inner());
            let total: usize = queues.values().map(PaneQueue::len).sum();
            if total >= MAX_QUEUED_TOTAL {
                return Err((
                    ErrorCode::QueueRefused,
                    format!(
                        "this daemon already holds {MAX_QUEUED_TOTAL} queued prompts across every pane; remove one before adding another"
                    ),
                ));
            }
            let queue = queues.entry(key.clone()).or_default();
            let entry_id = queue
                .add(label, prompt)
                .map_err(|message| (ErrorCode::QueueRefused, message))?;
            (entry_id, durable_queue(&key, queue))
        };
        self.commit_queue_change(&key, &durable);
        Ok(entry_id)
    }

    pub fn queue_remove(
        &self,
        workspace_id: &str,
        pane_id: &str,
        entry_id: u64,
    ) -> Result<(), (ErrorCode, String)> {
        self.require_pane(workspace_id, pane_id)?;
        let key = (workspace_id.to_owned(), pane_id.to_owned());
        let durable = {
            let mut queues = self.queues.lock().unwrap_or_else(|p| p.into_inner());
            let queue = queues.get_mut(&key).ok_or_else(|| {
                (
                    ErrorCode::QueueRefused,
                    format!("this pane has no queued entry {entry_id}"),
                )
            })?;
            queue
                .remove(entry_id)
                .map_err(|message| (ErrorCode::QueueRefused, message))?;
            durable_queue(&key, queue)
        };
        self.commit_queue_change(&key, &durable);
        Ok(())
    }

    pub fn queue_clear(
        &self,
        workspace_id: &str,
        pane_id: &str,
    ) -> Result<usize, (ErrorCode, String)> {
        self.require_pane(workspace_id, pane_id)?;
        let key = (workspace_id.to_owned(), pane_id.to_owned());
        let (removed, durable) = {
            let mut queues = self.queues.lock().unwrap_or_else(|p| p.into_inner());
            let Some(queue) = queues.get_mut(&key) else {
                return Ok(0);
            };
            (queue.clear(), durable_queue(&key, queue))
        };
        self.commit_queue_change(&key, &durable);
        Ok(removed)
    }

    /// Arms or disarms auto-feed for one pane.
    ///
    /// The refusals are the point. Under the default trust a pane whose agent has never reported a
    /// state can never satisfy the feed rule, so arming it would produce a queue that sits there
    /// forever looking broken — `PaneQueue::arm` refuses it in words that name `dock hooks
    /// --install`. Under `--auto-feed-trust=screen` there is no report to check, so the question
    /// becomes whether there is an agent there at all: feeding a shell would type a sentence at a
    /// `$` prompt and press return, and that refusal is worth making before the queue is armed as
    /// well as at every feed.
    ///
    /// Nothing about arming is persisted. See [`restore_pane_queues`].
    pub fn queue_set_auto(
        &self,
        workspace_id: &str,
        pane_id: &str,
        enabled: bool,
    ) -> Result<(), (ErrorCode, String)> {
        self.require_pane(workspace_id, pane_id)?;
        let key = (workspace_id.to_owned(), pane_id.to_owned());
        if !enabled {
            let mut queues = self.queues.lock().unwrap_or_else(|p| p.into_inner());
            queues.entry(key.clone()).or_default().disarm();
            drop(queues);
            self.note_queue_change(&key);
            return Ok(());
        }
        let trust = self.auto_feed_trust();
        let run_id = self
            .layout
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .pane_run(workspace_id, pane_id);
        if trust == AutoFeedTrust::Screen {
            // One pulse, on a human action, so the answer is this moment's rather than whatever a
            // subscriber last happened to leave in the classification cache. Under the default
            // trust this is skipped entirely: a reported state already proves an agent.
            let _ = self.pulse();
            let agent = run_id.as_deref().and_then(|run_id| {
                self.agent_states
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .get(run_id)
                    .and_then(|classified| classified.agent)
            });
            if agent.is_none() {
                return Err((
                    ErrorCode::QueueRefused,
                    "nothing in that pane looks like an agent; auto-feed would type into a shell"
                        .to_string(),
                ));
            }
        }
        let has_reported = run_id.as_deref().is_some_and(|run_id| {
            self.reported_states
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .contains_key(run_id)
        });
        let mut queues = self.queues.lock().unwrap_or_else(|p| p.into_inner());
        queues
            .entry(key.clone())
            .or_default()
            .arm(has_reported, trust)
            .map_err(|message| (ErrorCode::QueueRefused, message))?;
        drop(queues);
        self.note_queue_change(&key);
        Ok(())
    }

    /// The daemon-wide kill switch.
    ///
    /// Persisted, and a persistence failure is an error rather than a line on stderr: pausing
    /// before you walk away is a decision that has to survive a restart, so a pause the daemon
    /// could not write down is a pause the caller must be told it does not have.
    pub fn queue_set_paused(&self, paused: bool) -> Result<(), (ErrorCode, String)> {
        self.store
            .set_queue_pause(paused)
            .map_err(|message| (ErrorCode::Internal, message))?;
        self.queue_paused.store(paused, Ordering::Release);
        // Every queue changed, because the pause is what each of them is now doing. Saying so per
        // pane rather than daemon-wide is what lets `Event::QueueChanged` carry a pane at all: a
        // client that hears about the pause hears about it the same way it hears about a drain.
        let keys: Vec<QueueKey> = self
            .queues
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .keys()
            .cloned()
            .collect();
        for key in &keys {
            self.note_queue_change(key);
        }
        if keys.is_empty() {
            self.queue_generation.fetch_add(1, Ordering::AcqRel);
        }
        Ok(())
    }

    /// Every queue the daemon holds, in one listing, so a board fills its runs lane in one round
    /// trip rather than one request per pane.
    pub fn queue_snapshots(&self) -> Vec<PaneQueueSnapshot> {
        let runs: HashMap<(String, String), String> = {
            let layout = self.layout.lock().unwrap_or_else(|p| p.into_inner());
            layout
                .snapshot()
                .workspaces
                .into_iter()
                .flat_map(|workspace| {
                    workspace
                        .panes
                        .into_iter()
                        .filter_map(move |(pane_id, pane)| {
                            pane.run_id
                                .map(|run_id| ((workspace.workspace_id.clone(), pane_id), run_id))
                        })
                })
                .collect()
        };
        let queues = self.queues.lock().unwrap_or_else(|p| p.into_inner());
        let mut snapshots: Vec<PaneQueueSnapshot> = queues
            .iter()
            .map(|((workspace_id, pane_id), queue)| PaneQueueSnapshot {
                workspace_id: workspace_id.clone(),
                pane_id: pane_id.clone(),
                run_id: runs.get(&(workspace_id.clone(), pane_id.clone())).cloned(),
                auto_feed: queue.auto_feed(),
                awaiting_ack: queue.awaiting_ack(),
                holding_because: queue.holding_because().map(str::to_owned),
                entries: queue
                    .entries()
                    .iter()
                    .map(|entry| QueueEntrySnapshot {
                        entry_id: entry.entry_id,
                        label: entry.label.clone(),
                        // A preview, never the prompt: sixteen 8 KiB prompts across a handful of
                        // panes would not fit in one protocol message.
                        preview: entry.preview(),
                        bytes: entry.bytes(),
                    })
                    .collect(),
            })
            .collect();
        snapshots.sort_by(|a, b| (&a.workspace_id, &a.pane_id).cmp(&(&b.workspace_id, &b.pane_id)));
        snapshots
    }

    /// Forgets one pane's queue, in memory and on disk.
    ///
    /// Called when a pane closes. Without it a queue file outlives the pane it was keyed to
    /// forever, and the entries in it would reappear the day somebody created a pane with the same
    /// name.
    pub fn forget_pane_queue(&self, workspace_id: &str, pane_id: &str) {
        let key = (workspace_id.to_owned(), pane_id.to_owned());
        let existed = self
            .queues
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .remove(&key)
            .is_some();
        self.queue_revisions
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .remove(&key);
        if let Err(message) = self.store.remove_pane_queue(workspace_id, pane_id) {
            eprintln!("dockd: could not remove the queue for a closed pane: {message}");
        }
        if existed {
            self.queue_generation.fetch_add(1, Ordering::AcqRel);
        }
    }

    /// One pass of the auto-feed machinery.
    ///
    /// Driven by a dedicated 250ms daemon thread rather than by the 16ms loop in `stream_events`,
    /// and that is not a tuning choice. That loop is a *subscriber* loop: a queue driven from it
    /// would only advance while a TUI happened to be attached, and would advance N times over with
    /// N clients connected. Both are wrong in ways a user would experience as the daemon being
    /// haunted.
    ///
    /// The early return matters as much as the rest. A daemon with no queues at all — which is
    /// every daemon until somebody queues something — does one lock and one length comparison four
    /// times a second and never touches `pulse`, so the whole subsystem costs nothing until it is
    /// used.
    pub fn queue_tick(&self) {
        if self
            .queues
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .is_empty()
        {
            return;
        }
        let reported = self
            .reported_states
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .clone();
        let observations: Vec<QueueObservation> = self
            .pulse()
            .into_iter()
            .map(|pulse| QueueObservation {
                // Not "has this agent ever reported", but "is *this* answer the agent's own".
                // A report that has been superseded by the screen is an inference again, and
                // guard (4) is about the provenance of the `Done` in hand.
                reported: reported.get(&pulse.run_id) == Some(&pulse.agent_state),
                run_id: pulse.run_id,
                agent: pulse.agent,
                state: pulse.agent_state,
            })
            .collect();
        self.queue_tick_from(&observations, Instant::now());
    }

    /// The tick with its two impure inputs — what every run is doing, and what time it is — handed
    /// in.
    ///
    /// Split out so the wiring is testable without a real agent in a real process table. `pulse`
    /// can only report an agent it can actually detect, so a test driven through [`Self::queue_tick`]
    /// could never reach a feed at all: every pane would classify as a shell and refuse, and the
    /// test would pass by never testing anything.
    fn queue_tick_from(&self, observations: &[QueueObservation], now: Instant) {
        // Only the panes that actually have a queue are looked up, and they are looked up one at a
        // time. Walking the layout instead would clone every workspace and every pane four times a
        // second to discover, on a machine with one queue, fifteen panes it has nothing to say
        // about. `queues` is a leaf lock, so the keys are taken and the lock dropped before
        // `layout` is touched at all — a feed calls `pane_input`, which takes `layout` and `runs`
        // itself.
        let keys: Vec<QueueKey> = self
            .queues
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .keys()
            .cloned()
            .collect();
        if keys.is_empty() {
            return;
        }
        let by_run: HashMap<String, QueueKey> = {
            let layout = self.layout.lock().unwrap_or_else(|p| p.into_inner());
            keys.into_iter()
                .filter_map(|key| layout.pane_run(&key.0, &key.1).map(|run_id| (run_id, key)))
                .collect()
        };
        let panes: Vec<(QueueKey, &QueueObservation)> = observations
            .iter()
            .filter_map(|observation| {
                by_run
                    .get(&observation.run_id)
                    .cloned()
                    .map(|key| (key, observation))
            })
            .collect();
        let paused = self.queue_paused();
        let trust = self.auto_feed_trust();
        let mut feeds: Vec<(QueueKey, String, DurablePaneQueue)> = Vec::new();
        let mut held: Vec<QueueKey> = Vec::new();
        {
            let mut queues = self.queues.lock().unwrap_or_else(|p| p.into_inner());
            for (key, observation) in panes {
                // `get_mut`, never `entry`: a pane nobody has queued anything for has no queue,
                // and the tick must not conjure one for every pane on the machine.
                let Some(queue) = queues.get_mut(&key) else {
                    continue;
                };
                let before = queue.holding_because().map(str::to_owned);
                match queue.poll(
                    observation.agent,
                    observation.state,
                    observation.reported,
                    trust,
                    paused,
                    now,
                ) {
                    Some(prompt) => {
                        let durable = durable_queue(&key, queue);
                        feeds.push((key, prompt, durable));
                    }
                    None => {
                        if before.as_deref() != queue.holding_because() {
                            held.push(key);
                        }
                    }
                }
            }
        }
        // A held queue changed nothing but its explanation, which the runs lane still shows, so a
        // client is told — and nothing is written to disk, because the sentence is about the last
        // few seconds of this process and would be a lie after a restart.
        for key in held {
            self.note_queue_change(&key);
        }
        for (key, prompt, durable) in feeds {
            let (workspace_id, pane_id) = key.clone();
            // The same call the client's keystrokes go through, with all four of its binding
            // re-validations intact. Auto-feed gets no privileged path into a pane, which is what
            // makes "an auto-feeding queue of depth sixteen creates zero worktrees" structural
            // rather than a promise: there is no other verb here to reach for.
            match self.pane_input(&workspace_id, &pane_id, prompt.as_bytes()) {
                Ok(_) => self.commit_queue_change(&key, &durable),
                Err((_, message)) => {
                    // The entry goes back and the pane is disarmed. Retrying into a pane whose
                    // binding just changed is how one wrong feed becomes many.
                    let durable = {
                        let mut queues = self.queues.lock().unwrap_or_else(|p| p.into_inner());
                        let Some(queue) = queues.get_mut(&key) else {
                            continue;
                        };
                        queue.feed_failed(&message);
                        durable_queue(&key, queue)
                    };
                    self.commit_queue_change(&key, &durable);
                }
            }
        }
    }

    /// Refuses an operation against a pane that is not there, before anything is allocated for it.
    fn require_pane(&self, workspace_id: &str, pane_id: &str) -> Result<(), (ErrorCode, String)> {
        if self
            .layout
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .pane_exists(workspace_id, pane_id)
        {
            Ok(())
        } else {
            Err((
                ErrorCode::PaneNotFound,
                format!("no pane {pane_id} in workspace {workspace_id}"),
            ))
        }
    }

    /// Records that a queue moved and writes it down.
    ///
    /// A write failure is one line on stderr rather than an error to the caller, and the asymmetry
    /// with [`Self::queue_set_paused`] is deliberate: an entry that fails to persist is still
    /// queued and still feeds correctly for the life of this daemon, and refusing work that has
    /// already been accepted would be a stranger answer than losing it at the next restart. A
    /// pause that fails to persist is a safety promise the daemon cannot keep, so that one is
    /// refused.
    fn commit_queue_change(&self, key: &QueueKey, durable: &DurablePaneQueue) {
        if let Err(message) = self.store.save_pane_queue(durable) {
            eprintln!(
                "dockd: could not persist the queue for {}/{}: {message}",
                key.0, key.1
            );
        }
        self.note_queue_change(key);
    }

    fn note_queue_change(&self, key: &QueueKey) {
        let generation = self.queue_generation.fetch_add(1, Ordering::AcqRel) + 1;
        self.queue_revisions
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .insert(key.clone(), generation);
    }

    /// Resizes the PTY behind one Dock-owned pane and records the geometry, so a run launched
    /// into that pane later starts at the measured size instead of the 24x80 fallback. A pane
    /// with no live Dock-owned run is refused: Dock has no PTY to resize and nothing to record
    /// the size against, since an unbound pane may not even exist.
    pub fn pane_resize(
        &self,
        workspace_id: &str,
        pane_id: &str,
        rows: u16,
        cols: u16,
    ) -> Result<(), (ErrorCode, String)> {
        if rows == 0 || cols == 0 {
            return Err((
                ErrorCode::InvalidBinding,
                "pane size must be at least one row and one column".into(),
            ));
        }
        let size = PtySize { rows, cols };
        let run_id = self
            .layout
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .pane_run(workspace_id, pane_id)
            .ok_or_else(|| {
                (
                    ErrorCode::InvalidBinding,
                    "pane is not bound to a live Dock-owned run".into(),
                )
            })?;
        let entry = self
            .runs
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .get(&run_id)
            .and_then(RuntimeSlot::active)
            .cloned();
        // Held across the resize so two concurrent resizes of one pane cannot record their sizes
        // in one order and issue their TIOCSWINSZ ioctls in the other, which would leave the
        // kernel winsize disagreeing with the emulated screen.
        let mut sizes = self.pane_sizes.lock().unwrap_or_else(|p| p.into_inner());
        sizes.insert(pane_size_key(workspace_id, pane_id), size);
        let Some(entry) = entry else {
            return Ok(());
        };
        entry
            .runtime
            .resize(size)
            .map_err(|message| (ErrorCode::UnsupportedOperation, message))
    }

    pub fn with_run_screen<T>(
        &self,
        run_id: &str,
        apply: impl FnOnce(&PaneScreen) -> T,
    ) -> Option<T> {
        self.with_run_output(run_id, |output| apply(output.screen()))
    }

    /// The pane's screen and its undelivered raw output, under one lock. The streaming path
    /// needs both in one look: bytes handed out and the screen they reach must come from the
    /// same instant, or the next poll would forward bytes the subscriber already has.
    pub fn with_run_output<T>(
        &self,
        run_id: &str,
        apply: impl FnOnce(&PaneOutput) -> T,
    ) -> Option<T> {
        let entry = self
            .runs
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .get(run_id)
            .and_then(RuntimeSlot::active)
            .cloned()?;
        Some(entry.runtime.with_output(apply))
    }

    /// Scrollback rows every pane's terminal retains. Announced to subscribers so a client's
    /// replica retains exactly what the daemon does rather than guessing at the default.
    pub fn scrollback_rows(&self) -> usize {
        self.scrollback_rows
    }

    /// Bytes of raw output every pane retains. The attach frame announces a row capacity
    /// derived from this budget, so a client's replica is sized to hold the history it will
    /// be sent rather than the daemon's own parser depth.
    pub fn pane_history_bytes(&self) -> usize {
        self.pane_history_bytes
    }

    #[must_use]
    pub fn with_pane_history_bytes(mut self, bytes: usize) -> Self {
        self.pane_history_bytes = bytes;
        self
    }

    fn pane_size(&self, workspace_id: &str, pane_id: &str) -> PtySize {
        self.pane_sizes
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .get(&pane_size_key(workspace_id, pane_id))
            .copied()
            .unwrap_or(UNMEASURED_PANE_SIZE)
    }

    pub fn submit_handoff(
        &self,
        packet: HandoffPacket,
    ) -> Result<HandoffRecord, (ErrorCode, String)> {
        packet
            .validate()
            .map_err(|message| (ErrorCode::InvalidHandoff, message.into()))?;
        validate_concise_safe_packet(&packet)
            .map_err(|message| (ErrorCode::InvalidHandoff, message))?;
        let snapshot = self.inspect(Some(&packet.run_id))?.remove(0);
        let expected = (
            snapshot.external_task_ref.as_str(),
            snapshot.workspace_id.as_str(),
            snapshot.pane_id.as_str(),
            snapshot.worktree.as_str(),
            snapshot.branch.as_str(),
            snapshot.base_sha.as_str(),
        );
        let supplied = (
            packet.task_id.as_str(),
            packet.workspace_id.as_str(),
            packet.pane_id.as_str(),
            packet.worktree.as_str(),
            packet.branch.as_str(),
            packet.base_sha.as_str(),
        );
        if supplied != expected {
            return Err((
                ErrorCode::InvalidHandoff,
                "handoff binding does not exactly match the active bound run".into(),
            ));
        }
        let facts = GitAdapter::new(&snapshot.worktree)
            .facts(&snapshot.base_sha)
            .map_err(|message| (ErrorCode::InvalidHandoff, message))?;
        let live_worktree = facts.worktree.display().to_string();
        if live_worktree != snapshot.worktree
            || facts.branch != snapshot.branch
            || facts.base_sha != snapshot.base_sha
        {
            return Err((
                ErrorCode::InvalidHandoff,
                format!(
                    "live Git binding no longer agrees with the bound run (worktree {live_worktree:?}, branch {:?}, base {:?})",
                    facts.branch, facts.base_sha
                ),
            ));
        }
        let record = HandoffRecord {
            packet,
            evidence: HandoffEvidence {
                branch: facts.branch,
                base_sha: facts.base_sha,
                head_sha: facts.head_sha,
                status_entries: facts.status_entries,
                changed_files: facts.changed_files,
                insertions: facts.insertions,
                deletions: facts.deletions,
            },
        };
        self.store.save_handoff_record(&record).map_err(|message| {
            let code = if message.contains("handoff") && message.contains("already exists") {
                ErrorCode::DuplicateHandoff
            } else {
                ErrorCode::Internal
            };
            (code, message)
        })?;
        Ok(record)
    }

    pub fn review_inbox(&self) -> Result<Vec<HandoffRecord>, (ErrorCode, String)> {
        let records = self
            .store
            .list_handoff_records()
            .map_err(|message| (ErrorCode::Internal, message))?;
        let mut pending = Vec::new();
        for record in records {
            if !self
                .store
                .decision_exists(&record.packet.run_id)
                .map_err(|message| (ErrorCode::Internal, message))?
            {
                pending.push(record);
            }
        }
        Ok(pending)
    }

    pub fn decide(
        &self,
        run_id: String,
        route: ReviewRoute,
        note: String,
    ) -> Result<ReviewDecision, (ErrorCode, String)> {
        self.store.load_handoff_record(&run_id).map_err(|_| {
            (
                ErrorCode::HandoffNotFound,
                format!("no handoff exists for run {run_id:?}"),
            )
        })?;
        let decision = ReviewDecision::new(run_id, route, note)
            .map_err(|message| (ErrorCode::InvalidHandoff, message.into()))?;
        self.store.save_decision(&decision).map_err(|message| {
            let code = if message.contains("already exists") {
                ErrorCode::DecisionAlreadyRecorded
            } else {
                ErrorCode::Internal
            };
            (code, message)
        })?;
        Ok(decision)
    }

    fn receipt_path(&self, run_id: &str) -> Result<PathBuf, String> {
        validate_run_id(run_id)?;
        Ok(self.receipts.join(format!("{run_id}.json")))
    }
}

fn validate_concise_safe_packet(packet: &HandoffPacket) -> Result<(), String> {
    if packet.summary.len() > 2_000
        || packet
            .question
            .as_ref()
            .is_some_and(|value| value.len() > 1_000)
        || packet.checks.len() > 64
        || packet.checks.iter().any(|check| check.name.len() > 200)
    {
        return Err("handoff evidence exceeds concise local record limits".into());
    }
    let contains_secret_marker = |value: &str| likely_contains_secret(value);
    if contains_secret_marker(&packet.summary)
        || packet
            .question
            .as_deref()
            .is_some_and(contains_secret_marker)
        || packet
            .checks
            .iter()
            .any(|check| contains_secret_marker(&check.name))
    {
        return Err("handoff evidence contains a prohibited secret marker".into());
    }
    Ok(())
}

fn likely_contains_secret(value: &str) -> bool {
    // These deliberately favor rejection: handoff prose and check labels have no reason to
    // contain credential-shaped values, and the record is durable human-review evidence.
    const PATTERNS: &[&str] = &[
        r#"(?i)authorization\s*['"]?\s*[:=]\s*['"]?\s*(?:bearer|basic)\s+[a-z0-9+/_.=-]{8,}"#,
        r"(?i)\b(?:bearer|basic)\s+[a-z0-9+/_.=-]{16,}",
        r"\b(?:gh[pousr]_[A-Za-z0-9]{20,}|github_pat_[A-Za-z0-9_]{20,})\b",
        r"\bsk-(?:proj-)?[A-Za-z0-9_-]{20,}\b",
        r"\b(?:AKIA|ASIA)[A-Z0-9]{16}\b",
        r#"(?i)\b(?:aws_?secret_?access_?key|secret_?access_?key|api_?key|apikey|access_?token|auth_?token|token|password)\b\s*["']?\s*[:=]\s*["']?[^\s"',}]{8,}"#,
        r"(?i)-----BEGIN [A-Z ]*PRIVATE KEY-----",
    ];
    PATTERNS.iter().any(|pattern| {
        Regex::new(pattern)
            .expect("static secret pattern must compile")
            .is_match(value)
    })
}

fn ensure_private_directory(path: &Path, label: &str) -> Result<(), String> {
    // macOS exposes its standard temporary directory as the root-owned `/tmp` symlink to
    // `/private/tmp`.  Resolve that one platform alias before the no-follow component walk so
    // absolute mktemp paths remain usable; all caller-controlled symlink ancestors are still
    // rejected by openat/fstatat below.
    let traversal_path = if path.is_absolute() && path.starts_with("/tmp") {
        fs::canonicalize("/tmp")
            .map(|temporary| temporary.join(path.strip_prefix("/tmp").unwrap()))
            .map_err(|error| format!("could not resolve system temporary directory: {error}"))?
    } else {
        path.to_path_buf()
    };
    let components = traversal_path
        .components()
        .filter_map(|component| match component {
            Component::Normal(name) => Some(Ok(name)),
            Component::RootDir | Component::CurDir => None,
            Component::ParentDir | Component::Prefix(_) => Some(Err(format!(
                "refusing untrusted {label} directory {}: parent and platform-prefix components are not allowed",
                path.display()
            ))),
        })
        .collect::<Result<Vec<_>, _>>()?;
    if components.is_empty() {
        return Err(format!("refusing empty {label} directory path"));
    }

    let start = if traversal_path.is_absolute() {
        Path::new("/")
    } else {
        Path::new(".")
    };
    let mut directory = OpenOptions::new()
        .read(true)
        .custom_flags(nix::libc::O_DIRECTORY | nix::libc::O_CLOEXEC | nix::libc::O_NOFOLLOW)
        .open(start)
        .map_err(|error| format!("could not open {label} directory traversal root: {error}"))?;
    let effective_uid = unsafe { nix::libc::geteuid() };

    for (index, component) in components.iter().enumerate() {
        let name = CString::new(component.as_bytes())
            .map_err(|_| format!("refusing {label} directory with a NUL path component"))?;
        let is_final = index + 1 == components.len();
        let mut created = false;
        let mut fd = unsafe {
            nix::libc::openat(
                directory.as_raw_fd(),
                name.as_ptr(),
                nix::libc::O_RDONLY
                    | nix::libc::O_DIRECTORY
                    | nix::libc::O_CLOEXEC
                    | nix::libc::O_NOFOLLOW,
            )
        };
        if fd < 0 {
            let open_error = std::io::Error::last_os_error();
            if open_error.kind() != std::io::ErrorKind::NotFound {
                let mut metadata = std::mem::MaybeUninit::<nix::libc::stat>::uninit();
                let inspect_result = unsafe {
                    nix::libc::fstatat(
                        directory.as_raw_fd(),
                        name.as_ptr(),
                        metadata.as_mut_ptr(),
                        nix::libc::AT_SYMLINK_NOFOLLOW,
                    )
                };
                if inspect_result == 0 {
                    return Err(format!(
                        "refusing untrusted {label} directory {}: could not open a real directory component without following symlinks: {open_error}",
                        path.display()
                    ));
                }
                let inspect_error = std::io::Error::last_os_error();
                if inspect_error.kind() != std::io::ErrorKind::NotFound {
                    return Err(format!(
                        "refusing untrusted {label} directory {}: could not safely inspect a directory component after open failed ({open_error}): {inspect_error}",
                        path.display()
                    ));
                }
            }
            let result = unsafe { nix::libc::mkdirat(directory.as_raw_fd(), name.as_ptr(), 0o700) };
            if result != 0 {
                let create_error = std::io::Error::last_os_error();
                if create_error.kind() != std::io::ErrorKind::AlreadyExists {
                    return Err(format!(
                        "could not create {label} directory component in {}: {create_error}",
                        path.display()
                    ));
                }
            } else {
                created = true;
            }
            fd = unsafe {
                nix::libc::openat(
                    directory.as_raw_fd(),
                    name.as_ptr(),
                    nix::libc::O_RDONLY
                        | nix::libc::O_DIRECTORY
                        | nix::libc::O_CLOEXEC
                        | nix::libc::O_NOFOLLOW,
                )
            };
            if fd < 0 {
                return Err(format!(
                    "refusing untrusted {label} directory {}: could not verify a directory component after creation without following symlinks: {}",
                    path.display(),
                    std::io::Error::last_os_error()
                ));
            }
            // A competing creator is acceptable only when it produced the same kind of private
            // directory we would have created. Verify before using it as a traversal root.
            if !created {
                let raced_directory = unsafe { File::from_raw_fd(fd) };
                verify_private_directory(&raced_directory, effective_uid, path, label)?;
                directory = raced_directory;
                continue;
            }
        }
        directory = unsafe { File::from_raw_fd(fd) };
        if created || is_final {
            verify_private_directory(&directory, effective_uid, path, label)?;
        }
    }
    Ok(())
}

fn verify_private_directory(
    directory: &File,
    effective_uid: u32,
    path: &Path,
    label: &str,
) -> Result<(), String> {
    let metadata = directory
        .metadata()
        .map_err(|error| format!("could not inspect {label} directory: {error}"))?;
    if metadata.uid() != effective_uid
        || metadata.mode() & 0o700 != 0o700
        || metadata.mode() & 0o077 != 0
    {
        return Err(format!(
            "refusing untrusted {label} directory {}: it must be a real directory owned by the current user with mode 0700 or stricter",
            path.display()
        ));
    }
    Ok(())
}

fn layout_error(message: String) -> (ErrorCode, String) {
    let code = if message == "workspace not found" {
        ErrorCode::WorkspaceNotFound
    } else if message.contains("pane not found") {
        ErrorCode::PaneNotFound
    } else {
        ErrorCode::InvalidLayout
    };
    (code, message)
}

fn validate_binding(request: &DispatchRequest) -> Result<RunBinding, String> {
    validate_external_run_id(&request.run_id)?;
    if request.external_task_ref.trim().is_empty() {
        return Err("external_task_ref is required".into());
    }
    reject_parent_components(Path::new(&request.repository_root), "repository_root")?;
    reject_parent_components(Path::new(&request.worktree), "worktree")?;
    let repository_root = fs::canonicalize(&request.repository_root)
        .map_err(|e| format!("could not canonicalize repository root: {e}"))?;
    let worktree = fs::canonicalize(&request.worktree)
        .map_err(|e| format!("could not canonicalize supplied worktree: {e}"))?;
    if !worktree.starts_with(&repository_root) {
        return Err("supplied worktree escapes the canonical repository root".into());
    }
    let declared_root = git_toplevel(&repository_root)?;
    if declared_root != repository_root {
        return Err(format!(
            "repository_root is not the canonical Git top-level: {}",
            declared_root.display()
        ));
    }
    let actual_root = git_toplevel(&worktree)?;
    if actual_root != worktree {
        return Err(format!(
            "supplied worktree must be its Git worktree top-level, not a subdirectory of {}",
            actual_root.display()
        ));
    }
    let repository_common = git_common_dir(&repository_root)?;
    let worktree_common = git_common_dir(&worktree)?;
    if repository_common != worktree_common {
        return Err(format!(
            "repository mismatch: worktree Git directory {} does not belong to {}",
            worktree_common.display(),
            repository_common.display()
        ));
    }
    let branch = git(&worktree, &["branch", "--show-current"])?;
    let branch = if branch.is_empty() {
        "DETACHED".into()
    } else {
        branch
    };
    let base_sha = git(&worktree, &["rev-parse", "HEAD"])?;
    let workspace_id = format!("workspace_{}", repository_id(&repository_root));
    Ok(RunBinding {
        binding_kind: BindingKind::Repository,
        repository_root,
        external_task_ref: request.external_task_ref.clone(),
        run_id: request.run_id.clone(),
        worktree,
        branch,
        base_sha,
        workspace_id,
        pane_id: format!("pane_{}", request.run_id),
    })
}

fn canonical_terminal_directory(path: &Path) -> Result<PathBuf, String> {
    reject_parent_components(path, "runtime_directory")?;
    let path = fs::canonicalize(path)
        .map_err(|error| format!("could not canonicalize runtime_directory: {error}"))?;
    if !path.is_dir() {
        return Err("runtime_directory must be an existing directory".into());
    }
    Ok(path)
}

fn validate_durable_adapter(request: &DispatchRequest) -> Result<(), String> {
    if request.adapter.id == crate::adapter::AdapterId::Generic
        || request.adapter.executable.is_some()
        || !request.adapter.arguments.is_empty()
    {
        return Err(
            "durable programme gates require an argument-free built-in adapter; raw commands and explicit executable paths are not persisted"
                .into(),
        );
    }
    Ok(())
}

fn validate_durable_gate(gate: &DurableProgrammeGate) -> Result<(), String> {
    if gate.schema_version != 2 {
        return Err(format!(
            "unsupported durable programme gate schema version {}",
            gate.schema_version
        ));
    }
    validate_run_id(&gate.upstream_run_id)?;
    validate_durable_adapter(&gate.dispatch)?;
    let binding = validate_binding(&gate.dispatch)?;
    if repository_id(&binding.repository_root) != gate.downstream_repository_id {
        return Err("durable programme gate downstream repository identity no longer matches its validated binding".into());
    }
    if gate.upstream_repository_id.is_empty() {
        return Err("durable programme gate has no upstream repository identity".into());
    }
    gate.dispatch
        .adapter
        .resolve()
        .map_err(|error| format!("durable programme gate adapter is unavailable: {error}"))?;
    Ok(())
}

fn validate_upstream_dispatch_receipt(
    receipts: &Path,
    gate: &DurableProgrammeGate,
) -> Result<(), String> {
    let path = receipts.join(format!("{}.json", gate.upstream_run_id));
    let bytes = fs::read(&path)
        .map_err(|error| format!("could not read exact upstream dispatch receipt: {error}"))?;
    let receipt: DispatchReceipt = serde_json::from_slice(&bytes)
        .map_err(|error| format!("could not parse exact upstream dispatch receipt: {error}"))?;
    if receipt.protocol_version != crate::protocol::PROTOCOL_VERSION {
        return Err("exact upstream dispatch receipt has an invalid protocol version".into());
    }
    if receipt.run_id != gate.upstream_run_id {
        return Err("exact upstream dispatch receipt does not match the gate run binding".into());
    }
    if receipt.repository_id != gate.upstream_repository_id {
        return Err(
            "exact upstream dispatch receipt does not match the gate repository identity".into(),
        );
    }
    if !receipt.repository_root_canonical
        || !receipt.worktree_canonical
        || !receipt.shared_git_common_directory
    {
        return Err(
            "exact upstream dispatch receipt does not contain validated binding authority".into(),
        );
    }
    Ok(())
}

fn gate_for_storage(
    receipts: &Path,
    gate: &DurableProgrammeGate,
) -> Result<DurableProgrammeGate, String> {
    let state_dir = receipts
        .parent()
        .ok_or("dispatch receipt directory has no state parent")?;
    let mut stored = gate.clone();
    stored.dispatch.repository_root =
        relative_path(state_dir, Path::new(&gate.dispatch.repository_root))?
            .display()
            .to_string();
    stored.dispatch.worktree = relative_path(state_dir, Path::new(&gate.dispatch.worktree))?
        .display()
        .to_string();
    Ok(stored)
}

fn restore_durable_gate(
    state_dir: &Path,
    mut gate: DurableProgrammeGate,
) -> Result<DurableProgrammeGate, String> {
    for value in [
        &mut gate.dispatch.repository_root,
        &mut gate.dispatch.worktree,
    ] {
        let path = Path::new(value);
        if path.is_absolute() {
            return Err("durable programme gates must not contain absolute local paths".into());
        }
        *value = fs::canonicalize(state_dir.join(path))
            .map_err(|error| format!("could not restore durable gate path binding: {error}"))?
            .display()
            .to_string();
    }
    Ok(gate)
}

fn relative_path(from: &Path, to: &Path) -> Result<PathBuf, String> {
    if !from.is_absolute() || !to.is_absolute() {
        return Err("durable path binding requires canonical absolute inputs".into());
    }
    let from: Vec<_> = from.components().collect();
    let to: Vec<_> = to.components().collect();
    let shared = from
        .iter()
        .zip(&to)
        .take_while(|(left, right)| left == right)
        .count();
    let mut relative = PathBuf::new();
    for _ in shared..from.len() {
        relative.push("..");
    }
    for component in &to[shared..] {
        relative.push(component.as_os_str());
    }
    Ok(relative)
}

/// Run ids that arrive from a client. Dock's own pane shells own the `dock_sh_` namespace and
/// are identified by it inside the registry, so a caller must not be able to mint an id there:
/// a real run wearing a pane's shell identity would be stopped and unbound by the next dispatch
/// into that pane.
fn validate_external_run_id(value: &str) -> Result<(), String> {
    validate_run_id(value)?;
    if value.starts_with(PANE_SHELL_RUN_ID_PREFIX) {
        return Err(format!(
            "run_id prefix {PANE_SHELL_RUN_ID_PREFIX:?} is reserved for Dock-owned pane shells"
        ));
    }
    Ok(())
}

fn validate_run_id(value: &str) -> Result<(), String> {
    if !value.starts_with("dock_")
        || value.len() <= 5
        || !value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-'))
    {
        return Err("run_id must be Dock-generated: dock_ followed by letters, numbers, hyphens, or underscores".into());
    }
    Ok(())
}

fn reject_parent_components(path: &Path, field: &str) -> Result<(), String> {
    if path.components().any(|part| part == Component::ParentDir) {
        return Err(format!(
            "{field} must not contain parent-directory traversal"
        ));
    }
    Ok(())
}

fn git_toplevel(worktree: &Path) -> Result<PathBuf, String> {
    let path = PathBuf::from(git(worktree, &["rev-parse", "--show-toplevel"])?);
    fs::canonicalize(path).map_err(|e| format!("could not canonicalize Git top-level: {e}"))
}

fn git_common_dir(worktree: &Path) -> Result<PathBuf, String> {
    let path = PathBuf::from(git(worktree, &["rev-parse", "--git-common-dir"])?);
    let path = if path.is_absolute() {
        path
    } else {
        worktree.join(path)
    };
    fs::canonicalize(path).map_err(|e| format!("could not canonicalize Git common directory: {e}"))
}
fn git(worktree: &Path, args: &[&str]) -> Result<String, String> {
    let output = Command::new("git")
        .arg("-C")
        .arg(worktree)
        .args(args)
        .output()
        .map_err(|e| format!("failed to start Git validation: {e}"))?;
    if !output.status.success() {
        return Err(format!(
            "Git validation failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    String::from_utf8(output.stdout)
        .map(|s| s.trim().into())
        .map_err(|e| format!("Git emitted non-UTF-8 output: {e}"))
}

fn reserve_run_id(path: &Path) -> Result<(), String> {
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
        .map_err(|e| {
            format!(
                "could not reserve durable run id at {}: {e}",
                path.display()
            )
        })?;
    file.set_permissions(fs::Permissions::from_mode(0o600))
        .map_err(|e| format!("could not secure durable run-id reservation: {e}"))?;
    if let Err(error) = file.write_all(b"{}\n").and_then(|_| file.sync_all()) {
        let _ = fs::remove_file(path);
        return Err(format!("could not persist run-id reservation: {error}"));
    }
    sync_parent(path)
}

fn rollback_run_id_reservation(path: &Path) -> Result<(), String> {
    match fs::remove_file(path) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => {
            return Err(format!(
                "could not remove durable run-id reservation at {}: {error}",
                path.display()
            ));
        }
    }
    #[cfg(test)]
    if let Some(hook) = ROLLBACK_AFTER_UNLINK_HOOK.with(|hook| hook.borrow_mut().take()) {
        hook()?;
    }
    sync_parent(path)
}

#[cfg(test)]
type RollbackAfterUnlinkHook = Box<dyn FnOnce() -> Result<(), String>>;

#[cfg(test)]
thread_local! {
    static ROLLBACK_AFTER_UNLINK_HOOK: std::cell::RefCell<Option<RollbackAfterUnlinkHook>> =
        const { std::cell::RefCell::new(None) };
}

fn sync_parent(path: &Path) -> Result<(), String> {
    File::open(
        path.parent()
            .ok_or("durable record has no parent directory")?,
    )
    .and_then(|directory| directory.sync_all())
    .map_err(|error| format!("could not sync durable record directory: {error}"))
}

fn dispatch_receipt_is_committed(path: &Path, run_id: &str) -> bool {
    fs::read(path)
        .ok()
        .and_then(|bytes| serde_json::from_slice::<DispatchReceipt>(&bytes).ok())
        .is_some_and(|receipt| {
            receipt.run_id == run_id
                && receipt.protocol_version == crate::protocol::PROTOCOL_VERSION
                && receipt.repository_root_canonical
                && receipt.worktree_canonical
                && receipt.shared_git_common_directory
        })
}

fn save_receipt(path: &Path, snapshot: &RuntimeSnapshot) -> Result<(), String> {
    let repository_root = Path::new(&snapshot.repository_root);
    let worktree = Path::new(&snapshot.worktree);
    let relative = worktree
        .strip_prefix(repository_root)
        .map_err(|_| "validated worktree no longer belongs to its repository root")?;
    let receipt = DispatchReceipt {
        protocol_version: crate::protocol::PROTOCOL_VERSION,
        repository_id: repository_id(repository_root),
        worktree_relative: if relative.as_os_str().is_empty() {
            ".".into()
        } else {
            relative.display().to_string()
        },
        repository_root_canonical: true,
        worktree_canonical: true,
        shared_git_common_directory: true,
        external_task_ref: snapshot.external_task_ref.clone(),
        run_id: snapshot.run_id.clone(),
        branch: snapshot.branch.clone(),
        base_sha: snapshot.base_sha.clone(),
        workspace_id: snapshot.workspace_id.clone(),
        pane_id: snapshot.pane_id.clone(),
        pid: snapshot.pid,
        process_group_id: snapshot.process_group_id,
        state: snapshot.state.clone(),
        diagnostic: snapshot.diagnostic.clone(),
        adapter: snapshot.adapter.clone(),
        process_capabilities: snapshot.process_capabilities.clone(),
        adapter_capabilities: snapshot.adapter_capabilities.clone(),
        provider_state: snapshot.provider_state,
    };
    let bytes = serde_json::to_vec_pretty(&receipt)
        .map_err(|e| format!("could not serialize dispatch receipt: {e}"))?;
    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| format!("invalid dispatch receipt path {}", path.display()))?;
    let temporary = path.with_file_name(format!(".{file_name}.tmp-{}", std::process::id()));
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&temporary)
        .map_err(|e| format!("could not create private dispatch receipt: {e}"))?;
    let result = (|| {
        file.set_permissions(fs::Permissions::from_mode(0o600))?;
        file.write_all(&bytes)?;
        file.write_all(b"\n")?;
        file.sync_all()?;
        fs::rename(&temporary, path)?;
        File::open(
            path.parent()
                .ok_or_else(|| std::io::Error::other("dispatch receipt has no parent directory"))?,
        )?
        .sync_all()
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result.map_err(|e| format!("could not atomically persist dispatch receipt: {e}"))
}

fn is_capacity_active(snapshot: &RuntimeSnapshot) -> bool {
    matches!(snapshot.state, crate::protocol::ProcessState::Running)
}

fn slot_reserves_capacity(slot: &RuntimeSlot) -> bool {
    if slot.pane_shell {
        // Ruling R20: capacity bounds concurrent agent work. A terminal the user opened is
        // infrastructure, so counting it would make the setting mean "agents plus panes" and
        // would silently refuse both the next agent dispatch and the next pane's own shell.
        return false;
    }
    match &slot.state {
        RuntimeSlotState::Launching { .. }
        | RuntimeSlotState::Restarting { .. }
        | RuntimeSlotState::ReceiptRollbackStopping { .. } => true,
        RuntimeSlotState::RollbackPending { layout, .. } => layout.is_some(),
        RuntimeSlotState::Active(entry) => is_capacity_active(&entry.runtime.snapshot()),
    }
}

/// Geometry for a pane the client has not measured yet. A PTY must be created with some size,
/// and 24x80 is the size every terminal falls back to; the first `pane_resize` corrects it.
const UNMEASURED_PANE_SIZE: PtySize = PtySize { rows: 24, cols: 80 };

fn pane_size_key(workspace_id: &str, pane_id: &str) -> String {
    format!("{workspace_id}/{pane_id}")
}

/// A pane's shell is identified by the pane it serves, not by the launch that created it, so the
/// same pane always reclaims the same identity across relaunches.
/// How a queue is keyed: `(workspace_id, pane_id)`.
///
/// Named because it is written out a dozen times and because the *pane* half is the decision. A
/// run dies and is replaced by a resume, a respawn or a daemon restart; the pane is the identity
/// `layout.json` persists and the one the user thinks in.
type QueueKey = (String, String);

/// What one tick knows about one run.
///
/// Gathered from `pulse` in production and by hand in tests, which is the whole reason it is a
/// struct: the wiring around `PaneQueue` has to be reachable without a real agent process.
struct QueueObservation {
    run_id: String,
    agent: Option<AgentKind>,
    state: AgentState,
    /// Whether this exact state is what the agent said about itself, rather than what its screen
    /// was read to mean. Guard (4) is about the provenance of the `Done` in hand.
    reported: bool,
}

/// One queue in the form that goes to disk. Entries and the id counter; nothing else, by design.
fn durable_queue(key: &QueueKey, queue: &PaneQueue) -> DurablePaneQueue {
    DurablePaneQueue {
        schema_version: crate::storage::QUEUE_SCHEMA_VERSION,
        workspace_id: key.0.clone(),
        pane_id: key.1.clone(),
        next_entry_id: queue.next_entry_id(),
        entries: queue
            .entries()
            .iter()
            .map(|entry| DurableQueueEntry {
                entry_id: entry.entry_id,
                label: entry.label.clone(),
                prompt: entry.prompt.clone(),
            })
            .collect(),
    }
}

/// Reads every stored queue back, and refuses to bring three things back with it.
///
/// **Arming.** `PaneQueue::restored` forces `auto_feed` off and says so, and nothing here can
/// override that — the file does not carry the flag in the first place. A daemon that comes back
/// from a crash and immediately starts typing at agents is precisely the unattended behaviour the
/// whole design guards against, so a restart is a disarm, whatever the pane was before.
///
/// **A queue whose pane is gone.** Dropped, and its file with it, so a pane closed while the
/// daemon was down does not leave an entry to be inherited by the next pane to wear its name.
///
/// **A file that will not parse.** Quarantined and stepped over, exactly as an unreadable
/// programme gate is: the file is the only copy of work somebody queued, and a daemon that refuses
/// to boot because one of them is corrupt is worse than one that boots without it.
fn restore_pane_queues(
    store: &LocalStore,
    layout: &LayoutRegistry,
) -> HashMap<QueueKey, PaneQueue> {
    let records = match store.list_pane_queues() {
        Ok(records) => records,
        Err(message) => {
            eprintln!("dockd: could not read stored queues: {message}");
            return HashMap::new();
        }
    };
    let mut queues = HashMap::new();
    for record in records {
        let stored = match record.queue {
            Ok(stored) => stored,
            Err(message) => {
                eprintln!(
                    "dockd: quarantining an unreadable queue {}: {message}",
                    record.identity
                );
                if let Err(message) = store.quarantine_pane_queue(&record.identity) {
                    eprintln!("dockd: could not quarantine that queue: {message}");
                }
                continue;
            }
        };
        if !layout.pane_exists(&stored.workspace_id, &stored.pane_id) {
            if let Err(message) = store.remove_pane_queue(&stored.workspace_id, &stored.pane_id) {
                eprintln!("dockd: could not drop the queue of a departed pane: {message}");
            }
            continue;
        }
        let entries = stored
            .entries
            .into_iter()
            .map(|entry| QueueEntry {
                entry_id: entry.entry_id,
                label: entry.label,
                prompt: entry.prompt,
            })
            .collect();
        queues.insert(
            (stored.workspace_id, stored.pane_id),
            PaneQueue::restored(entries, stored.next_entry_id),
        );
    }
    queues
}

/// How often the queue thread looks at the world.
///
/// Auto-feed is not latency-critical — a quarter second after an agent finishes is invisible — and
/// 250ms keeps the cost of the process-table walk, already time-limited at 500ms, in the noise.
pub const QUEUE_TICK_INTERVAL: Duration = Duration::from_millis(250);

/// Starts the daemon's queue thread.
///
/// A thread of its own rather than a step in the subscriber loop. That loop is per-connection, so
/// a queue driven from it would advance only while a TUI was attached and would advance N times
/// over with N clients — a queue that drains four entries because four dashboards are open is not
/// a queue anybody can reason about.
pub fn spawn_queue_tick(runtime: Arc<RuntimeRegistry>) -> std::io::Result<thread::JoinHandle<()>> {
    thread::Builder::new()
        .name("dock-queue".into())
        .spawn(move || {
            loop {
                runtime.queue_tick();
                thread::sleep(QUEUE_TICK_INTERVAL);
            }
        })
}

fn pane_shell_run_id(workspace_id: &str, pane_id: &str) -> String {
    format!("{PANE_SHELL_RUN_ID_PREFIX}{workspace_id}_{pane_id}")
}

const PANE_SHELL_RUN_ID_PREFIX: &str = "dock_sh_";

/// How long a process-table snapshot is reused before another is taken.
///
/// The event stream polls every 16ms, and it used to take a fresh snapshot each time. On a machine
/// with 839 processes that call costs about 39ms — more than the interval itself — so the daemon
/// burned upwards of two cores continuously, per subscribed dashboard, and could not keep pace
/// with its own loop. What it was asking is which agent runs under a pane, and that answer changes
/// when a person starts a program: at human speed, not at frame rate. Half a second is far below
/// the point anyone notices a new agent appearing and roughly thirty times cheaper.
const PROCESS_TABLE_TTL: Duration = Duration::from_millis(500);

/// The longest a process-table snapshot is reused when not one pane has written a byte.
///
/// [`RuntimeRegistry::process_table`] reuses a snapshot indefinitely while every run's output mark
/// stands still, on the grounds that an agent starting or exiting writes something. This is the
/// backstop for a case that reasoning cannot cover — an agent that starts and prints nothing at
/// all, in a pane that prints nothing at all — so the worst the inference can cost is a slower
/// answer rather than a permanently wrong one. Five seconds is two hundred times cheaper than the
/// unconditional half-second refresh and still well under the time it takes somebody to look up.
const PROCESS_TABLE_QUIET_TTL: Duration = Duration::from_secs(5);

/// The last process table taken, and whether another is on its way.
#[derive(Default)]
struct ProcessTableCache {
    latest: Option<ProcessTableSnapshot>,
    /// Set while a refresh thread is out, so a poll loop running at 62Hz starts one `ps` rather
    /// than one per tick for as long as the first takes to answer.
    refreshing: bool,
}

/// One process table, and the state of the world it was taken against.
struct ProcessTableSnapshot {
    taken: Instant,
    /// Bumped for each new table, so a memoised classification can tell that the table underneath
    /// it changed without comparing the tables themselves.
    generation: u64,
    tree: Arc<ProcessTree>,
    /// Every run's output mark as at the moment this table was taken. A later poll whose marks all
    /// match cannot be a poll where a new agent appeared, because starting one writes to the pane.
    marks: HashMap<String, OutputMark>,
}

impl ProcessTableSnapshot {
    fn take(generation: u64, marks: HashMap<String, OutputMark>) -> Option<Self> {
        // Indexed once and shared. Every pane asks the same snapshot the same question, and
        // building a private index per pane was two dozen full parses a second on a busy layout.
        let tree = Arc::new(ProcessTree::parse(&read_process_table()?));
        Some(Self {
            taken: Instant::now(),
            generation,
            tree,
            marks,
        })
    }
}

/// How recently a pane must have written for its agent to count as working.
///
/// This is the signal that does not need to know what any agent looks like. An agent that is
/// thinking, running a tool, or printing an answer is writing to its terminal; one that is waiting
/// for a person is silent. Three rounds of patterns tried to recognise "working" from the text on
/// screen and were wrong each time, because every CLI spells it differently and respells it
/// between releases. Bytes are bytes.
///
/// Generous enough to bridge the gaps between spinner frames and short pauses between tool calls,
/// short enough that a finished answer stops looking busy while the reader is still looking at it.
const WORKING_SILENCE: Duration = Duration::from_millis(1200);

/// How long output must have been arriving before it counts as work rather than animation.
///
/// Agents redraw while idle: Claude's footer counts elapsed seconds, and a cursor blinks. Those
/// are single short bursts seconds apart, and treating any recent byte as work made the state flip
/// every time one landed inside the window. Real work streams — a burst that is still going a
/// moment later is generation, and one that was over as soon as it started was a clock.
const SUSTAINED_OUTPUT: Duration = Duration::from_millis(400);

/// How long a pane must be silent before silence *alone* is read as the turn coming back.
///
/// A separate number from [`WORKING_SILENCE`], because the two answer different questions and
/// one constant was answering both. "Is this pane still streaming" is a question about the last
/// moment, and 1.2 seconds is right for it. "Has this agent finished" is a claim, and 1.2
/// seconds is nowhere near enough evidence for it: agents pause far longer than that inside a
/// single turn — waiting on a tool call, on the model's first token, between reasoning steps —
/// so a working agent flipped to "your turn" and back on every pause.
///
/// It matters most for the agents Dock cannot read: an agent whose input chrome is recognised
/// says "between turns" through [`AgentState::Done`] from its screen and never reaches this
/// rule, but Copilot and Amp have no such pattern, and for them silence is the only evidence
/// there is. This is the number that decides what they look like.
///
/// Six seconds is the shortest span that outlasts a model's first token on a slow day, and
/// short enough that a finished answer settles while the reader is still looking at the pane.
/// The cost of being wrong is asymmetric and this errs on the safe side: an agent reported busy
/// a moment too long is a small lie that resolves itself, while one reported finished mid-turn
/// invites a person to type into a session that is still running.
const SILENT_HANDOVER: Duration = Duration::from_secs(6);

/// How long a pane must stop writing before the next byte counts as a fresh burst.
///
/// This is what makes "sustained" mean anything. It has to sit in the gap between two rhythms:
/// the frames of a stream, which arrive as fast as the poll can see them (sixteen milliseconds),
/// and the tick of an idle animation, which is about a second. Two hundred milliseconds is an
/// order of magnitude above the first and five times below the second, so a generating agent keeps
/// one long burst while a footer clock produces one short burst per tick and never accumulates.
///
/// The first version of this fix measured the gap against [`WORKING_SILENCE`] instead, and that is
/// the bug it was written to fix: a clock ticking every second never opened a gap of 1.2 seconds,
/// so the burst that began when the pane opened never ended, and after the first second every idle
/// pane looked like it had been streaming for as long as it had been alive.
const BURST_GAP: Duration = Duration::from_millis(200);

/// How long a new answer must hold before the roster is allowed to show it.
///
/// Panes are polled every sixteen milliseconds and every input to the decision is a sample: a
/// screen scraped mid-repaint, a burst that had not finished arriving. Without a dwell, one
/// unlucky frame in sixty is a visible change of state, and a roster that changes its mind sixty
/// times a second is one nobody can read. Six hundred milliseconds is long enough to outlast any
/// single bad sample and short enough that a real handover still lands well inside the second it
/// takes a person to look over. `Blocked` is exempt: see [`StateTracker::settle`].
const STATE_DWELL: Duration = Duration::from_millis(600);

/// Whether output arriving in a pane is generation rather than an idle redraw.
///
/// A free function so the judgement can be tested at the timings that actually caused trouble.
/// Reaching it through a registry needs a real agent process in the table, which a unit test
/// cannot conjure — an earlier attempt at this test drove a pane shell instead, where detection
/// finds no agent and the decision below is never reached, so it passed against the very logic it
/// was written to catch.
fn output_looks_like_work(quiet_for: Duration, growing_for: Duration) -> bool {
    quiet_for < WORKING_SILENCE && growing_for >= SUSTAINED_OUTPUT
}

/// How far one run's output log has got, read under the pane's own lock.
///
/// A free function rather than a closure at each call site, because three of them now need the
/// same pair and a pane's mark meaning something different in one of them would be a bug that
/// looked like a caching bug.
fn output_mark(output: &PaneOutput) -> OutputMark {
    (output.log().epoch(), output.log().end())
}

/// One snapshot of the process table, shared by every run in a single `inspect` and reused across
/// calls for [`PROCESS_TABLE_TTL`]. Agent detection sits on the event-stream hot path, so it must
/// cost neither a subprocess per run nor a subprocess per frame.
fn read_process_table() -> Option<String> {
    let output = Command::new("ps")
        .args(["-axo", "pid=,ppid=,pgid=,comm="])
        .output()
        .ok()?;
    Some(String::from_utf8_lossy(&output.stdout).into_owned())
}

fn repository_id(path: &Path) -> String {
    // FNV-1a is specified here rather than delegated to Rust's Hash implementation, whose
    // algorithm is intentionally not a durable-format contract.
    let mut digest = 0xcbf29ce484222325_u64;
    for byte in path.as_os_str().as_bytes() {
        digest ^= u64::from(*byte);
        digest = digest.wrapping_mul(0x100000001b3);
    }
    format!("repo-v2-{digest:016x}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Check;
    // Only the tests build launch requests by hand now; the runtime destructures the one the
    // protocol already typed.
    use crate::protocol::DashboardProfile;
    use std::{
        os::unix::fs::symlink,
        sync::atomic::{AtomicU64, Ordering},
        thread,
        time::{Duration, Instant},
    };
    static SEQ: AtomicU64 = AtomicU64::new(0);

    /// A registry over a throwaway state directory. Panes auto-launch real shells, so the guard
    /// retires every run it still owns rather than leaking process groups into the test run.
    struct TestRegistry {
        registry: RuntimeRegistry,
        state: PathBuf,
    }
    impl std::ops::Deref for TestRegistry {
        type Target = RuntimeRegistry;
        fn deref(&self) -> &Self::Target {
            &self.registry
        }
    }
    impl Drop for TestRegistry {
        fn drop(&mut self) {
            let run_ids: Vec<_> = self
                .registry
                .runs
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .keys()
                .cloned()
                .collect();
            for run_id in run_ids {
                let _ = self.registry.lifecycle(&run_id, LifecycleOperation::Stop);
            }
            let _ = fs::remove_dir_all(&self.state);
        }
    }

    fn registry() -> TestRegistry {
        registry_with_capacity(CapacityPolicy::default())
    }

    fn registry_with_capacity(capacity: CapacityPolicy) -> TestRegistry {
        let state = std::env::current_dir()
            .unwrap()
            .join("target")
            .join(format!(
                "dock-registry-{}-{}",
                std::process::id(),
                SEQ.fetch_add(1, Ordering::Relaxed)
            ));
        fs::create_dir_all(&state).unwrap();
        fs::set_permissions(&state, fs::Permissions::from_mode(0o700)).unwrap();
        let registry = RuntimeRegistry::with_capacity(&state, 2000, capacity).unwrap();
        TestRegistry { registry, state }
    }

    #[test]
    fn a_registry_reports_the_pane_history_budget_it_was_built_with() {
        let registry = registry();
        assert_eq!(
            registry.pane_history_bytes(),
            crate::terminal::PANE_HISTORY_BYTES,
            "an unconfigured registry uses the default budget"
        );
        drop(registry);

        let state = std::env::current_dir()
            .unwrap()
            .join("target")
            .join(format!(
                "dock-registry-{}-{}",
                std::process::id(),
                SEQ.fetch_add(1, Ordering::Relaxed)
            ));
        fs::create_dir_all(&state).unwrap();
        fs::set_permissions(&state, fs::Permissions::from_mode(0o700)).unwrap();
        let configured = RuntimeRegistry::new(&state, 2000)
            .expect("registry")
            .with_pane_history_bytes(4 << 20);
        assert_eq!(configured.pane_history_bytes(), 4 << 20);
        let _ = fs::remove_dir_all(&state);
    }

    #[test]
    fn creating_a_workspace_launches_a_shell_so_the_pane_is_never_inert() {
        let registry = registry();
        registry
            .workspace(WorkspaceRequest::Create {
                workspace_id: "w1".into(),
                name: "Daily".into(),
                pane_id: "p1".into(),
            })
            .expect("create workspace");
        let layout = registry.layout();
        let workspace = &layout.workspaces[0];
        let pane = &workspace.panes["p1"];
        assert!(
            pane.run_id.is_some(),
            "new pane must be bound to a shell run"
        );
        assert_eq!(pane.runtime, PaneRuntime::Running);
    }

    #[test]
    fn splitting_a_pane_launches_a_shell_in_the_new_pane() {
        let registry = registry();
        registry
            .workspace(WorkspaceRequest::Create {
                workspace_id: "w1".into(),
                name: "Daily".into(),
                pane_id: "p1".into(),
            })
            .expect("create workspace");
        registry
            .workspace(WorkspaceRequest::Split {
                workspace_id: "w1".into(),
                pane_id: "p1".into(),
                new_pane_id: "p2".into(),
                axis: crate::layout::SplitAxis::Vertical,
                kind: PaneKind::Terminal,
            })
            .expect("split pane");
        let layout = registry.layout();
        assert!(layout.workspaces[0].panes["p2"].run_id.is_some());
    }

    /// `dock` auto-starts `dockd`, so a reboot restarts the daemon under every dashboard. A
    /// restored pane comes back with no run at all, and without this every pane on the screen
    /// would be a pane the user cannot type into.
    #[test]
    fn a_pane_restored_after_a_daemon_restart_is_given_a_fresh_shell_and_becomes_usable() {
        let state = std::env::current_dir()
            .unwrap()
            .join("target")
            .join(format!(
                "dock-restored-panes-{}-{}",
                std::process::id(),
                SEQ.fetch_add(1, Ordering::Relaxed)
            ));
        fs::create_dir_all(&state).unwrap();
        fs::set_permissions(&state, fs::Permissions::from_mode(0o700)).unwrap();
        let original = {
            let first = RuntimeRegistry::new(&state, 2000).unwrap();
            first
                .workspace(WorkspaceRequest::Create {
                    workspace_id: "w1".into(),
                    name: "Daily".into(),
                    pane_id: "p1".into(),
                })
                .expect("create workspace");
            let original = first.layout().workspaces[0].panes["p1"]
                .run_id
                .clone()
                .expect("a new pane auto-launches a shell");
            first
                .lifecycle(&original, LifecycleOperation::Stop)
                .expect("stop the first shell");
            original
        };

        let restored = TestRegistry {
            registry: RuntimeRegistry::new(&state, 2000).unwrap(),
            state: state.clone(),
        };
        // The durable layout deliberately records no PID, PGID, or screen content, so the pane
        // returns with no run: there is nothing to reattach to and nothing is reattached.
        assert!(restored.layout().workspaces[0].panes["p1"].run_id.is_none());
        assert!(restored.pane_input("w1", "p1", b"x").is_err());

        restored.revive_restored_panes();
        let revived = restored.layout().workspaces[0].panes["p1"]
            .run_id
            .clone()
            .expect("a restored pane is given a fresh Dock-owned shell");
        assert_eq!(
            revived, original,
            "the shell identity belongs to the pane, not to one launch"
        );
        assert!(restored.pane_input("w1", "p1", b"echo revived\n").is_ok());
    }

    /// The one property that makes a Board pane a board rather than a terminal with a picture in
    /// it, asserted where it is hardest to assert: as the absence of a process.
    ///
    /// Every one of the three paths that gives a pane a shell is exercised here, because the
    /// guard lives inside `launch_pane_shell` rather than at its callers — and a guard at the
    /// callers is a guard the fourth caller forgets.
    #[test]
    fn a_board_pane_is_never_given_a_shell_on_create_split_or_revive() {
        let state = std::env::current_dir()
            .unwrap()
            .join("target")
            .join(format!(
                "dock-board-pane-{}-{}",
                std::process::id(),
                SEQ.fetch_add(1, Ordering::Relaxed)
            ));
        fs::create_dir_all(&state).unwrap();
        fs::set_permissions(&state, fs::Permissions::from_mode(0o700)).unwrap();
        let shell_run_id = pane_shell_run_id("w1", "board");

        {
            // A plain registry rather than a `TestRegistry`: this one has to leave the state
            // directory behind for the restart below, and only its own shells are stopped.
            let first = RuntimeRegistry::new(&state, 2000).unwrap();
            first
                .workspace(WorkspaceRequest::Create {
                    workspace_id: "w1".into(),
                    name: "Daily".into(),
                    pane_id: "p1".into(),
                })
                .expect("create workspace");
            first
                .workspace(WorkspaceRequest::Split {
                    workspace_id: "w1".into(),
                    pane_id: "p1".into(),
                    new_pane_id: "board".into(),
                    axis: crate::layout::SplitAxis::Vertical,
                    kind: PaneKind::Board,
                })
                .expect("split a board onto the canvas");

            let layout = first.layout();
            assert!(
                layout.workspaces[0].panes["p1"].run_id.is_some(),
                "splitting a board must not cost the other half its shell"
            );
            assert_eq!(layout.workspaces[0].panes["board"].run_id, None);
            let runs = first.inspect(None).expect("inspect");
            assert!(
                !runs.iter().any(|run| run.pane_id == "board"),
                "no run may exist for a board pane, not even a stopped one"
            );
            assert!(
                first.inspect(Some(&shell_run_id)).is_err(),
                "the pane shell identity a board would have had was never reserved"
            );
            // No PTY, so no input path either — and this is the daemon refusing rather than the
            // client declining to ask.
            assert!(first.pane_input("w1", "board", b"x").is_err());

            let terminal_shell = first.layout().workspaces[0].panes["p1"]
                .run_id
                .clone()
                .expect("the terminal half has a shell");
            first
                .lifecycle(&terminal_shell, LifecycleOperation::Stop)
                .expect("stop the terminal half's shell");
        }

        // A restart is the other half of the claim: the kind is durable, so the pane comes back
        // a board, and reviving restored panes must walk straight past it.
        let restored = TestRegistry {
            registry: RuntimeRegistry::new(&state, 2000).unwrap(),
            state: state.clone(),
        };
        assert_eq!(
            restored.layout().workspaces[0].panes["board"].kind,
            PaneKind::Board
        );
        restored.revive_restored_panes();
        assert_eq!(
            restored.layout().workspaces[0].panes["board"].run_id,
            None,
            "a restored board must not be handed a shell by the revive sweep"
        );
        assert!(
            restored.layout().workspaces[0].panes["p1"].run_id.is_some(),
            "and the terminal beside it must still be revived"
        );
        assert!(
            !restored
                .inspect(None)
                .expect("inspect")
                .iter()
                .any(|run| run.pane_id == "board")
        );

        // Respawn is the third path, and it is refused by name rather than silently doing
        // nothing: `Ctrl+B R` on a board should say why, not look broken.
        let (_, message) = restored
            .workspace(WorkspaceRequest::Respawn {
                workspace_id: "w1".into(),
                pane_id: "board".into(),
            })
            .expect_err("respawning a board must be refused");
        assert!(message.contains("board"), "{message}");
        assert_eq!(restored.layout().workspaces[0].panes["board"].run_id, None);
    }

    /// Typing `exit` leaves a pane with a dead shell. Recovery is a keyboard command, so the
    /// request behind it must work on a dead pane and refuse a live one.
    #[test]
    fn respawning_revives_a_dead_pane_and_never_displaces_a_live_one() {
        let registry = registry();
        registry
            .workspace(WorkspaceRequest::Create {
                workspace_id: "w1".into(),
                name: "Daily".into(),
                pane_id: "p1".into(),
            })
            .expect("create workspace");
        let original = registry.layout().workspaces[0].panes["p1"]
            .run_id
            .clone()
            .expect("a new pane auto-launches a shell");
        let error = registry
            .workspace(WorkspaceRequest::Respawn {
                workspace_id: "w1".into(),
                pane_id: "p1".into(),
            })
            .expect_err("a running pane must never be respawned out from under the user");
        assert_eq!(error.0, ErrorCode::UnsupportedOperation);

        registry
            .lifecycle(&original, LifecycleOperation::Stop)
            .expect("stop the shell");
        let workspace = registry
            .workspace(WorkspaceRequest::Respawn {
                workspace_id: "w1".into(),
                pane_id: "p1".into(),
            })
            .expect("respawn a dead pane")
            .expect("respawn returns the workspace");
        assert_eq!(
            workspace.panes["p1"].run_id.as_deref(),
            Some(original.as_str())
        );
        assert!(registry.pane_input("w1", "p1", b"echo alive\n").is_ok());

        let missing = registry
            .workspace(WorkspaceRequest::Respawn {
                workspace_id: "w1".into(),
                pane_id: "nope".into(),
            })
            .expect_err("respawning a pane that does not exist must be refused");
        assert_eq!(missing.0, ErrorCode::InvalidLayout);
    }

    #[test]
    fn pane_resize_requires_a_live_owned_binding_and_reports_why() {
        let registry = registry();
        let error = registry
            .pane_resize("missing", "pane", 24, 80)
            .expect_err("unbound pane must be refused");
        assert_eq!(error.0, ErrorCode::InvalidBinding);
        assert!(error.1.contains("not bound"));
    }

    #[test]
    fn pane_resize_reaches_the_exact_owned_runtime() {
        let registry = registry();
        registry
            .workspace(WorkspaceRequest::Create {
                workspace_id: "w1".into(),
                name: "Daily".into(),
                pane_id: "p1".into(),
            })
            .expect("create workspace");
        registry
            .pane_resize("w1", "p1", 40, 120)
            .expect("resize owned pane");
        let snapshot = registry.inspect(None).expect("inspect");
        let run = snapshot
            .iter()
            .find(|run| run.pane_id == "p1")
            .expect("bound run");
        assert_eq!((run.rows, run.cols), (40, 120));
    }

    /// A dispatch can be refused after adapter resolution by six further checks. Retiring the
    /// pane's shell before any of them would leave the pane permanently inert, which is the
    /// exact failure auto-launch exists to eliminate.
    #[test]
    fn a_dispatch_refused_after_adapter_resolution_leaves_the_pane_shell_running() {
        let registry = registry();
        registry
            .workspace(WorkspaceRequest::Create {
                workspace_id: "w1".into(),
                name: "Daily".into(),
                pane_id: "p1".into(),
            })
            .expect("create workspace");
        registry
            .workspace(WorkspaceRequest::Split {
                workspace_id: "w1".into(),
                pane_id: "p1".into(),
                new_pane_id: "p2".into(),
                axis: crate::layout::SplitAxis::Vertical,
                kind: PaneKind::Terminal,
            })
            .expect("split pane");
        let directory = registry.state.display().to_string();
        registry
            .terminal_launch(crate::protocol::TerminalLaunchRequest {
                workspace_id: "w1".into(),
                pane_id: "p2".into(),
                run_id: "dock_taken".into(),
                profile: DashboardProfile::Fixture,
                runtime_directory: directory.clone(),
                arguments: Vec::new(),
                external_task_ref: String::new(),
            })
            .expect("first launch claims the run id");

        let refused = registry.terminal_launch(crate::protocol::TerminalLaunchRequest {
            workspace_id: "w1".into(),
            pane_id: "p1".into(),
            run_id: "dock_taken".into(),
            profile: DashboardProfile::Fixture,
            runtime_directory: directory,
            arguments: Vec::new(),
            external_task_ref: String::new(),
        });
        assert!(matches!(refused, Err((ErrorCode::DuplicateRunId, _))));

        let shell_run_id = pane_shell_run_id("w1", "p1");
        let layout = registry.layout();
        let pane = &layout.workspaces[0].panes["p1"];
        assert_eq!(
            pane.run_id.as_deref(),
            Some(shell_run_id.as_str()),
            "a refused dispatch must leave the pane bound to its shell"
        );
        assert_eq!(pane.runtime, PaneRuntime::Running);
        let snapshot = registry.inspect(Some(&shell_run_id)).expect("shell run");
        assert_eq!(snapshot[0].state, crate::protocol::ProcessState::Running);
    }

    #[test]
    fn a_terminal_launch_passes_supplied_arguments_and_they_displace_the_built_in_ones() {
        let registry = registry();
        registry
            .workspace(WorkspaceRequest::Create {
                workspace_id: "w1".into(),
                name: "Workspace".into(),
                pane_id: "p1".into(),
            })
            .expect("create workspace");
        // This is the shape a resume takes on the unbound path: the same profile, launched with
        // the agent's own "continue where you left off" arguments.
        let snapshot = registry
            .terminal_launch(crate::protocol::TerminalLaunchRequest {
                workspace_id: "w1".into(),
                pane_id: "p1".into(),
                run_id: "dock_resumed".into(),
                profile: DashboardProfile::Fixture,
                runtime_directory: registry.state.display().to_string(),
                arguments: vec!["-c".into(), "printf resumed".into()],
                external_task_ref: String::new(),
            })
            .expect("launch with supplied arguments");
        assert!(
            snapshot.command.contains(&"printf resumed".to_owned()),
            "supplied arguments must reach the agent: {:?}",
            snapshot.command
        );
        assert!(
            !snapshot
                .command
                .iter()
                .any(|argument| argument.contains("Dock-owned fixture ready")),
            "supplied arguments must displace the built-in ones rather than joining them: {:?}",
            snapshot.command
        );
        registry
            .lifecycle("dock_resumed", LifecycleOperation::Stop)
            .expect("stop the resumed run");
    }

    #[test]
    fn what_an_agent_says_about_itself_beats_what_its_screen_looks_like() {
        let registry = registry();
        registry
            .workspace(WorkspaceRequest::Create {
                workspace_id: "w1".into(),
                name: "W".into(),
                pane_id: "p1".into(),
            })
            .expect("create workspace");
        let run_id = pane_shell_run_id("w1", "p1");

        // A hook fires on the agent's own turn boundaries, so it knows what a pattern can only
        // infer — and a freshly launched shell writing its prompt would otherwise read as working.
        registry
            .report_agent_state(&run_id, AgentState::Done)
            .expect("report");
        let reported = registry
            .inspect(Some(&run_id))
            .expect("inspect")
            .remove(0)
            .agent_state;
        assert_eq!(reported, AgentState::Done);

        // Sticky: it holds until the agent says otherwise, because "finished" stays true until the
        // next turn starts and a timeout would invent a transition nobody observed.
        std::thread::sleep(Duration::from_millis(50));
        assert_eq!(
            registry
                .inspect(Some(&run_id))
                .expect("inspect")
                .remove(0)
                .agent_state,
            AgentState::Done
        );
        registry
            .report_agent_state(&run_id, AgentState::Working)
            .expect("report again");
        assert_eq!(
            registry
                .inspect(Some(&run_id))
                .expect("inspect")
                .remove(0)
                .agent_state,
            AgentState::Working
        );
    }

    #[test]
    fn a_lone_redraw_is_a_clock_and_a_continuing_stream_is_work() {
        // Agents animate between turns: Claude's footer counts elapsed seconds. Those arrive as a
        // short burst every second or so, and treating any recent byte as work made the roster
        // flip between working and your-turn while nothing was happening.
        let tick = Duration::from_millis(0);
        assert!(
            !output_looks_like_work(Duration::from_millis(10), tick),
            "a burst that was over as soon as it started is a redraw"
        );
        // Generation keeps arriving, so the run of output has age by the time it is looked at.
        assert!(output_looks_like_work(
            Duration::from_millis(10),
            Duration::from_millis(500)
        ));
        // And output that stopped a while ago is the turn handed back, however long it ran.
        assert!(!output_looks_like_work(
            Duration::from_secs(3),
            Duration::from_secs(9)
        ));
    }

    /// One frame of the loop the server polls panes on. The rhythm matters: the accumulator below
    /// is fed at frame rate, and the whole defect was a threshold that only misbehaves when it is.
    const FRAME: Duration = Duration::from_millis(16);

    /// Replays a pane's polling history through a real [`StateTracker`] and returns every state it
    /// reported, so a test can assert on the sequence rather than on one sampled instant.
    ///
    /// `output` says how many bytes the pane wrote on a given frame, and `screen` what
    /// classification the screen produced on it. Time is handed in rather than slept through: a
    /// test that waits out six seconds of a one-hertz animation is a test nobody runs.
    fn replay(
        frames: u64,
        output: impl FnMut(u64) -> u64,
        mut screen: impl FnMut(u64) -> AgentState,
    ) -> Vec<AgentState> {
        replay_read(frames, output, move |frame| screen(frame).into())
    }

    /// [`replay`], for a pane whose terminal title is part of the evidence.
    fn replay_read(
        frames: u64,
        mut output: impl FnMut(u64) -> u64,
        mut screen: impl FnMut(u64) -> ScreenRead,
    ) -> Vec<AgentState> {
        let start = Instant::now();
        let mut tracker = StateTracker::new((1, 0), start);
        let mut written = 0;
        (0..frames)
            .map(|frame| {
                let now = start + FRAME * frame as u32;
                written += output(frame);
                tracker.observe((1, written), now);
                tracker.decide(now, Some(AgentKind::Claude), screen(frame), None)
            })
            .collect()
    }

    /// A poll that could not read the process table must not answer "there is no agent here".
    ///
    /// `decide` commits the absence of an agent with no dwell, so this answer is not one wrong
    /// pane for one poll — it is every agent on the canvas reset to idle at once.
    #[test]
    fn a_poll_with_no_process_table_holds_the_agent_it_last_saw() {
        // Two panes: one shell with codex started inside it, and one shell with nothing in it.
        let table = ProcessTree::parse(
            "\
  501   1  501 zsh
  902 501  902 codex
  777   1  777 zsh
",
        );
        let seen = ClassifiedAgent {
            generation: 7,
            agent: Some(AgentKind::Codex),
            screen: ((1, 0), (24, 80)),
            from_screen: AgentState::Working.into(),
        };

        // The table this poll is already looking at: memoised, no walk.
        assert_eq!(
            RuntimeRegistry::resolved_agent(Some(seen), 7, Some(&table), Some(902)),
            Some(AgentKind::Codex)
        );
        // A newer table, walked afresh.
        assert_eq!(
            RuntimeRegistry::resolved_agent(Some(seen), 8, Some(&table), Some(902)),
            Some(AgentKind::Codex)
        );
        // No table at all — the daemon could not look, so the pane keeps the agent it had. The
        // generation is deliberately one no snapshot carries, which is what a missing table is
        // reported as, so the memo above cannot be what answers here.
        assert_eq!(
            RuntimeRegistry::resolved_agent(Some(seen), u64::MAX, None, Some(902)),
            Some(AgentKind::Codex),
            "a poll that could not read the process table has not looked, \
             which is not the same as having looked and found nothing"
        );
        // …and with nothing previously seen there is genuinely nothing to hold.
        assert_eq!(
            RuntimeRegistry::resolved_agent(None, u64::MAX, None, Some(902)),
            None
        );
        // A table that really does say this pane has no agent in it still says so, and the held
        // answer above must not become a reason to keep reporting one that has exited.
        assert_eq!(
            RuntimeRegistry::resolved_agent(Some(seen), 8, Some(&table), Some(777)),
            None
        );
    }

    /// A pane writing nothing, with a spinner up the whole time.
    fn spinning(frames: u64) -> Vec<AgentState> {
        replay_read(
            frames,
            |_| 0,
            |_| ScreenRead {
                state: AgentState::Idle,
                title_working: true,
            },
        )
    }

    /// The frame the roster first said the turn had come back, and how long after the pane went
    /// still that was.
    fn handover(states: &[AgentState], stopped_at: u64) -> Duration {
        let frame = states
            .iter()
            .position(|state| *state == AgentState::Done)
            .expect("the turn has to come back eventually");
        FRAME * (frame as u64 - stopped_at) as u32
    }

    /// [`replay_read`], with extra reads landing between the event stream's own polls.
    ///
    /// `inspect` resolves state through the same tracker the stream does, so every client
    /// refresh and every `dock inspect` from another terminal adds calls the stream did not
    /// make. They arrive at their own instants and nobody reads their answers.
    fn replay_read_interleaved(
        frames: u64,
        extra_per_frame: u32,
        mut output: impl FnMut(u64) -> u64,
        mut screen: impl FnMut(u64) -> ScreenRead,
    ) -> Vec<AgentState> {
        let start = Instant::now();
        let mut tracker = StateTracker::new((1, 0), start);
        let mut written = 0;
        (0..frames)
            .map(|frame| {
                let now = start + FRAME * frame as u32;
                written += output(frame);
                tracker.observe((1, written), now);
                let reported = tracker.decide(now, Some(AgentKind::Claude), screen(frame), None);
                // The interlopers, spread across the gap before the next poll. Same pane, same
                // screen, later instants — and their answers thrown away, exactly as a snapshot
                // request's are as far as the stream is concerned.
                for step in 1..=extra_per_frame {
                    let later = now + (FRAME / (extra_per_frame + 1)) * step;
                    tracker.observe((1, written), later);
                    tracker.decide(later, Some(AgentKind::Claude), screen(frame), None);
                }
                reported
            })
            .collect()
    }

    /// A snapshot request must not be able to change what the roster says.
    ///
    /// `inspect` and the event stream both drive one `StateTracker`, so a read-shaped call
    /// mutates the hysteresis that decides when a turn is reported as handed back. That is only
    /// safe because every arm of the decision is written against the clock rather than against a
    /// count of calls: an extra poll re-asks a question whose answer depends on elapsed time, so
    /// it can observe a transition sooner within a frame but can never invent one, shorten
    /// [`STATE_DWELL`], or leave two callers trading a `pending` neither of them ever commits.
    ///
    /// Asserted rather than assumed, because the alternative — threading a read-only path
    /// through the registry — is a great deal of machinery to buy a property the design already
    /// has, and nothing but a test can say whether it still has it.
    #[test]
    fn a_snapshot_request_between_polls_never_moves_when_the_turn_comes_back() {
        let stopped_at = 100;
        let output = |frame: u64| if frame < stopped_at { 60 } else { 0 };
        let screen = |frame: u64| ScreenRead {
            state: AgentState::Idle,
            title_working: frame < stopped_at,
        };

        let alone = replay_read(700, output, screen);
        for extra in [1, 3, 7] {
            let crowded = replay_read_interleaved(700, extra, output, screen);
            let (quiet, busy) = (handover(&alone, stopped_at), handover(&crowded, stopped_at));
            // Within one frame: an interloper landing mid-gap can see the dwell elapse before
            // the next poll would have, which is the poll being early rather than being wrong.
            let drift = quiet.abs_diff(busy);
            assert!(
                drift <= FRAME,
                "{extra} extra reads per frame moved the handover by {drift:?} \
                 ({quiet:?} alone, {busy:?} crowded)"
            );
        }
    }

    /// The case the silence clock is structurally unable to see: an agent that is thinking, has
    /// written nothing for ten seconds, and is saying so in its title the entire time.
    ///
    /// Before the title was evidence this pane read as finished after six seconds, and six seconds
    /// was chosen precisely because it is the longest anyone was willing to wait for an answer
    /// that was only ever a guess. A spinning title is not a guess.
    #[test]
    fn an_agent_that_spins_its_title_while_silent_is_working_rather_than_finished() {
        let states = spinning(625);
        assert!(
            states.iter().all(|state| *state == AgentState::Working),
            "ten silent seconds with the spinner up is still one turn: {:?}",
            states.iter().find(|state| **state != AgentState::Working)
        );
    }

    /// …and when the spinner stops, that is the agent saying the turn is over — which is better
    /// evidence than silence, and so is believed sooner.
    #[test]
    fn the_title_going_still_hands_the_turn_back_sooner_than_silence_alone_can() {
        let stopped_at = 100;
        let states = replay_read(
            700,
            |frame| if frame < stopped_at { 60 } else { 0 },
            |frame| ScreenRead {
                state: AgentState::Idle,
                title_working: frame < stopped_at,
            },
        );
        let took = handover(&states, stopped_at);
        assert!(
            took <= WORKING_SILENCE + STATE_DWELL + FRAME * 2,
            "two agreeing signals should settle in {:?}, took {took:?}",
            WORKING_SILENCE + STATE_DWELL
        );
        assert!(
            took < SILENT_HANDOVER,
            "and must beat the number that exists for having no evidence at all"
        );
    }

    /// The fast handover is earned by evidence, not granted to everyone. A pane that never spun a
    /// title has told Dock nothing, and still waits out the full silence — this is what keeps the
    /// arm above from quietly becoming a shorter `SILENT_HANDOVER` for every agent.
    #[test]
    fn a_pane_that_never_spun_a_title_still_waits_out_the_whole_silence() {
        let stopped_at = 100;
        let states = replay(
            700,
            |frame| if frame < stopped_at { 60 } else { 0 },
            |_| AgentState::Idle,
        );
        let took = handover(&states, stopped_at);
        assert!(
            took >= SILENT_HANDOVER,
            "no title, no shortcut: took {took:?}"
        );
    }

    /// An agent thinking between tool calls must not be reported as having handed the turn back.
    ///
    /// Silence is the only evidence Dock has for agents whose input chrome it cannot match —
    /// Copilot and Amp have no awaiting patterns at all — and it was believing that evidence
    /// after 1.2 seconds. Agents pause far longer than that mid-turn: waiting on a tool call, on
    /// the model's first token, between reasoning steps. So a working agent flipped to "your
    /// turn" and back on every pause, which is the flicker this was reported as.
    ///
    /// Two seconds of quiet is a pause. It is not a handover.
    #[test]
    fn an_agent_pausing_between_tool_calls_is_not_reported_as_finished() {
        // A burst of generation, then two seconds of thought, then more generation — one turn,
        // with a gap in the middle of it.
        let frames = 200;
        let states = replay(
            frames,
            |frame| {
                if (30..=155).contains(&frame) { 0 } else { 64 }
            },
            |_| AgentState::Idle,
        );
        // The quiet stretch is frames 30..155 — about two seconds at sixteen milliseconds a
        // frame. Nothing in it may claim the agent has finished.
        let during_the_pause = &states[30..156];
        assert!(
            !during_the_pause.contains(&AgentState::Done),
            "a pause mid-turn is not a handover: {during_the_pause:?}"
        );
    }

    /// The other half: an agent that really has stopped is still reported as finished, or the
    /// fix above would simply have traded one wrong answer for another.
    #[test]
    fn an_agent_that_has_genuinely_stopped_is_still_reported_as_finished() {
        let states = replay(
            700,
            |frame| if frame < 30 { 64 } else { 0 },
            |_| AgentState::Idle,
        );
        assert_eq!(
            states.last().copied(),
            Some(AgentState::Done),
            "silence long enough really is a handover: {:?}",
            &states[states.len().saturating_sub(4)..]
        );
    }

    #[test]
    fn an_idle_pane_redrawing_its_clock_once_a_second_never_flickers_back_to_working() {
        // The defect, at the timings that produced it. Claude's footer counts the elapsed seconds,
        // so an idle pane writes a short burst about once a second and never falls silent for as
        // long as `WORKING_SILENCE` asks. Meanwhile the one rule that says "between turns" is a
        // regex over the visible screen, and it misses whenever a frame is sampled mid-repaint or
        // the footer wraps — so the roster flipped to "working" on those frames and back on the
        // next, on a pane where nothing at all was happening.
        //
        // The earlier unit test passed throughout, because it hand-fed a `growing_for` of zero:
        // a value the real accumulator stops producing after the first second.
        let states = replay(
            375, // six seconds
            |frame| u64::from(frame % 63 == 0) * 120,
            |frame| {
                if frame % 17 == 3 {
                    AgentState::Idle
                } else {
                    AgentState::Done
                }
            },
        );
        let flips = states.windows(2).filter(|pair| pair[0] != pair[1]).count();
        assert_eq!(
            (states[0], flips),
            (AgentState::Done, 0),
            "an idle pane must read as your-turn on every frame, not flicker: \
             {} of {} frames read as working",
            states
                .iter()
                .filter(|state| **state == AgentState::Working)
                .count(),
            states.len(),
        );
    }

    #[test]
    fn a_pane_streaming_a_reply_reads_as_working_until_it_stops() {
        // The other half of the same judgement. Steadiness bought by never noticing work would be
        // no better than the flicker: generation arrives on nearly every frame, and that unbroken
        // rhythm — not the mere fact that something was written — is what separates it from a
        // clock ticking once a second.
        let stops_at = 250usize;
        // Long enough to contain the handover: the stream stops at four seconds, and silence
        // alone is not read as a handover until `SILENT_HANDOVER` has passed on top of that.
        let states = replay(
            800,
            |frame| u64::from(frame < stops_at as u64) * 40,
            // Mid-turn chrome that none of the rules recognise, so the stream is the only witness.
            |_| AgentState::Idle,
        );
        assert!(
            states[..stops_at].iter().all(|s| *s == AgentState::Working),
            "a streaming pane must read as working throughout"
        );
        let handed_back = states
            .iter()
            .rposition(|state| *state == AgentState::Working)
            .expect("the pane was working");
        assert_eq!(states[states.len() - 1], AgentState::Done);
        // And the turn comes back promptly once the stream stops: silence long enough to mean it,
        // plus the dwell, and no longer. Measured against `SILENT_HANDOVER` rather than
        // `WORKING_SILENCE` — ceasing to stream and having finished are different claims, and
        // this test is about the second.
        assert!(
            FRAME * (handed_back - stops_at) as u32
                <= SILENT_HANDOVER + STATE_DWELL + Duration::from_millis(50),
            "the handover took {:?}",
            FRAME * (handed_back - stops_at) as u32
        );
    }

    #[test]
    fn a_new_answer_has_to_hold_before_the_roster_will_show_it() {
        // Every input to this decision is a sample: a screen scraped mid-repaint, a burst that had
        // not finished arriving. At sixty frames a second, one unlucky sample without a dwell in
        // front of it is a visible change of state, and a roster that changes its mind that often
        // is one nobody can read.
        let start = Instant::now();
        let mut tracker = StateTracker::new((1, 0), start);
        assert_eq!(
            tracker.decide(
                start,
                Some(AgentKind::Claude),
                AgentState::Done.into(),
                None
            ),
            AgentState::Done,
            "the first answer for a run commits at once: there is nothing yet to change from"
        );

        // A turn begins, and the chrome on screen is not something the rules can read, so the
        // stream is the only evidence there is.
        let began = 1_000;
        let (mut written, mut first_working) = (0, None);
        for ms in (began..began + 3_000).step_by(FRAME.as_millis() as usize) {
            written += 60;
            let now = start + Duration::from_millis(ms);
            tracker.observe((1, written), now);
            let state = tracker.decide(now, Some(AgentKind::Claude), AgentState::Idle.into(), None);
            if state == AgentState::Working && first_working.is_none() {
                first_working = Some(Duration::from_millis(ms - began));
            }
        }
        let took = first_working.expect("a stream this steady is work by any measure");
        assert!(
            took >= SUSTAINED_OUTPUT + STATE_DWELL,
            "working showed after {took:?}, before the stream had earned it"
        );
        assert!(
            took < SUSTAINED_OUTPUT + STATE_DWELL + FRAME * 2,
            "working showed after {took:?}, long after the stream had earned it"
        );
    }

    #[test]
    fn an_agent_that_is_stuck_says_so_without_waiting_out_the_dwell() {
        // The dwell exists to stop the roster twitching. An agent that cannot continue until
        // somebody answers it is the one thing in the roster that costs the user throughput while
        // it waits, so it is the one thing that must never be held back to look calm.
        let start = Instant::now();
        let mut tracker = StateTracker::new((1, 0), start);
        let (mut written, mut now) = (0, start);
        for _ in 0..60 {
            written += 60;
            now += FRAME;
            tracker.observe((1, written), now);
            tracker.decide(now, Some(AgentKind::Claude), AgentState::Idle.into(), None);
        }
        assert_eq!(
            tracker.decide(now, Some(AgentKind::Claude), AgentState::Idle.into(), None),
            AgentState::Working
        );
        now += FRAME;
        assert_eq!(
            tracker.decide(
                now,
                Some(AgentKind::Claude),
                AgentState::Blocked.into(),
                None
            ),
            AgentState::Blocked,
            "a permission prompt is not something to sit on for half a second"
        );
    }

    #[test]
    fn what_an_agent_reports_about_itself_lands_at_once_and_is_what_inference_resumes_from() {
        // A hook fires on the agent's own turn boundaries, so it outranks anything read off a
        // screen and has nothing to prove by waiting. It also replaces the record rather than
        // sitting on top of it: if the reports later stop — the agent was restarted without its
        // hook, a wrapper fell away — inference has to carry on from where the agent actually was,
        // not from the stale guess it happened to be holding when the first report arrived.
        let start = Instant::now();
        let mut tracker = StateTracker::new((1, 0), start);
        let reported = tracker.decide(
            start + FRAME,
            Some(AgentKind::Claude),
            AgentState::Idle.into(),
            Some(AgentState::Done),
        );
        assert_eq!(reported, AgentState::Done);
        assert_eq!(
            tracker.decide(
                start + FRAME * 2,
                Some(AgentKind::Claude),
                AgentState::Idle.into(),
                None
            ),
            AgentState::Done,
            "with the reports gone and the screen unreadable, the last thing the agent said \
             about itself is the best answer available"
        );
    }

    #[test]
    fn what_a_dead_run_said_about_itself_is_not_inherited_by_the_next_run_of_its_name() {
        let registry = registry();
        registry
            .workspace(WorkspaceRequest::Create {
                workspace_id: "w1".into(),
                name: "Daily".into(),
                pane_id: "p1".into(),
            })
            .expect("create workspace");
        let live = registry
            .pulse()
            .first()
            .map(|pulse| pulse.run_id.clone())
            .expect("the pane has a shell");
        // A run that has since ended, and the report it left behind. Reports are sticky by
        // design and run ids come back — a pane shell wears the same identity every time its
        // pane opens one — so this one would outrank everything the new run's own screen said.
        let departed = "dock_sh_w1_p1_gone".to_owned();
        registry
            .reported_states
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .insert(departed.clone(), AgentState::Done);
        registry
            .output_marks
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .insert(departed.clone(), StateTracker::new((1, 0), Instant::now()));

        registry.pulse();
        assert!(
            !registry
                .reported_states
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .contains_key(&departed),
            "a dead run's sticky report would win forever over the next run of its name"
        );
        assert!(
            registry
                .output_marks
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .contains_key(&live),
            "the sweep must not forget a run that is still going"
        );
    }

    #[test]
    fn a_report_for_a_run_that_is_not_ours_is_refused() {
        let registry = registry();
        // The socket is reachable from inside any pane, so a report naming someone else's run has
        // to be refused rather than recorded against nothing.
        assert!(
            registry
                .report_agent_state("dock_not_mine", AgentState::Working)
                .is_err()
        );
    }

    /// Ruling R20: capacity bounds concurrent agent work, so the terminals a user opens must
    /// neither be refused by it nor consume it.
    #[test]
    fn pane_shells_neither_consume_nor_are_refused_by_agent_capacity() {
        let registry = registry_with_capacity(CapacityPolicy {
            global_run_capacity: 2,
            per_repository_run_capacity: 2,
            human_review_reserved: 0,
        });
        registry
            .workspace(WorkspaceRequest::Create {
                workspace_id: "w1".into(),
                name: "Daily".into(),
                pane_id: "p1".into(),
            })
            .expect("create workspace");
        for (from, new) in [("p1", "p2"), ("p2", "p3")] {
            registry
                .workspace(WorkspaceRequest::Split {
                    workspace_id: "w1".into(),
                    pane_id: from.into(),
                    new_pane_id: new.into(),
                    axis: crate::layout::SplitAxis::Vertical,
                    kind: PaneKind::Terminal,
                })
                .expect("split pane");
        }
        let layout = registry.layout();
        for pane_id in ["p1", "p2", "p3"] {
            let pane = &layout.workspaces[0].panes[pane_id];
            assert_eq!(
                pane.runtime,
                PaneRuntime::Running,
                "pane {pane_id} must have a shell even past the agent capacity of 2"
            );
        }
        assert_eq!(
            registry.inspect_programme().global_active,
            0,
            "pane shells are infrastructure and must not be counted as agent runs"
        );
        registry
            .terminal_launch(crate::protocol::TerminalLaunchRequest {
                workspace_id: "w1".into(),
                pane_id: "p1".into(),
                run_id: "dock_agent_run".into(),
                profile: DashboardProfile::Fixture,
                runtime_directory: registry.state.display().to_string(),
                arguments: Vec::new(),
                external_task_ref: String::new(),
            })
            .expect("agent dispatch must still be admitted with three panes open");
        assert_eq!(registry.inspect_programme().global_active, 1);
    }

    #[test]
    fn the_pane_shell_run_id_namespace_is_reserved_against_callers() {
        let registry = registry();
        registry
            .workspace(WorkspaceRequest::Create {
                workspace_id: "w1".into(),
                name: "Daily".into(),
                pane_id: "p1".into(),
            })
            .expect("create workspace");
        let error = registry
            .terminal_launch(crate::protocol::TerminalLaunchRequest {
                workspace_id: "w1".into(),
                pane_id: "p1".into(),
                run_id: pane_shell_run_id("w1", "p1"),
                profile: DashboardProfile::Fixture,
                runtime_directory: registry.state.display().to_string(),
                arguments: Vec::new(),
                external_task_ref: String::new(),
            })
            .expect_err("a caller must not mint a pane-shell identity");
        assert_eq!(error.0, ErrorCode::InvalidBinding);
        assert!(error.1.contains("reserved"), "{}", error.1);
    }

    /// Auto-launch means `workspace()` no longer produces an empty pane, so the empty-target
    /// binding path needs its own coverage rather than riding on the replace-a-shell path.
    #[test]
    fn launch_into_pane_binds_a_genuinely_empty_pane() {
        let repo = Repo::new("launch-into-empty-pane");
        let registry = RuntimeRegistry::new(&repo.state, 256).unwrap();
        *registry.suppress_pane_shells.lock().unwrap() = true;
        registry
            .workspace(WorkspaceRequest::Create {
                workspace_id: "ui_workspace".into(),
                name: "UI workspace".into(),
                pane_id: "ui_pane".into(),
            })
            .unwrap();
        assert!(
            registry
                .layout()
                .workspaces
                .iter()
                .all(|workspace| workspace.panes["ui_pane"].run_id.is_none())
        );
        let snapshot = registry
            .launch_into_pane(
                repo.request("dock_empty_pane_target"),
                "ui_workspace".into(),
                "ui_pane".into(),
            )
            .unwrap();
        assert_eq!(snapshot.pane_id, "ui_pane");
        assert_eq!(
            registry
                .layout
                .lock()
                .unwrap()
                .pane_run("ui_workspace", "ui_pane")
                .as_deref(),
            Some("dock_empty_pane_target")
        );
    }

    /// A shell whose retirement `stop` fails stays registered under the pane's reserved identity.
    /// Nothing retries that retirement, so unless the identity is reclaimed the pane's next shell
    /// is refused as a duplicate run id and the recreated pane is permanently inert.
    #[test]
    fn a_pane_shell_that_failed_to_stop_does_not_poison_the_panes_next_shell() {
        let registry = registry();
        registry
            .workspace(WorkspaceRequest::Create {
                workspace_id: "w1".into(),
                name: "Daily".into(),
                pane_id: "p1".into(),
            })
            .expect("create workspace");
        let shell_run_id = pane_shell_run_id("w1", "p1");
        *registry
            .retire_pane_shell_stop_failure
            .lock()
            .unwrap_or_else(|p| p.into_inner()) = Some("injected retirement stop failure".into());
        registry
            .terminal_launch(crate::protocol::TerminalLaunchRequest {
                workspace_id: "w1".into(),
                pane_id: "p1".into(),
                run_id: "dock_replacement".into(),
                profile: DashboardProfile::Fixture,
                runtime_directory: registry.state.display().to_string(),
                arguments: Vec::new(),
                external_task_ref: String::new(),
            })
            .expect("launch replaces the pane shell");
        assert!(
            registry
                .runs
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .contains_key(&shell_run_id),
            "the failed retirement must leave the stale shell registered, or this proves nothing"
        );

        registry
            .workspace(WorkspaceRequest::Close {
                workspace_id: "w1".into(),
                pane_id: "p1".into(),
            })
            .expect("close pane");
        registry
            .workspace(WorkspaceRequest::Create {
                workspace_id: "w1".into(),
                name: "Daily".into(),
                pane_id: "p1".into(),
            })
            .expect("recreate workspace");

        let layout = registry.layout();
        let pane = &layout.workspaces[0].panes["p1"];
        assert_eq!(
            pane.run_id.as_deref(),
            Some(shell_run_id.as_str()),
            "the recreated pane must reclaim its own shell identity"
        );
        assert_eq!(
            pane.runtime,
            PaneRuntime::Running,
            "a stale run under the reserved identity must never leave a pane inert"
        );
    }

    /// The same property stated directly against the residue itself, independent of which path
    /// produced it: any run holding a pane's reserved shell identity without owning the pane is
    /// stale, and the pane's shell must launch regardless.
    #[test]
    fn a_stale_run_under_a_reserved_shell_identity_is_reclaimed_on_pane_creation() {
        let registry = registry();
        let shell_run_id = pane_shell_run_id("w1", "p1");
        let receipt = registry.receipt_path(&shell_run_id).expect("receipt path");
        reserve_run_id(&receipt).expect("seed a durable reservation");
        registry
            .runs
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .insert(
                shell_run_id.clone(),
                RuntimeSlot {
                    transition: Arc::new(Mutex::new(())),
                    state: RuntimeSlotState::Launching {
                        repository_root: registry.state.clone(),
                    },
                    pane_shell: true,
                },
            );

        registry
            .workspace(WorkspaceRequest::Create {
                workspace_id: "w1".into(),
                name: "Daily".into(),
                pane_id: "p1".into(),
            })
            .expect("create workspace");

        let layout = registry.layout();
        let pane = &layout.workspaces[0].panes["p1"];
        assert_eq!(pane.run_id.as_deref(), Some(shell_run_id.as_str()));
        assert_eq!(pane.runtime, PaneRuntime::Running);
        let snapshot = registry.inspect(Some(&shell_run_id)).expect("shell run");
        assert_eq!(snapshot[0].state, crate::protocol::ProcessState::Running);
    }

    #[test]
    fn snapshots_report_agent_identity_and_state_without_screen_content() {
        let registry = registry();
        registry
            .workspace(WorkspaceRequest::Create {
                workspace_id: "w1".into(),
                name: "Daily".into(),
                pane_id: "p1".into(),
            })
            .expect("create workspace");
        let snapshot = registry.inspect(None).expect("inspect");
        let encoded = serde_json::to_string(&snapshot).expect("serialize");
        assert!(!encoded.contains("scrollback"));
        let run = &snapshot[0];
        assert_eq!(run.agent, None, "a plain shell is not an agent");
        assert_eq!(run.agent_state, AgentState::Idle);
    }

    #[test]
    fn terminal_launch_uses_exact_non_git_directory_and_pane_without_control_plane_facts() {
        let base = std::env::current_dir()
            .unwrap()
            .join("target")
            .join(format!(
                "dock-terminal-{}-{}-{}",
                std::process::id(),
                SEQ.fetch_add(1, Ordering::Relaxed),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_nanos()
            ));
        let runtime_dir = base.join("plain");
        fs::create_dir_all(&runtime_dir).unwrap();
        fs::create_dir(base.join("state")).unwrap();
        fs::set_permissions(base.join("state"), fs::Permissions::from_mode(0o700)).unwrap();
        let registry = RuntimeRegistry::new(base.join("state"), 4096).unwrap();
        registry
            .workspace(WorkspaceRequest::Create {
                workspace_id: "w".into(),
                name: "plain".into(),
                pane_id: "p".into(),
            })
            .unwrap();
        let snapshot = registry
            .terminal_launch(crate::protocol::TerminalLaunchRequest {
                workspace_id: "w".into(),
                pane_id: "p".into(),
                run_id: "dock_terminal_1".into(),
                profile: DashboardProfile::Fixture,
                runtime_directory: runtime_dir.display().to_string(),
                arguments: Vec::new(),
                external_task_ref: String::new(),
            })
            .unwrap();
        assert_eq!(snapshot.binding_kind, BindingKind::Terminal);
        assert_eq!(snapshot.external_task_ref, "");
        assert_eq!(snapshot.branch, "");
        assert_eq!(snapshot.base_sha, "");
        assert_eq!(snapshot.workspace_id, "w");
        assert_eq!(snapshot.pane_id, "p");
        assert!(!runtime_dir.join(".git").exists());
        assert!(!runtime_dir.join(".dock").exists());
        registry
            .lifecycle("dock_terminal_1", LifecycleOperation::Stop)
            .unwrap();
        fs::remove_dir_all(base).unwrap();
    }

    fn wait_for_owned_group_exit(process_group_id: i32) {
        let deadline = crate::testing::deadline(3);
        while unsafe { nix::libc::kill(-process_group_id, 0) } == 0 && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(10));
        }
        assert_ne!(
            unsafe { nix::libc::kill(-process_group_id, 0) },
            0,
            "retired Dock-owned group {process_group_id} survived lifecycle completion"
        );
    }

    // ---------------------------------------------------------------------------------------
    // The queue's wiring.
    //
    // `queue.rs` proves the *decision* — every guard in §8.4, with no process in it. What is
    // below proves the *wiring*: that the decision is asked the right questions, that the answer
    // reaches a pane through the same door a keystroke does, and that the three things a restart
    // must and must not carry over actually behave that way. None of it depends on a real agent
    // existing, because `pulse` can only report an agent it can genuinely detect and a test that
    // waited for one would pass by never reaching a feed at all.
    // ---------------------------------------------------------------------------------------

    /// A state directory that outlives one registry, for the restart tests.
    fn queue_state_dir(label: &str) -> PathBuf {
        let state = std::env::current_dir()
            .unwrap()
            .join("target")
            .join(format!(
                "dock-queue-{label}-{}-{}",
                std::process::id(),
                SEQ.fetch_add(1, Ordering::Relaxed)
            ));
        let _ = fs::remove_dir_all(&state);
        fs::create_dir_all(&state).unwrap();
        fs::set_permissions(&state, fs::Permissions::from_mode(0o700)).unwrap();
        state
    }

    /// A workspace with one pane, and the id of the shell that pane auto-launched.
    fn queue_pane(registry: &RuntimeRegistry) -> String {
        registry
            .workspace(WorkspaceRequest::Create {
                workspace_id: "w1".into(),
                name: "Daily".into(),
                pane_id: "p1".into(),
            })
            .expect("create workspace");
        registry.layout().workspaces[0].panes["p1"]
            .run_id
            .clone()
            .expect("a new pane auto-launches a shell")
    }

    /// One tick's worth of "what this run is doing", built by hand.
    fn observation(run_id: &str, state: AgentState, reported: bool) -> QueueObservation {
        QueueObservation {
            run_id: run_id.to_owned(),
            agent: Some(AgentKind::Claude),
            state,
            reported,
        }
    }

    /// The one queue a test made, or nothing.
    fn only_queue(registry: &RuntimeRegistry) -> Option<crate::protocol::PaneQueueSnapshot> {
        registry.queue_snapshots().into_iter().next()
    }

    /// Drives a pane from working to a settled, reported `Done` — the full shape of one auto-feed
    /// cycle, with the clock supplied rather than waited on.
    fn feed_cycle(registry: &RuntimeRegistry, run_id: &str, base: Instant) {
        registry.queue_tick_from(&[observation(run_id, AgentState::Working, true)], base);
        registry.queue_tick_from(
            &[observation(run_id, AgentState::Done, true)],
            base + Duration::from_millis(250),
        );
        // Past QUEUE_SETTLE, so guard (5) is satisfied by the clock the test chose rather than by
        // a sleep that could flake on a loaded machine.
        registry.queue_tick_from(
            &[observation(run_id, AgentState::Done, true)],
            base + Duration::from_secs(4),
        );
    }

    /// Every worktree Git knows about, as a set that can be compared before and after.
    fn worktrees(root: &Path) -> Vec<String> {
        let output = Command::new("git")
            .arg("-C")
            .arg(root)
            .args(["worktree", "list", "--porcelain"])
            .output()
            .expect("list worktrees");
        let mut lines: Vec<String> = String::from_utf8_lossy(&output.stdout)
            .lines()
            .filter(|line| line.starts_with("worktree "))
            .map(str::to_owned)
            .collect();
        lines.sort();
        lines
    }

    fn bound_run_ids(registry: &RuntimeRegistry) -> Vec<String> {
        let mut ids: Vec<String> = registry
            .runs
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .keys()
            .cloned()
            .collect();
        ids.sort();
        ids
    }

    fn receipt_count(state: &Path) -> usize {
        fs::read_dir(state.join("dispatches"))
            .map(|entries| entries.count())
            .unwrap_or(0)
    }

    /// The acceptance criterion in one test: *after a daemon restart every pane is unarmed,
    /// whatever it was before, and says so.*
    ///
    /// The entries come back — they are work somebody queued and nothing has happened to it. The
    /// arming does not, and cannot: the file has no field for it. A daemon that came back from a
    /// crash and immediately started typing at agents is exactly the unattended behaviour the
    /// standing safety decision guards against.
    #[test]
    fn a_daemon_restart_disarms_every_pane_and_says_so() {
        let state = queue_state_dir("restart-disarms");
        {
            let first = RuntimeRegistry::new(&state, 2000).unwrap();
            let run_id = queue_pane(&first);
            first
                .report_agent_state(&run_id, AgentState::Done)
                .expect("a hooked agent reports its state");
            first
                .queue_add("w1", "p1", "card 7".into(), "keep going".into())
                .expect("queue a prompt");
            first
                .queue_set_auto("w1", "p1", true)
                .expect("a hooked agent can be armed");
            assert!(
                only_queue(&first).expect("one queue").auto_feed,
                "the pane must actually be armed, or the restart proves nothing"
            );
            let _ = first.lifecycle(&run_id, LifecycleOperation::Stop);
        }

        let restored = TestRegistry {
            registry: RuntimeRegistry::new(&state, 2000).unwrap(),
            state,
        };
        let queue = only_queue(&restored).expect("the queue came back");
        assert_eq!(
            queue.entries.len(),
            1,
            "queued work survives a restart; it is work somebody asked for"
        );
        assert!(
            !queue.auto_feed,
            "arming must not survive a restart, whatever the pane was before"
        );
        assert_eq!(
            queue.holding_because.as_deref(),
            Some(crate::queue::DISARMED_BY_RESTART),
            "and the pane must say why, or a queue that stopped feeding looks broken"
        );
    }

    /// The other half of the restart story, and it goes the opposite way on purpose: *`dock queue
    /// pause` stops every feed daemon-wide, and survives a restart.*
    ///
    /// Pausing before you walk away is a decision that has to hold while you are away, including
    /// across the crash you were not there for. Arming is a decision that must not. The two are
    /// persisted differently because they fail safe in opposite directions.
    #[test]
    fn a_pause_survives_a_restart_and_overrides_an_armed_pane() {
        let state = queue_state_dir("pause-survives");
        {
            let first = RuntimeRegistry::new(&state, 2000).unwrap();
            first.queue_set_paused(true).expect("pause the daemon");
        }
        let restored = TestRegistry {
            registry: RuntimeRegistry::new(&state, 2000).unwrap(),
            state,
        };
        assert!(
            restored.queue_paused(),
            "a pause taken before a restart must still hold after it"
        );
        let run_id = queue_pane(&restored);
        restored
            .report_agent_state(&run_id, AgentState::Done)
            .expect("report a state");
        restored
            .queue_add("w1", "p1", "card 7".into(), "keep going".into())
            .expect("queue a prompt");
        restored
            .queue_set_auto("w1", "p1", true)
            .expect("arm the pane");

        feed_cycle(&restored, &run_id, Instant::now());

        let queue = only_queue(&restored).expect("one queue");
        assert_eq!(
            queue.entries.len(),
            1,
            "a paused daemon feeds nothing however armed a pane is"
        );
        assert!(
            queue
                .holding_because
                .as_deref()
                .is_some_and(|held| held.contains("paused")),
            "and the pause explains itself: {:?}",
            queue.holding_because
        );
    }

    /// `MAX_QUEUED_TOTAL` is the one cap a `PaneQueue` structurally cannot enforce — it can see
    /// itself and nothing else — so it is enforced here, and refused rather than dropped. A queue
    /// that discards work is worse than one that says no.
    #[test]
    fn the_daemon_refuses_a_prompt_past_its_total_cap_rather_than_dropping_one() {
        let registry = registry();
        registry
            .workspace(WorkspaceRequest::Create {
                workspace_id: "w1".into(),
                name: "Daily".into(),
                pane_id: "p0".into(),
            })
            .expect("create workspace");
        let panes = crate::queue::MAX_QUEUED_TOTAL / crate::queue::MAX_QUEUE_DEPTH;
        for index in 1..panes {
            registry
                .workspace(WorkspaceRequest::Split {
                    workspace_id: "w1".into(),
                    pane_id: format!("p{}", index - 1),
                    new_pane_id: format!("p{index}"),
                    axis: crate::layout::SplitAxis::Vertical,
                    kind: PaneKind::Terminal,
                })
                .expect("split a pane");
        }
        for index in 0..panes {
            for entry in 0..crate::queue::MAX_QUEUE_DEPTH {
                registry
                    .queue_add(
                        "w1",
                        &format!("p{index}"),
                        format!("card {entry}"),
                        "keep going".into(),
                    )
                    .expect("every entry up to the total cap fits");
            }
        }
        let (code, message) = registry
            .queue_add("w1", "p0", "one too many".into(), "keep going".into())
            .expect_err("the total cap must refuse");
        assert_eq!(code, ErrorCode::QueueRefused);
        assert!(
            message.contains(&crate::queue::MAX_QUEUED_TOTAL.to_string())
                && message.contains("every pane"),
            "the refusal must name the daemon-wide cap: {message}"
        );
        let total: usize = registry
            .queue_snapshots()
            .iter()
            .map(|queue| queue.entries.len())
            .sum();
        assert_eq!(
            total,
            crate::queue::MAX_QUEUED_TOTAL,
            "nothing may be dropped to make room"
        );
    }

    /// Refused at the daemon's edge, in the daemon's words, before an entry is built for it.
    /// Truncating would put half a prompt in front of an agent, which is worse than no prompt.
    #[test]
    fn a_prompt_over_the_byte_limit_is_refused_rather_than_truncated() {
        let registry = registry();
        queue_pane(&registry);
        let oversized = "x".repeat(MAX_PROMPT_BYTES + 1);
        let (code, message) = registry
            .queue_add("w1", "p1", "card 7".into(), oversized)
            .expect_err("an over-long prompt must be refused");
        assert_eq!(code, ErrorCode::QueueRefused);
        assert_eq!(
            message,
            format!(
                "that prompt is {} bytes; the limit is {MAX_PROMPT_BYTES}",
                MAX_PROMPT_BYTES + 1
            )
        );
        assert!(
            registry.queue_snapshots().is_empty(),
            "a refused prompt must not leave a queue behind it"
        );
    }

    /// A prompt the daemon could not deliver is work that was never done, so it goes back — and
    /// the pane is disarmed, because retrying into a pane whose binding just changed is how one
    /// wrong feed becomes many.
    #[test]
    fn a_feed_the_pane_refuses_goes_back_on_the_queue_and_disarms_it() {
        let registry = registry();
        let run_id = queue_pane(&registry);
        registry
            .report_agent_state(&run_id, AgentState::Done)
            .expect("report a state");
        registry
            .queue_add("w1", "p1", "card 7".into(), "keep going".into())
            .expect("queue a prompt");
        registry
            .queue_set_auto("w1", "p1", true)
            .expect("arm the pane");
        // The binding moves to a run this daemon has no authority over, which is what `pane_input`
        // exists to refuse. Nothing about the queue is special-cased: it goes through the same
        // four re-validations a keystroke does and is turned away by the same one.
        registry
            .layout
            .lock()
            .unwrap()
            .bind_run("w1", "p1", "replacement_run".into(), PaneRuntime::Running)
            .expect("rebind the pane");

        feed_cycle(&registry, "replacement_run", Instant::now());

        let queue = only_queue(&registry).expect("one queue");
        assert_eq!(
            queue.entries.len(),
            1,
            "an undelivered prompt must come back rather than being lost"
        );
        assert_eq!(queue.entries[0].label, "card 7");
        assert!(
            !queue.auto_feed,
            "a failed feed disarms the pane and asks for a human"
        );
        assert!(
            queue
                .holding_because
                .as_deref()
                .is_some_and(|held| held.contains("authority")),
            "the daemon's own refusal is what the pane shows: {:?}",
            queue.holding_because
        );
    }

    /// The safety claim of §8.5, proved rather than asserted: **an auto-feeding queue of depth
    /// sixteen creates zero worktrees.**
    ///
    /// A queue entry is text put in front of an agent that is already running in a worktree that
    /// already exists. Auto-feed never constructs a `DispatchRequest`, never calls
    /// `git::ensure_worktree`, never creates a branch, never binds a run and never creates a pane
    /// — and the reason it cannot is structural, because the only verb the tick has is
    /// `pane_input`. This measures all three of the observable consequences before and after a
    /// complete feed.
    #[test]
    fn a_full_auto_feed_cycle_creates_no_worktree_and_binds_no_run() {
        let repo = Repo::new("queue-no-worktree");
        let registry = TestRegistry {
            registry: RuntimeRegistry::new(&repo.state, 2000).unwrap(),
            state: repo.state.clone(),
        };
        let run_id = queue_pane(&registry);
        registry
            .report_agent_state(&run_id, AgentState::Done)
            .expect("report a state");
        registry
            .queue_add("w1", "p1", "card 7".into(), "keep going".into())
            .expect("queue a prompt");
        registry
            .queue_set_auto("w1", "p1", true)
            .expect("arm the pane");

        let worktrees_before = worktrees(&repo.root);
        let runs_before = bound_run_ids(&registry);
        let receipts_before = receipt_count(&repo.state);
        let panes_before = registry.layout().workspaces[0].panes.len();

        feed_cycle(&registry, &run_id, Instant::now());

        let queue = only_queue(&registry).expect("one queue");
        assert!(
            queue.entries.is_empty() && queue.awaiting_ack,
            "the point of the test is a feed that actually happened: {queue:?}"
        );
        assert_eq!(
            worktrees(&repo.root),
            worktrees_before,
            "auto-feed must not create a worktree"
        );
        assert_eq!(
            bound_run_ids(&registry),
            runs_before,
            "auto-feed must not bind a run"
        );
        assert_eq!(
            receipt_count(&repo.state),
            receipts_before,
            "auto-feed must not dispatch"
        );
        assert_eq!(
            registry.layout().workspaces[0].panes.len(),
            panes_before,
            "auto-feed must not create a pane"
        );
    }

    /// The refusal §8.4's guard (4) makes at arm time, in the exact words that name the command
    /// which fixes it. A queue that is silently never going to fire is worse than one that refuses
    /// to be armed.
    #[test]
    fn arming_a_pane_whose_agent_has_never_reported_names_the_hooks_command() {
        let registry = registry();
        queue_pane(&registry);
        let (code, message) = registry
            .queue_set_auto("w1", "p1", true)
            .expect_err("arming without a reported state must be refused");
        assert_eq!(code, ErrorCode::QueueRefused);
        assert_eq!(message, crate::queue::ARM_WITHOUT_REPORTED_STATE);
        assert!(
            message.contains("dock hooks --install"),
            "the refusal has to say what to do about it"
        );
    }

    /// The sharpest hazard in the design, refused before the queue is armed as well as at every
    /// feed. Under `--auto-feed-trust=screen` there is no hook report to stand in for "there is an
    /// agent here", so the question has to be asked directly: feeding a shell would type a
    /// sentence at a `$` prompt and press return.
    #[test]
    fn arming_a_shell_pane_under_screen_trust_is_refused_before_anything_is_armed() {
        let registry = registry();
        queue_pane(&registry);
        registry.set_auto_feed_trust(AutoFeedTrust::Screen);
        let (code, message) = registry
            .queue_set_auto("w1", "p1", true)
            .expect_err("a pane running a shell must not be armable");
        assert_eq!(code, ErrorCode::QueueRefused);
        assert_eq!(
            message,
            "nothing in that pane looks like an agent; auto-feed would type into a shell"
        );
        assert!(
            registry
                .queue_snapshots()
                .first()
                .is_none_or(|queue| !queue.auto_feed),
            "a refused arm must leave the pane disarmed"
        );
    }

    /// Guard (3) again, at the other end: even armed, even settled, even reported, a pane with no
    /// agent in it is never fed. This is the one that would type a sentence into a shell.
    #[test]
    fn a_pane_with_no_agent_is_never_fed_and_says_why() {
        let registry = registry();
        let run_id = queue_pane(&registry);
        registry
            .report_agent_state(&run_id, AgentState::Done)
            .expect("report a state");
        registry
            .queue_add("w1", "p1", "card 7".into(), "keep going".into())
            .expect("queue a prompt");
        registry
            .queue_set_auto("w1", "p1", true)
            .expect("arm the pane");

        let base = Instant::now();
        for (offset, state) in [
            (0, AgentState::Working),
            (250, AgentState::Idle),
            (4_000, AgentState::Idle),
        ] {
            registry.queue_tick_from(
                &[QueueObservation {
                    run_id: run_id.clone(),
                    agent: None,
                    state,
                    reported: true,
                }],
                base + Duration::from_millis(offset),
            );
        }

        let queue = only_queue(&registry).expect("one queue");
        assert_eq!(queue.entries.len(), 1, "a shell is never fed");
        assert!(
            queue
                .holding_because
                .as_deref()
                .is_some_and(|held| held.contains("shell")),
            "and it is an explicit refusal, not a silent skip: {:?}",
            queue.holding_because
        );
    }

    /// A queue is keyed by the pane, so when the pane goes the queue goes with it — in memory and
    /// on disk. Otherwise the file outlives the pane forever and its entries reappear the day
    /// somebody creates a pane with the same name.
    #[test]
    fn closing_a_pane_takes_its_queue_with_it() {
        let registry = registry();
        queue_pane(&registry);
        registry
            .workspace(WorkspaceRequest::Split {
                workspace_id: "w1".into(),
                pane_id: "p1".into(),
                new_pane_id: "p2".into(),
                axis: crate::layout::SplitAxis::Vertical,
                kind: PaneKind::Terminal,
            })
            .expect("split a second pane");
        registry
            .queue_add("w1", "p2", "card 7".into(), "keep going".into())
            .expect("queue a prompt");
        assert_eq!(registry.queue_snapshots().len(), 1);
        registry
            .workspace(WorkspaceRequest::Close {
                workspace_id: "w1".into(),
                pane_id: "p2".into(),
            })
            .expect("close the pane");
        assert!(
            registry.queue_snapshots().is_empty(),
            "a closed pane leaves no queue behind"
        );
        assert!(
            !registry.state.join("queues").join("w1_p2.json").exists(),
            "nor a file"
        );
    }

    /// A queue whose pane is gone by the time the daemon comes back is dropped, so a pane closed
    /// while the daemon was down does not leave work to be inherited by the next pane to wear its
    /// name.
    #[test]
    fn a_queue_whose_pane_is_gone_is_dropped_on_load() {
        let state = queue_state_dir("orphan-queue");
        {
            let first = RuntimeRegistry::new(&state, 2000).unwrap();
            let run_id = queue_pane(&first);
            first
                .queue_add("w1", "p1", "card 7".into(), "keep going".into())
                .expect("queue a prompt");
            // Written for a pane that will not be in the layout the next daemon loads.
            first
                .store
                .save_pane_queue(&DurablePaneQueue {
                    schema_version: crate::storage::QUEUE_SCHEMA_VERSION,
                    workspace_id: "w1".into(),
                    pane_id: "gone".into(),
                    next_entry_id: 2,
                    entries: vec![DurableQueueEntry {
                        entry_id: 1,
                        label: "card 9".into(),
                        prompt: "orphaned".into(),
                    }],
                })
                .expect("write an orphan queue");
            let _ = first.lifecycle(&run_id, LifecycleOperation::Stop);
        }
        let restored = TestRegistry {
            registry: RuntimeRegistry::new(&state, 2000).unwrap(),
            state,
        };
        let queues = restored.queue_snapshots();
        assert_eq!(
            queues.len(),
            1,
            "only the queue whose pane still exists comes back: {queues:?}"
        );
        assert_eq!(queues[0].pane_id, "p1");
        assert!(
            !restored.state.join("queues").join("w1_gone.json").exists(),
            "and the orphan's file is removed rather than left forever"
        );
    }

    /// A file the daemon cannot read is moved aside and stepped over, exactly as an unreadable
    /// programme gate is. The alternative — refusing to start — would make one corrupt queue cost
    /// the operator every pane on the machine.
    #[test]
    fn a_queue_file_the_daemon_cannot_parse_is_quarantined_rather_than_obeyed() {
        let state = queue_state_dir("quarantine-queue");
        {
            let first = RuntimeRegistry::new(&state, 2000).unwrap();
            let run_id = queue_pane(&first);
            let _ = first.lifecycle(&run_id, LifecycleOperation::Stop);
        }
        let queues = state.join("queues");
        fs::create_dir_all(&queues).unwrap();
        fs::write(
            queues.join("w1_p1.json"),
            br#"{"schema_version":99,"workspace_id":"w1","pane_id":"p1","next_entry_id":1,"entries":[]}"#,
        )
        .unwrap();

        let restored = TestRegistry {
            registry: RuntimeRegistry::new(&state, 2000).unwrap(),
            state,
        };
        assert!(
            restored.queue_snapshots().is_empty(),
            "a queue from a schema this daemon does not know is not loaded"
        );
        assert_eq!(
            restored
                .store
                .list_quarantined_pane_queue_ids()
                .expect("read the quarantine"),
            vec!["w1_p1".to_string()],
            "but it is kept, because it is the only copy of what somebody queued"
        );
    }

    /// Nothing but a human arms a queue. Entries alone feed nothing, however finished the agent
    /// looks and however long it has been finished for.
    #[test]
    fn a_queue_with_entries_feeds_nothing_until_a_human_arms_its_pane() {
        let registry = registry();
        let run_id = queue_pane(&registry);
        registry
            .report_agent_state(&run_id, AgentState::Done)
            .expect("report a state");
        registry
            .queue_add("w1", "p1", "card 7".into(), "keep going".into())
            .expect("queue a prompt");

        feed_cycle(&registry, &run_id, Instant::now());

        let queue = only_queue(&registry).expect("one queue");
        assert!(!queue.auto_feed, "a fresh queue is never armed");
        assert_eq!(queue.entries.len(), 1, "and an unarmed queue feeds nothing");
        assert!(
            queue
                .holding_because
                .as_deref()
                .is_some_and(|held| held.contains("not armed")),
            "which it says out loud: {:?}",
            queue.holding_because
        );
    }

    /// The subscriber's side of §7.2: a change to any queue bumps a generation the 16ms loop can
    /// read with one atomic load, and names the pane that moved so a client refreshes rather than
    /// polls. Without it queue depth would be the one thing on the runs lane that went stale.
    #[test]
    fn every_queue_change_names_the_pane_that_moved() {
        let registry = registry();
        assert_eq!(registry.queue_generation(), 0);
        queue_pane(&registry);
        let entry_id = registry
            .queue_add("w1", "p1", "card 7".into(), "keep going".into())
            .expect("queue a prompt");
        let after_add = registry.queue_generation();
        assert!(after_add > 0, "an add is a change");
        assert!(
            registry
                .queue_revisions()
                .contains_key(&("w1".to_string(), "p1".to_string())),
            "and it is attributed to the pane it happened to"
        );
        registry
            .queue_remove("w1", "p1", entry_id)
            .expect("remove the prompt");
        assert!(
            registry.queue_generation() > after_add,
            "so is a remove, or an open board would show a stale depth"
        );
    }

    /// Pane output now lives on the emulated screen rather than on the snapshot, so tests read
    /// it straight from the owned runtime the registry holds for that run.
    fn run_screen_text(registry: &RuntimeRegistry, run_id: &str) -> String {
        registry
            .runs
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .get(run_id)
            .and_then(|slot| slot.active())
            .map(|entry| entry.runtime.with_screen(|screen| screen.text_tail(60)))
            .unwrap_or_default()
    }

    fn wait_for_run_screen_text(registry: &RuntimeRegistry, run_id: &str, needle: &str) -> String {
        let deadline = crate::testing::deadline(3);
        loop {
            let text = run_screen_text(registry, run_id);
            if text.contains(needle) || Instant::now() >= deadline {
                return text;
            }
            thread::sleep(Duration::from_millis(10));
        }
    }

    /// Blocks until a `echo $$ > marker` fixture has finished announcing itself, and returns the
    /// pid it wrote.
    ///
    /// `echo $$ > marker` is not one step. The redirection creates the file, and only afterwards
    /// does the shell write into it, so a window exists in which the marker exists and is empty.
    /// Tests that resume on `marker.exists()` land in that window on a contended machine, and the
    /// rollback they then trigger SIGKILLs the fixture — so the pid never arrives at all and the
    /// test dies parsing an empty string rather than on anything it set out to check. Waiting for
    /// the trailing newline closes the window instead of narrowing it: it is the last byte the
    /// shell writes, so seeing it means the whole pid is on disk. The deadline is a liveness
    /// backstop, there to turn a fixture that never starts into a message rather than a hang.
    fn wait_for_fixture_pid(marker: &Path) -> i32 {
        let deadline = crate::testing::deadline(15);
        loop {
            if let Some(pid) = fs::read_to_string(marker)
                .ok()
                .and_then(|written| written.strip_suffix('\n').map(str::to_owned))
                .and_then(|pid| pid.trim().parse::<i32>().ok())
            {
                return pid;
            }
            assert!(
                Instant::now() < deadline,
                "fixture never wrote its pid to {}",
                marker.display()
            );
            thread::sleep(Duration::from_millis(10));
        }
    }

    struct Repo {
        root: PathBuf,
        state: PathBuf,
    }
    impl Repo {
        fn new(label: &str) -> Self {
            let root = std::env::current_dir()
                .unwrap()
                .join("target")
                .join(format!(
                    "dock-dispatch-{label}-{}-{}",
                    std::process::id(),
                    SEQ.fetch_add(1, Ordering::Relaxed)
                ));
            fs::create_dir_all(&root).unwrap();
            run(&root, &["init", "-q"]);
            run(&root, &["config", "user.email", "dock@example.invalid"]);
            run(&root, &["config", "user.name", "Dock Fixture"]);
            fs::write(root.join("tracked"), "fixture\n").unwrap();
            run(&root, &["add", "tracked"]);
            run(&root, &["commit", "-qm", "fixture"]);
            run(
                &root,
                &[
                    "worktree",
                    "add",
                    "-q",
                    "-b",
                    &format!("fixture-{label}"),
                    "fixture",
                ],
            );
            let state = root.join(".dock-test-state");
            Self { root, state }
        }
        fn request(&self, id: &str) -> DispatchRequest {
            DispatchRequest {
                repository_root: self.root.display().to_string(),
                external_task_ref: "TASK-42".into(),
                run_id: id.into(),
                worktree: self.root.join("fixture").display().to_string(),
                adapter: crate::adapter::AdapterSelection {
                    id: crate::adapter::AdapterId::Fixture,
                    executable: None,
                    arguments: vec!["-c".into(), "pwd".into()],
                },
            }
        }
    }
    impl Drop for Repo {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.root);
        }
    }
    fn run(dir: &Path, args: &[&str]) {
        assert!(
            Command::new("git")
                .arg("-C")
                .arg(dir)
                .args(args)
                .status()
                .unwrap()
                .success()
        );
    }

    #[test]
    fn launch_into_pane_binds_the_exact_empty_target_and_refuses_to_replace_it() {
        let repo = Repo::new("launch-into-pane");
        let registry = RuntimeRegistry::new(&repo.state, 256).unwrap();
        registry
            .workspace(WorkspaceRequest::Create {
                workspace_id: "ui_workspace".into(),
                name: "UI workspace".into(),
                pane_id: "ui_pane".into(),
            })
            .unwrap();

        let snapshot = registry
            .launch_into_pane(
                repo.request("dock_ui_target"),
                "ui_workspace".into(),
                "ui_pane".into(),
            )
            .unwrap();
        assert_eq!(snapshot.workspace_id, "ui_workspace");
        assert_eq!(snapshot.pane_id, "ui_pane");
        assert_eq!(
            registry
                .layout
                .lock()
                .unwrap()
                .pane_run("ui_workspace", "ui_pane")
                .as_deref(),
            Some("dock_ui_target")
        );

        let refused = repo.request("dock_ui_replacement");
        assert!(matches!(
            registry.launch_into_pane(refused.clone(), "ui_workspace".into(), "ui_pane".into()),
            Err((ErrorCode::InvalidLayout, _))
        ));
        assert!(!registry.receipt_path(&refused.run_id).unwrap().exists());
        assert!(!registry.runs.lock().unwrap().contains_key(&refused.run_id));
    }

    #[test]
    fn launch_into_pane_failed_exec_restores_exact_topology_and_all_authority() {
        use std::os::unix::fs::PermissionsExt;

        let repo = Repo::new("launch-into-pane-failed-exec");
        let registry = RuntimeRegistry::new(&repo.state, 256).unwrap();
        // This test asserts that a failed launch restores the exact prior topology and leaves no
        // run at all. Auto-launched pane shells would put a live run in every pane it creates,
        // measuring the placeholder instead of the rollback under test.
        *registry.suppress_pane_shells.lock().unwrap() = true;
        registry
            .workspace(WorkspaceRequest::Create {
                workspace_id: "ui_workspace".into(),
                name: "UI workspace".into(),
                pane_id: "selected_pane".into(),
            })
            .unwrap();
        registry
            .workspace(WorkspaceRequest::Split {
                workspace_id: "ui_workspace".into(),
                pane_id: "selected_pane".into(),
                new_pane_id: "launch_target".into(),
                axis: crate::layout::SplitAxis::Vertical,
                kind: PaneKind::Terminal,
            })
            .unwrap();
        registry
            .workspace(WorkspaceRequest::Focus {
                workspace_id: "ui_workspace".into(),
                pane_id: "selected_pane".into(),
            })
            .unwrap();
        let before = registry.layout();

        // The executable passes adapter discovery, then disappears at the exact launch boundary.
        // This deterministically exercises OwnedRuntime's FailedToLaunch result without a race.
        let broken = repo.root.join("broken-executable");
        fs::write(&broken, "#!/bin/sh\nexit 0\n").unwrap();
        fs::set_permissions(&broken, fs::Permissions::from_mode(0o700)).unwrap();
        let mut request = repo.request("dock_failed_exec_atomic");
        request.adapter = crate::adapter::AdapterSelection {
            id: crate::adapter::AdapterId::Generic,
            executable: Some(broken.display().to_string()),
            arguments: Vec::new(),
        };
        *registry.before_runtime_launch_hook.lock().unwrap() = Some(Arc::new({
            let broken = broken.clone();
            move || fs::remove_file(&broken).unwrap()
        }));

        assert!(matches!(
            registry.launch_into_pane(
                request.clone(),
                "ui_workspace".into(),
                "launch_target".into()
            ),
            Err((ErrorCode::AdapterUnavailable, _))
        ));
        assert_eq!(registry.layout(), before);
        assert!(registry.runs.lock().unwrap().is_empty());
        assert!(!registry.receipt_path(&request.run_id).unwrap().exists());
        assert_eq!(registry.inspect_programme().global_active, 0);

        request.adapter = crate::adapter::AdapterSelection {
            id: crate::adapter::AdapterId::Fixture,
            executable: None,
            arguments: vec!["-c".into(), "exit 0".into()],
        };
        let retry = registry
            .launch_into_pane(request, "ui_workspace".into(), "launch_target".into())
            .unwrap();
        assert_eq!(retry.run_id, "dock_failed_exec_atomic");
    }

    #[test]
    fn pane_input_requires_exact_live_bound_runtime_and_restores_no_authority() {
        let repo = Repo::new("pane-input-authority");
        let registry = RuntimeRegistry::new(&repo.state, 256).unwrap();
        assert!(matches!(
            registry.pane_input("missing", "missing", b"x"),
            Err((ErrorCode::InvalidBinding, _))
        ));
        let mut request = repo.request("dock_pane_input");
        request.adapter.arguments = vec![
            "-c".into(),
            "read value; printf 'got:%s' \"$value\"; sleep 30".into(),
        ];
        let snapshot = registry.dispatch(request).unwrap();
        assert_eq!(
            registry
                .pane_input(&snapshot.workspace_id, &snapshot.pane_id, b"hello\n")
                .unwrap(),
            6
        );
        assert!(
            wait_for_run_screen_text(&registry, &snapshot.run_id, "got:hello")
                .contains("got:hello")
        );
        registry
            .lifecycle(&snapshot.run_id, LifecycleOperation::Stop)
            .unwrap();
        assert!(
            registry
                .pane_input(&snapshot.workspace_id, &snapshot.pane_id, b"again\n")
                .is_err()
        );
        drop(registry);

        let restored = RuntimeRegistry::new(&repo.state, 256).unwrap();
        assert!(matches!(
            restored.pane_input(&snapshot.workspace_id, &snapshot.pane_id, b"again\n"),
            Err((ErrorCode::InvalidBinding, _))
        ));
    }

    #[test]
    fn pane_input_revalidates_binding_after_concurrent_rebind_before_enqueue() {
        let repo = Repo::new("pane-input-concurrent-rebind");
        let registry = Arc::new(RuntimeRegistry::new(&repo.state, 256).unwrap());
        let mut request = repo.request("dock_pane_input_race");
        request.adapter.arguments = vec![
            "-c".into(),
            "read value; printf 'stale:%s' \"$value\"; sleep 30".into(),
        ];
        let snapshot = registry.dispatch(request).unwrap();
        let (selected_tx, selected_rx) = std::sync::mpsc::channel();
        let (rebound_tx, rebound_rx) = std::sync::mpsc::channel();
        let rebound_rx = Mutex::new(rebound_rx);
        *registry
            .pane_input_before_final_validation_hook
            .lock()
            .unwrap() = Some(Arc::new(move || {
            selected_tx.send(()).unwrap();
            rebound_rx.lock().unwrap().recv().unwrap();
        }));
        let input_registry = Arc::clone(&registry);
        let workspace_id = snapshot.workspace_id.clone();
        let pane_id = snapshot.pane_id.clone();
        let input =
            thread::spawn(move || input_registry.pane_input(&workspace_id, &pane_id, b"wrong\n"));
        selected_rx.recv().unwrap();
        registry
            .layout
            .lock()
            .unwrap()
            .bind_run(
                &snapshot.workspace_id,
                &snapshot.pane_id,
                "replacement_run".into(),
                PaneRuntime::Running,
            )
            .unwrap();
        rebound_tx.send(()).unwrap();
        assert!(matches!(
            input.join().unwrap(),
            Err((ErrorCode::InvalidBinding, message)) if message.contains("changed")
        ));
        thread::sleep(Duration::from_millis(50));
        assert!(!run_screen_text(&registry, &snapshot.run_id).contains("stale:wrong"));
        registry
            .lifecycle(&snapshot.run_id, LifecycleOperation::Stop)
            .unwrap();
    }

    fn packet(snapshot: &RuntimeSnapshot) -> HandoffPacket {
        HandoffPacket {
            schema_version: 1,
            run_id: snapshot.run_id.clone(),
            task_id: snapshot.external_task_ref.clone(),
            workspace_id: snapshot.workspace_id.clone(),
            pane_id: snapshot.pane_id.clone(),
            worktree: snapshot.worktree.clone(),
            branch: snapshot.branch.clone(),
            base_sha: snapshot.base_sha.clone(),
            summary: "Implemented the bounded fixture change.".into(),
            question: Some("Accept this scope?".into()),
            checks: vec![Check {
                name: "cargo test".into(),
                passed: true,
            }],
        }
    }

    fn ready_gate(
        registry: &RuntimeRegistry,
        upstream_repo: &Repo,
        downstream_repo: &Repo,
        suffix: &str,
    ) -> String {
        let upstream_id = format!("dock_{suffix}_upstream");
        let downstream_id = format!("dock_{suffix}_downstream");
        let mut upstream_request = upstream_repo.request(&upstream_id);
        upstream_request.adapter.arguments = vec!["-c".into(), "sleep 30".into()];
        let upstream = registry.dispatch(upstream_request).unwrap();
        let mut downstream = downstream_repo.request(&downstream_id);
        downstream.adapter.arguments.clear();
        registry
            .queue_gated(
                downstream,
                upstream.run_id.clone(),
                ReviewRoute::AcceptScope,
            )
            .unwrap();
        registry.submit_handoff(packet(&upstream)).unwrap();
        registry
            .decide(
                upstream.run_id,
                ReviewRoute::AcceptScope,
                "release the exact downstream".into(),
            )
            .unwrap();
        downstream_id
    }

    #[test]
    fn valid_bound_fixture_runs_only_in_the_supplied_directory_and_persists_no_output() {
        let repo = Repo::new("valid");
        let registry = RuntimeRegistry::new(&repo.state, 256).unwrap();
        let mut request = repo.request("dock_valid");
        request
            .adapter
            .arguments
            .push("credential=do-not-persist".into());
        let initial = registry.dispatch(request).unwrap();
        assert_eq!(
            initial.repository_root,
            fs::canonicalize(&repo.root).unwrap().display().to_string()
        );
        let expected_worktree = fs::canonicalize(repo.root.join("fixture"))
            .unwrap()
            .display()
            .to_string();
        let deadline = crate::testing::deadline(3);
        let observed = loop {
            // The emulated screen wraps at the pane width, so a long path can span rows;
            // rejoining the rows compares the path itself rather than the wrap points.
            let observed: String = run_screen_text(&registry, "dock_valid")
                .split_whitespace()
                .collect();
            if observed == expected_worktree || Instant::now() >= deadline {
                break observed;
            }
            thread::sleep(Duration::from_millis(10));
        };
        assert_eq!(observed, expected_worktree);
        let receipt = fs::read_to_string(repo.state.join("dispatches/dock_valid.json")).unwrap();
        assert!(!receipt.contains("scrollback"));
        assert!(!receipt.contains("do-not-persist"));
        assert!(!receipt.contains(&initial.repository_root));
        assert!(!receipt.contains(&initial.worktree));
        assert!(
            serde_json::from_str::<serde_json::Value>(&receipt)
                .unwrap()
                .get("command")
                .is_none()
        );
        assert!(receipt.contains("TASK-42"));
        let receipt_json: serde_json::Value = serde_json::from_str(&receipt).unwrap();
        assert_eq!(receipt_json["worktree_relative"], "fixture");
        assert_eq!(receipt_json["repository_root_canonical"], true);
        assert_eq!(receipt_json["worktree_canonical"], true);
        assert_eq!(receipt_json["shared_git_common_directory"], true);
        assert!(
            receipt_json["repository_id"]
                .as_str()
                .is_some_and(|value| value.starts_with("repo-"))
        );
        assert_eq!(fs::metadata(&repo.state).unwrap().mode() & 0o777, 0o700);
        assert_eq!(
            fs::metadata(repo.state.join("dispatches")).unwrap().mode() & 0o777,
            0o700
        );
        assert_eq!(
            fs::metadata(repo.state.join("dispatches/dock_valid.json"))
                .unwrap()
                .mode()
                & 0o777,
            0o600
        );
    }

    #[test]
    fn invalid_layout_does_not_block_dispatch_or_programme_inspection() {
        for (label, bytes) in [
            ("corrupt-layout", b"{not-json}".as_slice()),
            // Schema 0 rather than a version from the future: nothing ever wrote 0, so it is a
            // mangling and quarantine is the right answer. A *newer* schema is a different
            // thing entirely and is asserted separately, below.
            (
                "unsupported-layout",
                br#"{"schema_version":0,"workspaces":[]}"#.as_slice(),
            ),
        ] {
            let repo = Repo::new(label);
            fs::create_dir_all(&repo.state).unwrap();
            fs::set_permissions(&repo.state, fs::Permissions::from_mode(0o700)).unwrap();
            fs::write(repo.state.join("layout.json"), bytes).unwrap();

            let registry = RuntimeRegistry::new(&repo.state, 64).unwrap();
            assert!(registry.inspect_programme().gates.is_empty());
            let snapshot = registry
                .dispatch(repo.request(&format!("dock_{label}")))
                .unwrap();
            assert_eq!(registry.inspect(Some(&snapshot.run_id)).unwrap().len(), 1);
            assert_eq!(
                fs::read_dir(repo.state.join("layout-quarantine"))
                    .unwrap()
                    .count(),
                1
            );
        }
    }

    #[test]
    fn a_layout_from_a_newer_dock_stops_the_daemon_rather_than_overwriting_it() {
        // The one refusal that must not start the daemon anyway. Starting empty would be
        // starting with a *wrong* answer — every workspace missing — and the first layout change
        // after that would persist the empty topology straight over the newer file. A downgrade
        // should cost the user nothing but the downgrade, so the daemon declines to start and
        // says which version wrote the file it will not touch.
        let repo = Repo::new("layout-from-the-future");
        fs::create_dir_all(&repo.state).unwrap();
        fs::set_permissions(&repo.state, fs::Permissions::from_mode(0o700)).unwrap();
        let path = repo.state.join("layout.json");
        fs::write(&path, br#"{"schema_version":99,"workspaces":[]}"#).unwrap();

        let Err(refusal) = RuntimeRegistry::new(&repo.state, 64) else {
            panic!("a layout from a newer Dock must not be loaded");
        };
        assert!(refusal.contains("newer Dock"), "{refusal}");
        assert_eq!(
            fs::read(&path).unwrap(),
            br#"{"schema_version":99,"workspaces":[]}"#,
            "the file the newer build still needs must be left exactly as it was"
        );
        assert!(!repo.state.join("layout-quarantine").exists());
    }

    #[test]
    fn dispatch_layout_persistence_failure_rolls_back_all_reservations() {
        let repo = Repo::new("dispatch-layout-rollback");
        let registry = RuntimeRegistry::with_capacity(
            &repo.state,
            64,
            CapacityPolicy {
                global_run_capacity: 1,
                per_repository_run_capacity: 1,
                human_review_reserved: 0,
            },
        )
        .unwrap();
        registry
            .layout
            .lock()
            .unwrap()
            .inject_persistence_failure(true);
        let launch_marker = repo.root.join("layout-failure-launched");
        let mut request = repo.request("dock_layout_rollback");
        request.adapter.arguments = vec![
            "-c".into(),
            format!("touch {}; sleep 30", launch_marker.display()),
        ];

        assert!(matches!(
            registry.dispatch(request.clone()),
            Err((ErrorCode::InvalidLayout, message))
                if message == "injected layout persistence failure"
        ));
        assert!(registry.runs.lock().unwrap().is_empty());
        assert!(registry.layout().workspaces.is_empty());
        assert!(!launch_marker.exists());
        assert!(!repo.state.join("layout.json").exists());
        assert!(
            !repo
                .state
                .join("dispatches/dock_layout_rollback.json")
                .exists()
        );

        registry
            .layout
            .lock()
            .unwrap()
            .inject_persistence_failure(false);
        let snapshot = registry.dispatch(request).unwrap();
        assert_eq!(snapshot.run_id, "dock_layout_rollback");
    }

    #[test]
    fn workspace_close_stop_failure_keeps_binding_and_capacity_for_retry() {
        let repo = Repo::new("workspace-close-stop-failure");
        let registry = RuntimeRegistry::with_capacity(
            &repo.state,
            64,
            CapacityPolicy {
                global_run_capacity: 1,
                per_repository_run_capacity: 1,
                human_review_reserved: 0,
            },
        )
        .unwrap();
        let mut request = repo.request("dock_close_stop_failure");
        request.adapter.arguments = vec!["-c".into(), "sleep 30".into()];
        let snapshot = registry.dispatch(request).unwrap();
        *registry.workspace_stop_failure.lock().unwrap() =
            Some("deterministic owned runtime stop failure".into());

        assert!(matches!(
            registry.workspace(WorkspaceRequest::Close {
                workspace_id: snapshot.workspace_id.clone(),
                pane_id: snapshot.pane_id.clone(),
            }),
            Err((ErrorCode::Internal, message))
                if message == "deterministic owned runtime stop failure"
        ));
        let layout = registry.layout();
        assert_eq!(layout.workspaces.len(), 1);
        assert_eq!(layout.workspaces[0].panes.len(), 1);
        assert_eq!(
            layout.workspaces[0].panes[&snapshot.pane_id]
                .run_id
                .as_deref(),
            Some(snapshot.run_id.as_str())
        );
        let still_owned = registry.inspect(Some(&snapshot.run_id)).unwrap().remove(0);
        assert!(matches!(
            still_owned.state,
            crate::protocol::ProcessState::Running
        ));
        assert!(matches!(
            registry.dispatch(repo.request("dock_close_capacity_probe")),
            Err((ErrorCode::CapacityExceeded, _))
        ));

        assert!(
            registry
                .workspace(WorkspaceRequest::Close {
                    workspace_id: snapshot.workspace_id.clone(),
                    pane_id: snapshot.pane_id.clone(),
                })
                .unwrap()
                .is_none()
        );
        assert!(registry.layout().workspaces.is_empty());
        assert!(matches!(
            registry.inspect(Some(&snapshot.run_id)),
            Err((ErrorCode::RunNotFound, _))
        ));
    }

    #[test]
    fn workspace_close_persistence_failure_retires_authority_and_retries() {
        let repo = Repo::new("workspace-close-persistence-failure");
        let registry = RuntimeRegistry::with_capacity(
            &repo.state,
            64,
            CapacityPolicy {
                global_run_capacity: 1,
                per_repository_run_capacity: 1,
                human_review_reserved: 0,
            },
        )
        .unwrap();
        let mut request = repo.request("dock_close_persist_failure");
        request.adapter.arguments = vec!["-c".into(), "sleep 30".into()];
        let snapshot = registry.dispatch(request).unwrap();
        let process_group_id = snapshot.process_group_id.expect("owned process group");
        registry
            .layout
            .lock()
            .unwrap()
            .inject_persistence_failure(true);

        assert!(matches!(
            registry.workspace(WorkspaceRequest::Close {
                workspace_id: snapshot.workspace_id.clone(),
                pane_id: snapshot.pane_id.clone(),
            }),
            Err((ErrorCode::InvalidLayout, message))
                if message == "injected layout persistence failure"
        ));
        wait_for_owned_group_exit(process_group_id);
        assert!(matches!(
            registry.inspect(Some(&snapshot.run_id)),
            Err((ErrorCode::RunNotFound, _))
        ));
        let failed = registry.layout();
        let pane = &failed.workspaces[0].panes[&snapshot.pane_id];
        assert_eq!(pane.run_id.as_deref(), Some(snapshot.run_id.as_str()));
        assert_eq!(pane.runtime, PaneRuntime::Exited);
        assert_eq!(
            registry
                .inspect_programme()
                .repositories
                .iter()
                .map(|repository| repository.active_capacity)
                .sum::<usize>(),
            0
        );

        registry
            .layout
            .lock()
            .unwrap()
            .inject_persistence_failure(false);
        assert!(
            registry
                .workspace(WorkspaceRequest::Close {
                    workspace_id: snapshot.workspace_id,
                    pane_id: snapshot.pane_id,
                })
                .unwrap()
                .is_none()
        );
        assert!(registry.layout().workspaces.is_empty());
    }

    #[test]
    fn restart_stop_failure_never_spawns_replacement_and_retains_capacity() {
        let repo = Repo::new("restart-stop-failure");
        let registry = RuntimeRegistry::with_capacity(
            &repo.state,
            64,
            CapacityPolicy {
                global_run_capacity: 1,
                per_repository_run_capacity: 1,
                human_review_reserved: 0,
            },
        )
        .unwrap();
        let mut request = repo.request("dock_restart_stop_failure");
        request.adapter.arguments = vec!["-c".into(), "sleep 30".into()];
        let first = registry.dispatch(request).unwrap();
        *registry.restart_stop_failure.lock().unwrap() =
            Some("deterministic restart retire failure".into());

        assert!(matches!(
            registry.lifecycle(&first.run_id, LifecycleOperation::Restart),
            Err((ErrorCode::Internal, message))
                if message == "deterministic restart retire failure"
        ));
        let retained = registry.inspect(Some(&first.run_id)).unwrap().remove(0);
        assert_eq!(retained.pid, first.pid);
        assert_eq!(retained.process_group_id, first.process_group_id);
        assert_eq!(retained.state, crate::protocol::ProcessState::Running);
        assert!(matches!(
            registry.dispatch(repo.request("dock_restart_capacity_probe")),
            Err((ErrorCode::CapacityExceeded, _))
        ));

        let replacement = registry
            .lifecycle(&first.run_id, LifecycleOperation::Restart)
            .unwrap();
        assert_ne!(replacement.process_group_id, first.process_group_id);
        wait_for_owned_group_exit(first.process_group_id.unwrap());
        assert_eq!(registry.inspect(None).unwrap().len(), 1);
        registry
            .lifecycle(&first.run_id, LifecycleOperation::Stop)
            .unwrap();
    }

    #[test]
    fn restart_reserves_global_and_repository_capacity_then_terminalizes_launch_failure() {
        use std::sync::Barrier;

        let first_repo = Repo::new("restart-reservation-first");
        let second_repo = Repo::new("restart-reservation-second");
        let third_repo = Repo::new("restart-reservation-third");
        let registry = Arc::new(
            RuntimeRegistry::with_capacity(
                &first_repo.state,
                64,
                CapacityPolicy {
                    global_run_capacity: 2,
                    per_repository_run_capacity: 1,
                    human_review_reserved: 0,
                },
            )
            .unwrap(),
        );
        let executable = first_repo.root.join("one-shot-agent");
        fs::write(&executable, "#!/bin/sh\nsleep 30\n").unwrap();
        fs::set_permissions(&executable, fs::Permissions::from_mode(0o700)).unwrap();
        let mut first_request = first_repo.request("dock_restart_launch_failure");
        first_request.adapter = AdapterSelection {
            id: crate::adapter::AdapterId::Generic,
            executable: Some(executable.display().to_string()),
            arguments: vec![],
        };
        let first = registry.dispatch(first_request).unwrap();
        let entered = Arc::new(Barrier::new(2));
        let release = Arc::new(Barrier::new(2));
        *registry.restart_after_stop_hook.lock().unwrap() = Some(Arc::new({
            let entered = Arc::clone(&entered);
            let release = Arc::clone(&release);
            let executable = executable.clone();
            move || {
                fs::remove_file(&executable).unwrap();
                entered.wait();
                release.wait();
            }
        }));
        let restarting = {
            let registry = Arc::clone(&registry);
            let run_id = first.run_id.clone();
            thread::spawn(move || registry.lifecycle(&run_id, LifecycleOperation::Restart))
        };
        entered.wait();
        assert!(matches!(
            registry.dispatch(first_repo.request("dock_restart_same_repo_probe")),
            Err((ErrorCode::CapacityExceeded, message)) if message.contains("repository run capacity")
        ));
        let mut second_request = second_repo.request("dock_restart_capacity_other");
        second_request.adapter.arguments = vec!["-c".into(), "sleep 30".into()];
        let second = registry.dispatch(second_request).unwrap();
        assert!(matches!(
            registry.dispatch(third_repo.request("dock_restart_global_probe")),
            Err((ErrorCode::CapacityExceeded, message)) if message.contains("global run capacity")
        ));
        release.wait();
        assert!(matches!(
            restarting.join().unwrap(),
            Err((ErrorCode::AdapterUnavailable, _))
        ));
        assert!(matches!(
            registry.inspect(Some(&first.run_id)),
            Err((ErrorCode::RunNotFound, _))
        ));
        let layout = registry.layout();
        let pane = layout
            .workspaces
            .iter()
            .find_map(|workspace| workspace.panes.get(&first.pane_id))
            .unwrap();
        assert_eq!(pane.run_id, None);
        assert_eq!(pane.runtime, PaneRuntime::Empty);
        let replacement_capacity = registry
            .dispatch(first_repo.request("dock_restart_after_terminal"))
            .unwrap();
        registry
            .lifecycle(&replacement_capacity.run_id, LifecycleOperation::Stop)
            .unwrap();
        registry
            .lifecycle(&second.run_id, LifecycleOperation::Stop)
            .unwrap();
    }

    #[test]
    fn two_repository_capacity_and_exact_human_gate_are_deterministic() {
        let upstream_repo = Repo::new("programme-upstream");
        let downstream_repo = Repo::new("programme-downstream");
        let registry = RuntimeRegistry::with_capacity(
            &upstream_repo.state,
            64,
            CapacityPolicy {
                global_run_capacity: 3,
                per_repository_run_capacity: 1,
                human_review_reserved: 1,
            },
        )
        .unwrap();
        let mut upstream_request = upstream_repo.request("dock_programme_upstream");
        upstream_request.adapter.arguments = vec!["-c".into(), "sleep 30".into()];
        let upstream = registry.dispatch(upstream_request).unwrap();

        let mut same_repo = upstream_repo.request("dock_same_repo_refused");
        same_repo.adapter.arguments = vec!["-c".into(), "sleep 30".into()];
        assert!(matches!(
            registry.dispatch(same_repo),
            Err((ErrorCode::CapacityExceeded, _))
        ));
        assert!(
            !upstream_repo
                .state
                .join("dispatches/dock_same_repo_refused.json")
                .exists()
        );

        let mut downstream = downstream_repo.request("dock_programme_downstream");
        downstream.adapter.arguments.clear();
        let queued = registry
            .queue_gated(
                downstream.clone(),
                upstream.run_id.clone(),
                ReviewRoute::AcceptScope,
            )
            .unwrap();
        assert_eq!(queued.state, GateState::AwaitingHandoff);
        assert!(matches!(
            registry.dispatch(downstream),
            Err((ErrorCode::GateBlocked, message))
                if message == "run id \"dock_programme_downstream\" is queued in programme state; direct dispatch is forbidden and the dependency gate must be released explicitly"
        ));
        assert!(
            !upstream_repo
                .state
                .join("dispatches/dock_programme_downstream.json")
                .exists()
        );
        registry
            .programme
            .lock()
            .unwrap()
            .releasing
            .insert("dock_programme_downstream".into());
        assert!(matches!(
            registry.dispatch(downstream_repo.request("dock_programme_downstream")),
            Err((ErrorCode::GateBlocked, message))
                if message == "run id \"dock_programme_downstream\" is releasing in programme state; direct dispatch is forbidden and the dependency gate must be released explicitly"
        ));
        registry
            .programme
            .lock()
            .unwrap()
            .releasing
            .remove("dock_programme_downstream");
        assert!(matches!(
            registry.release_gate("dock_programme_downstream"),
            Err((ErrorCode::GateBlocked, _))
        ));
        assert!(
            !upstream_repo
                .state
                .join("dispatches/dock_programme_downstream.json")
                .exists()
        );

        let mut downstream_blocker = downstream_repo.request("dock_downstream_blocker");
        downstream_blocker.adapter.arguments = vec!["-c".into(), "sleep 30".into()];
        let blocker = registry.dispatch(downstream_blocker).unwrap();
        let queued_portfolio = registry.inspect_programme();
        assert_eq!(queued_portfolio.global_active, 2);
        assert_eq!(queued_portfolio.gates.len(), 1);
        assert_eq!(
            queued_portfolio
                .repositories
                .iter()
                .map(|repository| repository.queued_run_ids.len())
                .sum::<usize>(),
            1
        );

        registry.submit_handoff(packet(&upstream)).unwrap();
        assert_eq!(
            registry.inspect_programme().gates[0].state,
            GateState::AwaitingDecision
        );
        registry
            .decide(
                upstream.run_id.clone(),
                ReviewRoute::AcceptScope,
                "release the declared downstream only".into(),
            )
            .unwrap();
        assert_eq!(
            registry.inspect_programme().gates[0].state,
            GateState::Ready
        );
        assert!(matches!(
            registry.release_gate("dock_programme_downstream"),
            Err((ErrorCode::CapacityExceeded, _))
        ));
        assert_eq!(
            registry.inspect_programme().gates[0].state,
            GateState::Ready
        );
        assert!(
            !upstream_repo
                .state
                .join("dispatches/dock_programme_downstream.json")
                .exists()
        );
        registry
            .lifecycle(&blocker.run_id, LifecycleOperation::Stop)
            .unwrap();
        let stopped = registry.inspect_programme();
        assert_eq!(stopped.global_active, 1);
        assert_eq!(
            stopped
                .repositories
                .iter()
                .flat_map(|repository| repository.active_run_ids.iter())
                .collect::<Vec<_>>(),
            vec![&upstream.run_id]
        );
        assert_eq!(
            stopped
                .repositories
                .iter()
                .map(|repository| repository.active_capacity)
                .sum::<usize>(),
            1
        );
        let released = registry.release_gate("dock_programme_downstream").unwrap();
        assert_eq!(released.run_id, "dock_programme_downstream");
        assert!(matches!(
            registry.release_gate("dock_programme_downstream"),
            Err((ErrorCode::GateNotFound, _))
        ));

        let portfolio = registry.inspect_programme();
        assert_eq!(portfolio.repositories.len(), 2);
        assert_eq!(portfolio.global_active, 2);
        assert_eq!(portfolio.global_run_capacity, 2);
        assert_eq!(portfolio.human_review_reserved, 1);
        assert!(portfolio.gates.is_empty());
        registry
            .lifecycle(&upstream.run_id, LifecycleOperation::Stop)
            .unwrap();
        registry
            .lifecycle(&released.run_id, LifecycleOperation::Stop)
            .unwrap();
        let terminal = registry.inspect_programme();
        assert_eq!(terminal.global_active, 0);
        assert!(
            terminal
                .repositories
                .iter()
                .all(|repository| repository.active_run_ids.is_empty()
                    && repository.active_capacity == 0)
        );
    }

    #[test]
    fn global_capacity_alone_refuses_a_run_in_another_repository() {
        let first_repo = Repo::new("global-capacity-first");
        let second_repo = Repo::new("global-capacity-second");
        let registry = RuntimeRegistry::with_capacity(
            &first_repo.state,
            64,
            CapacityPolicy {
                global_run_capacity: 1,
                per_repository_run_capacity: 4,
                human_review_reserved: 0,
            },
        )
        .unwrap();
        let mut first = first_repo.request("dock_global_first");
        first.adapter.arguments = vec!["-c".into(), "sleep 30".into()];
        registry.dispatch(first).unwrap();
        let second = second_repo.request("dock_global_refused");
        assert!(matches!(
            registry.dispatch(second),
            Err((ErrorCode::CapacityExceeded, message)) if message.contains("global run capacity")
        ));
        assert!(
            !first_repo
                .state
                .join("dispatches/dock_global_refused.json")
                .exists()
        );
    }

    #[test]
    fn duplicate_run_identity_precedes_capacity_refusal() {
        let repo = Repo::new("duplicate-before-capacity");
        let registry = RuntimeRegistry::with_capacity(
            &repo.state,
            64,
            CapacityPolicy {
                global_run_capacity: 1,
                per_repository_run_capacity: 1,
                human_review_reserved: 0,
            },
        )
        .unwrap();
        let mut request = repo.request("dock_duplicate_at_capacity");
        request.adapter.arguments = vec!["-c".into(), "sleep 30".into()];
        registry.dispatch(request.clone()).unwrap();

        assert!(matches!(
            registry.dispatch(request),
            Err((ErrorCode::DuplicateRunId, _))
        ));
    }

    #[test]
    fn durable_gate_reloads_and_revalidates_after_registry_restart() {
        let upstream_repo = Repo::new("durable-upstream");
        let downstream_repo = Repo::new("durable-downstream");
        {
            let registry = RuntimeRegistry::new(&upstream_repo.state, 64).unwrap();
            let mut upstream = upstream_repo.request("dock_durable_upstream");
            upstream.adapter.arguments = vec!["-c".into(), "sleep 30".into()];
            registry.dispatch(upstream).unwrap();
            let mut downstream = downstream_repo.request("dock_durable_downstream");
            downstream.adapter.arguments.clear();
            registry
                .queue_gated(
                    downstream,
                    "dock_durable_upstream".into(),
                    ReviewRoute::AcceptScope,
                )
                .unwrap();
            let upstream = registry
                .inspect(Some("dock_durable_upstream"))
                .unwrap()
                .remove(0);
            registry.submit_handoff(packet(&upstream)).unwrap();
            registry
                .decide(
                    upstream.run_id,
                    ReviewRoute::AcceptScope,
                    "survive daemon restart".into(),
                )
                .unwrap();
            let durable = fs::read_to_string(
                upstream_repo
                    .state
                    .join("programme-gates/dock_durable_downstream.json"),
            )
            .unwrap();
            assert!(!durable.contains("sleep 30"));
            assert_eq!(
                fs::metadata(upstream_repo.state.join("programme-gates"))
                    .unwrap()
                    .mode()
                    & 0o777,
                0o700
            );
        }
        let restarted = RuntimeRegistry::new(&upstream_repo.state, 64).unwrap();
        let portfolio = restarted.inspect_programme();
        assert_eq!(portfolio.gates.len(), 1);
        assert_eq!(
            portfolio.gates[0].downstream_run_id,
            "dock_durable_downstream"
        );
        assert_eq!(portfolio.gates[0].state, GateState::Ready);
        let released = restarted.release_gate("dock_durable_downstream").unwrap();
        assert_eq!(released.run_id, "dock_durable_downstream");
        assert!(
            !upstream_repo
                .state
                .join("programme-gates/dock_durable_downstream.json")
                .exists()
        );
    }

    #[test]
    fn durable_gate_reload_quarantines_unbound_upstream_receipts_and_restores_valid_gate() {
        let upstream_repo = Repo::new("durable-receipt-upstream");
        let downstream_repo = Repo::new("durable-receipt-downstream");
        let cases = [
            ("missing", "dock_receipt_missing_upstream"),
            ("invalid", "dock_receipt_invalid_upstream"),
            ("truncated", "dock_receipt_truncated_upstream"),
            ("forged", "dock_receipt_forged_upstream"),
            ("run_mismatch", "dock_receipt_run_mismatch_upstream"),
            ("repo_mismatch", "dock_receipt_repo_mismatch_upstream"),
            ("valid", "dock_receipt_valid_upstream"),
        ];
        {
            let registry = RuntimeRegistry::new(&upstream_repo.state, 64).unwrap();
            for (case, upstream_id) in cases {
                let mut upstream_request = upstream_repo.request(upstream_id);
                upstream_request.adapter.arguments = vec!["-c".into(), "sleep 30".into()];
                registry.dispatch(upstream_request).unwrap();

                let mut downstream =
                    downstream_repo.request(&format!("dock_receipt_{case}_downstream"));
                downstream.adapter.arguments.clear();
                registry
                    .queue_gated(downstream, upstream_id.into(), ReviewRoute::AcceptScope)
                    .unwrap();
            }
            let stored_valid = fs::read_to_string(
                upstream_repo
                    .state
                    .join("programme-gates/dock_receipt_valid_downstream.json"),
            )
            .unwrap();
            assert!(!stored_valid.contains(&downstream_repo.root.display().to_string()));
            assert!(!stored_valid.contains("sleep 30"));
        }

        let receipts = upstream_repo.state.join("dispatches");
        fs::remove_file(receipts.join("dock_receipt_missing_upstream.json")).unwrap();
        fs::write(
            receipts.join("dock_receipt_invalid_upstream.json"),
            b"{not-json}\n",
        )
        .unwrap();
        fs::write(
            receipts.join("dock_receipt_truncated_upstream.json"),
            br#"{"protocol_version":6,"repository_id":"repo-v2-"#,
        )
        .unwrap();
        let forged_path = receipts.join("dock_receipt_forged_upstream.json");
        let forged_receipt: serde_json::Value =
            serde_json::from_slice(&fs::read(&forged_path).unwrap()).unwrap();
        fs::write(
            &forged_path,
            serde_json::to_vec_pretty(&serde_json::json!({
                "protocol_version": forged_receipt["protocol_version"],
                "run_id": forged_receipt["run_id"],
                "repository_id": forged_receipt["repository_id"],
            }))
            .unwrap(),
        )
        .unwrap();
        let run_mismatch_path = receipts.join("dock_receipt_run_mismatch_upstream.json");
        let mut run_mismatch: serde_json::Value =
            serde_json::from_slice(&fs::read(&run_mismatch_path).unwrap()).unwrap();
        run_mismatch["run_id"] = serde_json::json!("dock_different_upstream");
        fs::write(
            &run_mismatch_path,
            serde_json::to_vec_pretty(&run_mismatch).unwrap(),
        )
        .unwrap();
        let repo_mismatch_path = receipts.join("dock_receipt_repo_mismatch_upstream.json");
        let mut repo_mismatch: serde_json::Value =
            serde_json::from_slice(&fs::read(&repo_mismatch_path).unwrap()).unwrap();
        repo_mismatch["repository_id"] = serde_json::json!("repo-v2-0000000000000000");
        fs::write(
            &repo_mismatch_path,
            serde_json::to_vec_pretty(&repo_mismatch).unwrap(),
        )
        .unwrap();

        let restarted = RuntimeRegistry::new(&upstream_repo.state, 64).unwrap();
        let portfolio = restarted.inspect_programme();
        assert_eq!(portfolio.gates.len(), 1);
        assert_eq!(
            portfolio.gates[0].downstream_run_id,
            "dock_receipt_valid_downstream"
        );
        assert_eq!(
            restarted
                .store
                .list_quarantined_programme_gate_ids()
                .unwrap(),
            vec![
                "dock_receipt_forged_downstream",
                "dock_receipt_invalid_downstream",
                "dock_receipt_missing_downstream",
                "dock_receipt_repo_mismatch_downstream",
                "dock_receipt_run_mismatch_downstream",
                "dock_receipt_truncated_downstream",
            ]
        );
    }

    #[test]
    fn corrupt_durable_gate_is_quarantined_without_blocking_valid_gate_restore() {
        let upstream_repo = Repo::new("durable-corrupt-upstream");
        let downstream_repo = Repo::new("durable-corrupt-downstream");
        {
            let registry = RuntimeRegistry::new(&upstream_repo.state, 64).unwrap();
            let mut upstream_request = upstream_repo.request("dock_quarantine_upstream");
            upstream_request.adapter.arguments = vec!["-c".into(), "sleep 30".into()];
            let upstream = registry.dispatch(upstream_request).unwrap();
            let mut valid = downstream_repo.request("dock_quarantine_valid");
            valid.adapter.arguments.clear();
            registry
                .queue_gated(valid, upstream.run_id, ReviewRoute::AcceptScope)
                .unwrap();
            let gates = upstream_repo.state.join("programme-gates");
            fs::write(gates.join("dock_quarantine_corrupt.json"), b"{not-json}\n").unwrap();
        }

        let restarted = RuntimeRegistry::new(&upstream_repo.state, 64).unwrap();
        let portfolio = restarted.inspect_programme();
        assert_eq!(portfolio.gates.len(), 1);
        assert_eq!(
            portfolio.gates[0].downstream_run_id,
            "dock_quarantine_valid"
        );
        assert!(matches!(
            restarted.dispatch(downstream_repo.request("dock_quarantine_corrupt")),
            Err((ErrorCode::GateBlocked, message))
                if message == "run id \"dock_quarantine_corrupt\" is sealed by an invalid durable programme gate"
        ));
        let quarantine = upstream_repo.state.join("programme-gate-quarantine");
        assert!(quarantine.join("dock_quarantine_corrupt.json").exists());
        assert_eq!(fs::metadata(quarantine).unwrap().mode() & 0o777, 0o700);

        drop(restarted);
        let restarted_again = RuntimeRegistry::new(&upstream_repo.state, 64).unwrap();
        assert!(matches!(
            restarted_again.dispatch(downstream_repo.request("dock_quarantine_corrupt")),
            Err((ErrorCode::GateBlocked, _))
        ));
    }

    #[test]
    fn released_run_and_gate_change_atomically_for_programme_inspection() {
        let upstream_repo = Repo::new("atomic-release-upstream");
        let downstream_repo = Repo::new("atomic-release-downstream");
        let registry = Arc::new(RuntimeRegistry::new(&upstream_repo.state, 64).unwrap());
        let downstream_id = ready_gate(
            &registry,
            &upstream_repo,
            &downstream_repo,
            "atomic_inspection",
        );
        let commit_barrier = Arc::new(std::sync::Barrier::new(2));
        let (entered_tx, entered_rx) = std::sync::mpsc::channel();
        *registry.release_commit_hook.lock().unwrap() = Some(Arc::new({
            let commit_barrier = Arc::clone(&commit_barrier);
            move || {
                entered_tx.send(()).unwrap();
                commit_barrier.wait();
            }
        }));
        let release = {
            let registry = Arc::clone(&registry);
            let downstream_id = downstream_id.clone();
            thread::spawn(move || registry.release_gate(&downstream_id))
        };
        entered_rx.recv_timeout(crate::testing::budget(3)).unwrap();

        let (inspection_tx, inspection_rx) = std::sync::mpsc::channel();
        let inspector = {
            let registry = Arc::clone(&registry);
            thread::spawn(move || inspection_tx.send(registry.inspect_programme()).unwrap())
        };
        assert!(
            inspection_rx
                .recv_timeout(crate::testing::budget_millis(100))
                .is_err()
        );
        commit_barrier.wait();
        release.join().unwrap().unwrap();
        let portfolio = inspection_rx
            .recv_timeout(crate::testing::budget(3))
            .unwrap();
        inspector.join().unwrap();
        assert!(portfolio.gates.is_empty());
        assert_eq!(
            portfolio
                .repositories
                .iter()
                .flat_map(|repository| repository.queued_run_ids.iter())
                .filter(|run_id| *run_id == &downstream_id)
                .count(),
            0
        );
        assert_eq!(
            portfolio
                .repositories
                .iter()
                .flat_map(|repository| repository.active_run_ids.iter())
                .filter(|run_id| *run_id == &downstream_id)
                .count(),
            1
        );
    }

    #[test]
    fn portfolio_capacity_uses_one_runtime_snapshot_when_state_flips() {
        let repo = Repo::new("portfolio-state-flip");
        let registry = RuntimeRegistry::new(&repo.state, 64).unwrap();
        let mut request = repo.request("dock_portfolio_state_flip");
        request.adapter.arguments = vec!["-c".into(), "sleep 30".into()];
        let snapshot = registry.dispatch(request).unwrap();
        let runtime = registry.runs.lock().unwrap()[&snapshot.run_id]
            .active()
            .unwrap()
            .runtime
            .clone();
        *registry.portfolio_capture_hook.lock().unwrap() = Some(Arc::new(move || {
            runtime.stop().unwrap();
        }));

        let portfolio = registry.inspect_programme();
        assert_eq!(portfolio.global_active, 1);
        assert_eq!(
            portfolio
                .repositories
                .iter()
                .map(|repository| repository.active_capacity)
                .sum::<usize>(),
            portfolio.global_active
        );
        assert_eq!(
            portfolio
                .repositories
                .iter()
                .map(|repository| repository.active_run_ids.len())
                .sum::<usize>(),
            portfolio.global_active
        );
        assert_eq!(registry.inspect_programme().global_active, 0);
    }

    #[test]
    fn portfolio_counts_launching_reservation_in_the_same_transition_snapshot_as_admission() {
        let repo = Repo::new("portfolio-launching");
        let registry = Arc::new(
            RuntimeRegistry::with_capacity(
                &repo.state,
                64,
                CapacityPolicy {
                    global_run_capacity: 1,
                    per_repository_run_capacity: 1,
                    human_review_reserved: 0,
                },
            )
            .unwrap(),
        );
        let entered = Arc::new(std::sync::Barrier::new(2));
        let release = Arc::new(std::sync::Barrier::new(2));
        *registry.after_launch_before_receipt_hook.lock().unwrap() = Some(Arc::new({
            let entered = Arc::clone(&entered);
            let release = Arc::clone(&release);
            move || {
                entered.wait();
                release.wait();
            }
        }));
        let request = repo.request("dock_launching_snapshot");
        let dispatch = {
            let registry = Arc::clone(&registry);
            thread::spawn(move || registry.dispatch(request))
        };
        entered.wait();

        let portfolio = registry.inspect_programme();
        assert_eq!(portfolio.global_active, 1);
        assert_eq!(portfolio.repositories.len(), 1);
        assert_eq!(portfolio.repositories[0].active_capacity, 1);
        assert_eq!(
            portfolio.repositories[0].active_run_ids,
            ["dock_launching_snapshot".to_owned()]
        );
        assert!(matches!(
            registry.dispatch(repo.request("dock_capacity_refused")),
            Err((ErrorCode::CapacityExceeded, _))
        ));
        release.wait();
        assert!(matches!(
            dispatch.join().unwrap(),
            Err((ErrorCode::Internal, _))
        ));
    }

    #[test]
    fn stable_repository_identifier_has_an_explicit_durable_algorithm() {
        assert_eq!(
            repository_id(Path::new("/canonical/repository")),
            "repo-v2-f38d7425ff328465"
        );
    }

    #[test]
    fn release_claim_storage_failure_launches_nothing_and_is_retryable() {
        let upstream_repo = Repo::new("claim-failure-upstream");
        let downstream_repo = Repo::new("claim-failure-downstream");
        let registry = RuntimeRegistry::new(&upstream_repo.state, 64).unwrap();
        let downstream_id =
            ready_gate(&registry, &upstream_repo, &downstream_repo, "claim_failure");
        let obstructing_path = upstream_repo
            .state
            .join("programme-releases")
            .join(format!("{downstream_id}.json"));
        fs::create_dir_all(&obstructing_path).unwrap();

        assert!(matches!(
            registry.release_gate(&downstream_id),
            Err((ErrorCode::Internal, message)) if message.contains("release claim")
        ));
        assert!(matches!(
            registry.inspect(Some(&downstream_id)),
            Err((ErrorCode::RunNotFound, _))
        ));
        assert!(
            upstream_repo
                .state
                .join("programme-gates")
                .join(format!("{downstream_id}.json"))
                .exists()
        );
        fs::remove_dir(&obstructing_path).unwrap();
        let released = registry.release_gate(&downstream_id).unwrap();
        assert_eq!(released.run_id, downstream_id);
    }

    #[test]
    fn release_cleanup_storage_failure_still_commits_once_without_an_orphan_gate() {
        let upstream_repo = Repo::new("cleanup-failure-upstream");
        let downstream_repo = Repo::new("cleanup-failure-downstream");
        let registry = RuntimeRegistry::new(&upstream_repo.state, 64).unwrap();
        let downstream_id = ready_gate(
            &registry,
            &upstream_repo,
            &downstream_repo,
            "cleanup_failure",
        );
        let claim = upstream_repo
            .state
            .join("programme-releases")
            .join(format!("{downstream_id}.json"));
        *registry.release_cleanup_hook.lock().unwrap() = Some(Arc::new({
            let claim = claim.clone();
            move || {
                fs::remove_file(&claim).unwrap();
                fs::create_dir(&claim).unwrap();
            }
        }));

        let released = registry.release_gate(&downstream_id).unwrap();
        assert_eq!(released.run_id, downstream_id);
        assert_eq!(registry.inspect(Some(&released.run_id)).unwrap().len(), 1);
        assert!(registry.inspect_programme().gates.is_empty());
        assert!(matches!(
            registry.release_gate(&released.run_id),
            Err((ErrorCode::GateNotFound, _))
        ));
    }

    #[test]
    fn restart_reconciles_a_committed_release_claim_without_duplicate_launch() {
        let upstream_repo = Repo::new("claim-recovery-upstream");
        let downstream_repo = Repo::new("claim-recovery-downstream");
        let downstream_id;
        {
            let registry = RuntimeRegistry::new(&upstream_repo.state, 64).unwrap();
            downstream_id = ready_gate(
                &registry,
                &upstream_repo,
                &downstream_repo,
                "claim_recovery",
            );
            let request = registry
                .programme
                .lock()
                .unwrap()
                .gates
                .get(&downstream_id)
                .unwrap()
                .dispatch
                .clone();
            registry.store.claim_programme_gate(&downstream_id).unwrap();
            registry
                .programme
                .lock()
                .unwrap()
                .releasing
                .insert(downstream_id.clone());
            registry
                .dispatch_with_gate_authorization(request, true, None)
                .unwrap();
        }

        let restarted = RuntimeRegistry::new(&upstream_repo.state, 64).unwrap();
        assert!(restarted.inspect_programme().gates.is_empty());
        assert!(
            !upstream_repo
                .state
                .join("programme-releases")
                .join(format!("{downstream_id}.json"))
                .exists()
        );
        assert!(matches!(
            restarted.release_gate(&downstream_id),
            Err((ErrorCode::GateNotFound, _))
        ));
    }

    #[test]
    fn restart_restores_a_claim_that_never_reserved_or_launched() {
        let upstream_repo = Repo::new("pre-reservation-recovery-upstream");
        let downstream_repo = Repo::new("pre-reservation-recovery-downstream");
        let downstream_id;
        {
            let registry = RuntimeRegistry::new(&upstream_repo.state, 64).unwrap();
            downstream_id = ready_gate(
                &registry,
                &upstream_repo,
                &downstream_repo,
                "pre_reservation_recovery",
            );
            registry.store.claim_programme_gate(&downstream_id).unwrap();
        }

        let restarted = RuntimeRegistry::new(&upstream_repo.state, 64).unwrap();
        assert_eq!(restarted.inspect_programme().gates.len(), 1);
        assert_eq!(
            restarted.inspect_programme().gates[0].downstream_run_id,
            downstream_id
        );
        assert!(
            upstream_repo
                .state
                .join("programme-gates")
                .join(format!("{downstream_id}.json"))
                .exists()
        );
    }

    #[test]
    fn restart_recovers_a_retryable_dispatch_failure_after_restore_failed() {
        let upstream_repo = Repo::new("failed-restore-upstream");
        let downstream_repo = Repo::new("failed-restore-downstream");
        let downstream_id;
        let obstruction;
        {
            let registry = RuntimeRegistry::with_capacity(
                &upstream_repo.state,
                64,
                CapacityPolicy {
                    global_run_capacity: 1,
                    per_repository_run_capacity: 1,
                    human_review_reserved: 0,
                },
            )
            .unwrap();
            downstream_id = ready_gate(
                &registry,
                &upstream_repo,
                &downstream_repo,
                "failed_restore",
            );
            obstruction = upstream_repo
                .state
                .join("programme-gates")
                .join(format!("{downstream_id}.json"));
            *registry.release_restore_hook.lock().unwrap() = Some(Arc::new({
                let obstruction = obstruction.clone();
                move || fs::create_dir(&obstruction).unwrap()
            }));
            assert!(matches!(
                registry.release_gate(&downstream_id),
                Err((ErrorCode::Internal, message)) if message.contains("could not be restored")
            ));
            assert!(
                upstream_repo
                    .state
                    .join("programme-releases")
                    .join(format!("{downstream_id}.json"))
                    .exists()
            );
            fs::remove_dir(&obstruction).unwrap();
        }

        let restarted = RuntimeRegistry::new(&upstream_repo.state, 64).unwrap();
        assert_eq!(restarted.inspect_programme().gates.len(), 1);
        assert_eq!(
            restarted.inspect_programme().gates[0].downstream_run_id,
            downstream_id
        );
    }

    #[test]
    fn restart_terminalizes_an_incomplete_first_release_reservation() {
        let upstream_repo = Repo::new("reservation-recovery-upstream");
        let downstream_repo = Repo::new("reservation-recovery-downstream");
        let downstream_id;
        {
            let registry = RuntimeRegistry::new(&upstream_repo.state, 64).unwrap();
            downstream_id = ready_gate(
                &registry,
                &upstream_repo,
                &downstream_repo,
                "reservation_recovery",
            );
            registry.store.claim_programme_gate(&downstream_id).unwrap();
            reserve_run_id(&registry.receipt_path(&downstream_id).unwrap()).unwrap();
        }

        let restarted = RuntimeRegistry::new(&upstream_repo.state, 64).unwrap();
        assert!(restarted.inspect_programme().gates.is_empty());
        assert!(matches!(
            restarted.release_gate(&downstream_id),
            Err((ErrorCode::GateNotFound, _))
        ));
        assert!(
            upstream_repo
                .state
                .join("dispatches")
                .join(format!("{downstream_id}.json"))
                .exists()
        );
    }

    #[test]
    fn crash_after_spawn_before_receipt_is_guarded_and_never_retried() {
        // The hook holds dispatch open until the child has announced its pid, because the pid is
        // the whole subject: the test has to name the orphan to check nobody is left holding it.
        // Waiting on the marker merely existing used to be enough to resume, and it is not — the
        // shell creates that file before it writes into it, and the rollback this dispatch is
        // about SIGKILLs the child inside that gap, so on a contended machine the pid was never
        // written and the test died on `parse` rather than on anything about guarding.
        let repo = Repo::new("after-spawn-before-receipt");
        let registry = RuntimeRegistry::new(&repo.state, 64).unwrap();
        let marker = repo.root.join("guarded-pid");
        let mut request = repo.request("dock_guarded_window");
        request.adapter.arguments = vec![
            "-c".into(),
            format!("echo $$ > {}; sleep 30", marker.display()),
        ];
        *registry.after_launch_before_receipt_hook.lock().unwrap() = Some(Arc::new({
            let marker = marker.clone();
            move || {
                wait_for_fixture_pid(&marker);
            }
        }));
        assert!(matches!(
            registry.dispatch(request.clone()),
            Err((ErrorCode::Internal, _))
        ));
        // Already on disk: the hook above did not return until it was.
        let pid = wait_for_fixture_pid(&marker);
        let deadline = crate::testing::deadline(15);
        while unsafe { nix::libc::kill(pid, 0) } == 0 && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(10));
        }
        assert_ne!(
            unsafe { nix::libc::kill(pid, 0) },
            0,
            "orphan survived guardian loss"
        );
        drop(registry);

        let restarted = RuntimeRegistry::new(&repo.state, 64).unwrap();
        assert!(matches!(
            restarted.dispatch(request),
            Err((ErrorCode::DuplicateRunId, _))
        ));
        assert_eq!(fs::read_to_string(marker).unwrap().lines().count(), 1);
    }

    #[test]
    fn receipt_failure_after_launch_rolls_back_exact_runtime_binding_and_capacity() {
        let repo = Repo::new("receipt-rollback");
        let registry = RuntimeRegistry::with_capacity(
            &repo.state,
            64,
            CapacityPolicy {
                global_run_capacity: 1,
                per_repository_run_capacity: 1,
                human_review_reserved: 0,
            },
        )
        .unwrap();
        let marker = repo.root.join("receipt-rollback-pid");
        let mut request = repo.request("dock_receipt_rollback");
        request.adapter.arguments = vec![
            "-c".into(),
            format!("echo $$ > {}; sleep 30", marker.display()),
        ];
        let binding = validate_binding(&request).unwrap();
        let receipt = registry.receipt_path(&request.run_id).unwrap();
        let temporary = receipt.with_file_name(format!(
            ".{}.tmp-{}",
            receipt.file_name().unwrap().to_str().unwrap(),
            std::process::id()
        ));
        *registry.before_save_receipt_hook.lock().unwrap() = Some(Arc::new({
            let marker = marker.clone();
            let temporary = temporary.clone();
            // Resumes on the pid rather than on the marker existing, for the reason
            // `wait_for_fixture_pid` gives: the empty file arrives first, and the rollback below
            // kills the child before it can fill it in.
            move || {
                wait_for_fixture_pid(&marker);
                fs::create_dir(&temporary).unwrap();
            }
        }));

        assert!(matches!(
            registry.dispatch(request.clone()),
            Err((ErrorCode::Internal, _))
        ));
        assert!(registry.runs.lock().unwrap().is_empty());
        assert_eq!(registry.inspect_programme().global_active, 0);
        assert_eq!(
            registry
                .layout
                .lock()
                .unwrap()
                .pane_run(&binding.workspace_id, &binding.pane_id),
            None
        );
        assert!(!receipt.exists());
        let pid = wait_for_fixture_pid(&marker);
        let deadline = crate::testing::deadline(3);
        while unsafe { nix::libc::kill(pid, 0) } == 0 && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(10));
        }
        assert_ne!(
            unsafe { nix::libc::kill(pid, 0) },
            0,
            "rolled-back child survived"
        );

        fs::remove_dir(&temporary).unwrap();
        drop(registry);
        let restarted = RuntimeRegistry::with_capacity(
            &repo.state,
            64,
            CapacityPolicy {
                global_run_capacity: 1,
                per_repository_run_capacity: 1,
                human_review_reserved: 0,
            },
        )
        .unwrap();
        assert!(restarted.layout().workspaces.is_empty());
        assert!(restarted.runs.lock().unwrap().is_empty());
        assert_eq!(restarted.inspect_programme().global_active, 0);
        assert!(!receipt.exists());
        let retry = restarted.dispatch(request).unwrap();
        assert_eq!(retry.run_id, "dock_receipt_rollback");
    }

    #[test]
    fn receipt_failure_stop_error_retains_authority_and_retry_completes_cleanup() {
        let repo = Repo::new("receipt-stop-failure");
        let registry = RuntimeRegistry::with_capacity(
            &repo.state,
            64,
            CapacityPolicy {
                global_run_capacity: 1,
                per_repository_run_capacity: 1,
                human_review_reserved: 0,
            },
        )
        .unwrap();
        let mut request = repo.request("dock_receipt_stop_failure");
        request.adapter.arguments = vec!["-c".into(), "sleep 30".into()];
        let binding = validate_binding(&request).unwrap();
        let receipt = registry.receipt_path(&request.run_id).unwrap();
        let temporary = receipt.with_file_name(format!(
            ".{}.tmp-{}",
            receipt.file_name().unwrap().to_str().unwrap(),
            std::process::id()
        ));
        *registry.before_save_receipt_hook.lock().unwrap() = Some(Arc::new({
            let temporary = temporary.clone();
            move || fs::create_dir(&temporary).unwrap()
        }));
        *registry.receipt_stop_failure.lock().unwrap() = Some(
            "could not signal Dock-owned process group: EPERM; exact group still exists".into(),
        );

        let error = registry.dispatch(request.clone()).unwrap_err();
        assert!(error.1.contains("EPERM"));
        assert!(receipt.exists(), "receipt reservation authority was lost");
        assert_eq!(
            registry
                .layout
                .lock()
                .unwrap()
                .pane_run(&binding.workspace_id, &binding.pane_id),
            Some(request.run_id.clone())
        );
        let process_group_id = {
            let runs = registry.runs.lock().unwrap();
            let slot = runs
                .get(&request.run_id)
                .expect("retryable stop transition");
            assert!(matches!(
                slot.state,
                RuntimeSlotState::ReceiptRollbackStopping { .. }
            ));
            slot.active()
                .unwrap()
                .runtime
                .snapshot()
                .process_group_id
                .unwrap()
        };
        assert_eq!(unsafe { nix::libc::kill(-process_group_id, 0) }, 0);
        assert_eq!(registry.inspect_programme().global_active, 1);

        fs::remove_dir(&temporary).unwrap();
        let retried = registry.dispatch(request).unwrap();
        assert_eq!(retried.run_id, "dock_receipt_stop_failure");
        assert_ne!(unsafe { nix::libc::kill(-process_group_id, 0) }, 0);
        registry
            .lifecycle(&retried.run_id, LifecycleOperation::Stop)
            .unwrap();
    }

    #[test]
    fn unlink_then_directory_sync_failure_advances_layout_and_retries_absent_receipt() {
        let repo = Repo::new("receipt-unlink-sync-retry");
        let registry = RuntimeRegistry::with_capacity(
            &repo.state,
            64,
            CapacityPolicy {
                global_run_capacity: 1,
                per_repository_run_capacity: 1,
                human_review_reserved: 0,
            },
        )
        .unwrap();
        let mut request = repo.request("dock_receipt_unlink_sync_retry");
        request.adapter.arguments = vec!["-c".into(), "sleep 30".into()];
        let receipt = registry.receipt_path(&request.run_id).unwrap();
        let temporary = receipt.with_file_name(format!(
            ".{}.tmp-{}",
            receipt.file_name().unwrap().to_str().unwrap(),
            std::process::id()
        ));
        *registry.before_save_receipt_hook.lock().unwrap() = Some(Arc::new({
            let temporary = temporary.clone();
            move || fs::create_dir(&temporary).unwrap()
        }));
        ROLLBACK_AFTER_UNLINK_HOOK.with(|hook| {
            *hook.borrow_mut() = Some(Box::new(|| {
                Err("could not sync durable record directory: injected failure".into())
            }));
        });

        let error = registry.dispatch(request.clone()).unwrap_err();
        assert!(error.1.contains("could not sync durable record directory"));
        assert!(
            !receipt.exists(),
            "unlink completed before directory sync failed"
        );
        {
            let runs = registry.runs.lock().unwrap();
            let slot = runs.get(&request.run_id).expect("pending receipt cleanup");
            assert!(matches!(
                &slot.state,
                RuntimeSlotState::RollbackPending { layout: None, .. }
            ));
            assert!(
                !slot_reserves_capacity(slot),
                "durably restored layout must release capacity"
            );
        }
        assert!(
            registry
                .layout
                .lock()
                .unwrap()
                .snapshot()
                .workspaces
                .is_empty()
        );

        fs::remove_dir(&temporary).unwrap();
        let retried = registry.dispatch(request).unwrap();
        assert_eq!(retried.run_id, "dock_receipt_unlink_sync_retry");
        assert!(
            registry
                .runs
                .lock()
                .unwrap()
                .get(&retried.run_id)
                .unwrap()
                .active()
                .is_some()
        );
        registry
            .lifecycle(&retried.run_id, LifecycleOperation::Stop)
            .unwrap();
    }

    #[test]
    fn receipt_and_layout_rollback_double_fault_reconciles_on_retry() {
        let repo = Repo::new("receipt-layout-double-fault");
        let registry = Arc::new(
            RuntimeRegistry::with_capacity(
                &repo.state,
                64,
                CapacityPolicy {
                    global_run_capacity: 1,
                    per_repository_run_capacity: 1,
                    human_review_reserved: 0,
                },
            )
            .unwrap(),
        );
        let mut request = repo.request("dock_receipt_layout_double_fault");
        request.adapter.arguments = vec!["-c".into(), "sleep 30".into()];
        let receipt = registry.receipt_path(&request.run_id).unwrap();
        let temporary = receipt.with_file_name(format!(
            ".{}.tmp-{}",
            receipt.file_name().unwrap().to_str().unwrap(),
            std::process::id()
        ));
        let weak = Arc::downgrade(&registry);
        *registry.before_save_receipt_hook.lock().unwrap() = Some(Arc::new({
            let temporary = temporary.clone();
            move || {
                fs::create_dir(&temporary).unwrap();
                weak.upgrade()
                    .unwrap()
                    .layout
                    .lock()
                    .unwrap()
                    .inject_persistence_failure(true);
            }
        }));

        let error = registry.dispatch(request.clone()).unwrap_err();
        assert!(error.1.contains("could not restore dispatch layout"));
        assert!(
            receipt.exists(),
            "identity reservation must survive the double fault"
        );
        let runs = registry.runs.lock().unwrap();
        let slot = runs.get(&request.run_id).expect("terminal retry authority");
        assert!(
            slot.active().is_none(),
            "retired process must not remain active"
        );
        assert!(matches!(
            slot.state,
            RuntimeSlotState::RollbackPending { .. }
        ));
        drop(runs);
        let portfolio = registry.inspect_programme();
        assert_eq!(portfolio.global_active, 1, "capacity remains reserved");
        assert!(portfolio.repositories[0].active_run_ids.is_empty());
        let failed_refresh = registry.layout();
        assert_eq!(
            failed_refresh.workspaces[0]
                .panes
                .values()
                .next()
                .unwrap()
                .runtime,
            PaneRuntime::Exited
        );

        registry
            .layout
            .lock()
            .unwrap()
            .inject_persistence_failure(false);
        fs::remove_dir(&temporary).unwrap();
        assert!(registry.layout().workspaces.is_empty());
        assert!(registry.runs.lock().unwrap().is_empty());
        assert_eq!(registry.inspect_programme().global_active, 0);
        assert!(!receipt.exists());
        assert_eq!(
            registry.dispatch(request).unwrap().run_id,
            "dock_receipt_layout_double_fault"
        );
    }

    #[test]
    fn receipt_failure_restores_existing_pane_across_reload() {
        let repo = Repo::new("receipt-existing-pane-rollback");
        let registry = RuntimeRegistry::new(&repo.state, 64).unwrap();
        // This test asserts that a failed dispatch leaves no run at all. Auto-launched pane
        // shells would put a live run in the pane it creates, measuring the placeholder instead
        // of the rollback under test.
        *registry.suppress_pane_shells.lock().unwrap() = true;
        let mut request = repo.request("dock_receipt_existing_pane_rollback");
        request.adapter.arguments = vec!["-c".into(), "sleep 30".into()];
        let binding = validate_binding(&request).unwrap();
        registry
            .workspace(WorkspaceRequest::Create {
                workspace_id: binding.workspace_id.clone(),
                name: "kept workspace".into(),
                pane_id: binding.pane_id.clone(),
            })
            .unwrap();
        registry
            .workspace(WorkspaceRequest::Rename {
                workspace_id: binding.workspace_id.clone(),
                pane_id: Some(binding.pane_id.clone()),
                name: "kept pane".into(),
            })
            .unwrap();
        let before = registry.layout();
        let receipt = registry.receipt_path(&request.run_id).unwrap();
        let temporary = receipt.with_file_name(format!(
            ".{}.tmp-{}",
            receipt.file_name().unwrap().to_str().unwrap(),
            std::process::id()
        ));
        *registry.before_save_receipt_hook.lock().unwrap() = Some(Arc::new({
            let temporary = temporary.clone();
            move || fs::create_dir(&temporary).unwrap()
        }));

        assert!(matches!(
            registry.dispatch(request),
            Err((ErrorCode::Internal, _))
        ));
        assert_eq!(registry.layout(), before);
        assert!(registry.runs.lock().unwrap().is_empty());
        assert!(!receipt.exists());
        fs::remove_dir(&temporary).unwrap();
        drop(registry);

        let restarted = RuntimeRegistry::new(&repo.state, 64).unwrap();
        let mut restored_before = before;
        restored_before.workspaces[0]
            .panes
            .get_mut(&binding.pane_id)
            .unwrap()
            .runtime = PaneRuntime::Restored;
        assert_eq!(restarted.layout(), restored_before);
        assert_eq!(
            restarted
                .layout
                .lock()
                .unwrap()
                .pane_run(&binding.workspace_id, &binding.pane_id),
            None
        );
        assert!(restarted.runs.lock().unwrap().is_empty());
        assert_eq!(restarted.inspect_programme().global_active, 0);
    }

    #[test]
    fn concurrent_release_is_duplicate_safe_and_launches_once() {
        let upstream_repo = Repo::new("release-race-upstream");
        let downstream_repo = Repo::new("release-race-downstream");
        let registry = Arc::new(RuntimeRegistry::new(&upstream_repo.state, 64).unwrap());
        let mut upstream_request = upstream_repo.request("dock_release_race_upstream");
        upstream_request.adapter.arguments = vec!["-c".into(), "sleep 30".into()];
        let upstream = registry.dispatch(upstream_request).unwrap();
        let mut downstream = downstream_repo.request("dock_release_race_downstream");
        downstream.adapter.arguments.clear();
        registry
            .queue_gated(
                downstream,
                upstream.run_id.clone(),
                ReviewRoute::AcceptScope,
            )
            .unwrap();
        registry.submit_handoff(packet(&upstream)).unwrap();
        registry
            .decide(
                upstream.run_id,
                ReviewRoute::AcceptScope,
                "release exactly once".into(),
            )
            .unwrap();
        let barrier = Arc::new(std::sync::Barrier::new(3));
        let handles: Vec<_> = (0..2)
            .map(|_| {
                let registry = Arc::clone(&registry);
                let barrier = Arc::clone(&barrier);
                thread::spawn(move || {
                    barrier.wait();
                    registry.release_gate("dock_release_race_downstream")
                })
            })
            .collect();
        barrier.wait();
        let results: Vec<_> = handles
            .into_iter()
            .map(|handle| handle.join().unwrap())
            .collect();
        assert_eq!(results.iter().filter(|result| result.is_ok()).count(), 1);
        assert_eq!(
            registry
                .inspect(Some("dock_release_race_downstream"))
                .unwrap()
                .len(),
            1
        );
        assert!(registry.inspect_programme().gates.is_empty());
    }

    #[test]
    fn concurrent_direct_dispatch_cannot_bypass_release_or_deadlock() {
        let upstream_repo = Repo::new("dispatch-release-race-upstream");
        let downstream_repo = Repo::new("dispatch-release-race-downstream");
        let registry = Arc::new(RuntimeRegistry::new(&upstream_repo.state, 64).unwrap());
        let downstream_id = ready_gate(
            &registry,
            &upstream_repo,
            &downstream_repo,
            "dispatch_release_race",
        );
        let direct_request = registry
            .programme
            .lock()
            .unwrap()
            .gates
            .get(&downstream_id)
            .unwrap()
            .dispatch
            .clone();
        let barrier = Arc::new(std::sync::Barrier::new(3));
        let (sender, receiver) = std::sync::mpsc::channel();
        let release = {
            let registry = Arc::clone(&registry);
            let barrier = Arc::clone(&barrier);
            let sender = sender.clone();
            let downstream_id = downstream_id.clone();
            thread::spawn(move || {
                barrier.wait();
                sender
                    .send((true, registry.release_gate(&downstream_id).map(|_| ())))
                    .unwrap();
            })
        };
        let direct = {
            let registry = Arc::clone(&registry);
            let barrier = Arc::clone(&barrier);
            thread::spawn(move || {
                barrier.wait();
                sender
                    .send((false, registry.dispatch(direct_request).map(|_| ())))
                    .unwrap();
            })
        };
        barrier.wait();
        let mut results = Vec::new();
        for _ in 0..2 {
            results.push(
                receiver
                    .recv_timeout(crate::testing::budget(5))
                    .expect("dispatch/release lock ordering deadlocked"),
            );
        }
        release.join().unwrap();
        direct.join().unwrap();
        assert!(
            results
                .iter()
                .any(|(is_release, result)| *is_release && result.is_ok())
        );
        assert!(results.iter().any(|(is_release, result)| {
            !*is_release
                && matches!(
                    result,
                    Err((ErrorCode::GateBlocked | ErrorCode::DuplicateRunId, _))
                )
        }));
        assert_eq!(registry.inspect(Some(&downstream_id)).unwrap().len(), 1);
    }

    #[test]
    fn missing_adapter_binary_creates_no_receipt_run_or_pane() {
        let repo = Repo::new("missing-adapter");
        let registry = RuntimeRegistry::new(&repo.state, 256).unwrap();
        let mut request = repo.request("dock_missing_adapter");
        request.adapter = crate::adapter::AdapterSelection {
            id: crate::adapter::AdapterId::Generic,
            executable: Some("/definitely/not/a/dock-agent".into()),
            arguments: vec![],
        };
        let error = registry.dispatch(request).unwrap_err();
        assert_eq!(error.0, ErrorCode::AdapterUnavailable);
        assert!(
            !repo
                .state
                .join("dispatches/dock_missing_adapter.json")
                .exists()
        );
        assert!(registry.inspect(None).unwrap().is_empty());
    }

    #[test]
    fn full_deterministic_workspace_refuses_before_launch_and_close_releases_capacity() {
        let repo = Repo::new("pane-capacity");
        let registry = RuntimeRegistry::new(&repo.state, 64).unwrap();
        let mut first = None;
        for index in 0..crate::layout::MAX_PANES_PER_WORKSPACE {
            let snapshot = registry
                .dispatch(repo.request(&format!("dock_capacity_{index}")))
                .unwrap();
            first.get_or_insert(snapshot);
        }
        assert_eq!(registry.layout().workspaces[0].panes.len(), 64);

        let refused_id = "dock_capacity_refused";
        let mut refused = repo.request(refused_id);
        refused.adapter.arguments = vec!["-c".into(), "sleep 30".into()];
        assert!(matches!(
            registry.dispatch(refused),
            Err((ErrorCode::CapacityExceeded, message)) if message.contains("workspace pane capacity 64")
        ));
        assert!(
            !repo
                .state
                .join(format!("dispatches/{refused_id}.json"))
                .exists()
        );
        assert!(matches!(
            registry.inspect(Some(refused_id)),
            Err((ErrorCode::RunNotFound, _))
        ));
        assert!(
            !registry.layout().workspaces[0]
                .panes
                .contains_key(&format!("pane_{refused_id}"))
        );

        let first = first.unwrap();
        registry
            .workspace(WorkspaceRequest::Close {
                workspace_id: first.workspace_id,
                pane_id: first.pane_id,
            })
            .unwrap();
        assert_eq!(
            registry
                .dispatch(repo.request("dock_capacity_after_close"))
                .unwrap()
                .run_id,
            "dock_capacity_after_close"
        );
        assert_eq!(registry.layout().workspaces[0].panes.len(), 64);
    }

    #[test]
    fn lifecycle_signals_only_the_registered_owned_group_and_restart_replaces_it() {
        let repo = Repo::new("lifecycle");
        let registry = RuntimeRegistry::new(&repo.state, 256).unwrap();
        let mut unrelated = Command::new("sleep").arg("30").spawn().unwrap();
        let mut request = repo.request("dock_lifecycle");
        request.adapter.arguments = vec!["-c".into(), "sleep 30".into()];
        let first = registry.dispatch(request).unwrap();
        let receipt_path = repo.state.join("dispatches/dock_lifecycle.json");
        let original_receipt = fs::read(&receipt_path).unwrap();
        registry
            .lifecycle("dock_lifecycle", LifecycleOperation::Interrupt)
            .unwrap();
        let restarted = registry
            .lifecycle("dock_lifecycle", LifecycleOperation::Restart)
            .unwrap();
        assert_ne!(first.process_group_id, restarted.process_group_id);
        wait_for_owned_group_exit(first.process_group_id.expect("first owned group"));
        // Dispatch receipts are immutable run-id reservations and launch evidence. Restart does
        // not risk a crash-torn update of process-local facts that cannot be recovered on reboot.
        assert_eq!(fs::read(&receipt_path).unwrap(), original_receipt);
        assert_eq!(fs::metadata(&receipt_path).unwrap().mode() & 0o777, 0o600);
        assert!(
            fs::read_dir(repo.state.join("dispatches"))
                .unwrap()
                .all(|entry| !entry
                    .unwrap()
                    .file_name()
                    .to_string_lossy()
                    .contains(".tmp-"))
        );
        assert_eq!(unsafe { nix::libc::kill(unrelated.id() as i32, 0) }, 0);
        registry
            .lifecycle("dock_lifecycle", LifecycleOperation::Stop)
            .unwrap();
        wait_for_owned_group_exit(restarted.process_group_id.expect("replacement owned group"));
        unrelated.kill().unwrap();
        unrelated.wait().unwrap();
    }

    #[test]
    fn restart_rediscovery_failure_keeps_the_prior_owned_runtime_retryable() {
        let repo = Repo::new("restart-binary-disappears");
        let registry = RuntimeRegistry::new(&repo.state, 64).unwrap();
        let executable = repo.root.join("ephemeral-agent");
        fs::write(&executable, "#!/bin/sh\nsleep 30\n").unwrap();
        fs::set_permissions(&executable, fs::Permissions::from_mode(0o700)).unwrap();
        let mut request = repo.request("dock_disappearing");
        request.adapter = AdapterSelection {
            id: crate::adapter::AdapterId::Generic,
            executable: Some(executable.display().to_string()),
            arguments: vec!["-c".into(), "sleep 30".into()],
        };
        let first = registry.dispatch(request).unwrap();
        fs::remove_file(executable).unwrap();

        let error = registry
            .lifecycle("dock_disappearing", LifecycleOperation::Restart)
            .unwrap_err();
        assert_eq!(error.0, ErrorCode::AdapterUnavailable);
        let still_owned = registry
            .inspect(Some("dock_disappearing"))
            .unwrap()
            .remove(0);
        assert_eq!(still_owned.pid, first.pid);
        assert_eq!(still_owned.process_group_id, first.process_group_id);
        assert_eq!(still_owned.state, crate::protocol::ProcessState::Running);
        registry
            .lifecycle("dock_disappearing", LifecycleOperation::Stop)
            .unwrap();
    }

    #[test]
    fn restart_preparation_does_not_block_registry_inspection() {
        use std::sync::Barrier;

        let repo = Repo::new("restart-nonblocking");
        let registry = Arc::new(RuntimeRegistry::new(&repo.state, 64).unwrap());
        let mut request = repo.request("dock_nonblocking");
        request.adapter.arguments = vec!["-c".into(), "sleep 30".into()];
        registry.dispatch(request).unwrap();

        let entered = Arc::new(Barrier::new(2));
        let release = Arc::new(Barrier::new(2));
        *registry
            .restart_hook
            .lock()
            .unwrap_or_else(|p| p.into_inner()) = Some(Arc::new({
            let entered = Arc::clone(&entered);
            let release = Arc::clone(&release);
            move || {
                entered.wait();
                release.wait();
            }
        }));
        let restarting = {
            let registry = Arc::clone(&registry);
            thread::spawn(move || {
                registry.lifecycle("dock_nonblocking", LifecycleOperation::Restart)
            })
        };
        entered.wait();
        // The barrier pair is what proves the point: the restart is parked inside its hook holding
        // whatever preparation locks it takes, and `release` is only reached below, after this
        // inspection returns. An inspection that really were blocked behind those locks would
        // therefore hang here forever rather than merely run late. This bound exists to turn that
        // hang into a message, so it is a generous backstop and not a latency budget — tightening
        // it cannot detect blocking the barrier does not already prove, and only converts
        // scheduler jitter on a loaded machine into a false failure.
        let started = Instant::now();
        let during = registry.inspect(Some("dock_nonblocking")).unwrap();
        assert!(
            started.elapsed() < crate::testing::budget(5),
            "registry inspection never returned while a restart held its preparation locks"
        );
        assert_eq!(during.len(), 1);
        release.wait();
        restarting.join().unwrap().unwrap();
        registry
            .lifecycle("dock_nonblocking", LifecycleOperation::Stop)
            .unwrap();
    }

    /// How long a reap parked on a SIGTERM-ignoring fixture actually stays parked.
    ///
    /// Not three seconds, and not whatever `set_stop_escalation` was asked for. Dock puts a launch
    /// guardian in the same process group as every worker it starts, and that guardian takes the
    /// reap's SIGTERM alongside the worker: it re-signals the group and then SIGKILLs it, which no
    /// trap can refuse. Measured on this machine the group is unsignalable — ESRCH, or EPERM for a
    /// group of zombies not yet waited on — between 240ms and 370ms after the reap signals it,
    /// with or without load. `stop`'s own SIGTERM-to-SIGKILL escalation never gets to run, so
    /// lengthening it buys a parked reap nothing at all.
    ///
    /// The two tests below are the ones that park a reap and then check that unrelated work still
    /// gets through. Both used to spend that window dispatching a real process under a real PTY,
    /// and one of them also created a workspace that auto-launched a login shell. That is unbounded
    /// work on a contended machine and it routinely overran a window a third of a second wide: the
    /// restart test then found the group it meant to release already reaped and failed on `kill`
    /// returning ESRCH, and the close test found its close already returned and failed on its own
    /// premise. Neither failure said anything about mutexes.
    ///
    /// So this constant is documentation, not a knob. What changed is what goes inside the window:
    /// only the property itself, which is nanoseconds of mutex acquisition and microseconds of
    /// registry work. The window is unchanged and still belongs to production; the margin against
    /// it went from about one to about ten thousand.
    const OBSERVED_PARKED_REAP_WINDOW: Duration = Duration::from_millis(240);

    /// Whether a mutex is held right now by somebody else.
    ///
    /// `try_lock` is the whole reason these tests can stop racing: asking whether the reap is
    /// holding the registry or the layout takes nanoseconds and cannot block, where asking the
    /// same question by timing a real dispatch takes an unbounded fraction of a second and can.
    /// A poisoned but unheld mutex is not held — a panic elsewhere is a different failure and
    /// should not be reported as this one.
    fn mutex_is_held<T>(mutex: &Mutex<T>) -> bool {
        matches!(mutex.try_lock(), Err(std::sync::TryLockError::WouldBlock))
    }

    #[test]
    fn blocked_restart_reap_does_not_block_unrelated_registry_or_layout_work() {
        use std::sync::mpsc;

        let repo = Repo::new("restart-blocked-reap");
        let registry = Arc::new(RuntimeRegistry::new(&repo.state, 64).unwrap());
        // The workspace this test creates mid-reap exists to prove the layout mutex is free, not
        // to launch anything. Suppressing the pane shell keeps that proof to a few microseconds
        // and stops the test leaving a login shell behind for the suite to reap.
        *registry.suppress_pane_shells.lock().unwrap() = true;
        let term_seen = repo.root.join("term-seen");
        let ready = repo.root.join("ready");
        let mut blocked = repo.request("dock_blocked_restart");
        blocked.adapter.arguments = vec![
            "-c".into(),
            format!(
                "trap 'touch {}' TERM; touch {}; while :; do :; done",
                term_seen.display(),
                ready.display()
            ),
        ];
        let first = registry.dispatch(blocked).unwrap();
        let deadline = crate::testing::deadline(3);
        while !ready.exists() && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(10));
        }
        assert!(ready.exists(), "TERM-ignoring fixture did not become ready");
        // A property of the fixture, not of the reap, so it is established before the reap starts
        // rather than raced against it.
        assert_eq!(
            registry
                .inspect(Some("dock_blocked_restart"))
                .unwrap()
                .len(),
            1
        );
        // Not what keeps the reap parked — see `OBSERVED_PARKED_REAP_WINDOW`; the guardian decides
        // that. What it does buy is patience in the join `stop` performs once the group is gone,
        // which waits on a real reaper thread and has no business failing because a loaded machine
        // took a moment over it.
        registry.set_stop_escalation(&first.run_id, Duration::from_secs(60));

        let restarting = {
            let registry = Arc::clone(&registry);
            thread::spawn(move || {
                registry.lifecycle("dock_blocked_restart", LifecycleOperation::Restart)
            })
        };
        // A one-millisecond poll rather than ten: what follows has to be observed while the reap
        // is still parked, and the window is only a few hundred milliseconds wide, so the delay
        // between the reap parking and this loop noticing is margin spent for nothing.
        let deadline = crate::testing::deadline(3);
        while !term_seen.exists() && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(1));
        }
        assert!(
            term_seen.exists(),
            "restart did not reach its blocking reap"
        );

        // The reap is inside `stop` right now. These two questions are the property, they take
        // nanoseconds each, and unlike calling into the registry they cannot deadlock — so they
        // are asked first and from this thread.
        let restart_parked = !restarting.is_finished();
        let runs_held = mutex_is_held(&registry.runs);
        let layout_held = mutex_is_held(&registry.layout);

        // The same property again through the public API, because a mutex nobody holds is only
        // interesting if callers actually get through. On its own thread: a reap that did hold
        // either mutex would hang this rather than fail it, and the receive turns that hang into
        // a message. The bound is a liveness backstop and nothing else — the work behind it is
        // three in-memory registry calls.
        let (sent, received) = mpsc::channel();
        let worker = {
            let registry = Arc::clone(&registry);
            thread::spawn(move || {
                let inspected = registry
                    .inspect(Some("dock_blocked_restart"))
                    .map(|s| s.len());
                let created = registry.workspace(WorkspaceRequest::Create {
                    workspace_id: "work_unrelated_manual".into(),
                    name: "unrelated".into(),
                    pane_id: "pane_unrelated_manual".into(),
                });
                let workspaces = registry.layout().workspaces.len();
                sent.send((inspected, created, workspaces)).unwrap();
            })
        };
        let (inspected, created, workspaces) = received
            .recv_timeout(crate::testing::budget(10))
            .expect("unrelated inspect/layout/workspace work blocked behind the restart reap");

        assert!(
            restart_parked,
            "the restart's reap had already returned before the test could look at it, so this \
             run observed nothing about the registry or layout mutexes; a parked reap lasts about \
             {OBSERVED_PARKED_REAP_WINDOW:?}, so something released this one early"
        );
        assert!(
            !runs_held,
            "the restart held the registry mutex across its reap"
        );
        assert!(
            !layout_held,
            "the restart held the layout mutex across its reap"
        );
        assert_eq!(inspected.unwrap(), 1);
        created.unwrap();
        assert!(workspaces >= 2);

        // Nothing here kills the old group. The guardian in it does that on its own, a fraction of
        // a second after the reap's SIGTERM, and a test that asserted it could still signal that
        // group was asserting it had won a race against production's own cleanup.
        let replacement = restarting.join().unwrap().unwrap();
        worker.join().unwrap();
        registry
            .lifecycle(&replacement.run_id, LifecycleOperation::Stop)
            .unwrap();
        registry
            .workspace(WorkspaceRequest::Close {
                workspace_id: "work_unrelated_manual".into(),
                pane_id: "pane_unrelated_manual".into(),
            })
            .unwrap();
    }

    #[test]
    fn blocked_close_reap_does_not_hold_runs_or_layout_mutexes() {
        use std::sync::mpsc;

        let repo = Repo::new("close-blocked-reap");
        let registry = Arc::new(RuntimeRegistry::new(&repo.state, 64).unwrap());
        let term_seen = repo.root.join("close-term-seen");
        let ready = repo.root.join("close-ready");
        let mut blocked = repo.request("dock_blocked_close");
        blocked.adapter.arguments = vec![
            "-c".into(),
            format!(
                "trap 'touch {}' TERM; touch {}; while :; do :; done",
                term_seen.display(),
                ready.display()
            ),
        ];
        let first = registry.dispatch(blocked).unwrap();
        let deadline = crate::testing::deadline(3);
        while !ready.exists() && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(10));
        }
        assert!(ready.exists());
        // A property of the fixture, established before the reap rather than raced against it.
        assert_eq!(registry.inspect(None).unwrap().len(), 1);
        assert!(!registry.layout().workspaces.is_empty());
        // As in the restart test above: this does not hold the group, it only keeps the join that
        // follows the group's death patient. See `OBSERVED_PARKED_REAP_WINDOW`.
        registry.set_stop_escalation(&first.run_id, Duration::from_secs(60));
        let closing = {
            let registry = Arc::clone(&registry);
            let workspace_id = first.workspace_id.clone();
            let pane_id = first.pane_id.clone();
            thread::spawn(move || {
                registry.workspace(WorkspaceRequest::Close {
                    workspace_id,
                    pane_id,
                })
            })
        };
        // A one-millisecond poll, for the reason the restart test above gives.
        let deadline = crate::testing::deadline(3);
        while !term_seen.exists() && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(1));
        }
        assert!(term_seen.exists(), "close did not reach its blocking reap");

        // The property, asked directly and without blocking, while the reap is demonstrably still
        // inside `stop`.
        let close_parked = !closing.is_finished();
        let runs_held = mutex_is_held(&registry.runs);
        let layout_held = mutex_is_held(&registry.layout);

        // And again through the public API, on its own thread so a reap that did hold a mutex
        // reports instead of hanging the suite. The bound is a liveness backstop: what is behind
        // it is two in-memory registry reads.
        let (sent, received) = mpsc::channel();
        let worker = {
            let registry = Arc::clone(&registry);
            thread::spawn(move || {
                let inspected = registry.inspect(None).map(|snapshots| snapshots.len());
                let _ = registry.layout();
                sent.send(inspected).unwrap();
            })
        };
        let inspected = received
            .recv_timeout(crate::testing::budget(10))
            .expect("unrelated registry/layout work blocked behind the close reap");

        assert!(
            close_parked,
            "the close's reap had already returned before the test could look at it, so this run \
             observed nothing about the registry or layout mutexes; a parked reap lasts about \
             {OBSERVED_PARKED_REAP_WINDOW:?}, so something released this one early"
        );
        assert!(
            !runs_held,
            "the close held the registry mutex across its reap"
        );
        assert!(
            !layout_held,
            "the close held the layout mutex across its reap"
        );
        // How many runs and workspaces there are by now depends on whether the reap has finished,
        // which is the one thing this test refuses to have an opinion about. That the calls
        // *returned* does not depend on it, and the counts are asserted above, before the reap
        // starts, where they mean something unambiguous.
        inspected.unwrap();

        // The guardian in the fixture's own process group retires it; the test does not have to,
        // and asserting that it still could was asserting a race against production's cleanup.
        closing.join().unwrap().unwrap();
        worker.join().unwrap();
    }

    #[test]
    fn concurrent_close_stops_the_exact_restarted_owned_run_before_removing_pane() {
        use std::sync::Barrier;

        let repo = Repo::new("restart-close-ownership");
        let registry = Arc::new(RuntimeRegistry::new(&repo.state, 64).unwrap());
        let mut request = repo.request("dock_restart_close");
        request.adapter.arguments = vec!["-c".into(), "sleep 30".into()];
        let first = registry.dispatch(request).unwrap();
        let entered = Arc::new(Barrier::new(2));
        let release = Arc::new(Barrier::new(2));
        *registry.restart_hook.lock().unwrap() = Some(Arc::new({
            let entered = Arc::clone(&entered);
            let release = Arc::clone(&release);
            move || {
                entered.wait();
                release.wait();
            }
        }));

        let restarting = {
            let registry = Arc::clone(&registry);
            thread::spawn(move || {
                registry.lifecycle("dock_restart_close", LifecycleOperation::Restart)
            })
        };
        entered.wait();
        let closing = {
            let registry = Arc::clone(&registry);
            let workspace_id = first.workspace_id.clone();
            let pane_id = first.pane_id.clone();
            thread::spawn(move || {
                registry.workspace(WorkspaceRequest::Close {
                    workspace_id,
                    pane_id,
                })
            })
        };
        release.wait();
        let replacement = restarting.join().unwrap().unwrap();
        assert!(closing.join().unwrap().unwrap().is_none());
        wait_for_owned_group_exit(
            replacement
                .process_group_id
                .expect("replacement owned group"),
        );
        assert!(registry.layout().workspaces.is_empty());
        assert!(matches!(
            registry.inspect(Some("dock_restart_close")),
            Err((ErrorCode::RunNotFound, _))
        ));
    }

    #[test]
    fn rejects_an_existing_state_directory_accessible_to_other_users() {
        let repo = Repo::new("state-permissions");
        fs::create_dir(&repo.state).unwrap();
        fs::set_permissions(&repo.state, fs::Permissions::from_mode(0o755)).unwrap();
        fs::write(repo.state.join("keep"), "untouched\n").unwrap();

        assert!(
            RuntimeRegistry::new(&repo.state, 64)
                .err()
                .is_some_and(|message| message.contains("refusing untrusted state directory"))
        );
        assert_eq!(
            fs::read_to_string(repo.state.join("keep")).unwrap(),
            "untouched\n"
        );
        assert_eq!(
            fs::symlink_metadata(&repo.state)
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o755
        );
        assert!(!repo.state.join("dispatches").exists());
    }

    #[test]
    fn rejects_a_symlinked_state_ancestor_without_mutating_its_target() {
        let repo = Repo::new("state-symlink-ancestor");
        let target = repo.root.join("outside-state-target");
        fs::create_dir(&target).unwrap();
        fs::write(target.join("keep"), "untouched\n").unwrap();
        let substituted = repo.root.join("substituted-state-parent");
        symlink(&target, &substituted).unwrap();
        let state = substituted.join("local");

        let error = RuntimeRegistry::new(&state, 64)
            .err()
            .expect("symlink ancestor must fail");

        assert!(error.contains("without following symlinks"), "{error}");
        assert_eq!(
            fs::read_to_string(target.join("keep")).unwrap(),
            "untouched\n"
        );
        assert!(!target.join("local").exists());
        assert!(
            fs::symlink_metadata(substituted)
                .unwrap()
                .file_type()
                .is_symlink()
        );
    }

    #[test]
    fn creates_every_missing_nested_state_directory_with_private_permissions() {
        let repo = Repo::new("nested-state-creation");
        let first = repo.root.join("missing-state-parent");
        let second = first.join("nested");
        let state = second.join("local");

        let registry = RuntimeRegistry::new(&state, 64).unwrap();
        drop(registry);

        for directory in [&first, &second, &state, &state.join("dispatches")] {
            let metadata = fs::symlink_metadata(directory).unwrap();
            assert!(
                metadata.is_dir(),
                "{} was not a directory",
                directory.display()
            );
            assert_eq!(
                metadata.permissions().mode() & 0o777,
                0o700,
                "{} was not owner-only",
                directory.display()
            );
        }
    }

    #[test]
    fn creates_missing_final_state_component_on_slice5_absolute_tmp_path() {
        let smoke_dir = PathBuf::from(format!(
            "/tmp/dock-slice5.test-{}-{}",
            std::process::id(),
            SEQ.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&smoke_dir).unwrap();
        fs::set_permissions(&smoke_dir, fs::Permissions::from_mode(0o700)).unwrap();
        let state = smoke_dir.join("state");

        ensure_private_directory(&state, "state").unwrap();

        let metadata = fs::symlink_metadata(&state).unwrap();
        assert!(metadata.is_dir());
        assert_eq!(metadata.permissions().mode() & 0o777, 0o700);
        fs::remove_dir_all(smoke_dir).unwrap();
    }

    #[test]
    fn rejects_a_non_git_repository_root_before_launch_or_receipt() {
        let repo = Repo::new("non-git-root");
        let registry = RuntimeRegistry::new(&repo.state, 64).unwrap();
        let non_git = std::env::temp_dir().join(format!(
            "dock-non-git-{}-{}",
            std::process::id(),
            SEQ.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&non_git).unwrap();
        let mut request = repo.request("dock_non_git");
        request.repository_root = non_git.display().to_string();
        request.worktree = non_git.display().to_string();

        assert!(
            matches!(registry.dispatch(request), Err((ErrorCode::InvalidBinding, message)) if message.contains("Git validation failed"))
        );
        assert!(!repo.state.join("dispatches/dock_non_git.json").exists());
        fs::remove_dir(non_git).unwrap();
    }

    #[test]
    fn invalid_task_duplicate_id_mismatch_traversal_and_symlink_escape_are_rejected_before_launch()
    {
        let repo = Repo::new("reject");
        let other = Repo::new("other");
        let registry = RuntimeRegistry::new(&repo.state, 64).unwrap();
        let mut missing = repo.request("dock_missing");
        missing.external_task_ref = "  ".into();
        assert!(matches!(
            registry.dispatch(missing),
            Err((ErrorCode::InvalidBinding, _))
        ));
        registry.dispatch(repo.request("dock_duplicate")).unwrap();
        assert!(matches!(
            registry.dispatch(repo.request("dock_duplicate")),
            Err((ErrorCode::DuplicateRunId, _))
        ));
        let unrelated = repo.root.join("unrelated");
        fs::create_dir(&unrelated).unwrap();
        run(&unrelated, &["init", "-q"]);
        run(
            &unrelated,
            &["config", "user.email", "dock@example.invalid"],
        );
        run(&unrelated, &["config", "user.name", "Dock Fixture"]);
        fs::write(unrelated.join("tracked"), "other\n").unwrap();
        run(&unrelated, &["add", "tracked"]);
        run(&unrelated, &["commit", "-qm", "other"]);
        let mut mismatch = repo.request("dock_mismatch");
        mismatch.worktree = unrelated.display().to_string();
        assert!(
            matches!(registry.dispatch(mismatch), Err((ErrorCode::InvalidBinding, message)) if message.contains("repository mismatch"))
        );
        let mut traversal = repo.request("dock_traversal");
        traversal.worktree = repo.root.join("fixture/../fixture").display().to_string();
        assert!(matches!(
            registry.dispatch(traversal),
            Err((ErrorCode::InvalidBinding, _))
        ));
        symlink(&other.root, repo.root.join("escape")).unwrap();
        let mut escape = repo.request("dock_escape");
        escape.worktree = repo.root.join("escape").display().to_string();
        assert!(matches!(
            registry.dispatch(escape),
            Err((ErrorCode::InvalidBinding, _))
        ));
        assert!(!repo.state.join("dispatches/dock_missing.json").exists());
        assert!(!repo.state.join("dispatches/dock_mismatch.json").exists());
    }

    #[test]
    fn strict_handoff_attaches_current_git_evidence_and_routes_explicit_decisions() {
        let repo = Repo::new("handoff");
        let registry = RuntimeRegistry::new(&repo.state, 64).unwrap();
        let first = registry
            .dispatch(repo.request("dock_handoff_accept"))
            .unwrap();
        fs::write(
            repo.root.join("fixture/tracked"),
            "fixture\nreview change\n",
        )
        .unwrap();

        let record = registry.submit_handoff(packet(&first)).unwrap();
        assert_eq!(record.evidence.branch, first.branch);
        assert_eq!(record.evidence.base_sha, first.base_sha);
        assert_eq!(record.evidence.head_sha, first.base_sha);
        assert_eq!(record.evidence.status_entries, 1);
        assert_eq!(
            (
                record.evidence.changed_files,
                record.evidence.insertions,
                record.evidence.deletions
            ),
            (1, 1, 0)
        );
        assert_eq!(registry.review_inbox().unwrap(), vec![record.clone()]);

        let persisted =
            fs::read_to_string(repo.state.join("handoffs/dock_handoff_accept.json")).unwrap();
        assert!(!persisted.contains("scrollback"));
        assert!(!persisted.contains("command"));
        let decision = registry
            .decide(
                first.run_id.clone(),
                ReviewRoute::AcceptScope,
                "Scope accepted for review routing only.".into(),
            )
            .unwrap();
        assert!(!decision.git_mutated);
        assert!(!decision.external_task_completed);
        assert!(registry.review_inbox().unwrap().is_empty());
        assert_eq!(
            fs::read_to_string(repo.root.join("fixture/tracked")).unwrap(),
            "fixture\nreview change\n"
        );

        let second = registry
            .dispatch(repo.request("dock_handoff_change"))
            .unwrap();
        registry.submit_handoff(packet(&second)).unwrap();
        let requested = registry
            .decide(
                second.run_id,
                ReviewRoute::RequestChange,
                "Please add the missing boundary test.".into(),
            )
            .unwrap();
        assert_eq!(requested.route, ReviewRoute::RequestChange);
        assert!(!requested.git_mutated && !requested.external_task_completed);
    }

    #[test]
    fn handoff_rejects_unknown_runs_binding_mismatch_future_schema_and_secret_markers() {
        let repo = Repo::new("handoff-reject");
        let registry = RuntimeRegistry::new(&repo.state, 64).unwrap();
        let snapshot = registry
            .dispatch(repo.request("dock_handoff_reject"))
            .unwrap();
        let mut unknown = packet(&snapshot);
        unknown.run_id = "dock_unknown".into();
        assert!(matches!(
            registry.submit_handoff(unknown),
            Err((ErrorCode::RunNotFound, _))
        ));
        let mut mismatch = packet(&snapshot);
        mismatch.base_sha = "deadbeef".into();
        assert!(matches!(
            registry.submit_handoff(mismatch),
            Err((ErrorCode::InvalidHandoff, _))
        ));
        let mut future = packet(&snapshot);
        future.schema_version = 2;
        assert!(matches!(
            registry.submit_handoff(future),
            Err((ErrorCode::InvalidHandoff, _))
        ));
        let mut secret = packet(&snapshot);
        secret.summary = "token=do-not-store".into();
        assert!(matches!(
            registry.submit_handoff(secret),
            Err((ErrorCode::InvalidHandoff, _))
        ));
        assert!(
            !repo
                .state
                .join("handoffs/dock_handoff_reject.json")
                .exists()
        );
    }

    #[test]
    fn duplicate_handoff_is_explicit_and_preserves_first_evidence() {
        let repo = Repo::new("handoff-duplicate");
        let registry = RuntimeRegistry::new(&repo.state, 64).unwrap();
        let snapshot = registry
            .dispatch(repo.request("dock_handoff_duplicate"))
            .unwrap();
        let first = registry.submit_handoff(packet(&snapshot)).unwrap();
        let mut second = packet(&snapshot);
        second.summary = "Attempted replacement".into();

        assert!(matches!(
            registry.submit_handoff(second),
            Err((ErrorCode::DuplicateHandoff, message)) if message.contains("already exists")
        ));
        assert_eq!(
            registry
                .store
                .load_handoff_record(&snapshot.run_id)
                .unwrap(),
            first
        );
    }

    #[test]
    fn handoff_rejects_live_branch_drift_and_untracked_files() {
        let repo = Repo::new("handoff-live-git");
        let registry = RuntimeRegistry::new(&repo.state, 64).unwrap();
        let first = registry
            .dispatch(repo.request("dock_handoff_branch_drift"))
            .unwrap();
        run(
            &repo.root.join("fixture"),
            &["checkout", "-qb", "unexpected-branch"],
        );
        assert!(matches!(
            registry.submit_handoff(packet(&first)),
            Err((ErrorCode::InvalidHandoff, message)) if message.contains("live Git binding")
        ));

        let second = registry
            .dispatch(repo.request("dock_handoff_untracked"))
            .unwrap();
        fs::write(repo.root.join("fixture/untracked-secret.txt"), "local\n").unwrap();
        assert!(matches!(
            registry.submit_handoff(packet(&second)),
            Err((ErrorCode::InvalidHandoff, message)) if message.contains("untracked files")
        ));
    }

    #[test]
    fn rejects_common_secret_shapes_in_every_free_text_field() {
        let cases = [
            "Authorization: Bearer abcdefghijklmnopqrstuvwxyz",
            "authorization = Basic QWxhZGRpbjpvcGVuIHNlc2FtZQ==",
            "token=abcdefghijk123456789",
            r#"{\"api_key\":\"sk-proj-abcdefghijklmnopqrstuvwxyz\"}"#,
            "github_token: ghp_abcdefghijklmnopqrstuvwxyz123456",
            "AWS_ACCESS_KEY_ID=AKIAIOSFODNN7EXAMPLE",
            "aws_secret_access_key = wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY",
        ];
        for value in cases {
            assert!(
                likely_contains_secret(value),
                "secret was accepted: {value}"
            );
        }

        let repo = Repo::new("handoff-secret-fields");
        let registry = RuntimeRegistry::new(&repo.state, 64).unwrap();
        let snapshot = registry
            .dispatch(repo.request("dock_handoff_secret_fields"))
            .unwrap();
        for (field, secret) in cases.iter().take(3).enumerate() {
            let mut candidate = packet(&snapshot);
            match field {
                0 => candidate.summary = (*secret).into(),
                1 => candidate.question = Some((*secret).into()),
                _ => candidate.checks[0].name = (*secret).into(),
            }
            assert!(matches!(
                registry.submit_handoff(candidate),
                Err((ErrorCode::InvalidHandoff, _))
            ));
        }
    }

    /// Which process-table snapshot the registry is currently answering from.
    ///
    /// `u64::MAX` for a registry that has never taken one, so a test asserting that none was taken
    /// cannot pass by coincidence against a generation that happens to be zero.
    fn process_table_generation(registry: &RuntimeRegistry) -> u64 {
        registry
            .process_table
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .latest
            .as_ref()
            .map_or(u64::MAX, |latest| latest.generation)
    }

    /// Polls at the rate the event stream does, for as long as it is given.
    fn poll_for(registry: &RuntimeRegistry, window: Duration) {
        let until = Instant::now() + window;
        while Instant::now() < until {
            registry.pulse();
            thread::sleep(Duration::from_millis(16));
        }
    }

    /// The cost behind this: `ps -axo pid=,ppid=,pgid=,comm=` on an ordinary machine of 949
    /// processes costs about 35ms of CPU, and it is a *subprocess*, so that CPU is charged to `ps`
    /// rather than to the daemon. Taking one every [`PROCESS_TABLE_TTL`] regardless was ten
    /// percent of a core burned by a daemon doing nothing whatsoever — measured at 9.8% total
    /// against 0.6% for the daemon process itself, which is why it went unexplained for so long.
    ///
    /// A run that never writes is the case the inference rests on: no pane has produced a byte, so
    /// no agent has started or exited, so a fresh table cannot say anything the last one did not.
    #[test]
    fn a_daemon_whose_runs_are_all_silent_stops_taking_process_tables() {
        let repo = Repo::new("quiet-process-table");
        let registry = RuntimeRegistry::new(&repo.state, 256).unwrap();
        *registry.suppress_pane_shells.lock().unwrap() = true;
        let mut request = repo.request("dock_quiet_run");
        // Silent for the whole test, on purpose. A pane shell would paint a prompt and, depending
        // on whose shell it is, keep repainting it, which is the one thing this test must rule out
        // rather than measure.
        request.adapter.arguments = vec!["-c".into(), "sleep 30".into()];
        registry.dispatch(request).expect("dispatch a silent run");
        // Long enough for the run to be launched and the first table — the one every later poll
        // reuses — to have been taken.
        poll_for(&registry, Duration::from_millis(700));
        let taken = process_table_generation(&registry);
        assert_ne!(
            taken,
            u64::MAX,
            "a run with a process group must have caused one table to be taken"
        );

        // Three times the floor between refreshes and comfortably inside the quiet backstop, so
        // the old behaviour would have taken three more tables across this window.
        poll_for(&registry, PROCESS_TABLE_TTL * 3);
        assert_eq!(
            process_table_generation(&registry),
            taken,
            "a poll where no run has written a byte must reuse the table it already has"
        );
        let _ = registry.lifecycle("dock_quiet_run", LifecycleOperation::Stop);
    }

    /// The other half of the bargain: reuse is conditional on silence, and a pane that writes is a
    /// pane where an agent may just have started. Starting one prints a banner and leaving one
    /// hands back a shell that prints a prompt, so output is the signal, and it has to be acted on
    /// within the same half-second the old unconditional refresh offered.
    #[test]
    fn a_run_that_writes_is_given_a_fresh_process_table_within_the_usual_interval() {
        let repo = Repo::new("writing-process-table");
        let registry = RuntimeRegistry::new(&repo.state, 256).unwrap();
        *registry.suppress_pane_shells.lock().unwrap() = true;
        let mut request = repo.request("dock_writing_run");
        request.adapter.arguments = vec![
            "-c".into(),
            "while :; do echo the agent is printing; sleep 0.1; done".into(),
        ];
        registry.dispatch(request).expect("dispatch a writing run");
        poll_for(&registry, Duration::from_millis(700));
        let taken = process_table_generation(&registry);

        poll_for(&registry, PROCESS_TABLE_TTL * 3);
        let refreshed = process_table_generation(&registry);
        assert!(
            refreshed > taken,
            "a run that is writing must still get fresh process tables, but the generation stayed \
             at {taken}"
        );
        let _ = registry.lifecycle("dock_writing_run", LifecycleOperation::Stop);
    }

    /// A refresh runs on its own thread because taking one inline stalled the sixteen-millisecond
    /// poll loop for the sixty milliseconds `ps` takes to answer — twice a second, four frames
    /// dropped each time, measured as a p99 of 55ms against a p50 of 0.02ms. Only the very first
    /// table is taken inline, because a daemon that has none cannot answer at all without it.
    #[test]
    fn taking_a_fresh_process_table_does_not_stall_the_poll_it_was_asked_on() {
        let repo = Repo::new("unstalled-process-table");
        let registry = RuntimeRegistry::new(&repo.state, 256).unwrap();
        *registry.suppress_pane_shells.lock().unwrap() = true;
        let mut request = repo.request("dock_unstalled_run");
        request.adapter.arguments = vec![
            "-c".into(),
            "while :; do echo the agent is printing; sleep 0.1; done".into(),
        ];
        registry.dispatch(request).expect("dispatch a writing run");
        // Past the first, inline, table.
        poll_for(&registry, Duration::from_millis(700));
        let taken = process_table_generation(&registry);

        let mut slowest = Duration::ZERO;
        let until = Instant::now() + PROCESS_TABLE_TTL * 4;
        while Instant::now() < until {
            let polled = Instant::now();
            registry.pulse();
            slowest = slowest.max(polled.elapsed());
            thread::sleep(Duration::from_millis(16));
        }
        assert!(
            process_table_generation(&registry) > taken,
            "this window has to contain a refresh or it proves nothing about refreshes"
        );
        // Not a claim about how fast a poll is. The two populations this has to tell apart are a
        // poll that reads a cached table, which is tens of microseconds, and a poll that spawns
        // `ps` and waits for it, which measured 55ms at the ninety-ninth percentile on an idle
        // machine and 426ms under load. Forty milliseconds sits between them with room on both
        // sides, and scales with the suite's timeout scale for a contended runner.
        assert!(
            slowest < crate::testing::budget_millis(40),
            "a poll took {slowest:?}, which is long enough to have taken a process table on the \
             thread the event stream is polling from"
        );
        let _ = registry.lifecycle("dock_unstalled_run", LifecycleOperation::Stop);
    }
}
