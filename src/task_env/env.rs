//! Task environment identity, ownership and lifecycle state.
//!
//! Ownership rests on three durable facts, not on paths alone:
//!
//! * an `instance_id` that names one concrete environment, so a stale handle
//!   can never act on a newer environment that reused the same task id;
//! * the `(device, inode)` identity of each disposable root, so a path that was
//!   swapped for a symlink or another directory is rejected instead of removed;
//! * a per-task exclusive lock held for the lifetime of the handle, so two
//!   handles cannot own the same environment and release each other's slot.

use std::ffi::OsStr;
use std::fs::{File, OpenOptions};
use std::os::fd::AsRawFd;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Mutex};

use serde::{Deserialize, Serialize};

use crate::error::{FlowError, Result};
use crate::util;

use super::boundary::DeliveryTarget;
use super::registry;

/// Environment variable that marks every process the task starts.
///
/// The value is the environment's `instance_id`. It is inherited across
/// `fork`/`exec`, survives `setsid()`, and remains readable at
/// `/proc/<pid>/environ` after the process is orphaned. Cleanup therefore has
/// positive evidence of ownership instead of guessing from process groups.
pub const MARKER_ENV: &str = "HERDR_TASK_ENV_INSTANCE";

/// Lifecycle state of a task environment.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EnvironmentState {
    Creating,
    Active,
    Destroying,
    DestroyFailed,
    Destroyed,
}

impl EnvironmentState {
    pub fn is_live(self) -> bool {
        matches!(
            self,
            EnvironmentState::Creating | EnvironmentState::Active | EnvironmentState::DestroyFailed
        )
    }
}

/// Stable identity of a directory, from `stat(2)`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct RootIdentity {
    pub dev: u64,
    pub ino: u64,
}

impl RootIdentity {
    pub fn of(metadata: &std::fs::Metadata) -> Self {
        Self {
            dev: metadata.dev(),
            ino: metadata.ino(),
        }
    }
}

/// A worktree owned by the environment, plus the repository it belongs to.
///
/// The repository is captured at registration time (from `.git`), so cleanup
/// can remove the administrative record even if the working tree is already
/// gone. `.git` is only ever used to locate the repository, never as a path to
/// delete.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorktreeRegistration {
    pub path: PathBuf,
    pub repo: Option<PathBuf>,
}

/// A registered child process.
///
/// `pgid` must equal `pid`: the environment only takes ownership of processes
/// that lead their own process group, which is what makes descendant cleanup
/// airtight (a late child inherits the group). `start_time` guards against pid
/// reuse after the original process exits.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RegisteredProcess {
    pub pid: i32,
    pub pgid: i32,
    pub start_time: Option<u64>,
    /// Descendants visible at registration. Captured while the process is
    /// alive so a later orphan that leaves the process group or session can
    /// still be attributed to this environment.
    #[serde(default)]
    pub descendants: Vec<ProcessRecord>,
    /// Direct children of the supervisor at registration. Any *new* direct
    /// child later is an orphan reparented to the supervisor (which registers
    /// itself as a child subreaper) and belongs to this environment.
    #[serde(default)]
    pub direct_children: Vec<ProcessRecord>,
}

/// A process identity that survives pid reuse only while the start time
/// matches.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProcessRecord {
    pub pid: i32,
    pub start_time: Option<u64>,
}

/// A live task environment with an explicit disposable boundary.
#[derive(Debug, Serialize, Deserialize)]
pub struct TaskEnvironment {
    pub task_id: String,
    pub instance_id: String,
    pub workspace_root: PathBuf,
    pub temp_root: PathBuf,
    pub workspace_identity: Option<RootIdentity>,
    pub temp_identity: Option<RootIdentity>,
    pub delivery_targets: Vec<DeliveryTarget>,
    pub state: EnvironmentState,
    pub created_at: f64,
    pub updated_at: f64,
    pub worktree_registrations: Vec<WorktreeRegistration>,
    pub child_processes: Vec<RegisteredProcess>,
    pub open_files: Vec<PathBuf>,
    pub state_file: PathBuf,
    /// Descriptors owned by this handle (not durable).
    #[serde(skip, default = "new_handle_slot")]
    file_handles: Arc<Mutex<Vec<File>>>,
    /// Per-task ownership lock (not durable).
    #[serde(skip, default = "new_lock_slot")]
    ownership_lock: Arc<Mutex<Option<File>>>,
}

