//! Task environment identity, ownership and lifecycle state.

use std::fs::File;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use serde::{Deserialize, Serialize};

use crate::error::{FlowError, Result};
use crate::util;

use super::boundary::{validate_delivery_targets, DeliveryTarget};
use super::registry;

/// Lifecycle state of a task environment.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EnvironmentState {
    /// Registered, directories not yet fully materialised.
    Creating,
    /// Fully created and usable.
    Active,
    /// Cleanup plan written; resources are being released.
    Destroying,
    /// Release failed and the slot stays occupied until a retry succeeds.
    DestroyFailed,
    /// Everything was released and the slot is gone.
    Destroyed,
}

impl EnvironmentState {
    /// False once the environment is no longer usable for new work.
    pub fn is_live(self) -> bool {
        matches!(
            self,
            EnvironmentState::Creating | EnvironmentState::Active | EnvironmentState::DestroyFailed
        )
    }
}

/// A registered child process, identified by pid and start time so a recycled
/// pid cannot be mistaken for the original process.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RegisteredProcess {
    pub pid: i32,
    /// Kernel start time in clock ticks from `/proc/<pid>/stat` when known.
    pub start_time: Option<u64>,
}

/// A live task environment with an explicit disposable boundary.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskEnvironment {
    pub task_id: String,
    /// Root that holds all disposable task work (resolved, real path).
    pub workspace_root: PathBuf,
    /// Root for temporary files and caches (resolved, real path).
    pub temp_root: PathBuf,
    /// Formal delivery targets, always outside the disposable roots.
    pub delivery_targets: Vec<DeliveryTarget>,
    pub state: EnvironmentState,
    pub created_at: f64,
    pub updated_at: f64,
    /// Git worktrees created inside the boundary that need removal.
    pub worktree_registrations: Vec<PathBuf>,
    /// Child processes that must be terminated before directories go away.
    pub child_processes: Vec<RegisteredProcess>,
    /// Paths of files kept open by the environment owner.
    pub open_files: Vec<PathBuf>,
    /// State document location.
    pub state_file: PathBuf,
    /// Open descriptors kept alive for the lifetime of this handle only.
    #[serde(skip, default = "new_handle_slot")]
    file_handles: std::sync::Arc<Mutex<Vec<File>>>,
}

fn new_handle_slot() -> std::sync::Arc<Mutex<Vec<File>>> {
    std::sync::Arc::new(Mutex::new(Vec::new()))
}

impl TaskEnvironment {
    pub fn state_file(&self) -> &Path {
        &self.state_file
    }

    /// Persist the current document atomically.
    pub fn persist(&self) -> Result<()> {
        let value = serde_json::to_value(self)?;
        util::atomic_write_json(&self.state_file, &value)
    }

    fn transition(&mut self, state: EnvironmentState) -> Result<()> {
        self.state = state;
        self.updated_at = util::now();
        self.persist()
    }

    pub fn mark_active(&mut self) -> Result<()> {
        self.transition(EnvironmentState::Active)
    }

    pub fn mark_destroying(&mut self) -> Result<()> {
        self.transition(EnvironmentState::Destroying)
    }

    pub fn mark_destroy_failed(&mut self) -> Result<()> {
        self.transition(EnvironmentState::DestroyFailed)
    }

    pub fn mark_destroyed(&mut self) -> Result<()> {
        self.transition(EnvironmentState::Destroyed)
    }

    /// Register a worktree that lives inside the disposable boundary.
    pub fn register_worktree(&mut self, path: &Path) -> Result<()> {
        let resolved = util::resolve_path(path);
        if !util::is_relative_to(&resolved, &self.workspace_root) {
            return Err(FlowError::progress(format!(
                "worktree {} is outside the disposable workspace {}",
                resolved.display(),
                self.workspace_root.display()
            )));
        }
        if !self.worktree_registrations.contains(&resolved) {
            self.worktree_registrations.push(resolved);
            self.persist()?;
        }
        Ok(())
    }

    /// Register a child process. Non-positive pids are rejected because
    /// `kill(-1, ...)` would signal unrelated processes.
    pub fn register_process(&mut self, pid: i32) -> Result<()> {
        self.register_process_with_start(pid, None)
    }

    /// Register a child process together with its kernel start time.
    pub fn register_process_with_start(&mut self, pid: i32, start_time: Option<u64>) -> Result<()> {
        if pid <= 1 {
            return Err(FlowError::progress(format!(
                "invalid process id {pid}: must be greater than 1"
            )));
        }
        let start_time = start_time.or_else(|| process_start_time(pid));
        let entry = RegisteredProcess { pid, start_time };
        if !self.child_processes.contains(&entry) {
            self.child_processes.push(entry);
            self.persist()?;
        }
        Ok(())
    }

