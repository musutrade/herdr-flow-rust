//! Crash-safe, identity-checked destruction of a task environment.
//!
//! Every deletion is verified against recorded ownership rather than against a
//! path string:
//!
//! * disposable roots are removed only when `(device, inode)` still matches the
//!   value recorded at creation, so a swapped-in symlink or a different
//!   directory is refused instead of followed;
//! * registered files are unlinked relative to an `O_NOFOLLOW` directory walk,
//!   so a leaf swapped for a symlink loses only the link, never its target;
//! * worktrees are removed through the repository recorded at registration, and
//!   the removal is verified against `git worktree list`; a stale registration
//!   keeps the pending item instead of reporting success;
//! * processes are killed by process group until no member remains, which also
//!   catches children spawned while SIGTERM is being handled;
//! * directory permissions are widened on directories only, so a hard link to a
//!   delivery file can never have its mode changed through the environment.
//!
//! A plan is persisted before anything is touched and removed only after every
//! step succeeded. A failure keeps the plan and the resource slot; retries go
//! through [`cleanup_failed_environment`].

use std::fs::File;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::error::{FlowError, Result};
use crate::util;

use super::env::{
    identity_at, kind_at, open_beneath, open_dir_nofollow, unlink_at, EntryKind, EnvironmentState,
    RegisteredProcess, RootIdentity, TaskEnvironment, WorktreeRegistration,
};
use super::registry;

/// A disposable root that still has to be removed.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PendingDirectory {
    pub path: PathBuf,
    pub identity: Option<RootIdentity>,
}

/// Durable cleanup plan.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CleanupState {
    pub task_id: String,
    pub instance_id: String,
    /// Roots the environment owns; fixed for the life of the plan.
    pub roots: Vec<PendingDirectory>,
    pub pending_processes: Vec<RegisteredProcess>,
    pub pending_worktrees: Vec<WorktreeRegistration>,
    pub pending_files: Vec<PathBuf>,
    /// Roots still present on disk; a subset of `roots`.
    pub pending_root_removals: Vec<PathBuf>,
    pub started_at: f64,
    pub updated_at: f64,
    pub attempts: u32,
    pub last_error: Option<String>,
}

/// Maximum number of cleanup retries before manual attention is required.
pub const MAX_ATTEMPTS: u32 = 5;

impl CleanupState {
    fn from_environment(env: &TaskEnvironment) -> Result<Self> {
        let now = util::now();
        let mut roots = Vec::new();
        for (path, identity) in env.roots() {
            for target in &env.delivery_targets {
                if util::overlap(&path, &target.path) {
                    return Err(FlowError::progress(format!(
                        "refusing to destroy: disposable root {} overlaps delivery target {}",
                        path.display(),
                        target.path.display()
                    )));
                }
            }
            roots.push(PendingDirectory { path, identity });
        }
        let pending_root_removals = roots.iter().map(|root| root.path.clone()).collect();
        Ok(Self {
            task_id: env.task_id.clone(),
            instance_id: env.instance_id.clone(),
            roots,
            pending_processes: env.child_processes.clone(),
            pending_worktrees: env.worktree_registrations.clone(),
            pending_files: env.open_files.clone(),
            pending_root_removals,
            started_at: now,
            updated_at: now,
            attempts: 0,
            last_error: None,
        })
    }

    fn is_complete(&self) -> bool {
        self.pending_processes.is_empty()
            && self.pending_worktrees.is_empty()
            && self.pending_files.is_empty()
            && self.pending_root_removals.is_empty()
    }

