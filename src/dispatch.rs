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
    sync::{Arc, Mutex},
};

use regex::Regex;
use serde::{Deserialize, Serialize};

use crate::{
    adapter::AdapterSelection,
    detect::{AgentState, agent_in_process_table, classify_screen},
    git::GitAdapter,
    layout::{LayoutRegistry, LayoutSnapshot, PaneRuntime, WorkspaceLayout},
    model::{HandoffEvidence, HandoffPacket, HandoffRecord, ReviewDecision, ReviewRoute},
    protocol::{
        BindingKind, DashboardProfile, DependencyGateSnapshot, DispatchRequest,
        DurableProgrammeGate, ErrorCode, GateState, LifecycleOperation, ProgrammeSnapshot,
        RepositoryPortfolioSnapshot, RuntimeSnapshot, WorkspaceRequest,
    },
    runtime::{OwnedRuntime, PtySize, RunBinding},
    storage::LocalStore,
    terminal::PaneScreen,
};

pub struct RuntimeRegistry {
    runs: Mutex<HashMap<String, RuntimeSlot>>,
    receipts: PathBuf,
    scrollback_rows: usize,
    store: LocalStore,
    programme: Mutex<ProgrammeState>,
    capacity: CapacityPolicy,
    layout: Mutex<LayoutRegistry>,
    /// Last geometry reported for each `workspace/pane`, so a run launched into an already
    /// measured pane starts at the size the client is drawing rather than the fallback. This is
    /// a leaf lock: it is never taken while `runs` or `layout` is held.
    pane_sizes: Mutex<HashMap<String, PtySize>>,
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
        Ok(Self {
            runs: Mutex::new(HashMap::new()),
            receipts,
            scrollback_rows,
            store,
            programme: Mutex::new(programme),
            capacity,
            layout: Mutex::new(layout),
            pane_sizes: Mutex::new(HashMap::new()),
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

    pub fn terminal_launch(
        &self,
        workspace_id: String,
        pane_id: String,
        run_id: String,
        profile: DashboardProfile,
        runtime_directory: String,
    ) -> Result<RuntimeSnapshot, (ErrorCode, String)> {
        validate_external_run_id(&run_id).map_err(|m| (ErrorCode::InvalidBinding, m))?;
        let directory = canonical_terminal_directory(Path::new(&runtime_directory))
            .map_err(|m| (ErrorCode::InvalidBinding, m))?;
        let adapter_id = crate::adapter::AdapterId::from(profile);
        let arguments = if adapter_id == crate::adapter::AdapterId::Fixture {
            vec![
                "-c".into(),
                "printf 'Dock-owned fixture ready\\n'; sleep 30".into(),
            ]
        } else {
            vec![]
        };
        let request = DispatchRequest {
            repository_root: directory.display().to_string(),
            external_task_ref: String::new(),
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

    /// Every Dock pane is a working terminal from the moment it exists. This is a Dock-created
    /// PTY in a Dock-created process group like any other owned run, so the no-adoption
    /// invariant is untouched.
    fn launch_pane_shell(&self, workspace_id: &str, pane_id: &str) {
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
            if let Some(run_id) = &run_id {
                // Stop irrevocably retired this capability. A persistence failure retains an
                // Exited pane marker for a safe Close retry, but never dead Active authority.
                runs.remove(run_id);
                if result.is_err() {
                    layout.set_runtime(run_id, PaneRuntime::Exited);
                }
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
            } => layout
                .split(&workspace_id, &pane_id, new_pane_id.clone(), axis)
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
        ProgrammeSnapshot {
            global_active: capacity_snapshots
                .iter()
                .filter(|(_, _, reserves_capacity)| *reserves_capacity)
                .count(),
            global_run_capacity: self.capacity.agent_capacity(),
            human_review_reserved: self.capacity.human_review_reserved,
            repositories,
            gates,
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

    pub fn inspect(
        &self,
        run_id: Option<&str>,
    ) -> Result<Vec<RuntimeSnapshot>, (ErrorCode, String)> {
        // Snapshots are taken outside the registry lock: agent classification reads each run's
        // emulated screen, and holding `runs` across that would serialise every dispatch behind
        // the event stream's continuous polling.
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
                (runtime, snapshot)
            })
            .collect();
        runs.sort_by(|a, b| a.1.run_id.cmp(&b.1.run_id));
        // Exactly one `ps` per call, shared by every run: one per run would make this hot path
        // cost a subprocess spawn for each pane on the screen.
        let table = runs
            .iter()
            .any(|(_, snapshot)| snapshot.process_group_id.is_some())
            .then(process_table)
            .flatten();
        Ok(runs
            .into_iter()
            .map(|(runtime, mut snapshot)| {
                let agent = snapshot
                    .process_group_id
                    .zip(table.as_deref())
                    .and_then(|(pgid, table)| agent_in_process_table(table, pgid));
                snapshot.agent_state = match agent {
                    Some(kind) => {
                        runtime.with_screen(|screen| classify_screen(kind, &screen.text_tail(40)))
                    }
                    None => AgentState::Idle,
                };
                snapshot.agent = agent;
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
        let entry = self
            .runs
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .get(run_id)
            .and_then(RuntimeSlot::active)
            .cloned()?;
        Some(entry.runtime.with_screen(apply))
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
fn pane_shell_run_id(workspace_id: &str, pane_id: &str) -> String {
    format!("{PANE_SHELL_RUN_ID_PREFIX}{workspace_id}_{pane_id}")
}

const PANE_SHELL_RUN_ID_PREFIX: &str = "dock_sh_";

/// One snapshot of the process table, shared by every run in a single `inspect`. Agent detection
/// sits on the event-stream hot path, so it must not cost one subprocess per run.
fn process_table() -> Option<String> {
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
            })
            .expect("split pane");
        let layout = registry.layout();
        assert!(layout.workspaces[0].panes["p2"].run_id.is_some());
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
            })
            .expect("split pane");
        let directory = registry.state.display().to_string();
        registry
            .terminal_launch(
                "w1".into(),
                "p2".into(),
                "dock_taken".into(),
                DashboardProfile::Fixture,
                directory.clone(),
            )
            .expect("first launch claims the run id");

        let refused = registry.terminal_launch(
            "w1".into(),
            "p1".into(),
            "dock_taken".into(),
            DashboardProfile::Fixture,
            directory,
        );
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
            .terminal_launch(
                "w1".into(),
                "p1".into(),
                "dock_agent_run".into(),
                DashboardProfile::Fixture,
                registry.state.display().to_string(),
            )
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
            .terminal_launch(
                "w1".into(),
                "p1".into(),
                pane_shell_run_id("w1", "p1"),
                DashboardProfile::Fixture,
                registry.state.display().to_string(),
            )
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
            .terminal_launch(
                "w1".into(),
                "p1".into(),
                "dock_replacement".into(),
                DashboardProfile::Fixture,
                registry.state.display().to_string(),
            )
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
            .terminal_launch(
                "w".into(),
                "p".into(),
                "dock_terminal_1".into(),
                DashboardProfile::Fixture,
                runtime_dir.display().to_string(),
            )
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
        let deadline = Instant::now() + Duration::from_secs(3);
        while unsafe { nix::libc::kill(-process_group_id, 0) } == 0 && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(10));
        }
        assert_ne!(
            unsafe { nix::libc::kill(-process_group_id, 0) },
            0,
            "retired Dock-owned group {process_group_id} survived lifecycle completion"
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
        let deadline = Instant::now() + Duration::from_secs(3);
        loop {
            let text = run_screen_text(registry, run_id);
            if text.contains(needle) || Instant::now() >= deadline {
                return text;
            }
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
        let deadline = Instant::now() + Duration::from_secs(3);
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
            (
                "unsupported-layout",
                br#"{"schema_version":99,"workspaces":[]}"#.as_slice(),
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
        entered_rx.recv_timeout(Duration::from_secs(3)).unwrap();

        let (inspection_tx, inspection_rx) = std::sync::mpsc::channel();
        let inspector = {
            let registry = Arc::clone(&registry);
            thread::spawn(move || inspection_tx.send(registry.inspect_programme()).unwrap())
        };
        assert!(
            inspection_rx
                .recv_timeout(Duration::from_millis(100))
                .is_err()
        );
        commit_barrier.wait();
        release.join().unwrap().unwrap();
        let portfolio = inspection_rx.recv_timeout(Duration::from_secs(3)).unwrap();
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
                let deadline = Instant::now() + Duration::from_secs(15);
                while !marker.exists() && Instant::now() < deadline {
                    thread::sleep(Duration::from_millis(10));
                }
                assert!(
                    marker.exists(),
                    "guarded child did not reach the injected window"
                );
            }
        }));
        assert!(matches!(
            registry.dispatch(request.clone()),
            Err((ErrorCode::Internal, _))
        ));
        let pid: i32 = fs::read_to_string(&marker).unwrap().trim().parse().unwrap();
        let deadline = Instant::now() + Duration::from_secs(15);
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
            move || {
                let deadline = Instant::now() + Duration::from_secs(3);
                while !marker.exists() && Instant::now() < deadline {
                    thread::sleep(Duration::from_millis(10));
                }
                assert!(
                    marker.exists(),
                    "child did not reach receipt failure window"
                );
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
        let pid: i32 = fs::read_to_string(&marker).unwrap().trim().parse().unwrap();
        let deadline = Instant::now() + Duration::from_secs(3);
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
                    .recv_timeout(Duration::from_secs(5))
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
        let started = Instant::now();
        let during = registry.inspect(Some("dock_nonblocking")).unwrap();
        assert!(started.elapsed() < Duration::from_millis(250));
        assert_eq!(during.len(), 1);
        release.wait();
        restarting.join().unwrap().unwrap();
        registry
            .lifecycle("dock_nonblocking", LifecycleOperation::Stop)
            .unwrap();
    }

    #[test]
    fn blocked_restart_reap_does_not_block_unrelated_registry_or_layout_work() {
        use std::sync::mpsc;

        let repo = Repo::new("restart-blocked-reap");
        let registry = Arc::new(RuntimeRegistry::new(&repo.state, 64).unwrap());
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
        let deadline = Instant::now() + Duration::from_secs(3);
        while !ready.exists() && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(10));
        }
        assert!(ready.exists(), "TERM-ignoring fixture did not become ready");

        let restarting = {
            let registry = Arc::clone(&registry);
            thread::spawn(move || {
                registry.lifecycle("dock_blocked_restart", LifecycleOperation::Restart)
            })
        };
        let deadline = Instant::now() + Duration::from_secs(3);
        while !term_seen.exists() && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(10));
        }
        assert!(
            term_seen.exists(),
            "restart did not reach its blocking reap"
        );

        let (sent, received) = mpsc::channel();
        let worker = {
            let registry = Arc::clone(&registry);
            let mut request = repo.request("dock_unrelated_while_reaping");
            request.adapter.arguments = vec!["-c".into(), "sleep 30".into()];
            thread::spawn(move || {
                let inspected = registry.inspect(Some("dock_blocked_restart")).unwrap();
                let unrelated = registry.dispatch(request).unwrap();
                registry
                    .lifecycle(&unrelated.run_id, LifecycleOperation::Interrupt)
                    .unwrap();
                registry
                    .workspace(WorkspaceRequest::Create {
                        workspace_id: "work_unrelated_manual".into(),
                        name: "unrelated".into(),
                        pane_id: "pane_unrelated_manual".into(),
                    })
                    .unwrap();
                sent.send((inspected.len(), unrelated.run_id)).unwrap();
            })
        };
        let (inspected, unrelated_id) = received
            .recv_timeout(Duration::from_secs(2))
            .expect("unrelated inspect/dispatch/lifecycle/layout blocked behind restart reap");
        assert_eq!(inspected, 1);

        let old_group = first.process_group_id.unwrap();
        assert_eq!(
            unsafe { nix::libc::kill(-old_group, nix::libc::SIGKILL) },
            0
        );
        let replacement = restarting.join().unwrap().unwrap();
        worker.join().unwrap();
        registry
            .lifecycle(&replacement.run_id, LifecycleOperation::Stop)
            .unwrap();
        registry
            .lifecycle(&unrelated_id, LifecycleOperation::Stop)
            .unwrap();
        // The pane created mid-test auto-launched a real login shell. Close it through the
        // ownership-safe path so the suite does not leak a Dock-owned process group.
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
        let deadline = Instant::now() + Duration::from_secs(3);
        while !ready.exists() && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(10));
        }
        assert!(ready.exists());
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
        let deadline = Instant::now() + Duration::from_secs(3);
        while !term_seen.exists() && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(10));
        }
        assert!(term_seen.exists(), "close did not reach its blocking reap");

        let (sent, received) = mpsc::channel();
        let worker = {
            let registry = Arc::clone(&registry);
            let mut request = repo.request("dock_unrelated_while_closing");
            request.adapter.arguments = vec!["-c".into(), "sleep 30".into()];
            thread::spawn(move || {
                assert_eq!(registry.inspect(None).unwrap().len(), 1);
                let unrelated = registry.dispatch(request).unwrap();
                registry
                    .lifecycle(&unrelated.run_id, LifecycleOperation::Interrupt)
                    .unwrap();
                let layout = registry.layout();
                sent.send((unrelated.run_id, layout.workspaces.len()))
                    .unwrap();
            })
        };
        let (unrelated_id, workspace_count) = received
            .recv_timeout(Duration::from_secs(2))
            .expect("unrelated registry/layout work blocked behind close reap");
        assert!(!closing.is_finished());
        assert!(!registry.layout().workspaces.is_empty());
        assert!(workspace_count >= 1);

        let old_group = first.process_group_id.unwrap();
        assert_eq!(
            unsafe { nix::libc::kill(-old_group, nix::libc::SIGKILL) },
            0
        );
        closing.join().unwrap().unwrap();
        worker.join().unwrap();
        registry
            .lifecycle(&unrelated_id, LifecycleOperation::Stop)
            .unwrap();
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
}