    /// Take ownership of an already-open descriptor.
    ///
    /// The descriptor is closed when the environment is destroyed, so a file
    /// the task opened cannot keep its storage alive past cleanup. Only the
    /// owner of a descriptor can close it: a handle kept by another process or
    /// by the caller itself is outside this contract.
    pub fn register_handle(&mut self, path: &Path, handle: File) -> Result<()> {
        let resolved = util::resolve_path(path);
        if self.open_files.contains(&resolved) {
            return Ok(());
        }
        self.file_handles
            .lock()
            .expect("file handle slot poisoned")
            .push(handle);
        self.open_files.push(resolved);
        self.persist()
    }

    /// Open `path` and take ownership of the descriptor.
    pub fn register_file(&mut self, path: &Path) -> Result<()> {
        let resolved = util::resolve_path(path);
        if self.open_files.contains(&resolved) {
            return Ok(());
        }
        let handle = File::open(&resolved).map_err(|error| {
            FlowError::progress(format!(
                "cannot open {} for registration: {error}",
                resolved.display()
            ))
        })?;
        self.register_handle(&resolved, handle)
    }

    pub fn unregister_worktree(&mut self, path: &Path) -> Result<()> {
        let resolved = util::resolve_path(path);
        self.worktree_registrations.retain(|item| item != &resolved);
        self.persist()
    }

    pub fn unregister_process(&mut self, pid: i32) -> Result<()> {
        self.child_processes.retain(|entry| entry.pid != pid);
        self.persist()
    }

    pub fn unregister_file(&mut self, path: &Path) -> Result<()> {
        let resolved = util::resolve_path(path);
        if let Some(position) = self.open_files.iter().position(|item| item == &resolved) {
            self.open_files.remove(position);
            let mut handles = self.file_handles.lock().expect("file handle slot poisoned");
            if position < handles.len() {
                handles.remove(position);
            }
        }
        self.persist()
    }

    /// Drop every descriptor this handle still owns.
    pub fn release_file_handles(&self) {
        self.file_handles
            .lock()
            .expect("file handle slot poisoned")
            .clear();
    }

    /// Number of descriptors this handle still keeps open.
    pub fn open_handle_count(&self) -> usize {
        self.file_handles
            .lock()
            .expect("file handle slot poisoned")
            .len()
    }

    /// True when `path` lies inside the disposable boundary.
    pub fn owns_path(&self, path: &Path) -> bool {
        let resolved = util::resolve_path(path);
        util::is_relative_to(&resolved, &self.workspace_root)
            || util::is_relative_to(&resolved, &self.temp_root)
    }
}

/// Read the kernel start time of a process, if it is visible.
pub fn process_start_time(pid: i32) -> Option<u64> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    // The comm field may contain spaces and parentheses, so parse after the
    // last ')' and take starttime (field 22, index 19 after the split).
    let rest = stat.rsplit_once(')')?.1;
    rest.split_whitespace().nth(19)?.parse().ok()
}

/// Kernel state letter of a process from `/proc/<pid>/stat`, if visible.
fn process_state(pid: i32) -> Option<char> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let rest = stat.rsplit_once(')')?.1;
    rest.split_whitespace().next()?.chars().next()
}

/// Whether a registered process is still the same live (non-zombie) process.
///
/// A zombie still has `/proc/<pid>`, but it has already released the resources
/// the environment cares about, so it counts as exited.
pub fn process_is_alive(entry: &RegisteredProcess) -> bool {
    if entry.pid <= 1 {
        return false;
    }
    if matches!(process_state(entry.pid), Some('Z') | None) {
        return false;
    }
    let Some(current) = process_start_time(entry.pid) else {
        return false;
    };
    match entry.start_time {
        Some(expected) => expected == current,
        None => true,
    }
}