    fn record(&mut self, error: Option<String>) {
        self.attempts += 1;
        self.updated_at = util::now();
        self.last_error = error;
    }
}

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
/// Idempotent for the same instance. A stale handle whose task id was reused
/// by a newer environment is refused and cannot unregister the newer owner.
pub fn destroy(env: &TaskEnvironment) -> Result<()> {
    registry::validate_task_id(&env.task_id)?;

    // The handle normally owns the per-task lock; after a failed destroy or for
    // a plan-driven retry it must be re-acquired.
    let transient_lock = if env.owns_lock() {
        None
    } else {
        Some(registry::try_lock_task(&env.task_id)?.ok_or_else(|| {
            FlowError::progress(format!(
                "environment for task {} is owned by another live handle",
                env.task_id
            ))
        })?)
    };

    let state_file = registry::environment_state_path(&env.task_id)?;
    let mut document = match std::fs::read_to_string(&state_file) {
        Ok(text) => Some(
            serde_json::from_str::<TaskEnvironment>(&text).map_err(|error| {
                FlowError::progress(format!("cannot parse {}: {error}", state_file.display()))
            })?,
        ),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => return Err(error.into()),
    };

    if let Some(doc) = document.as_ref() {
        if doc.instance_id != env.instance_id {
            return Err(FlowError::progress(format!(
                "task {} now belongs to environment instance {}; refusing to destroy it from a stale handle",
                env.task_id, doc.instance_id
            )));
        }
    }

    let mut state = match plan_exists(&env.task_id) {
        true => {
            let plan = load_plan(&env.task_id)?;
            if plan.instance_id != env.instance_id {
                return Err(FlowError::progress(format!(
                    "cleanup plan for task {} belongs to another environment instance",
                    env.task_id
                )));
            }
            plan
        }
        false => {
            if document.is_none() {
                // Nothing durable remains: already destroyed.
                env.release_ownership();
                return Ok(());
            }
            let plan = CleanupState::from_environment(env)?;
            save_plan(&plan)?;
            plan
        }
    };

    if let Some(doc) = document.as_mut() {
        doc.state = EnvironmentState::Destroying;
        let _ = doc.persist();
    }

    env.release_file_handles();
    let outcome = run_cleanup(&mut state);

    if state.is_complete() {
        finish(&env.task_id, &env.instance_id)?;
        env.release_ownership();
        return Ok(());
    }

    save_plan(&state)?;
    if let Some(doc) = document.as_mut() {
        doc.state = EnvironmentState::DestroyFailed;
        let _ = doc.persist();
    }
    // Allow a retry to take the lock; a new environment with the same id is
    // still refused because the registry entry and plan remain.
    env.release_ownership();
    drop(transient_lock);
    outcome
}

/// Resume a failed cleanup for `task_id`.
pub fn cleanup_failed_environment(task_id: &str) -> Result<()> {
    registry::validate_task_id(task_id)?;
    let _lock = registry::try_lock_task(task_id)?.ok_or_else(|| {
        FlowError::progress(format!(
            "environment for task {task_id} is owned by another live handle"
        ))
    })?;

    let mut state = load_plan(task_id)?;
    if state.attempts >= MAX_ATTEMPTS {
        return Err(FlowError::progress(format!(
            "cleanup for task {task_id} already failed {} times; manual attention required",
            state.attempts
        )));
    }

    let outcome = run_cleanup(&mut state);
    if state.is_complete() {
        finish(task_id, &state.instance_id)?;
        return Ok(());
    }
    save_plan(&state)?;
    outcome
}

