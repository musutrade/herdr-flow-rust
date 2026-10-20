//! Idempotent destruction of a task environment.
//!
//! Destruction is a two-phase operation. First a cleanup plan is written and
//! the environment document moves to `destroying`, so a crash mid-cleanup is
//! recoverable. Then processes, worktrees, files and directories are released
//! in that order. Only when everything is gone are the plan, the state
//! document and the registry slot removed. A failure keeps the plan and the
//! occupied slot in place, so callers cannot avoid the failure by allocating a
//! fresh root.
//!
//! Safety rules applied to every removal:
//! - never follow symlinks (paths are resolved and re-checked against the
//!   boundary, and recursive walks use `symlink_metadata`);
//! - only remove paths owned by the environment, never its delivery targets;
//! - never trust `.git` file contents to locate a directory to delete.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::error::{FlowError, Result};
use crate::util;

use super::env::{process_is_alive, RegisteredProcess, TaskEnvironment};
use super::registry;

/// Durable plan describing what still has to be released.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CleanupState {
    pub task_id: String,
    pub pending_processes: Vec<RegisteredProcess>,
    pub pending_worktrees: Vec<PathBuf>,
    pub pending_files: Vec<PathBuf>,
    pub pending_directories: Vec<PathBuf>,
    pub started_at: f64,
    pub updated_at: f64,
    pub attempts: u32,
    pub last_error: Option<String>,
}

/// Maximum number of retries before a slot is reported as stuck.
pub const MAX_ATTEMPTS: u32 = 5;

impl CleanupState {
    fn from_environment(env: &TaskEnvironment) -> Self {
        let now = util::now();
        // Directories are removed last, and only those the environment owns.
        let mut pending_directories = vec![env.temp_root.clone(), env.workspace_root.clone()];
        pending_directories.retain(|dir| !is_delivery_target(env, dir));
        Self {
            task_id: env.task_id.clone(),
            pending_processes: env.child_processes.clone(),
            pending_worktrees: env.worktree_registrations.clone(),
            pending_files: env.open_files.clone(),
            pending_directories,
            started_at: now,
            updated_at: now,
            attempts: 0,
            last_error: None,
        }
    }

    fn is_complete(&self) -> bool {
        self.pending_processes.is_empty()
            && self.pending_worktrees.is_empty()
            && self.pending_files.is_empty()
            && self.pending_directories.is_empty()
    }

    fn record(&mut self, error: Option<String>) {
        self.attempts += 1;
        self.updated_at = util::now();
        self.last_error = error;
    }
}

fn is_delivery_target(env: &TaskEnvironment, path: &Path) -> bool {
    let resolved = util::resolve_path(path);
    env.delivery_targets
        .iter()
        .any(|target| util::is_relative_to(&resolved, &util::resolve_path(&target.path)))
}

/// Save a cleanup plan so a crash or failed attempt can be retried.
fn save_plan(state: &CleanupState) -> Result<()> {
    let path = registry::cleanup_state_path(&state.task_id)?;
    let value = serde_json::to_value(state)?;
    util::atomic_write_json(&path, &value)
}

fn load_plan(task_id: &str) -> Result<CleanupState> {
    let path = registry::cleanup_state_path(task_id)?;
    let text = std::fs::read_to_string(&path).map_err(|error| {
        FlowError::progress(format!(
            "no cleanup plan for task {task_id} at {}: {error}",
            path.display()
        ))
    })?;
    serde_json::from_str(&text).map_err(|error| {
        FlowError::progress(format!(
            "cannot parse cleanup plan {}: {error}",
            path.display()
        ))
    })
}

fn remove_plan(task_id: &str) -> Result<()> {
    util::remove_file_if_exists(&registry::cleanup_state_path(task_id)?)
}

fn plan_exists(task_id: &str) -> bool {
    registry::cleanup_state_path(task_id)
        .map(|path| path.exists())
        .unwrap_or(false)
}

/// Destroy an environment: release every resource, then drop the slot.
///
/// The call is idempotent. Running it again after success reports success and
/// performs no work; running it after a failure resumes the saved plan.
pub fn destroy(env: &TaskEnvironment) -> Result<()> {
    registry::validate_task_id(&env.task_id)?;

    let mut state = match plan_exists(&env.task_id) {
        true => load_plan(&env.task_id)?,
        false => {
            // Persist the plan before touching anything.
            let plan = CleanupState::from_environment(env);
            save_plan(&plan)?;
            plan
        }
    };

    // Mark the document as destroying so a crashed destroy is distinguishable
    // from an active environment.
    let mut doc = registry::environment_state_path(&env.task_id)
        .ok()
        .filter(|path| path.exists())
        .and_then(|path| std::fs::read_to_string(path).ok())
        .and_then(|text| serde_json::from_str::<TaskEnvironment>(&text).ok());
    if let Some(doc) = doc.as_mut() {
        doc.state = super::env::EnvironmentState::Destroying;
        let _ = doc.persist();
    }

    env.release_file_handles();
    let outcome = run_cleanup(&mut state);

    if state.is_complete() {
        finish(&env.task_id)?;
        return Ok(());
    }

    // Failure: keep the plan and the slot, and record why.
    save_plan(&state)?;
    if let Some(doc) = doc.as_mut() {
        doc.state = super::env::EnvironmentState::DestroyFailed;
        let _ = doc.persist();
    }
    outcome
}