fn new_handle_slot() -> Arc<Mutex<Vec<File>>> {
    Arc::new(Mutex::new(Vec::new()))
}

fn new_lock_slot() -> Arc<Mutex<Option<File>>> {
    Arc::new(Mutex::new(None))
}

impl TaskEnvironment {
    pub fn state_file(&self) -> &Path {
        &self.state_file
    }

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

    /// Release the per-task ownership lock so a retry can proceed.
    pub fn release_ownership(&self) {
        self.ownership_lock
            .lock()
            .expect("ownership slot poisoned")
            .take();
    }

    /// Whether this handle still holds the ownership lock.
    pub fn owns_lock(&self) -> bool {
        self.ownership_lock
            .lock()
            .expect("ownership slot poisoned")
            .is_some()
    }

    /// Build a command that starts a task process owned by this environment.
    ///
    /// The child is placed in its own session so it leads its own process
    /// group, and it inherits the environment marker. Only processes started
    /// this way (or descended from them) can be registered and reclaimed.
    pub fn command(&self, program: impl AsRef<OsStr>) -> Command {
        let mut command = Command::new(program);
        command.env(MARKER_ENV, &self.instance_id);
        // SAFETY: setsid has no memory arguments and is called in the child
        // between fork and exec.
        unsafe {
            use std::os::unix::process::CommandExt as _;
            command.pre_exec(|| {
                if libc::setsid() < 0 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
        command
    }

    /// Whether `pid` carries this environment's marker.
    pub fn owns_process(&self, pid: i32) -> bool {
        process_has_marker(pid, &self.instance_id)
    }

    /// Register a worktree that lives inside the disposable workspace.
    pub fn register_worktree(&mut self, path: &Path) -> Result<()> {
        let resolved = util::resolve_path(path);
        if !self.owns_path(&resolved) {
            return Err(FlowError::progress(format!(
                "worktree {} is outside the disposable boundary",
                resolved.display()
            )));
        }
        if self
            .worktree_registrations
            .iter()
            .any(|item| item.path == resolved)
        {
            return Ok(());
        }
        let registration = WorktreeRegistration {
            repo: discover_repository(&resolved),
            path: resolved,
        };
        self.worktree_registrations.push(registration);
        self.persist()
    }

    /// Take ownership of a child process.
    ///
    /// The process must lead its own process group (spawn with `setsid` or
    /// `setpgid(0, 0)`); otherwise descendants cannot be reclaimed reliably
    /// and registration is refused.
    pub fn register_process(&mut self, pid: i32) -> Result<()> {
        if pid <= 1 {
            return Err(FlowError::progress(format!(
                "invalid process id {pid}: must be greater than 1"
            )));
        }
        if !self.owns_process(pid) {
            return Err(FlowError::progress(format!(
                "pid {pid} was not started with this environment's marker {}; spawn it with TaskEnvironment::command so it can be reclaimed",
                MARKER_ENV
            )));
        }
        let pgid = process_group(pid).ok_or_else(|| {
            FlowError::progress(format!("cannot read process group of pid {pid}"))
        })?;
        // SAFETY: getpgrp takes no pointers and has no caller preconditions.
        let own_group = unsafe { libc::getpgrp() };
        if pgid != pid {
            return Err(FlowError::progress(format!(
                "pid {pid} is in process group {pgid}, not its own group; spawn it with setsid so its descendants can be reclaimed"
            )));
        }
        if pgid == own_group {
            return Err(FlowError::progress(format!(
                "pid {pid} shares the supervisor's process group"
            )));
        }
        let entry = RegisteredProcess {
            pid,
            pgid,
            start_time: process_start_time(pid),
            descendants: descendants_snapshot(pid),
            direct_children: direct_children_of(std::process::id() as i32),
        };
        if !self.child_processes.contains(&entry) {
            self.child_processes.push(entry);
            self.persist()?;
        }
        Ok(())
    }

    /// Take ownership of an already-open descriptor.
    ///
    /// Only the owner of a descriptor can close it, so this is the supported
    /// way to guarantee a file does not outlive the environment.
    pub fn register_handle(&mut self, path: &Path, handle: File) -> Result<()> {
        let resolved = util::resolve_path(path);
        if !self.owns_path(&resolved) {
            return Err(FlowError::progress(format!(
                "refusing to register {} outside the disposable boundary",
                resolved.display()
            )));
        }
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

    /// Open `path` without following a final symlink and take ownership.
    pub fn register_file(&mut self, path: &Path) -> Result<()> {
        let resolved = util::resolve_path(path);
        if !self.owns_path(&resolved) {
            return Err(FlowError::progress(format!(
                "refusing to register {} outside the disposable boundary",
                resolved.display()
            )));
        }
        if self.open_files.contains(&resolved) {
            return Ok(());
        }
        let handle = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(&resolved)
            .map_err(|error| {
                FlowError::progress(format!(
                    "cannot open {} for registration: {error}",
                    resolved.display()
                ))
            })?;
        self.register_handle(&resolved, handle)
    }

    pub fn unregister_worktree(&mut self, path: &Path) -> Result<()> {
        let resolved = util::resolve_path(path);
        self.worktree_registrations
            .retain(|item| item.path != resolved);
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

    pub fn open_handle_count(&self) -> usize {
        self.file_handles
            .lock()
            .expect("file handle slot poisoned")
            .len()
    }

    /// True when `path` lexically lies inside the disposable boundary.
    pub fn owns_path(&self, path: &Path) -> bool {
        let resolved = util::resolve_path(path);
        util::is_relative_to(&resolved, &self.workspace_root)
            || util::is_relative_to(&resolved, &self.temp_root)
    }

    /// Durable roots with their recorded identities.
    pub fn roots(&self) -> [(PathBuf, Option<RootIdentity>); 2] {
        [
            (self.workspace_root.clone(), self.workspace_identity),
            (self.temp_root.clone(), self.temp_identity),
        ]
    }

    fn attach_lock(&self, lock: File) {
        *self.ownership_lock.lock().expect("ownership slot poisoned") = Some(lock);
    }
}

/// Process group of `pid`, if visible.
pub fn process_group(pid: i32) -> Option<i32> {
    // SAFETY: getpgid takes only a scalar and performs no memory access.
    let pgid = unsafe { libc::getpgid(pid) };
    if pgid > 0 {
        Some(pgid)
    } else {
        None
    }
}

/// Kernel start time of a process.
pub fn process_start_time(pid: i32) -> Option<u64> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let rest = stat.rsplit_once(')')?.1;
    rest.split_whitespace().nth(19)?.parse().ok()
}

/// Locate the main repository of a linked worktree from its `.git` file.
///
/// Only used to find the repository for `git worktree` commands. The parsed
/// path is verified to sit at `<repo>/.git/worktrees/<name>`.
pub fn discover_repository(worktree: &Path) -> Option<PathBuf> {
    let git = worktree.join(".git");
    let metadata = std::fs::symlink_metadata(&git).ok()?;
    if !metadata.is_file() || metadata.file_type().is_symlink() {
        return None;
    }
    let text = std::fs::read_to_string(&git).ok()?;
    let gitdir = text.strip_prefix("gitdir:")?.trim();
    let gitdir = Path::new(gitdir);
    let admin = if gitdir.is_absolute() {
        gitdir.to_path_buf()
    } else {
        worktree.join(gitdir)
    };
    let admin = admin.canonicalize().ok()?;
    let worktrees = admin.parent()?;
    if worktrees.file_name()? != "worktrees" {
        return None;
    }
    let dot_git = worktrees.parent()?;
    if dot_git.file_name()? != ".git" {
        return None;
    }
    dot_git.parent().map(Path::to_path_buf)
}

fn process_state(pid: i32) -> Option<char> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let rest = stat.rsplit_once(')')?.1;
    rest.split_whitespace().next()?.chars().next()
}

/// Whether a recorded process identity still names a live process.
pub fn process_record_is_alive(record: &ProcessRecord) -> bool {
    if record.pid <= 1 {
        return false;
    }
    if matches!(process_state(record.pid), Some('Z') | None) {
        return false;
    }
    match (process_start_time(record.pid), record.start_time) {
        (Some(current), Some(expected)) => current == expected,
        (Some(_), None) => true,
        (None, _) => false,
    }
}

/// Current direct children of `pid`.
pub fn direct_children_of(pid: i32) -> Vec<ProcessRecord> {
    let Ok(entries) = std::fs::read_dir("/proc") else {
        return Vec::new();
    };
    let mut children = Vec::new();
    for entry in entries.flatten() {
        let Some(name) = entry.file_name().to_str().map(str::to_string) else {
            continue;
        };
        let Ok(child) = name.parse::<i32>() else {
            continue;
        };
        if parent_of(child) == Some(pid) {
            children.push(ProcessRecord {
                pid: child,
                start_time: process_start_time(child),
            });
        }
    }
    children.sort_by_key(|record| record.pid);
    children
}

/// Whether a process carries the marker for `instance_id`.
pub fn process_has_marker(pid: i32, instance_id: &str) -> bool {
    let Ok(bytes) = std::fs::read(format!("/proc/{pid}/environ")) else {
        return false;
    };
    let needle = format!("{MARKER_ENV}={instance_id}");
    bytes
        .split(|byte| *byte == 0)
        .any(|entry| entry == needle.as_bytes())
}

/// Every process that carries the marker for `instance_id`.
pub fn marked_processes(instance_id: &str) -> Vec<ProcessRecord> {
    let mut records = Vec::new();
    let Ok(entries) = std::fs::read_dir("/proc") else {
        return records;
    };
    for entry in entries.flatten() {
        let Some(name) = entry.file_name().to_str().map(str::to_string) else {
            continue;
        };
        let Ok(pid) = name.parse::<i32>() else {
            continue;
        };
        if process_has_marker(pid, instance_id) {
            records.push(ProcessRecord {
                pid,
                start_time: process_start_time(pid),
            });
        }
    }
    records.sort_by_key(|record| record.pid);
    records
}

/// All transitive children of `pid` that are currently visible.
pub fn descendants_snapshot(pid: i32) -> Vec<ProcessRecord> {
    let mut parents = std::collections::BTreeMap::new();
    let Ok(entries) = std::fs::read_dir("/proc") else {
        return Vec::new();
    };
    for entry in entries.flatten() {
        let Some(name) = entry.file_name().to_str().map(str::to_string) else {
            continue;
        };
        let Ok(child) = name.parse::<i32>() else {
            continue;
        };
        if let Some(parent) = parent_of(child) {
            parents.insert(child, parent);
        }
    }

    let mut found = Vec::new();
    let mut frontier = vec![pid];
    while let Some(current) = frontier.pop() {
        for (child, parent) in &parents {
            if *parent == current
                && !found
                    .iter()
                    .any(|record: &ProcessRecord| record.pid == *child)
            {
                found.push(ProcessRecord {
                    pid: *child,
                    start_time: process_start_time(*child),
                });
                frontier.push(*child);
            }
        }
    }
    found
}

fn parent_of(pid: i32) -> Option<i32> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    // After comm: state ppid pgrp ...
    stat.rsplit_once(')')?
        .1
        .split_whitespace()
        .nth(1)?
        .parse()
        .ok()
}

/// Whether a registered process is still the same live (non-zombie) process.
pub fn process_is_alive(entry: &RegisteredProcess) -> bool {
    if entry.pid <= 1 {
        return false;
    }
    if matches!(process_state(entry.pid), Some('Z') | None) {
        return false;
    }
    match (process_start_time(entry.pid), entry.start_time) {
        (Some(current), Some(expected)) => current == expected,
        (Some(_), None) => true,
        (None, _) => false,
    }
}

/// Create a new task environment.
///
/// Protocol (each step leaves a recoverable state):
/// 1. take the per-task lock (exclusive ownership);
/// 2. validate ids, paths, targets and root claims;
/// 3. write the environment document with `creating` **before** any directory
///    or index entry exists, so a crash here is always visible;
/// 4. claim the roots in the index (unique ownership, resource limit);
/// 5. materialise the roots and record their identities;
/// 6. mark the document `active`.
pub fn create(
    task_id: &str,
    workspace_root: &Path,
    delivery_targets: Vec<DeliveryTarget>,
    temp_root: &Path,
) -> Result<TaskEnvironment> {
    registry::validate_task_id(task_id)?;

    let lock = registry::try_lock_task(task_id)?.ok_or_else(|| {
        FlowError::progress(format!(
            "environment for task {task_id} is already owned by another live handle"
        ))
    })?;

    let workspace_root = util::resolve_path(workspace_root);
    let temp_root = util::resolve_path(temp_root);
    let delivery_targets =
        super::boundary::validate_delivery_targets(delivery_targets, &workspace_root, &temp_root)?;
    for target in &delivery_targets {
        for root in [&workspace_root, &temp_root] {
            if util::overlap(&target.path, root) {
                return Err(FlowError::progress(format!(
                    "delivery target {} overlaps with disposable root {}",
                    target.path.display(),
                    root.display()
                )));
            }
        }
    }
    if util::overlap(&workspace_root, &temp_root) {
        return Err(FlowError::progress(format!(
            "workspace {} and temp root {} overlap",
            workspace_root.display(),
            temp_root.display()
        )));
    }

    require_vacant(&workspace_root)?;
    require_vacant(&temp_root)?;
    validate_ancestors(&workspace_root)?;
    validate_ancestors(&temp_root)?;

    let state_file = registry::environment_state_path(task_id)?;
    if state_file.exists() {
        return Err(FlowError::progress(format!(
            "environment for task {task_id} already exists; destroy or recover it first"
        )));
    }

    let now = util::now();
    let mut env = TaskEnvironment {
        task_id: task_id.to_string(),
        instance_id: util::uuid_hex(),
        workspace_root: workspace_root.clone(),
        temp_root: temp_root.clone(),
        workspace_identity: None,
        temp_identity: None,
        delivery_targets,
        state: EnvironmentState::Creating,
        created_at: now,
        updated_at: now,
        worktree_registrations: Vec::new(),
        child_processes: Vec::new(),
        open_files: Vec::new(),
        state_file: state_file.clone(),
        file_handles: new_handle_slot(),
        ownership_lock: new_lock_slot(),
    };
    env.attach_lock(lock);

    // Step 3: durable intent first.
    if let Err(error) = env.persist() {
        env.release_ownership();
        return Err(error);
    }

    // Step 4: unique root claim and resource slot.
    let entry = registry::RegistryEntry {
        task_id: task_id.to_string(),
        instance_id: env.instance_id.clone(),
        workspace_root: workspace_root.clone(),
        temp_root: temp_root.clone(),
        roots: vec![workspace_root.clone(), temp_root.clone()],
        state_file: state_file.clone(),
        registered_at: now,
    };
    if let Err(error) = registry::register_environment(entry) {
        let _ = std::fs::remove_file(&state_file);
        env.release_ownership();
        return Err(error);
    }

    // Step 5: create and fingerprint the roots.
    if let Err(error) = materialize(&workspace_root, &temp_root) {
        let _ = remove_created(&workspace_root);
        let _ = remove_created(&temp_root);
        let _ = std::fs::remove_file(&state_file);
        let _ = registry::unregister_environment(task_id, &env.instance_id);
        env.release_ownership();
        return Err(error);
    }
    let identities = fingerprint(&workspace_root, &temp_root);
    let (workspace_identity, temp_identity) = match identities {
        Ok(pair) => pair,
        Err(error) => {
            let _ = remove_created(&workspace_root);
            let _ = remove_created(&temp_root);
            let _ = std::fs::remove_file(&state_file);
            let _ = registry::unregister_environment(task_id, &env.instance_id);
            env.release_ownership();
            return Err(error);
        }
    };
    env.workspace_identity = Some(workspace_identity);
    env.temp_identity = Some(temp_identity);

    // Step 6: usable.
    if let Err(error) = env.mark_active() {
        env.release_ownership();
        return Err(error);
    }
    Ok(env)
}

/// Load a live environment document and take ownership of it.
///
/// Fails while another live handle owns the task, which prevents a restored
/// handle from releasing descriptors or slots it does not own.
pub fn restore(task_id: &str) -> Result<TaskEnvironment> {
    registry::validate_task_id(task_id)?;
    let lock = registry::try_lock_task(task_id)?.ok_or_else(|| {
        FlowError::progress(format!(
            "environment for task {task_id} is owned by another live handle"
        ))
    })?;
    let state_file = registry::environment_state_path(task_id)?;
    if !state_file.exists() {
        return Err(FlowError::progress(format!(
            "environment for task {task_id} does not exist"
        )));
    }
    let text = std::fs::read_to_string(&state_file)?;
    let mut env: TaskEnvironment = serde_json::from_str(&text).map_err(|error| {
        FlowError::progress(format!("cannot parse {}: {error}", state_file.display()))
    })?;
    env.state_file = state_file;
    env.file_handles = new_handle_slot();
    env.ownership_lock = new_lock_slot();
    env.attach_lock(lock);
    Ok(env)
}

/// Recover an environment left behind by an interrupted create or destroy.
pub fn recover_interrupted(task_id: &str) -> Result<bool> {
    registry::validate_task_id(task_id)?;
    let state_file = registry::environment_state_path(task_id)?;
    if !state_file.exists() {
        // A plan can outlive its document when unlink of the document failed.
        if registry::cleanup_state_path(task_id)
            .map(|path| path.exists())
            .unwrap_or(false)
        {
            super::cleanup::cleanup_failed_environment(task_id)?;
            return Ok(true);
        }
        return Ok(false);
    }
    let env = restore(task_id)?;
    // Any non-active document, or a leftover plan, describes an interrupted
    // create or destroy that must be completed.
    let interrupted = env.state != EnvironmentState::Active
        || registry::cleanup_state_path(task_id)
            .map(|path| path.exists())
            .unwrap_or(false);
    if !interrupted {
        return Ok(false);
    }
    super::cleanup::destroy(&env)?;
    Ok(true)
}

/// Refuse to take over an existing directory that already holds data.
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
            if std::fs::read_dir(path)?.next().is_some() {
                return Err(FlowError::progress(format!(
                    "refusing to claim non-empty directory {}",
                    path.display()
                )));
            }
            Ok(())
        }
    }
}