/// Remove the plan, the document and the index entry for this instance only.
fn finish(task_id: &str, instance_id: &str) -> Result<()> {
    registry::unregister_environment(task_id, instance_id)?;

    let state_file = registry::environment_state_path(task_id)?;
    if let Ok(text) = std::fs::read_to_string(&state_file) {
        if let Ok(doc) = serde_json::from_str::<TaskEnvironment>(&text) {
            if doc.instance_id == instance_id {
                util::remove_file_if_exists(&state_file)?;
            }
        }
    }
    remove_plan(task_id)?;
    Ok(())
}

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
        .retain(|worktree| match remove_worktree(worktree, &state.roots) {
            Ok(()) => false,
            Err(error) => {
                errors.push(format!("worktree {}: {error}", worktree.path.display()));
                true
            }
        });

    state
        .pending_files
        .retain(|path| match remove_registered_file(path, &state.roots) {
            Ok(()) => false,
            Err(error) => {
                errors.push(format!("file {}: {error}", path.display()));
                true
            }
        });

    state.pending_root_removals.retain(|path| {
        let root = state.roots.iter().find(|root| &root.path == path);
        let Some(root) = root else {
            errors.push(format!("root {} is not owned by this plan", path.display()));
            return true;
        };
        match remove_root(root) {
            Ok(()) => false,
            Err(error) => {
                errors.push(format!("root {}: {error}", path.display()));
                true
            }
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

/// Terminate a process group until no member remains.
///
/// Members are re-enumerated after every signal, so a SIGTERM handler that
/// spawns a child cannot escape: the child inherits the group and is killed in
/// the next pass. A process that already exited is not signalled at all, which
/// also protects against pid reuse.
fn terminate_process(entry: &RegisteredProcess) -> Result<()> {
    if entry.pid <= 1 || entry.pgid <= 1 {
        return Err(FlowError::progress(format!(
            "refusing to signal invalid process {}/{}",
            entry.pid, entry.pgid
        )));
    }
    // SAFETY: getpgrp takes no pointers and has no caller preconditions.
    if entry.pgid == unsafe { libc::getpgrp() } {
        return Err(FlowError::progress(format!(
            "refusing to signal the supervisor's process group {}",
            entry.pgid
        )));
    }
    if group_members(entry.pgid).is_empty() {
        return Ok(());
    }

    signal_group(entry.pgid, libc::SIGTERM);
    if wait_for_group(entry.pgid, 1_000) {
        return Ok(());
    }
    signal_group(entry.pgid, libc::SIGKILL);
    if wait_for_group(entry.pgid, 1_000) {
        Ok(())
    } else {
        Err(FlowError::progress(format!(
            "process group {} still has members: {:?}",
            entry.pgid,
            group_members(entry.pgid)
        )))
    }
}

fn signal_group(pgid: i32, signal: i32) {
    // SAFETY: kill with a negated group id takes only scalars; pgid > 1 and
    // differs from our own group (checked by the caller).
    unsafe {
        libc::kill(-pgid, signal);
    }
}

fn wait_for_group(pgid: i32, millis: u64) -> bool {
    let deadline = std::time::Instant::now() + std::time::Duration::from_millis(millis);
    loop {
        if group_members(pgid).is_empty() {
            return true;
        }
        if std::time::Instant::now() >= deadline {
            return group_members(pgid).is_empty();
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
}

/// Live (non-zombie) processes whose process group is `pgid`.
fn group_members(pgid: i32) -> Vec<i32> {
    let mut members = Vec::new();
    let Ok(entries) = std::fs::read_dir("/proc") else {
        return members;
    };
    for entry in entries.flatten() {
        let Some(name) = entry.file_name().to_str().map(str::to_string) else {
            continue;
        };
        let Ok(pid) = name.parse::<i32>() else {
            continue;
        };
        if process_group_of(pid) == Some(pgid) && !is_zombie(pid) {
            members.push(pid);
        }
    }
    members.sort_unstable();
    members
}

fn process_group_of(pid: i32) -> Option<i32> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    // After comm: state ppid pgrp ...
    stat.rsplit_once(')')?
        .1
        .split_whitespace()
        .nth(2)?
        .parse()
        .ok()
}

fn is_zombie(pid: i32) -> bool {
    let Ok(stat) = std::fs::read_to_string(format!("/proc/{pid}/stat")) else {
        return false;
    };
    matches!(
        stat.rsplit_once(')')
            .and_then(|(_, rest)| rest.split_whitespace().next())
            .and_then(|state| state.chars().next()),
        Some('Z')
    )
}

/// Remove a registered file by unlinking its entry, never its symlink target.
fn remove_registered_file(file: &Path, roots: &[PendingDirectory]) -> Result<()> {
    let Some(root) = owner_of(file, roots) else {
        return Err(FlowError::progress(format!(
            "registered file {} is outside every owned root",
            file.display()
        )));
    };
    let relative = file.strip_prefix(&root.path).map_err(|_| {
        FlowError::progress(format!(
            "{} is not beneath {}",
            file.display(),
            root.path.display()
        ))
    })?;
    let Some(name) = relative.file_name() else {
        return Err(FlowError::progress(format!(
            "refusing to unlink a root {}",
            file.display()
        )));
    };
    let parent_relative = relative.parent().unwrap_or_else(|| Path::new(""));

    let root_dir = open_dir_nofollow(&root.path)?;
    verify_root_identity(&root_dir, root)?;
    let parent = open_beneath(&root_dir, parent_relative)?;
    match kind_at(&parent, name)? {
        None => Ok(()),
        // Removing the link itself is always safe: the target is untouched.
        Some(EntryKind::Symlink) | Some(EntryKind::File) | Some(EntryKind::Other) => {
            unlink_at(&parent, name)
        }
        Some(EntryKind::Directory) => Err(FlowError::progress(format!(
            "registered file {} became a directory; refusing to unlink",
            file.display()
        ))),
    }
}

fn owner_of<'a>(path: &Path, roots: &'a [PendingDirectory]) -> Option<&'a PendingDirectory> {
    roots
        .iter()
        .find(|root| util::is_relative_to(path, &root.path))
}

/// Remove a root after verifying its recorded `(device, inode)` identity.
fn remove_root(root: &PendingDirectory) -> Result<()> {
    let Some(parent_path) = root.path.parent() else {
        return Err(FlowError::progress(format!(
            "{} has no parent directory",
            root.path.display()
        )));
    };
    let Some(name) = root.path.file_name() else {
        return Err(FlowError::progress(format!(
            "{} has no file name",
            root.path.display()
        )));
    };

    let parent = open_dir_nofollow(parent_path)?;
    match kind_at(&parent, name)? {
        None => Ok(()),
        Some(EntryKind::Symlink) => Err(FlowError::progress(format!(
            "{} was replaced by a symlink; refusing to follow it",
            root.path.display()
        ))),
        Some(EntryKind::Directory) => {
            let actual = identity_at(&parent, name)?.ok_or_else(|| {
                FlowError::progress(format!("{} vanished during cleanup", root.path.display()))
            })?;
            if let Some(expected) = root.identity {
                if actual != expected {
                    return Err(FlowError::progress(format!(
                        "{} was replaced by a different directory (inode {} -> {}); refusing to remove",
                        root.path.display(),
                        expected.ino,
                        actual.ino
                    )));
                }
            }
            widen_directories(&root.path);
            match std::fs::remove_dir_all(&root.path) {
                Ok(()) => Ok(()),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
                Err(first) => {
                    unmount_under(&root.path);
                    std::fs::remove_dir_all(&root.path).map_err(|second| {
                        FlowError::progress(format!(
                            "cannot remove {}: {first} (after retry: {second})",
                            root.path.display()
                        ))
                    })
                }
            }
        }
        Some(EntryKind::File) | Some(EntryKind::Other) => Err(FlowError::progress(format!(
            "{} was replaced by a non-directory; refusing to remove",
            root.path.display()
        ))),
    }
}

fn verify_root_identity(root_dir: &File, root: &PendingDirectory) -> Result<()> {
    let metadata = root_dir.metadata()?;
    let actual = RootIdentity::of(&metadata);
    match root.identity {
        Some(expected) if actual != expected => Err(FlowError::progress(format!(
            "{} identity changed (inode {} -> {}); refusing to operate",
            root.path.display(),
            expected.ino,
            actual.ino
        ))),
        _ => Ok(()),
    }
}

/// Add owner write/execute bits to directories only.
///
/// Files are never `chmod`-ed: unlinking only needs write permission on the
/// parent directory, and touching a file by hard link would otherwise change
/// the mode of a delivery copy that shares the inode.
fn widen_directories(dir: &Path) {
    use std::os::unix::fs::PermissionsExt;

    let Ok(metadata) = std::fs::symlink_metadata(dir) else {
        return;
    };
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return;
    }
    let mut permissions = metadata.permissions();
    if permissions.mode() & 0o700 != 0o700 {
        permissions.set_mode(permissions.mode() | 0o700);
        let _ = std::fs::set_permissions(dir, permissions);
    }

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
            widen_directories(&path);
        }
    }
}