/// Create a new task environment.
///
/// Ordering matters for crash recovery: the resource slot and the state
/// document exist before any directory is created, so an interrupted create is
/// observable and its partial directories are reclaimed by destroy.
pub fn create(
    task_id: &str,
    workspace_root: &Path,
    delivery_targets: Vec<DeliveryTarget>,
    temp_root: &Path,
) -> Result<TaskEnvironment> {
    registry::validate_task_id(task_id)?;

    let workspace_root = util::resolve_path(workspace_root);
    let temp_root = util::resolve_path(temp_root);
    let delivery_targets =
        validate_delivery_targets(delivery_targets, &workspace_root, &temp_root)?;

    if util::overlap(&workspace_root, &temp_root) {
        return Err(FlowError::progress(format!(
            "workspace {} and temp root {} overlap",
            workspace_root.display(),
            temp_root.display()
        )));
    }

    // Refuse to take ownership of an existing non-empty directory: the
    // environment may only ever destroy what it created itself.
    require_vacant(&workspace_root)?;
    require_vacant(&temp_root)?;
    validate_ancestors(&workspace_root)?;
    validate_ancestors(&temp_root)?;

    let state_file = registry::environment_state_path(task_id)?;
    if state_file.exists() {
        return Err(FlowError::progress(format!(
            "environment for task {task_id} already exists"
        )));
    }

    let now = util::now();
    let mut env = TaskEnvironment {
        task_id: task_id.to_string(),
        workspace_root: workspace_root.clone(),
        temp_root: temp_root.clone(),
        delivery_targets,
        state: EnvironmentState::Creating,
        created_at: now,
        updated_at: now,
        worktree_registrations: Vec::new(),
        child_processes: Vec::new(),
        open_files: Vec::new(),
        state_file: state_file.clone(),
        file_handles: new_handle_slot(),
    };

    // Reserve the slot and write the state before touching the filesystem.
    registry::register_environment(task_id, &state_file)?;
    if let Err(error) = env.persist() {
        let _ = registry::unregister_environment(task_id);
        return Err(error);
    }

    if let Err(error) = materialize(&workspace_root, &temp_root) {
        // Reclaim whatever this attempt created, then drop the slot.
        let _ = remove_created(&workspace_root);
        let _ = remove_created(&temp_root);
        let _ = std::fs::remove_file(&state_file);
        let _ = registry::unregister_environment(task_id);
        return Err(error);
    }

    env.mark_active()?;
    Ok(env)
}

/// Load a live environment document, initialising transient fields.
pub fn restore(task_id: &str) -> Result<TaskEnvironment> {
    registry::validate_task_id(task_id)?;
    let state_file = registry::environment_state_path(task_id)?;
    if !state_file.exists() {
        return Err(FlowError::progress(format!(
            "environment for task {task_id} does not exist"
        )));
    }
    let text = std::fs::read_to_string(&state_file)?;
    let mut env: TaskEnvironment = serde_json::from_str(&text)?;
    env.state_file = state_file;
    env.file_handles = new_handle_slot();
    Ok(env)
}

/// Reject paths that already hold data owned by someone else.
fn require_vacant(path: &Path) -> Result<()> {
    match std::fs::symlink_metadata(path) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error.into()),
        Ok(metadata) => {
            if metadata.file_type().is_symlink() {
                return Err(FlowError::progress(format!(
                    "refusing to claim symlink {}",
                    path.display()
                )));
            }
            if !metadata.is_dir() {
                return Err(FlowError::progress(format!(
                    "refusing to claim non-directory {}",
                    path.display()
                )));
            }
            let mut entries = std::fs::read_dir(path)?;
            if entries.next().is_some() {
                return Err(FlowError::progress(format!(
                    "refusing to claim non-empty directory {}",
                    path.display()
                )));
            }
            Ok(())
        }
    }
}

/// Create the disposable roots. Missing ancestors are created private.
fn materialize(workspace_root: &Path, temp_root: &Path) -> Result<()> {
    for root in [workspace_root, temp_root] {
        if root.exists() {
            continue;
        }
        std::fs::create_dir_all(root).map_err(|error| {
            FlowError::progress(format!("cannot create {}: {error}", root.display()))
        })?;
    }
    Ok(())
}

fn remove_created(path: &Path) -> std::io::Result<()> {
    match std::fs::remove_dir(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
}

/// Largest acceptable ancestor mode: owner-writable, not group/world writable.
fn ancestor_is_trusted(metadata: &std::fs::Metadata) -> bool {
    use std::os::unix::fs::MetadataExt as _;
    // SAFETY: geteuid takes no pointers and has no caller preconditions.
    let uid = unsafe { libc::geteuid() };
    let owner_ok = metadata.uid() == uid || metadata.uid() == 0;
    // A root-owned sticky directory such as /tmp is writable but cannot have
    // its entries replaced, which is the property that matters here.
    let root_sticky = metadata.uid() == 0 && metadata.mode() & libc::S_ISVTX != 0;
    owner_ok && (metadata.mode() & 0o022 == 0 || root_sticky)
}

/// Verify that every ancestor of `path` is trusted and not a symlink.
pub fn validate_ancestors(path: &Path) -> Result<()> {
    let mut cursor = path.to_path_buf();
    while let Some(parent) = cursor.parent() {
        if !parent.as_os_str().is_empty() && parent.exists() {
            let metadata = std::fs::symlink_metadata(parent)?;
            if metadata.file_type().is_symlink() {
                return Err(FlowError::guard(format!(
                    "ancestor {} is a symlink",
                    parent.display()
                )));
            }
            if !ancestor_is_trusted(&metadata) {
                return Err(FlowError::guard(format!(
                    "ancestor {} is group/world writable or owned by another user",
                    parent.display()
                )));
            }
        }
        cursor = parent.to_path_buf();
    }
    Ok(())
}