/// Create only the roots themselves. Parents must already exist, so an
/// environment never becomes the owner of directories it did not record.
fn materialize(workspace_root: &Path, temp_root: &Path) -> Result<()> {
    for root in [workspace_root, temp_root] {
        if root.exists() {
            continue;
        }
        let parent_ok = root.parent().map(Path::is_dir).unwrap_or(false);
        if !parent_ok {
            return Err(FlowError::progress(format!(
                "parent of {} must already exist",
                root.display()
            )));
        }
        std::fs::create_dir(root).map_err(|error| {
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

fn fingerprint(workspace_root: &Path, temp_root: &Path) -> Result<(RootIdentity, RootIdentity)> {
    let workspace = std::fs::symlink_metadata(workspace_root)?;
    let temp = std::fs::symlink_metadata(temp_root)?;
    if workspace.file_type().is_symlink() || !workspace.is_dir() {
        return Err(FlowError::progress(format!(
            "workspace {} must be a real directory",
            workspace_root.display()
        )));
    }
    if temp.file_type().is_symlink() || !temp.is_dir() {
        return Err(FlowError::progress(format!(
            "temp root {} must be a real directory",
            temp_root.display()
        )));
    }
    Ok((RootIdentity::of(&workspace), RootIdentity::of(&temp)))
}

/// Largest acceptable ancestor mode: owner-writable, not group/world writable
/// (a root-owned sticky directory such as `/tmp` is acceptable).
fn ancestor_is_trusted(metadata: &std::fs::Metadata) -> bool {
    // SAFETY: geteuid takes no pointers and has no caller preconditions.
    let uid = unsafe { libc::geteuid() };
    let owner_ok = metadata.uid() == uid || metadata.uid() == 0;
    let root_sticky = metadata.uid() == 0 && metadata.mode() & libc::S_ISVTX != 0;
    owner_ok && (metadata.mode() & 0o022 == 0 || root_sticky)
}

/// Verify no ancestor of `path` is a symlink or untrusted directory.
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

/// Open a directory without following symlinks.
pub(crate) fn open_dir_nofollow(path: &Path) -> Result<File> {
    OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(path)
        .map_err(|error| FlowError::progress(format!("cannot open {}: {error}", path.display())))
}

/// Open `relative` beneath an already-open directory, component by component,
/// never following a symlink.
pub(crate) fn open_beneath(root: &File, relative: &Path) -> Result<File> {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt as _;

    let mut current = root.try_clone()?;
    for component in relative.components() {
        let std::path::Component::Normal(name) = component else {
            return Err(FlowError::progress(format!(
                "invalid relative path {}",
                relative.display()
            )));
        };
        let name = CString::new(name.as_bytes())
            .map_err(|_| FlowError::progress("path component contains NUL"))?;
        // SAFETY: current is a live directory fd and name is NUL-terminated.
        let fd = unsafe {
            libc::openat(
                current.as_raw_fd(),
                name.as_ptr(),
                libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            )
        };
        if fd < 0 {
            return Err(FlowError::progress(format!(
                "cannot open {} beneath root: {}",
                relative.display(),
                std::io::Error::last_os_error()
            )));
        }
        // SAFETY: successful openat returns a fresh descriptor we now own.
        current = unsafe { <File as std::os::fd::FromRawFd>::from_raw_fd(fd) };
    }
    Ok(current)
}

/// Entry kind relative to an open directory, without following a symlink.
pub(crate) fn kind_at(dir: &File, name: &std::ffi::OsStr) -> Result<Option<EntryKind>> {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt as _;

    let name_c = CString::new(name.as_bytes())
        .map_err(|_| FlowError::progress("path component contains NUL"))?;
    // SAFETY: zeroed stat is a valid initial value for fstatat to fill.
    let mut stat: libc::stat = unsafe { std::mem::zeroed() };
    // SAFETY: dir is live, name is NUL-terminated, stat is writable.
    let result = unsafe {
        libc::fstatat(
            dir.as_raw_fd(),
            name_c.as_ptr(),
            &mut stat,
            libc::AT_SYMLINK_NOFOLLOW,
        )
    };
    if result != 0 {
        let error = std::io::Error::last_os_error();
        if error.kind() == std::io::ErrorKind::NotFound {
            return Ok(None);
        }
        return Err(FlowError::progress(format!(
            "cannot stat {name:?}: {error}"
        )));
    }
    let mode = stat.st_mode & libc::S_IFMT;
    let kind = if mode == libc::S_IFLNK {
        EntryKind::Symlink
    } else if mode == libc::S_IFDIR {
        EntryKind::Directory
    } else if mode == libc::S_IFREG {
        EntryKind::File
    } else {
        EntryKind::Other
    };
    Ok(Some(kind))
}

/// Kind of a directory entry, from `lstat` semantics.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum EntryKind {
    File,
    Directory,
    Symlink,
    Other,
}

/// Unlink an entry relative to an open directory. Removes the link itself; a
/// symlink target is never touched.
pub(crate) fn unlink_at(dir: &File, name: &std::ffi::OsStr) -> Result<()> {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt as _;

    let name = CString::new(name.as_bytes())
        .map_err(|_| FlowError::progress("path component contains NUL"))?;
    // SAFETY: dir is live and name is NUL-terminated; flags are zero.
    let result = unsafe { libc::unlinkat(dir.as_raw_fd(), name.as_ptr(), 0) };
    if result == 0 {
        return Ok(());
    }
    let error = std::io::Error::last_os_error();
    if error.kind() == std::io::ErrorKind::NotFound {
        return Ok(());
    }
    Err(FlowError::progress(format!(
        "cannot unlink {name:?}: {error}"
    )))
}

/// Open a child directory relative to `parent`, without following a symlink.
pub(crate) fn open_child_dir(parent: &File, name: &std::ffi::OsStr) -> Result<Option<File>> {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt as _;

    let name = CString::new(name.as_bytes())
        .map_err(|_| FlowError::progress("path component contains NUL"))?;
    // SAFETY: parent is a live directory fd, name is NUL-terminated.
    let fd = unsafe {
        libc::openat(
            parent.as_raw_fd(),
            name.as_ptr(),
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
        )
    };
    if fd < 0 {
        let error = std::io::Error::last_os_error();
        if matches!(
            error.raw_os_error(),
            Some(libc::ENOENT) | Some(libc::ENOTDIR) | Some(libc::ELOOP)
        ) {
            return Ok(None);
        }
        return Err(FlowError::progress(format!(
            "cannot open {:?}: {error}",
            name
        )));
    }
    // SAFETY: successful openat returns a fresh descriptor we now own.
    Ok(Some(unsafe {
        <File as std::os::fd::FromRawFd>::from_raw_fd(fd)
    }))
}

/// Names of the entries in an open directory.
pub(crate) fn directory_entries(dir: &File) -> Result<Vec<std::ffi::OsString>> {
    let alias = format!("/proc/self/fd/{}", dir.as_raw_fd());
    let mut names = Vec::new();
    for entry in std::fs::read_dir(alias)?.flatten() {
        names.push(entry.file_name());
    }
    Ok(names)
}

/// Make an open directory writable by its owner.
pub(crate) fn make_dir_writable(dir: &File) -> Result<()> {
    let metadata = dir.metadata()?;
    let mode = metadata.mode() & 0o7777;
    if mode & 0o700 == 0o700 {
        return Ok(());
    }
    // SAFETY: dir is a live descriptor; fchmod takes scalars.
    if unsafe { libc::fchmod(dir.as_raw_fd(), mode | 0o700) } != 0 {
        return Err(FlowError::progress(format!(
            "cannot chmod open directory: {}",
            std::io::Error::last_os_error()
        )));
    }
    Ok(())
}

/// Remove every entry inside an open directory, then leave the directory
/// itself in place. All operations are relative to pinned descriptors, so a
/// path swapped after verification cannot redirect the deletion.
pub(crate) fn clear_directory(dir: &File, depth: usize) -> Result<()> {
    if depth > 128 {
        return Err(FlowError::progress("directory nesting too deep"));
    }
    make_dir_writable(dir)?;
    for name in directory_entries(dir)? {
        match kind_at(dir, &name)? {
            None => continue,
            Some(EntryKind::Directory) => {
                let Some(child) = open_child_dir(dir, &name)? else {
                    continue;
                };
                clear_directory(&child, depth + 1)?;
                unlink_at_dir(dir, &name)?;
            }
            Some(_) => unlink_at(dir, &name)?,
        }
    }
    Ok(())
}

/// `unlinkat(..., AT_REMOVEDIR)` relative to an open directory.
pub(crate) fn unlink_at_dir(dir: &File, name: &std::ffi::OsStr) -> Result<()> {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt as _;

    let name = CString::new(name.as_bytes())
        .map_err(|_| FlowError::progress("path component contains NUL"))?;
    // SAFETY: dir is live, name is NUL-terminated, AT_REMOVEDIR is scalar.
    let result = unsafe { libc::unlinkat(dir.as_raw_fd(), name.as_ptr(), libc::AT_REMOVEDIR) };
    if result == 0 {
        return Ok(());
    }
    let error = std::io::Error::last_os_error();
    if matches!(
        error.raw_os_error(),
        Some(libc::ENOENT) | Some(libc::ENOTEMPTY)
    ) {
        return Ok(());
    }
    Err(FlowError::progress(format!(
        "cannot remove directory {name:?}: {error}"
    )))
}
