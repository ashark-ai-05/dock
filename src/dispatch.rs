use std::{
    collections::HashMap,
    fs::{self, OpenOptions},
    io::Write,
    os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt},
    path::{Component, Path, PathBuf},
    process::Command,
    sync::{Arc, Mutex},
};

use serde::Serialize;

use crate::{
    protocol::{DispatchRequest, ErrorCode, RuntimeSnapshot},
    runtime::{OwnedRuntime, RunBinding},
};

pub struct RuntimeRegistry {
    runs: Mutex<HashMap<String, Arc<OwnedRuntime>>>,
    receipts: PathBuf,
    scrollback_capacity: usize,
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
        })
    }

    pub fn dispatch(
        &self,
        request: DispatchRequest,
    ) -> Result<RuntimeSnapshot, (ErrorCode, String)> {
        let binding = validate_binding(&request).map_err(|m| (ErrorCode::InvalidBinding, m))?;
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
            request.command,
            self.scrollback_capacity,
        ));
        let snapshot = runtime.snapshot();
        save_receipt(&receipt, &snapshot).map_err(|m| (ErrorCode::Internal, m))?;
        runs.insert(snapshot.run_id.clone(), runtime);
        Ok(snapshot)
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
            return Ok(vec![run.snapshot()]);
        }
        let mut snapshots: Vec<_> = runs.values().map(|run| run.snapshot()).collect();
        snapshots.sort_by(|a, b| a.run_id.cmp(&b.run_id));
        Ok(snapshots)
    }

    fn receipt_path(&self, run_id: &str) -> Result<PathBuf, String> {
        validate_run_id(run_id)?;
        Ok(self.receipts.join(format!("{run_id}.json")))
    }
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
    if request.command.is_empty() || request.command[0].trim().is_empty() {
        return Err("fixture command is required".into());
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
    };
    let bytes = serde_json::to_vec_pretty(&receipt)
        .map_err(|e| format!("could not serialize dispatch receipt: {e}"))?;
    let mut file = OpenOptions::new()
        .write(true)
        .truncate(true)
        .open(path)
        .map_err(|e| format!("could not open dispatch receipt {}: {e}", path.display()))?;
    file.write_all(&bytes)
        .and_then(|_| file.sync_all())
        .map_err(|e| format!("could not persist dispatch receipt: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;
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
                command: vec!["sh".into(), "-c".into(), "pwd".into()],
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
    fn valid_bound_fixture_runs_only_in_the_supplied_directory_and_persists_no_output() {
        let repo = Repo::new("valid");
        let registry = RuntimeRegistry::new(&repo.state, 256).unwrap();
        let mut request = repo.request("dock_valid");
        request.command.push("credential=do-not-persist".into());
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
}
