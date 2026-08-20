use std::{
    collections::HashMap,
    fs::{self, File, OpenOptions},
    io::Write,
    os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt},
    path::{Component, Path, PathBuf},
    process::Command,
    sync::{Arc, Mutex},
};

use regex::Regex;
use serde::Serialize;

use crate::{
    adapter::AdapterSelection,
    git::GitAdapter,
    model::{HandoffEvidence, HandoffPacket, HandoffRecord, ReviewDecision, ReviewRoute},
    protocol::{DispatchRequest, ErrorCode, LifecycleOperation, RuntimeSnapshot},
    runtime::{OwnedRuntime, RunBinding},
    storage::LocalStore,
};

pub struct RuntimeRegistry {
    runs: Mutex<HashMap<String, RuntimeEntry>>,
    receipts: PathBuf,
    scrollback_capacity: usize,
    store: LocalStore,
    #[cfg(test)]
    restart_hook: Mutex<Option<Arc<dyn Fn() + Send + Sync>>>,
}

#[derive(Clone)]
struct RuntimeEntry {
    runtime: Arc<OwnedRuntime>,
    selection: AdapterSelection,
}

#[derive(Debug, Serialize)]
struct DispatchReceipt<'a> {
    protocol_version: u16,
    repository_root: &'a str,
    external_task_ref: &'a str,
    run_id: &'a str,
    worktree: &'a str,
    branch: &'a str,
    base_sha: &'a str,
    workspace_id: &'a str,
    pane_id: &'a str,
    pid: Option<u32>,
    process_group_id: Option<i32>,
    state: &'a crate::protocol::ProcessState,
    diagnostic: &'a Option<String>,
    adapter: &'a crate::adapter::AdapterId,
    process_capabilities: &'a crate::adapter::ProcessCapabilities,
    adapter_capabilities: &'a crate::adapter::AdapterCapabilities,
    provider_state: &'a crate::protocol::ProviderState,
}

impl RuntimeRegistry {
    pub fn new(state_dir: impl Into<PathBuf>, scrollback_capacity: usize) -> Result<Self, String> {
        let state_dir = state_dir.into();
        ensure_private_directory(&state_dir, "state")?;
        let receipts = state_dir.join("dispatches");
        ensure_private_directory(&receipts, "dispatch receipt")?;
        Ok(Self {
            runs: Mutex::new(HashMap::new()),
            receipts,
            scrollback_capacity,
            store: LocalStore::new(state_dir),
            #[cfg(test)]
            restart_hook: Mutex::new(None),
        })
    }

    pub fn dispatch(
        &self,
        request: DispatchRequest,
    ) -> Result<RuntimeSnapshot, (ErrorCode, String)> {
        let binding = validate_binding(&request).map_err(|m| (ErrorCode::InvalidBinding, m))?;
        // Adapter discovery is intentionally before the registry lock, receipt reservation, and
        // runtime construction: a missing binary must leave no run, pane, or durable receipt.
        let adapter = request
            .adapter
            .resolve()
            .map_err(|m| (ErrorCode::AdapterUnavailable, m))?;
        let mut runs = self.runs.lock().unwrap_or_else(|p| p.into_inner());
        let receipt = self
            .receipt_path(&request.run_id)
            .map_err(|m| (ErrorCode::InvalidBinding, m))?;
        if runs.contains_key(&request.run_id) || receipt.exists() {
            return Err((
                ErrorCode::DuplicateRunId,
                format!("run id {:?} already exists", request.run_id),
            ));
        }
        reserve_run_id(&receipt).map_err(|m| (ErrorCode::Internal, m))?;
        let runtime = Arc::new(OwnedRuntime::launch(
            binding,
            adapter,
            self.scrollback_capacity,
        ));
        let snapshot = runtime.snapshot();
        save_receipt(&receipt, &snapshot).map_err(|m| (ErrorCode::Internal, m))?;
        runs.insert(
            snapshot.run_id.clone(),
            RuntimeEntry {
                runtime,
                selection: request.adapter,
            },
        );
        Ok(snapshot)
    }

    pub fn lifecycle(
        &self,
        run_id: &str,
        operation: LifecycleOperation,
    ) -> Result<RuntimeSnapshot, (ErrorCode, String)> {
        let entry = self
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
        let runtime = entry.runtime;
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
                // Rediscovery and launch can block. Keep the prior owned runtime untouched and do
                // all preparation outside the registry lock.
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
                let replacement = Arc::new(OwnedRuntime::launch(
                    runtime.binding(),
                    adapter,
                    self.scrollback_capacity,
                ));
                let snapshot = replacement.snapshot();
                if snapshot.pid.is_none() {
                    return Err((
                        ErrorCode::AdapterUnavailable,
                        snapshot
                            .diagnostic
                            .clone()
                            .unwrap_or_else(|| "replacement adapter failed to launch".into()),
                    ));
                }

                let mut runs = self.runs.lock().unwrap_or_else(|p| p.into_inner());
                let current = runs.get(run_id).ok_or_else(|| {
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
                runs.insert(
                    run_id.to_owned(),
                    RuntimeEntry {
                        runtime: Arc::clone(&replacement),
                        selection: entry.selection,
                    },
                );
                drop(runs);
                // The replacement is now the sole registered capability. Only then retire the
                // exact prior group whose Arc identity won the compare-and-swap above.
                runtime.stop().map_err(|m| (ErrorCode::Internal, m))?;
                Ok(snapshot)
            }
        }
    }