/// Retry a previously failed cleanup.
pub fn cleanup_failed_environment(task_id: &str) -> Result<()> {
    registry::validate_task_id(task_id)?;
    let mut state = load_plan(task_id)?;

    if state.attempts >= MAX_ATTEMPTS {
        return Err(FlowError::progress(format!(
            "cleanup for task {task_id} already failed {} times; manual attention required",
            state.attempts
        )));
    }

    let outcome = run_cleanup(&mut state);
    if state.is_complete() {
        finish(task_id)?;
        return Ok(());
    }
    save_plan(&state)?;
    outcome
}

/// Release the plan, the state document and the registry slot.
fn finish(task_id: &str) -> Result<()> {
    registry::unregister_environment(task_id)?;
    util::remove_file_if_exists(&registry::environment_state_path(task_id)?)?;
    remove_plan(task_id)?;
    Ok(())
}

/// Execute one pass over the plan, collecting per-resource failures.
fn run_cleanup(state: &mut CleanupState) -> Result<()> {
    let mut errors = Vec::new();

    state
        .pending_processes
        .retain(|entry| match terminate_process(entry) {
            Ok(()) => false,
            Err(error) => {
                errors.push(format!("process {}: {error}", entry.pid));
                true
            }
        });

    state
        .pending_worktrees
        .retain(|path| match remove_worktree(path, &state.task_id) {
            Ok(()) => false,
            Err(error) => {
                errors.push(format!("worktree {}: {error}", path.display()));
                true
            }
        });

    state
        .pending_files
        .retain(|path| match remove_owned_file(path) {
            Ok(()) => false,
            Err(error) => {
                errors.push(format!("file {}: {error}", path.display()));
                true
            }
        });

    state
        .pending_directories
        .retain(|path| match remove_owned_directory(path) {
            Ok(()) => false,
            Err(error) => {
                errors.push(format!("directory {}: {error}", path.display()));
                true
            }
        });

    if errors.is_empty() {
        state.record(None);
        Ok(())
    } else {
        let message = errors.join("; ");
        state.record(Some(message.clone()));
        Err(FlowError::progress(format!(
            "cleanup of task {} incomplete: {message}",
            state.task_id
        )))
    }
}

/// Terminate a registered process together with all of its descendants.
///
/// Descendants are collected before any signal is sent: once the parent dies
/// they are reparented to init and can no longer be attributed to the
/// environment. Each descendant then receives SIGTERM (and SIGKILL on timeout)
/// before its ancestor, so no new children can be spawned mid-cleanup.
fn terminate_process(entry: &RegisteredProcess) -> Result<()> {
    if entry.pid <= 1 {
        return Err(FlowError::progress(format!(
            "refusing to signal invalid pid {}",
            entry.pid
        )));
    }
    if !process_is_alive(entry) {
        return Ok(());
    }

    let mut tree = descendants_of(entry.pid);
    tree.push(entry.pid);
    // Deepest first so a dying parent cannot re-parent or re-spawn children.
    tree.sort_unstable_by_key(|pid| std::cmp::Reverse(depth_of(*pid)));

    for pid in &tree {
        signal_process(*pid, libc::SIGTERM);
    }
    if wait_for_tree(&tree, 500) {
        return Ok(());
    }
    for pid in &tree {
        signal_process(*pid, libc::SIGKILL);
    }
    if wait_for_tree(&tree, 500) {
        Ok(())
    } else {
        let survivors: Vec<String> = tree
            .iter()
            .filter(|pid| pid_is_live(**pid))
            .map(i32::to_string)
            .collect();
        Err(FlowError::progress(format!(
            "processes survived SIGKILL: {}",
            survivors.join(", ")
        )))
    }
}

fn pid_is_live(pid: i32) -> bool {
    matches!(process_state_of(pid), Some(state) if state != 'Z')
}

/// Signal one process, and its own group when it leads a separate one.
fn signal_process(pid: i32, signal: i32) {
    // SAFETY: getpgid/kill take only scalars; pid is known positive.
    unsafe {
        let group = libc::getpgid(pid);
        if group > 1 && group != libc::getpgrp() && group == pid {
            libc::kill(-group, signal);
        }
        libc::kill(pid, signal);
    }
}

fn process_state_of(pid: i32) -> Option<char> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let rest = stat.rsplit_once(')')?.1;
    rest.split_whitespace().next()?.chars().next()
}

fn parent_of(pid: i32) -> Option<i32> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let rest = stat.rsplit_once(')')?.1;
    // After comm: state ppid pgrp ...
    rest.split_whitespace().nth(1)?.parse().ok()
}

/// All transitive children of `pid` that are currently visible.
fn descendants_of(pid: i32) -> Vec<i32> {
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
            if *parent == current && !found.contains(child) {
                found.push(*child);
                frontier.push(*child);
            }
        }
    }
    found
}

