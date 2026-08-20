use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    ffi::CString,
    fs,
    io::Read,
    os::unix::{
        ffi::OsStrExt,
        fs::{MetadataExt, OpenOptionsExt},
        io::{AsRawFd, FromRawFd},
    },
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

#[cfg(test)]
thread_local! {
    static QUARANTINE_AFTER_RENAME_HOOK: std::cell::RefCell<Option<Box<dyn FnOnce()>>> =
        std::cell::RefCell::new(None);
}

pub const MAX_WORKSPACES: usize = 64;
pub const MAX_PANES_PER_WORKSPACE: usize = 64;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SplitAxis {
    Horizontal,
    Vertical,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PaneRuntime {
    Empty,
    Running,
    Exited,
    Restored,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PaneLayout {
    pub pane_id: String,
    pub name: String,
    pub run_id: Option<String>,
    pub runtime: PaneRuntime,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum LayoutNode {
    Pane {
        pane_id: String,
    },
    Split {
        axis: SplitAxis,
        ratio_milli: u16,
        first: Box<LayoutNode>,
        second: Box<LayoutNode>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkspaceLayout {
    pub workspace_id: String,
    pub name: String,
    pub focused_pane_id: String,
    pub panes: BTreeMap<String, PaneLayout>,
    pub root: LayoutNode,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LayoutSnapshot {
    pub workspaces: Vec<WorkspaceLayout>,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct DurableLayout {
    schema_version: u16,
    workspaces: Vec<DurableWorkspace>,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct DurableWorkspace {
    workspace_id: String,
    name: String,
    focused_pane_id: String,
    panes: BTreeMap<String, DurablePane>,
    root: LayoutNode,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct DurablePane {
    pane_id: String,
    name: String,
}

pub struct LayoutRegistry {
    path: PathBuf,
    workspaces: BTreeMap<String, WorkspaceLayout>,
    #[cfg(test)]
    fail_persistence: bool,
}

/// Exact semantic state displaced by an automatic dispatch binding.  Rollback applies an
/// ownership-checked inverse to the current layout instead of replacing the whole snapshot, so
/// workspace edits made while the process was launching are not lost.
#[derive(Clone)]
pub struct BindRollback {
    workspace_id: String,
    pane_id: String,
    run_id: String,
    prior_workspace: Option<WorkspaceLayout>,
}

impl LayoutRegistry {
    pub fn load(state_dir: &Path) -> Result<Self, String> {
        let path = state_dir.join("layout.json");
        let metadata = match fs::symlink_metadata(&path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(Self::empty(path));
            }
            Err(error) => return Err(format!("could not inspect layout metadata: {error}")),
        };
        if metadata.file_type().is_symlink() {
            // Inspect and remove the directory entry itself. Opening it, chmodding it, or moving
            // it into quarantine and then chmodding it could follow and mutate an arbitrary
            // target outside Dock's owner-only state directory.
            // SAFETY: geteuid(2) has no preconditions and does not access memory.
            let effective_uid = unsafe { nix::libc::geteuid() };
            if metadata.uid() != effective_uid {
                return Err(
                    "refusing layout metadata symlink not owned by the current user".into(),
                );
            }
            fs::remove_file(&path)
                .map_err(|e| format!("could not remove unsafe layout metadata symlink: {e}"))?;
            fs::File::open(state_dir)
                .and_then(|directory| directory.sync_all())
                .map_err(|e| format!("could not sync removed layout metadata symlink: {e}"))?;
            return Ok(Self::empty(path));
        }
        if !metadata.file_type().is_file() {
            return Err("layout metadata must be a regular file".into());
        }
        // SAFETY: geteuid(2) has no preconditions and does not access memory.
        let effective_uid = unsafe { nix::libc::geteuid() };
        if metadata.uid() != effective_uid {
            return Err("refusing layout metadata not owned by the current user".into());
        }
        let mut file = fs::OpenOptions::new()
            .read(true)
            .custom_flags(nix::libc::O_NOFOLLOW)
            .open(&path)
            .map_err(|e| format!("could not safely open layout metadata: {e}"))?;
        let mut bytes = Vec::new();
        file.read_to_end(&mut bytes)
            .map_err(|e| format!("could not read layout metadata: {e}"))?;
        let value = serde_json::from_slice::<DurableLayout>(&bytes)
            .map_err(|e| format!("could not parse layout metadata: {e}"))
            .and_then(|value| {
                if value.schema_version != 1 {
                    return Err("unsupported layout metadata schema version".into());
                }
                validate_durable_workspaces(&value.workspaces)?;
                Ok(value)
            });
        let value = match value {
            Ok(value) => value,
            Err(_) => {
                quarantine_invalid_layout(&path)?;
                return Ok(Self {
                    path,
                    workspaces: BTreeMap::new(),
                    #[cfg(test)]
                    fail_persistence: false,
                });
            }
        };
        Ok(Self {
            path,
            workspaces: value
                .workspaces
                .into_iter()
                .map(|w| {
                    let id = w.workspace_id.clone();
                    (id, w.into_runtime())
                })
                .collect(),
            #[cfg(test)]
            fail_persistence: false,
        })
    }
    fn empty(path: PathBuf) -> Self {
        Self {
            path,
            workspaces: BTreeMap::new(),
            #[cfg(test)]
            fail_persistence: false,
        }
    }

    #[cfg(test)]
    pub(crate) fn inject_persistence_failure(&mut self, fail: bool) {
        self.fail_persistence = fail;
    }
    pub fn snapshot(&self) -> LayoutSnapshot {
        LayoutSnapshot {
            workspaces: self.workspaces.values().cloned().collect(),
        }
    }
    pub fn pane_run(&self, workspace_id: &str, pane_id: &str) -> Option<String> {
        self.workspaces
            .get(workspace_id)?
            .panes
            .get(pane_id)?
            .run_id
            .clone()
    }
    pub fn set_runtime(&mut self, run_id: &str, runtime: PaneRuntime) {
        for workspace in self.workspaces.values_mut() {
            for pane in workspace.panes.values_mut() {
                if pane.run_id.as_deref() == Some(run_id) {
                    pane.runtime = runtime;
                }
            }
        }
    }
    pub fn unbind_run(&mut self, workspace_id: &str, pane_id: &str, run_id: &str) {
        if let Some(pane) = self
            .workspaces
            .get_mut(workspace_id)
            .and_then(|workspace| workspace.panes.get_mut(pane_id))
            .filter(|pane| pane.run_id.as_deref() == Some(run_id))
        {
            pane.run_id = None;
            pane.runtime = PaneRuntime::Empty;
        }
    }
    pub fn create_workspace(
        &mut self,
        id: String,
        name: String,
        pane_id: String,
    ) -> Result<WorkspaceLayout, String> {
        validate_id(&id)?;
        validate_id(&pane_id)?;
        validate_name(&name)?;
        if self.workspaces.len() >= MAX_WORKSPACES {
            return Err("workspace limit reached".into());
        }
        if self.workspaces.contains_key(&id) {
            return Err("workspace id already exists".into());
        }
        let pane = PaneLayout {
            pane_id: pane_id.clone(),
            name: "terminal".into(),
            run_id: None,
            runtime: PaneRuntime::Empty,
        };
        let workspace = WorkspaceLayout {
            workspace_id: id.clone(),
            name,
            focused_pane_id: pane_id.clone(),
            panes: [(pane_id.clone(), pane)].into(),
            root: LayoutNode::Pane { pane_id },
        };
        let mut candidate = self.workspaces.clone();
        candidate.insert(id, workspace.clone());
        self.commit(candidate)?;
        Ok(workspace)
    }
    pub fn split(
        &mut self,
        workspace_id: &str,
        pane_id: &str,
        new_id: String,
        axis: SplitAxis,
    ) -> Result<WorkspaceLayout, String> {
        validate_id(&new_id)?;
        let mut candidate = self.workspaces.clone();
        let workspace = candidate
            .get_mut(workspace_id)
            .ok_or("workspace not found")?;
        if !workspace.panes.contains_key(pane_id) {
            return Err("pane not found".into());
        }
        if workspace.panes.len() >= MAX_PANES_PER_WORKSPACE {
            return Err("pane limit reached".into());
        }
        if workspace.panes.contains_key(&new_id) {
            return Err("pane id already exists".into());
        }
        replace_leaf(&mut workspace.root, pane_id, &new_id, axis)?;
        workspace.panes.insert(
            new_id.clone(),
            PaneLayout {
                pane_id: new_id.clone(),
                name: "terminal".into(),
                run_id: None,
                runtime: PaneRuntime::Empty,
            },
        );
        workspace.focused_pane_id = new_id;
        let result = workspace.clone();
        self.commit(candidate)?;
        Ok(result)
    }
    pub fn focus(&mut self, workspace_id: &str, pane_id: &str) -> Result<WorkspaceLayout, String> {
        let mut candidate = self.workspaces.clone();
        let w = candidate
            .get_mut(workspace_id)
            .ok_or("workspace not found")?;
        if !w.panes.contains_key(pane_id) {
            return Err("pane not found".into());
        }
        w.focused_pane_id = pane_id.into();
        let result = w.clone();
        self.commit(candidate)?;
        Ok(result)
    }
    pub fn resize(
        &mut self,
        workspace_id: &str,
        pane_id: &str,
        ratio: u16,
    ) -> Result<WorkspaceLayout, String> {
        if !(100..=900).contains(&ratio) {
            return Err("split ratio must be between 100 and 900".into());
        }
        let mut candidate = self.workspaces.clone();
        let w = candidate
            .get_mut(workspace_id)
            .ok_or("workspace not found")?;
        if !w.panes.contains_key(pane_id) {
            return Err("pane not found".into());
        }
        resize_parent(&mut w.root, pane_id, ratio)?;
        let result = w.clone();
        self.commit(candidate)?;
        Ok(result)
    }
    pub fn rename(
        &mut self,
        workspace_id: &str,
        pane_id: Option<&str>,
        name: String,
    ) -> Result<WorkspaceLayout, String> {
        validate_name(&name)?;
        let mut candidate = self.workspaces.clone();
        let w = candidate
            .get_mut(workspace_id)
            .ok_or("workspace not found")?;
        if let Some(id) = pane_id {
            w.panes.get_mut(id).ok_or("pane not found")?.name = name;
        } else {
            w.name = name;
        }
        let result = w.clone();
        self.commit(candidate)?;
        Ok(result)
    }
    pub fn close(
        &mut self,
        workspace_id: &str,
        pane_id: &str,
    ) -> Result<Option<WorkspaceLayout>, String> {
        let mut candidate = self.workspaces.clone();
        let w = candidate
            .get_mut(workspace_id)
            .ok_or("workspace not found")?;
        if !w.panes.contains_key(pane_id) {
            return Err("pane not found".into());
        }
        if w.panes.len() == 1 {
            candidate.remove(workspace_id);
            self.commit(candidate)?;
            return Ok(None);
        }
        remove_leaf(&mut w.root, pane_id)?;
        w.panes.remove(pane_id);
        if w.focused_pane_id == pane_id {
            w.focused_pane_id = first_leaf(&w.root).into();
        }
        let result = w.clone();
        self.commit(candidate)?;
        Ok(Some(result))
    }
    pub fn bind_run(
        &mut self,
        workspace_id: &str,
        pane_id: &str,
        run_id: String,
        runtime: PaneRuntime,
    ) -> Result<(), String> {
        let p = self
            .workspace_mut(workspace_id)?
            .panes
            .get_mut(pane_id)
            .ok_or("pane not found")?;
        p.run_id = Some(run_id);
        p.runtime = runtime;
        // Runtime bindings are process-local authority and are never durable.
        Ok(())
    }
    pub fn ensure_bound_pane(
        &mut self,
        workspace_id: String,
        pane_id: String,
        run_id: String,
    ) -> Result<BindRollback, String> {
        let rollback = BindRollback {
            workspace_id: workspace_id.clone(),
            pane_id: pane_id.clone(),
            run_id: run_id.clone(),
            prior_workspace: self.workspaces.get(&workspace_id).cloned(),
        };
        let mut candidate = self.workspaces.clone();
        if !candidate.contains_key(&workspace_id) {
            validate_id(&workspace_id)?;
            validate_id(&pane_id)?;
            let pane = PaneLayout {
                pane_id: pane_id.clone(),
                name: "terminal".into(),
                run_id: None,
                runtime: PaneRuntime::Empty,
            };
            candidate.insert(
                workspace_id.clone(),
                WorkspaceLayout {
                    workspace_id: workspace_id.clone(),
                    name: workspace_id.clone(),
                    focused_pane_id: pane_id.clone(),
                    panes: [(pane_id.clone(), pane)].into(),
                    root: LayoutNode::Pane {
                        pane_id: pane_id.clone(),
                    },
                },
            );
        } else if !candidate[&workspace_id].panes.contains_key(&pane_id) {
            validate_id(&pane_id)?;
            let workspace = candidate.get_mut(&workspace_id).unwrap();
            if workspace.panes.len() >= MAX_PANES_PER_WORKSPACE {
                return Err("pane limit reached".into());
            }
            let focused = workspace.focused_pane_id.clone();
            replace_leaf(&mut workspace.root, &focused, &pane_id, SplitAxis::Vertical)?;
            workspace.panes.insert(
                pane_id.clone(),
                PaneLayout {
                    pane_id: pane_id.clone(),
                    name: "terminal".into(),
                    run_id: None,
                    runtime: PaneRuntime::Empty,
                },
            );
            workspace.focused_pane_id = pane_id.clone();
        }
        let pane = candidate
            .get_mut(&workspace_id)
            .and_then(|workspace| workspace.panes.get_mut(&pane_id))
            .ok_or("pane not found")?;
        pane.run_id = Some(run_id);
        pane.runtime = PaneRuntime::Running;
        self.commit(candidate)?;
        Ok(rollback)
    }

    pub fn rollback_bound_pane(&mut self, rollback: BindRollback) -> Result<(), String> {
        let Some(current_workspace) = self.workspaces.get(&rollback.workspace_id) else {
            return Ok(());
        };
        let Some(current_pane) = current_workspace.panes.get(&rollback.pane_id) else {
            return Ok(());
        };
        if current_pane.run_id.as_deref() != Some(&rollback.run_id) {
            return Err("dispatch layout binding changed during rollback".into());
        }

        let mut candidate = self.workspaces.clone();
        match rollback.prior_workspace {
            Some(prior) if prior.panes.contains_key(&rollback.pane_id) => {
                let old_pane = prior.panes[&rollback.pane_id].clone();
                let pane = self
                    .workspaces
                    .get_mut(&rollback.workspace_id)
                    .and_then(|workspace| workspace.panes.get_mut(&rollback.pane_id))
                    .ok_or("dispatch pane disappeared during rollback")?;
                pane.run_id = old_pane.run_id;
                pane.runtime = old_pane.runtime;
                // Bindings and runtime state are deliberately process-local, so the durable
                // topology already is the prior topology and must not be rewritten here.
                return Ok(());
            }
            Some(prior) => {
                let workspace = candidate
                    .get_mut(&rollback.workspace_id)
                    .ok_or("dispatch workspace disappeared during rollback")?;
                remove_leaf(&mut workspace.root, &rollback.pane_id)?;
                workspace.panes.remove(&rollback.pane_id);
                if workspace.focused_pane_id == rollback.pane_id {
                    workspace.focused_pane_id =
                        if workspace.panes.contains_key(&prior.focused_pane_id) {
                            prior.focused_pane_id
                        } else {
                            first_leaf(&workspace.root).into()
                        };
                }
            }
            None => {
                let workspace = candidate
                    .get_mut(&rollback.workspace_id)
                    .ok_or("dispatch workspace disappeared during rollback")?;
                if workspace.panes.len() == 1 {
                    candidate.remove(&rollback.workspace_id);
                } else {
                    remove_leaf(&mut workspace.root, &rollback.pane_id)?;
                    workspace.panes.remove(&rollback.pane_id);
                    if workspace.focused_pane_id == rollback.pane_id {
                        workspace.focused_pane_id = first_leaf(&workspace.root).into();
                    }
                }
            }
        }
        self.commit(candidate)
    }
    pub fn check_bind_capacity(&self, workspace_id: &str, pane_id: &str) -> Result<(), String> {
        match self.workspaces.get(workspace_id) {
            Some(workspace) => {
                if !workspace.panes.contains_key(pane_id)
                    && workspace.panes.len() >= MAX_PANES_PER_WORKSPACE
                {
                    return Err(format!(
                        "workspace pane capacity {MAX_PANES_PER_WORKSPACE} is in use; close a pane before dispatching another run"
                    ));
                }
            }
            None if self.workspaces.len() >= MAX_WORKSPACES => {
                return Err(format!(
                    "workspace capacity {MAX_WORKSPACES} is in use; close a workspace before dispatching into another repository"
                ));
            }
            None => {}
        }
        Ok(())
    }
    fn workspace_mut(&mut self, id: &str) -> Result<&mut WorkspaceLayout, String> {
        self.workspaces
            .get_mut(id)
            .ok_or_else(|| "workspace not found".into())
    }
    fn commit(&mut self, candidate: BTreeMap<String, WorkspaceLayout>) -> Result<(), String> {
        self.persist(&candidate)?;
        self.workspaces = candidate;
        Ok(())
    }
    fn persist(&self, workspaces: &BTreeMap<String, WorkspaceLayout>) -> Result<(), String> {
        #[cfg(test)]
        if self.fail_persistence {
            return Err("injected layout persistence failure".into());
        }
        let value = DurableLayout {
            schema_version: 1,
            workspaces: workspaces.values().map(DurableWorkspace::from).collect(),
        };
        validate_durable_workspaces(&value.workspaces)?;
        let bytes = serde_json::to_vec_pretty(&value).map_err(|e| e.to_string())?;
        let tmp = self
            .path
            .with_extension(format!("json.tmp-{}", std::process::id()));
        use std::{
            io::Write,
            os::unix::fs::{OpenOptionsExt, PermissionsExt},
        };
        let mut f = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&tmp)
            .map_err(|e| format!("could not create layout metadata: {e}"))?;
        let result = (|| -> std::io::Result<()> {
            f.set_permissions(fs::Permissions::from_mode(0o600))?;
            f.write_all(&bytes)?;
            f.sync_all()?;
            fs::rename(&tmp, &self.path)?;
            fs::File::open(self.path.parent().unwrap())?.sync_all()
        })();
        if result.is_err() {
            let _ = fs::remove_file(tmp);
        }
        result.map_err(|e| format!("could not persist layout metadata: {e}"))
    }
}

fn quarantine_invalid_layout(path: &Path) -> Result<(), String> {
    let state_dir = path
        .parent()
        .ok_or("layout metadata has no state directory")?;
    let quarantine = state_dir.join("layout-quarantine");
    let quarantine_directory = open_layout_quarantine(state_dir, &quarantine)?;
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|e| format!("system time is before Unix epoch: {e}"))?
        .as_nanos();
    let destination_name = CString::new(format!("layout-{nonce}.json")).unwrap();
    let source = CString::new(path.as_os_str().as_bytes())
        .map_err(|_| "layout metadata path contains a null byte")?;
    // SAFETY: both C strings are terminated, and quarantine_directory is an open directory.
    if unsafe {
        nix::libc::renameat(
            nix::libc::AT_FDCWD,
            source.as_ptr(),
            quarantine_directory.as_raw_fd(),
            destination_name.as_ptr(),
        )
    } != 0
    {
        return Err(format!(
            "could not quarantine invalid layout metadata: {}",
            std::io::Error::last_os_error()
        ));
    }
    #[cfg(test)]
    if let Some(hook) = QUARANTINE_AFTER_RENAME_HOOK.with(|hook| hook.borrow_mut().take()) {
        hook();
    }
    // Open relative to the verified directory so a path substitution cannot redirect this
    // inspection or chmod. O_NOFOLLOW also rejects a source swapped to a symlink before rename.
    let quarantined_fd = unsafe {
        nix::libc::openat(
            quarantine_directory.as_raw_fd(),
            destination_name.as_ptr(),
            nix::libc::O_RDONLY | nix::libc::O_CLOEXEC | nix::libc::O_NOFOLLOW,
        )
    };
    if quarantined_fd < 0 {
        let open_error = std::io::Error::last_os_error();
        cleanup_quarantined_entry(&quarantine_directory, &destination_name).map_err(|cleanup| {
            format!("could not safely open quarantined layout metadata: {open_error}; {cleanup}")
        })?;
        return Err(format!(
            "could not safely open quarantined layout metadata: {open_error}"
        ));
    }
    // SAFETY: openat returned a new owned descriptor.
    let quarantined = unsafe { fs::File::from_raw_fd(quarantined_fd) };
    let metadata = quarantined
        .metadata()
        .map_err(|e| format!("could not inspect quarantined layout metadata: {e}"))?;
    // A regular file alone is insufficient: a same-user attacker could replace the renamed
    // entry with a hardlink and make fchmod mutate another file. Only a singly-linked inode owned
    // by this process's effective user is safe to modify through the descriptor.
    // SAFETY: geteuid(2) has no preconditions and does not access memory.
    let effective_uid = unsafe { nix::libc::geteuid() };
    if !metadata.file_type().is_file() || metadata.uid() != effective_uid || metadata.nlink() != 1 {
        cleanup_quarantined_entry(&quarantine_directory, &destination_name)?;
        return Err(
            "refusing quarantined layout metadata without a regular, owned, single-link inode"
                .into(),
        );
    }
    // SAFETY: fchmod operates only on the verified, owned, single-link regular file descriptor.
    if unsafe { nix::libc::fchmod(quarantined.as_raw_fd(), 0o600) } != 0 {
        return Err(format!(
            "could not secure quarantined layout metadata: {}",
            std::io::Error::last_os_error()
        ));
    }
    fs::File::open(state_dir)
        .and_then(|directory| directory.sync_all())
        .and_then(|_| quarantine_directory.sync_all())
        .map_err(|e| format!("could not sync layout quarantine: {e}"))
}

fn cleanup_quarantined_entry(directory: &fs::File, name: &CString) -> Result<(), String> {
    let mut stat = std::mem::MaybeUninit::<nix::libc::stat>::uninit();
    // SAFETY: name is terminated, stat points to writable storage, and the lookup is relative to
    // the already-open quarantine directory. AT_SYMLINK_NOFOLLOW classifies the entry itself.
    if unsafe {
        nix::libc::fstatat(
            directory.as_raw_fd(),
            name.as_ptr(),
            stat.as_mut_ptr(),
            nix::libc::AT_SYMLINK_NOFOLLOW,
        )
    } != 0
    {
        return Err(format!(
            "could not inspect replacement quarantined layout metadata without following it: {}",
            std::io::Error::last_os_error()
        ));
    }
    // SAFETY: fstatat initialized stat after returning success.
    let stat = unsafe { stat.assume_init() };
    let flags = if stat.st_mode & nix::libc::S_IFMT == nix::libc::S_IFDIR {
        nix::libc::AT_REMOVEDIR
    } else {
        0
    };
    // This removes exactly one directory entry. AT_REMOVEDIR can remove only an empty directory;
    // cleanup never traverses or recursively deletes an untrusted replacement.
    if unsafe { nix::libc::unlinkat(directory.as_raw_fd(), name.as_ptr(), flags) } != 0 {
        return Err(format!(
            "could not remove replacement quarantined layout metadata: {}",
            std::io::Error::last_os_error()
        ));
    }
    Ok(())
}

fn open_layout_quarantine(state_dir: &Path, quarantine: &Path) -> Result<fs::File, String> {
    match fs::symlink_metadata(quarantine) {
        Ok(metadata) if metadata.file_type().is_symlink() => {
            // SAFETY: geteuid(2) has no preconditions and does not access memory.
            let effective_uid = unsafe { nix::libc::geteuid() };
            if metadata.uid() == effective_uid {
                fs::remove_file(quarantine).map_err(|e| {
                    format!("could not remove unsafe layout quarantine symlink: {e}")
                })?;
                fs::File::open(state_dir)
                    .and_then(|directory| directory.sync_all())
                    .map_err(|e| {
                        format!("could not sync removed layout quarantine symlink: {e}")
                    })?;
            }
            return Err("refusing symlinked layout quarantine".into());
        }
        Ok(metadata) if !metadata.file_type().is_dir() => {
            return Err("layout quarantine must be a directory".into());
        }
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            match fs::create_dir(quarantine) {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
                Err(error) => {
                    return Err(format!("could not create layout quarantine: {error}"));
                }
            }
        }
        Err(error) => return Err(format!("could not inspect layout quarantine: {error}")),
    }

    let directory = fs::OpenOptions::new()
        .read(true)
        .custom_flags(nix::libc::O_NOFOLLOW | nix::libc::O_DIRECTORY | nix::libc::O_CLOEXEC)
        .open(quarantine)
        .map_err(|e| format!("could not safely open layout quarantine: {e}"))?;
    let metadata = directory
        .metadata()
        .map_err(|e| format!("could not inspect open layout quarantine: {e}"))?;
    // SAFETY: geteuid(2) has no preconditions and does not access memory.
    let effective_uid = unsafe { nix::libc::geteuid() };
    if !metadata.file_type().is_dir() || metadata.uid() != effective_uid {
        return Err("refusing layout quarantine not owned by the current user".into());
    }
    // SAFETY: fchmod operates only on the verified, open directory.
    if unsafe { nix::libc::fchmod(directory.as_raw_fd(), 0o700) } != 0 {
        return Err(format!(
            "could not secure layout quarantine: {}",
            std::io::Error::last_os_error()
        ));
    }
    Ok(directory)
}

fn validate_id(v: &str) -> Result<(), String> {
    if v.is_empty()
        || v.len() > 96
        || !v
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-'))
    {
        Err("layout id must contain only letters, numbers, hyphens, or underscores".into())
    } else {
        Ok(())
    }
}
fn validate_name(v: &str) -> Result<(), String> {
    if v.trim().is_empty() || v.len() > 128 || v.contains(['\n', '\r', '\0']) {
        Err("layout name must be non-empty, single-line, and at most 128 bytes".into())
    } else {
        Ok(())
    }
}
impl DurableWorkspace {
    fn into_runtime(self) -> WorkspaceLayout {
        WorkspaceLayout {
            workspace_id: self.workspace_id,
            name: self.name,
            focused_pane_id: self.focused_pane_id,
            panes: self
                .panes
                .into_iter()
                .map(|(id, pane)| {
                    (
                        id,
                        PaneLayout {
                            pane_id: pane.pane_id,
                            name: pane.name,
                            run_id: None,
                            runtime: PaneRuntime::Restored,
                        },
                    )
                })
                .collect(),
            root: self.root,
        }
    }
}

impl From<&WorkspaceLayout> for DurableWorkspace {
    fn from(value: &WorkspaceLayout) -> Self {
        Self {
            workspace_id: value.workspace_id.clone(),
            name: value.name.clone(),
            focused_pane_id: value.focused_pane_id.clone(),
            panes: value
                .panes
                .iter()
                .map(|(id, pane)| {
                    (
                        id.clone(),
                        DurablePane {
                            pane_id: pane.pane_id.clone(),
                            name: pane.name.clone(),
                        },
                    )
                })
                .collect(),
            root: value.root.clone(),
        }
    }
}

fn validate_durable_workspaces(ws: &[DurableWorkspace]) -> Result<(), String> {
    if ws.len() > MAX_WORKSPACES {
        return Err("layout exceeds workspace limit".into());
    }
    let mut workspace_ids = std::collections::HashSet::new();
    let mut workspace_names = std::collections::HashSet::new();
    for w in ws {
        validate_id(&w.workspace_id)?;
        validate_name(&w.name)?;
        if !workspace_ids.insert(w.workspace_id.as_str()) {
            return Err("duplicate workspace id".into());
        }
        if !workspace_names.insert(w.name.as_str()) {
            return Err("duplicate workspace name".into());
        }
        if w.panes.is_empty()
            || w.panes.len() > MAX_PANES_PER_WORKSPACE
            || !w.panes.contains_key(&w.focused_pane_id)
        {
            return Err("invalid workspace pane set".into());
        }
        for (key, p) in &w.panes {
            validate_id(&p.pane_id)?;
            validate_name(&p.name)?;
            if key != &p.pane_id {
                return Err("pane map key does not match pane id".into());
            }
        }
        let mut leaves = std::collections::HashSet::new();
        validate_node(&w.root, &w.panes, &mut leaves)?;
        if leaves.len() != w.panes.len() {
            return Err("layout contains orphan panes".into());
        }
    }
    Ok(())
}

fn validate_node<'a>(
    node: &'a LayoutNode,
    panes: &'a BTreeMap<String, DurablePane>,
    leaves: &mut std::collections::HashSet<&'a str>,
) -> Result<(), String> {
    match node {
        LayoutNode::Pane { pane_id } => {
            if !panes.contains_key(pane_id) {
                return Err("layout references an unknown pane".into());
            }
            if !leaves.insert(pane_id) {
                return Err("pane has multiple parents".into());
            }
            Ok(())
        }
        LayoutNode::Split {
            ratio_milli,
            first,
            second,
            ..
        } => {
            if !(100..=900).contains(ratio_milli) {
                return Err("split ratio must be between 100 and 900".into());
            }
            let before = leaves.len();
            validate_node(first, panes, leaves)?;
            let after_first = leaves.len();
            validate_node(second, panes, leaves)?;
            if after_first == before || leaves.len() == after_first {
                return Err("split must reference two distinct non-empty children".into());
            }
            Ok(())
        }
    }
}
fn replace_leaf(n: &mut LayoutNode, t: &str, new: &str, a: SplitAxis) -> Result<(), String> {
    match n {
        LayoutNode::Pane { pane_id } if pane_id == t => {
            let old = pane_id.clone();
            *n = LayoutNode::Split {
                axis: a,
                ratio_milli: 500,
                first: Box::new(LayoutNode::Pane { pane_id: old }),
                second: Box::new(LayoutNode::Pane {
                    pane_id: new.into(),
                }),
            };
            Ok(())
        }
        LayoutNode::Pane { .. } => Err("pane not found in layout".into()),
        LayoutNode::Split { first, second, .. } => {
            replace_leaf(first, t, new, a).or_else(|_| replace_leaf(second, t, new, a))
        }
    }
}
fn resize_parent(n: &mut LayoutNode, t: &str, r: u16) -> Result<(), String> {
    match n {
        LayoutNode::Pane { .. } => Err("pane has no containing split".into()),
        LayoutNode::Split {
            ratio_milli,
            first,
            second,
            ..
        } => {
            if matches!(&**first, LayoutNode::Pane { pane_id } if pane_id == t)
                || matches!(&**second, LayoutNode::Pane { pane_id } if pane_id == t)
            {
                *ratio_milli = r;
                Ok(())
            } else {
                resize_parent(first, t, r).or_else(|_| resize_parent(second, t, r))
            }
        }
    }
}
fn remove_leaf(n: &mut LayoutNode, t: &str) -> Result<(), String> {
    match n {
        LayoutNode::Pane { .. } => Err("cannot remove root pane".into()),
        LayoutNode::Split { first, second, .. } => {
            if matches!(&**first,LayoutNode::Pane{pane_id}if pane_id==t) {
                *n = (**second).clone();
                Ok(())
            } else if matches!(&**second,LayoutNode::Pane{pane_id}if pane_id==t) {
                *n = (**first).clone();
                Ok(())
            } else {
                remove_leaf(first, t).or_else(|_| remove_leaf(second, t))
            }
        }
    }
}
fn first_leaf(n: &LayoutNode) -> &str {
    match n {
        LayoutNode::Pane { pane_id } => pane_id,
        LayoutNode::Split { first, .. } => first_leaf(first),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::{PermissionsExt, symlink};
    fn directory(label: &str) -> PathBuf {
        let p = std::env::current_dir()
            .unwrap()
            .join("target")
            .join(format!("layout-{label}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&p);
        fs::create_dir_all(&p).unwrap();
        fs::set_permissions(&p, fs::Permissions::from_mode(0o700)).unwrap();
        p
    }
    #[test]
    fn dynamic_split_focus_resize_rename_and_close_are_deterministic() {
        let dir = directory("operations");
        let mut layout = LayoutRegistry::load(&dir).unwrap();
        layout
            .create_workspace("work_1".into(), "daily".into(), "pane_1".into())
            .unwrap();
        layout
            .split("work_1", "pane_1", "pane_2".into(), SplitAxis::Vertical)
            .unwrap();
        layout.resize("work_1", "pane_2", 700).unwrap();
        layout.focus("work_1", "pane_1").unwrap();
        layout
            .rename("work_1", Some("pane_1"), "editor".into())
            .unwrap();
        let workspace = &layout.snapshot().workspaces[0];
        assert_eq!(workspace.panes.len(), 2);
        assert_eq!(workspace.focused_pane_id, "pane_1");
        assert_eq!(workspace.panes["pane_1"].name, "editor");
        assert!(layout.close("work_1", "pane_2").unwrap().is_some());
        assert!(layout.close("work_1", "pane_1").unwrap().is_none());
    }
    #[test]
    fn nested_resize_changes_only_the_immediate_containing_split() {
        let dir = directory("nested-resize");
        let mut layout = LayoutRegistry::load(&dir).unwrap();
        layout
            .create_workspace("w".into(), "nested".into(), "p1".into())
            .unwrap();
        layout
            .split("w", "p1", "p2".into(), SplitAxis::Vertical)
            .unwrap();
        layout
            .split("w", "p2", "p3".into(), SplitAxis::Horizontal)
            .unwrap();
        layout.resize("w", "p3", 700).unwrap();
        let workspace = &layout.snapshot().workspaces[0];
        let LayoutNode::Split {
            ratio_milli: outer,
            second,
            ..
        } = &workspace.root
        else {
            panic!("expected outer split")
        };
        let LayoutNode::Split {
            ratio_milli: inner, ..
        } = &**second
        else {
            panic!("expected nested split")
        };
        assert_eq!((*outer, *inner), (500, 700));
        assert_eq!(
            layout.resize("w", "missing", 600),
            Err("pane not found".into())
        );
    }
    #[test]
    fn restart_restores_only_bounded_layout_metadata() {
        let dir = directory("restart");
        {
            let mut layout = LayoutRegistry::load(&dir).unwrap();
            layout
                .create_workspace("work_1".into(), "daily".into(), "pane_1".into())
                .unwrap();
            layout
                .bind_run(
                    "work_1",
                    "pane_1",
                    "dock_private".into(),
                    PaneRuntime::Running,
                )
                .unwrap();
            let fresh = layout.snapshot();
            assert_eq!(
                fresh.workspaces[0].panes["pane_1"].runtime,
                PaneRuntime::Running
            );
        }
        let bytes = fs::read(dir.join("layout.json")).unwrap();
        let text = String::from_utf8(bytes).unwrap();
        assert!(!text.contains("command"));
        assert!(!text.contains("scrollback"));
        assert!(!text.contains("process_group"));
        assert!(!text.contains("/Users/"));
        let mut registry = LayoutRegistry::load(&dir).unwrap();
        let restored = registry.snapshot();
        let pane = &restored.workspaces[0].panes["pane_1"];
        assert_eq!(pane.runtime, PaneRuntime::Restored);
        assert_eq!(pane.run_id, None);
        registry.focus("work_1", "pane_1").unwrap();
        registry
            .rename("work_1", Some("pane_1"), "reopen".into())
            .unwrap();
        assert!(registry.close("work_1", "pane_1").unwrap().is_none());
    }
    #[test]
    fn quarantines_invalid_durable_layout_and_starts_empty() {
        let dir = directory("strict");
        fs::write(
            dir.join("layout.json"),
            br#"{"schema_version":1,"workspaces":[],"raw_transcript":"no"}"#,
        )
        .unwrap();
        let layout = LayoutRegistry::load(&dir).unwrap();
        assert!(layout.snapshot().workspaces.is_empty());
        assert!(!dir.join("layout.json").exists());
        let quarantine = dir.join("layout-quarantine");
        let records: Vec<_> = fs::read_dir(&quarantine).unwrap().collect();
        assert_eq!(records.len(), 1);
        assert_eq!(
            fs::metadata(&quarantine).unwrap().permissions().mode() & 0o777,
            0o700
        );
        assert_eq!(
            records[0]
                .as_ref()
                .unwrap()
                .metadata()
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );

        let dir = directory("unsupported");
        fs::write(
            dir.join("layout.json"),
            br#"{"schema_version":2,"workspaces":[]}"#,
        )
        .unwrap();
        assert!(
            LayoutRegistry::load(&dir)
                .unwrap()
                .snapshot()
                .workspaces
                .is_empty()
        );
        assert_eq!(
            fs::read_dir(dir.join("layout-quarantine")).unwrap().count(),
            1
        );

        let dir = directory("names");
        let mut layout = LayoutRegistry::load(&dir).unwrap();
        assert!(
            layout
                .create_workspace("work_1".into(), "x".repeat(129), "pane_1".into())
                .is_err()
        );
    }

    #[test]
    fn symlinked_layout_is_removed_without_touching_its_target() {
        let dir = directory("symlink");
        let target = dir.join("layout-target.json");
        let content = b"target must remain unchanged\n";
        fs::write(&target, content).unwrap();
        fs::set_permissions(&target, fs::Permissions::from_mode(0o640)).unwrap();
        symlink(&target, dir.join("layout.json")).unwrap();

        let layout = LayoutRegistry::load(&dir).unwrap();

        assert!(layout.snapshot().workspaces.is_empty());
        assert!(fs::symlink_metadata(dir.join("layout.json")).is_err());
        assert_eq!(fs::read(&target).unwrap(), content);
        assert_eq!(
            fs::metadata(&target).unwrap().permissions().mode() & 0o777,
            0o640
        );
        assert!(!dir.join("layout-quarantine").exists());
    }

    #[test]
    fn symlinked_layout_quarantine_is_removed_without_touching_its_target() {
        let dir = directory("quarantine-symlink");
        fs::write(dir.join("layout.json"), b"{invalid json\n").unwrap();
        let target = dir.join("quarantine-target");
        fs::create_dir(&target).unwrap();
        fs::set_permissions(&target, fs::Permissions::from_mode(0o750)).unwrap();
        let sentinel = target.join("sentinel");
        let content = b"target must remain unchanged\n";
        fs::write(&sentinel, content).unwrap();
        fs::set_permissions(&sentinel, fs::Permissions::from_mode(0o640)).unwrap();
        symlink(&target, dir.join("layout-quarantine")).unwrap();

        assert!(matches!(
            LayoutRegistry::load(&dir),
            Err(message) if message == "refusing symlinked layout quarantine"
        ));

        assert!(fs::symlink_metadata(dir.join("layout-quarantine")).is_err());
        assert_eq!(fs::read(&sentinel).unwrap(), content);
        assert_eq!(
            fs::metadata(&target).unwrap().permissions().mode() & 0o777,
            0o750
        );
        assert_eq!(
            fs::metadata(&sentinel).unwrap().permissions().mode() & 0o777,
            0o640
        );
    }

    #[test]
    fn substituted_quarantine_directory_is_removed_without_recursion_or_orphan() {
        let dir = directory("quarantine-substituted-directory");
        fs::write(dir.join("layout.json"), b"{invalid json\n").unwrap();
        let quarantine = dir.join("layout-quarantine");
        QUARANTINE_AFTER_RENAME_HOOK.with(|hook| {
            *hook.borrow_mut() = Some(Box::new({
                let quarantine = quarantine.clone();
                move || {
                    let replacement = fs::read_dir(&quarantine)
                        .unwrap()
                        .next()
                        .unwrap()
                        .unwrap()
                        .path();
                    fs::remove_file(&replacement).unwrap();
                    fs::create_dir(&replacement).unwrap();
                }
            }))
        });

        assert!(matches!(
            LayoutRegistry::load(&dir),
            Err(message) if message.contains("quarantined layout metadata")
        ));
        assert_eq!(fs::read_dir(&quarantine).unwrap().count(), 0);
        assert!(!dir.join("layout.json").exists());
    }

    #[test]
    fn substituted_quarantine_hardlink_is_unlinked_without_touching_its_target() {
        let dir = directory("quarantine-substituted-hardlink");
        fs::write(dir.join("layout.json"), b"{invalid json\n").unwrap();
        let quarantine = dir.join("layout-quarantine");
        let target = dir.join("hardlink-target");
        let content = b"target must remain unchanged\n";
        fs::write(&target, content).unwrap();
        fs::set_permissions(&target, fs::Permissions::from_mode(0o640)).unwrap();
        QUARANTINE_AFTER_RENAME_HOOK.with(|hook| {
            *hook.borrow_mut() = Some(Box::new({
                let quarantine = quarantine.clone();
                let target = target.clone();
                move || {
                    let replacement = fs::read_dir(&quarantine)
                        .unwrap()
                        .next()
                        .unwrap()
                        .unwrap()
                        .path();
                    fs::remove_file(&replacement).unwrap();
                    fs::hard_link(&target, &replacement).unwrap();
                }
            }))
        });

        assert!(matches!(
            LayoutRegistry::load(&dir),
            Err(message) if message.contains("single-link inode")
        ));
        assert_eq!(fs::read_dir(&quarantine).unwrap().count(), 0);
        assert_eq!(fs::read(&target).unwrap(), content);
        assert_eq!(
            fs::metadata(&target).unwrap().permissions().mode() & 0o777,
            0o640
        );
        assert_eq!(fs::metadata(&target).unwrap().nlink(), 1);
        assert!(!dir.join("layout.json").exists());
    }

    #[test]
    fn non_directory_layout_quarantine_is_rejected_without_mutation() {
        let dir = directory("quarantine-file");
        fs::write(dir.join("layout.json"), b"{invalid json\n").unwrap();
        let quarantine = dir.join("layout-quarantine");
        let content = b"not a directory\n";
        fs::write(&quarantine, content).unwrap();
        fs::set_permissions(&quarantine, fs::Permissions::from_mode(0o640)).unwrap();

        assert!(matches!(
            LayoutRegistry::load(&dir),
            Err(message) if message == "layout quarantine must be a directory"
        ));

        assert_eq!(fs::read(&quarantine).unwrap(), content);
        assert_eq!(
            fs::metadata(&quarantine).unwrap().permissions().mode() & 0o777,
            0o640
        );
    }

    #[test]
    fn durable_layout_bytes_never_contain_runtime_authority_or_process_data() {
        let dir = directory("privacy");
        let mut layout = LayoutRegistry::load(&dir).unwrap();
        layout
            .create_workspace("work_1".into(), "daily".into(), "pane_1".into())
            .unwrap();
        layout
            .bind_run(
                "work_1",
                "pane_1",
                "dock_SECRET_RUN_ID".into(),
                PaneRuntime::Running,
            )
            .unwrap();
        let bytes = fs::read(dir.join("layout.json")).unwrap();
        for secret in [
            b"dock_SECRET_RUN_ID".as_slice(),
            b"run_id".as_slice(),
            b"runtime".as_slice(),
            b"pid".as_slice(),
            b"process_group".as_slice(),
            b"/private/secret/worktree".as_slice(),
            b"raw transcript output".as_slice(),
        ] {
            assert!(!bytes.windows(secret.len()).any(|window| window == secret));
        }
        let reloaded = LayoutRegistry::load(&dir).unwrap().snapshot();
        let pane = &reloaded.workspaces[0].panes["pane_1"];
        assert_eq!(pane.run_id, None);
        assert_eq!(pane.runtime, PaneRuntime::Restored);
    }

    #[test]
    fn rejects_and_quarantines_invalid_topologies() {
        let invalid = [
            (
                "duplicate-workspace-id",
                r#"{"schema_version":1,"workspaces":[{"workspace_id":"w","name":"one","focused_pane_id":"p","panes":{"p":{"pane_id":"p","name":"p"}},"root":{"kind":"pane","pane_id":"p"}},{"workspace_id":"w","name":"two","focused_pane_id":"q","panes":{"q":{"pane_id":"q","name":"q"}},"root":{"kind":"pane","pane_id":"q"}}]}"#,
            ),
            (
                "duplicate-workspace-name",
                r#"{"schema_version":1,"workspaces":[{"workspace_id":"w1","name":"same","focused_pane_id":"p","panes":{"p":{"pane_id":"p","name":"p"}},"root":{"kind":"pane","pane_id":"p"}},{"workspace_id":"w2","name":"same","focused_pane_id":"q","panes":{"q":{"pane_id":"q","name":"q"}},"root":{"kind":"pane","pane_id":"q"}}]}"#,
            ),
            (
                "pane-map-mismatch",
                r#"{"schema_version":1,"workspaces":[{"workspace_id":"w","name":"w","focused_pane_id":"p","panes":{"p":{"pane_id":"q","name":"p"}},"root":{"kind":"pane","pane_id":"p"}}]}"#,
            ),
            (
                "bad-focus",
                r#"{"schema_version":1,"workspaces":[{"workspace_id":"w","name":"w","focused_pane_id":"missing","panes":{"p":{"pane_id":"p","name":"p"}},"root":{"kind":"pane","pane_id":"p"}}]}"#,
            ),
            (
                "unknown-leaf",
                r#"{"schema_version":1,"workspaces":[{"workspace_id":"w","name":"w","focused_pane_id":"p","panes":{"p":{"pane_id":"p","name":"p"}},"root":{"kind":"pane","pane_id":"q"}}]}"#,
            ),
            (
                "orphan",
                r#"{"schema_version":1,"workspaces":[{"workspace_id":"w","name":"w","focused_pane_id":"p","panes":{"p":{"pane_id":"p","name":"p"},"q":{"pane_id":"q","name":"q"}},"root":{"kind":"pane","pane_id":"p"}}]}"#,
            ),
            (
                "multiple-parent",
                r#"{"schema_version":1,"workspaces":[{"workspace_id":"w","name":"w","focused_pane_id":"p","panes":{"p":{"pane_id":"p","name":"p"}},"root":{"kind":"split","axis":"vertical","ratio_milli":500,"first":{"kind":"pane","pane_id":"p"},"second":{"kind":"pane","pane_id":"p"}}}]}"#,
            ),
            (
                "bad-ratio",
                r#"{"schema_version":1,"workspaces":[{"workspace_id":"w","name":"w","focused_pane_id":"p","panes":{"p":{"pane_id":"p","name":"p"},"q":{"pane_id":"q","name":"q"}},"root":{"kind":"split","axis":"vertical","ratio_milli":99,"first":{"kind":"pane","pane_id":"p"},"second":{"kind":"pane","pane_id":"q"}}}]}"#,
            ),
        ];
        for (label, json) in invalid {
            let dir = directory(label);
            fs::write(dir.join("layout.json"), json).unwrap();
            assert!(
                LayoutRegistry::load(&dir)
                    .unwrap()
                    .snapshot()
                    .workspaces
                    .is_empty()
            );
            assert_eq!(
                fs::read_dir(dir.join("layout-quarantine")).unwrap().count(),
                1
            );
        }
    }

    #[test]
    fn persistence_failure_rolls_back_every_durable_mutation() {
        let dir = directory("transactional");
        let mut layout = LayoutRegistry::load(&dir).unwrap();
        layout
            .create_workspace("w".into(), "workspace".into(), "p".into())
            .unwrap();
        layout
            .split("w", "p", "q".into(), SplitAxis::Vertical)
            .unwrap();
        let baseline = layout.snapshot();
        let durable = fs::read(dir.join("layout.json")).unwrap();
        layout.fail_persistence = true;
        assert!(layout.focus("w", "p").is_err());
        assert!(layout.rename("w", None, "changed".into()).is_err());
        assert!(layout.resize("w", "p", 600).is_err());
        assert!(
            layout
                .split("w", "p", "r".into(), SplitAxis::Vertical)
                .is_err()
        );
        assert!(layout.close("w", "p").is_err());
        assert!(
            layout
                .create_workspace("w2".into(), "other".into(), "p2".into())
                .is_err()
        );
        assert_eq!(layout.snapshot(), baseline);
        assert_eq!(fs::read(dir.join("layout.json")).unwrap(), durable);
    }
}