/// Remove a worktree through its recorded repository and verify the result.
fn remove_worktree(worktree: &WorktreeRegistration, roots: &[PendingDirectory]) -> Result<()> {
    if !roots
        .iter()
        .any(|root| util::is_relative_to(&worktree.path, &root.path))
    {
        return Err(FlowError::progress(format!(
            "worktree {} is outside every owned root",
            worktree.path.display()
        )));
    }

    let Some(repo) = worktree.repo.as_ref() else {
        // No repository was discoverable at registration; the tree itself is
        // still removed by the root pass, so nothing is left behind.
        return Ok(());
    };
    if !repo.exists() {
        // Without the repository there is no registration to remove; the
        // working tree, if any, is still handled by the root pass.
        return Ok(());
    }

    // Remove the working tree from the repository's records, then prune any
    // stale administrative entry. Failures are verified, not assumed.
    let _ = std::process::Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["worktree", "remove", "--force", "--force"])
        .arg(&worktree.path)
        .output();
    let _ = std::process::Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["worktree", "prune"])
        .output();

    if worktree_registered(repo, &worktree.path)? {
        return Err(FlowError::progress(format!(
            "repository {} still lists worktree {}",
            repo.display(),
            worktree.path.display()
        )));
    }
    Ok(())
}

/// Whether `git -C repo worktree list` still contains `path`.
fn worktree_registered(repo: &Path, path: &Path) -> Result<bool> {
    let output = std::process::Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["worktree", "list", "--porcelain"])
        .output()
        .map_err(|error| {
            FlowError::progress(format!(
                "cannot query worktrees of {}: {error}",
                repo.display()
            ))
        })?;
    if !output.status.success() {
        return Err(FlowError::progress(format!(
            "git worktree list failed in {}: {}",
            repo.display(),
            String::from_utf8_lossy(&output.stderr).trim()
        )));
    }
    let expected = util::resolve_path(path);
    let text = String::from_utf8_lossy(&output.stdout);
    Ok(text
        .lines()
        .filter_map(|line| line.strip_prefix("worktree "))
        .any(|listed| util::resolve_path(Path::new(listed.trim())) == expected))
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