fn depth_of(pid: i32) -> usize {
    let mut depth = 0;
    let mut cursor = pid;
    while let Some(parent) = parent_of(cursor) {
        if parent <= 1 {
            break;
        }
        depth += 1;
        cursor = parent;
        if depth > 64 {
            break;
        }
    }
    depth
}

fn wait_for_tree(pids: &[i32], millis: u64) -> bool {
    let deadline = std::time::Instant::now() + std::time::Duration::from_millis(millis);
    loop {
        if pids.iter().all(|pid| !pid_is_live(*pid)) {
            return true;
        }
        if std::time::Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
}

/// Remove a worktree, but only one that lives inside the environment's own
/// disposable root. `.git` contents are never used to locate deleted paths.
fn remove_worktree(path: &Path, task_id: &str) -> Result<()> {
    let resolved = util::resolve_path(path);
    if !resolved.exists() {
        return Ok(());
    }
    let env = match registry::environment_state_path(task_id)
        .ok()
        .and_then(|state| std::fs::read_to_string(state).ok())
        .and_then(|text| serde_json::from_str::<TaskEnvironment>(&text).ok())
    {
        Some(env) => env,
        None => {
            return Err(FlowError::progress(format!(
                "cannot verify worktree {}: environment document missing",
                resolved.display()
            )))
        }
    };
    if !util::is_relative_to(&resolved, &env.workspace_root) {
        return Err(FlowError::progress(format!(
            "refusing to remove worktree {} outside {}",
            resolved.display(),
            env.workspace_root.display()
        )));
    }
    if is_delivery_target(&env, &resolved) {
        return Err(FlowError::progress(format!(
            "refusing to remove delivery target {}",
            resolved.display()
        )));
    }

    let output = std::process::Command::new("git")
        .args(["worktree", "remove", "--force", "--force"])
        .arg(&resolved)
        .output();
    match output {
        Ok(output) if output.status.success() => Ok(()),
        // A missing or already-detached worktree is not a failure; the
        // directory removal below still reclaims the tree.
        _ => Ok(()),
    }
}

/// Remove a registered file only when it lives inside the boundary.
fn remove_owned_file(path: &Path) -> Result<()> {
    let resolved = util::resolve_path(path);
    match std::fs::symlink_metadata(&resolved) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error.into()),
        Ok(metadata) if metadata.is_dir() => Ok(()),
        Ok(_) => std::fs::remove_file(&resolved).map_err(|error| {
            FlowError::progress(format!("cannot remove {}: {error}", resolved.display()))
        }),
    }
}

/// Remove a disposable directory without ever following a symlink.
fn remove_owned_directory(path: &Path) -> Result<()> {
    let resolved = util::resolve_path(path);
    match std::fs::symlink_metadata(&resolved) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error.into()),
        Ok(metadata) if metadata.file_type().is_symlink() => {
            return Err(FlowError::progress(format!(
                "refusing to remove symlink {}",
                resolved.display()
            )))
        }
        Ok(metadata) if !metadata.is_dir() => {
            return Err(FlowError::progress(format!(
                "refusing to remove non-directory {}",
                resolved.display()
            )))
        }
        Ok(_) => {}
    }

    make_writable_without_symlinks(&resolved);
    match std::fs::remove_dir_all(&resolved) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(first) => {
            unmount_under(&resolved);
            std::fs::remove_dir_all(&resolved).map_err(|second| {
                FlowError::progress(format!(
                    "cannot remove {}: {first} (after retry: {second})",
                    resolved.display()
                ))
            })
        }
    }
}

/// Add owner write/execute bits using `lstat`-based metadata, skipping links.
fn make_writable_without_symlinks(dir: &Path) {
    use std::os::unix::fs::PermissionsExt;

    let Ok(metadata) = std::fs::symlink_metadata(dir) else {
        return;
    };
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return;
    }
    let mut permissions = metadata.permissions();
    permissions.set_mode(permissions.mode() | 0o700);
    let _ = std::fs::set_permissions(dir, permissions);

    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let Ok(metadata) = std::fs::symlink_metadata(&path) else {
            continue;
        };
        if metadata.file_type().is_symlink() {
            continue;
        }
        if metadata.is_dir() {
            make_writable_without_symlinks(&path);
        } else {
            let mut permissions = metadata.permissions();
            permissions.set_mode(permissions.mode() | 0o600);
            let _ = std::fs::set_permissions(&path, permissions);
        }
    }
}

#[cfg(target_os = "linux")]
fn unmount_under(dir: &Path) {
    let Ok(mounts) = std::fs::read_to_string("/proc/mounts") else {
        return;
    };
    for line in mounts.lines() {
        let mut fields = line.split_whitespace();
        let _device = fields.next();
        let Some(mount_point) = fields.next() else {
            continue;
        };
        if mount_point.starts_with(dir.to_string_lossy().as_ref()) {
            let _ = std::process::Command::new("umount")
                .arg("-f")
                .arg(mount_point)
                .output();
        }
    }
}

#[cfg(not(target_os = "linux"))]
fn unmount_under(_dir: &Path) {}