    pub fn inspect(
        &self,
        run_id: Option<&str>,
    ) -> Result<Vec<RuntimeSnapshot>, (ErrorCode, String)> {
        let runs = self.runs.lock().unwrap_or_else(|p| p.into_inner());
        if let Some(run_id) = run_id {
            let run = runs.get(run_id).ok_or_else(|| {
                (
                    ErrorCode::RunNotFound,
                    format!("run id {run_id:?} is not active in this daemon"),
                )
            })?;
            return Ok(vec![run.runtime.snapshot()]);
        }
        let mut snapshots: Vec<_> = runs.values().map(|run| run.runtime.snapshot()).collect();
        snapshots.sort_by(|a, b| a.run_id.cmp(&b.run_id));
        Ok(snapshots)
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
    match fs::symlink_metadata(path) {
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            fs::create_dir_all(path)
                .map_err(|e| format!("could not create {label} directory: {e}"))?;
            fs::set_permissions(path, fs::Permissions::from_mode(0o700))
                .map_err(|e| format!("could not secure {label} directory: {e}"))?;
        }
        Err(error) => return Err(format!("could not inspect {label} directory: {error}")),
    }

    let metadata = fs::symlink_metadata(path)
        .map_err(|e| format!("could not inspect {label} directory: {e}"))?;
    // SAFETY: geteuid(2) has no preconditions and does not access memory.
    let effective_uid = unsafe { nix::libc::geteuid() };
    if !metadata.file_type().is_dir()
        || metadata.uid() != effective_uid
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

fn validate_binding(request: &DispatchRequest) -> Result<RunBinding, String> {
    validate_run_id(&request.run_id)?;
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
    Ok(RunBinding {
        repository_root,
        external_task_ref: request.external_task_ref.clone(),
        run_id: request.run_id.clone(),
        worktree,
        branch,
        base_sha,
    })
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
    file.write_all(b"{}\n")
        .and_then(|_| file.sync_all())
        .map_err(|e| format!("could not persist run-id reservation: {e}"))
}

fn save_receipt(path: &Path, snapshot: &RuntimeSnapshot) -> Result<(), String> {
    let receipt = DispatchReceipt {
        protocol_version: crate::protocol::PROTOCOL_VERSION,
        repository_root: &snapshot.repository_root,
        external_task_ref: &snapshot.external_task_ref,
        run_id: &snapshot.run_id,
        worktree: &snapshot.worktree,
        branch: &snapshot.branch,
        base_sha: &snapshot.base_sha,
        workspace_id: &snapshot.workspace_id,
        pane_id: &snapshot.pane_id,
        pid: snapshot.pid,
        process_group_id: snapshot.process_group_id,
        state: &snapshot.state,
        diagnostic: &snapshot.diagnostic,
        adapter: &snapshot.adapter,
        process_capabilities: &snapshot.process_capabilities,
        adapter_capabilities: &snapshot.adapter_capabilities,
        provider_state: &snapshot.provider_state,
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
        let deadline = Instant::now() + Duration::from_secs(3);
        let snapshot = loop {
            let s = registry.inspect(Some("dock_valid")).unwrap().remove(0);
            if s.scrollback.contains("fixture") || Instant::now() >= deadline {
                break s;
            }
            thread::sleep(Duration::from_millis(10));
        };
        assert_eq!(
            snapshot.scrollback.trim(),
            fs::canonicalize(repo.root.join("fixture"))
                .unwrap()
                .display()
                .to_string()
        );
        let receipt = fs::read_to_string(repo.state.join("dispatches/dock_valid.json")).unwrap();
        assert!(!receipt.contains("scrollback"));
        assert!(!receipt.contains("do-not-persist"));
        assert!(
            serde_json::from_str::<serde_json::Value>(&receipt)
                .unwrap()
                .get("command")
                .is_none()
        );
        assert!(receipt.contains("TASK-42"));
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
    fn rejects_an_existing_state_directory_accessible_to_other_users() {
        let repo = Repo::new("state-permissions");
        fs::create_dir(&repo.state).unwrap();
        fs::set_permissions(&repo.state, fs::Permissions::from_mode(0o755)).unwrap();

        assert!(
            RuntimeRegistry::new(&repo.state, 64)
                .err()
                .is_some_and(|message| message.contains("refusing untrusted state directory"))
        );
        assert!(!repo.state.join("dispatches").exists());
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
