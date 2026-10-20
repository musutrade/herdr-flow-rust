//! Crash-safe, descriptor-anchored destruction of a task environment.
//!
//! Deletion never trusts a path that was merely checked earlier:
//!
//! * a disposable root is opened once with `O_NOFOLLOW`, its `(device, inode)`
//!   is compared with the value recorded at creation, and the tree is then
//!   emptied through that pinned descriptor. Renaming or replacing the path
//!   afterwards cannot redirect a single unlink;
//! * registered files are unlinked relative to the same pinned walk, so a leaf
//!   swapped for a symlink loses only the link, never its target;
//! * permissions are widened with `fchmod` on directories only, so a hard link
//!   can never expose a delivery file to a mode change;
//! * processes are frozen (`SIGSTOP`) before being enumerated, and the
//!   descendant set captured at registration is included. A descendant that
//!   calls `setsid()` or is spawned in a `SIGTERM` handler is therefore still
//!   attributed to the environment and killed;
//! * worktrees are removed through the repository recorded at registration and
//!   verified with `git worktree list`; a leftover record keeps the item
//!   pending instead of reporting success.
//!
//! A plan is persisted before anything is touched and removed only after every
//! step succeeded. A failure keeps the plan and the resource slot; retries go
//! through [`cleanup_failed_environment`], and items whose resources are
//! already gone are treated as complete so a crash mid-retry can finish.

use std::fs::File;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::error::{FlowError, Result};
use crate::util;

use super::env::{
    clear_directory, descendants_snapshot, kind_at, open_beneath, open_child_dir,
    open_dir_nofollow, process_is_alive, process_record_is_alive, process_start_time, unlink_at,
    unlink_at_dir, EntryKind, EnvironmentState, ProcessRecord, RegisteredProcess, RootIdentity,
    TaskEnvironment, WorktreeRegistration,
};
use super::registry;

/// A disposable root that the environment owns.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PendingDirectory {
    pub path: PathBuf,
    /// `None` only for a document written before creation completed; such a
    /// root is never removed on identity alone.
    pub identity: Option<RootIdentity>,
}

/// Durable cleanup plan.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CleanupState {
    pub task_id: String,
    pub instance_id: String,
    /// Every root the document knows about.
    pub roots: Vec<PendingDirectory>,
    pub pending_processes: Vec<RegisteredProcess>,
    pub pending_worktrees: Vec<WorktreeRegistration>,
    pub pending_files: Vec<PathBuf>,
    /// Roots still to release; a subset of `roots`.
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
        let mut pending_root_removals = Vec::new();

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

            let effective_identity = match identity {
                Some(identity) => Some(identity),
                None => reclaim_unclaimed_empty_root(&path, &env.task_id, &env.instance_id)?,
            };
            if effective_identity.is_some() {
                pending_root_removals.push(path.clone());
            }
            roots.push(PendingDirectory {
                path,
                identity: effective_identity,
            });
        }

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

/// A root without a recorded identity is only reclaimed when it is empty and no
/// other live task claims it. This is the recovery path for a crash between the
/// create intent and the root claim; it must never touch another task's data.
fn reclaim_unclaimed_empty_root(
    path: &Path,
    task_id: &str,
    instance_id: &str,
) -> Result<Option<RootIdentity>> {
    // The emptiness check and the ownership claim are one registry-locked
    // operation, so a concurrent task cannot claim the root in between.
    if !registry::claim_root_exclusively(task_id, instance_id, path)? {
        return Ok(None);
    }
    match std::fs::symlink_metadata(path) {
        Ok(metadata) => Ok(Some(RootIdentity::of(&metadata))),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error.into()),
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
/// The plan is built from the durable document, not from the caller's handle,
/// so a handle that is a stale snapshot cannot omit resources registered later.
pub fn destroy(env: &TaskEnvironment) -> Result<()> {
    registry::validate_task_id(&env.task_id)?;

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
            let Some(doc) = document.as_ref() else {
                env.release_ownership();
                drop(transient_lock);
                return Ok(());
            };
            let plan = CleanupState::from_environment(doc)?;
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
        drop(transient_lock);
        return Ok(());
    }

    save_plan(&state)?;
    if let Some(doc) = document.as_mut() {
        doc.state = EnvironmentState::DestroyFailed;
        let _ = doc.persist();
    }
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

    // Orphans adopted by the subreaper after registration are not in any
    // snapshot taken earlier; attribute them here, excluding identities owned
    // by other live environments.
    let claimed = registry::claimed_processes(&state.instance_id).unwrap_or_default();
    let baselines: Vec<ProcessRecord> = state
        .pending_processes
        .iter()
        .flat_map(|process| process.direct_children.iter().cloned())
        .collect();
    let self_pid = std::process::id() as i32;
    let adopted: Vec<ProcessRecord> = super::env::direct_children_of(self_pid)
        .into_iter()
        .filter(|child| !contains_record(&baselines, child))
        .filter(|child| !contains_record(&claimed, child))
        .filter(|child| !state.pending_processes.iter().any(|p| p.pid == child.pid))
        .collect();
    for child in &adopted {
        if let Err(error) = terminate_adopted(child) {
            errors.push(format!("adopted process {}: {error}", child.pid));
        }
    }

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

fn contains_record(records: &[ProcessRecord], candidate: &ProcessRecord) -> bool {
    records.iter().any(|record| {
        record.pid == candidate.pid
            && match (record.start_time, candidate.start_time) {
                (Some(left), Some(right)) => left == right,
                _ => true,
            }
    })
}

/// Freeze and kill a registered process together with every descendant we can
/// attribute to it.
///
/// `SIGSTOP` comes first so no `SIGTERM` handler can spawn a new child, and the
/// descendant snapshot taken at registration is included so a process that
/// called `setsid()` or was orphaned by an early-exiting parent is still found.
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

    let mut targets: Vec<ProcessRecord> = entry.descendants.clone();
    if process_is_alive(entry) {
        targets.push(ProcessRecord {
            pid: entry.pid,
            start_time: entry.start_time,
        });
        signal_group(entry.pgid, libc::SIGSTOP);
        targets.extend(descendants_snapshot(entry.pid));
    }
    for record in &entry.descendants {
        if process_record_is_alive(record) {
            targets.extend(descendants_snapshot(record.pid));
        }
    }
    for pid in group_members(entry.pgid) {
        targets.push(ProcessRecord {
            pid,
            start_time: process_start_time(pid),
        });
    }
    targets.sort_by_key(|record| record.pid);
    targets.dedup_by_key(|record| record.pid);

    if targets
        .iter()
        .all(|record| !process_record_is_alive(record))
    {
        return Ok(());
    }

    for record in &targets {
        // SAFETY: pids are positive scalars; a dead pid is a no-op.
        unsafe {
            libc::kill(record.pid, libc::SIGSTOP);
        }
    }
    for record in &targets {
        // SAFETY: pids are positive scalars; a dead pid is a no-op.
        unsafe {
            libc::kill(record.pid, libc::SIGKILL);
        }
    }
    signal_group(entry.pgid, libc::SIGKILL);

    let deadline = std::time::Instant::now() + std::time::Duration::from_millis(2_000);
    loop {
        let survivors: Vec<i32> = targets
            .iter()
            .filter(|record| process_record_is_alive(record))
            .map(|record| record.pid)
            .collect();
        if survivors.is_empty() && group_members(entry.pgid).is_empty() {
            return Ok(());
        }
        if std::time::Instant::now() >= deadline {
            return Err(FlowError::progress(format!(
                "processes survived SIGKILL: {survivors:?}"
            )));
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
}

fn signal_group(pgid: i32, signal: i32) {
    // SAFETY: kill with a negated group id takes only scalars; pgid > 1 and
    // differs from our own group.
    unsafe {
        libc::kill(-pgid, signal);
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
    match std::fs::symlink_metadata(&root.path) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error.into()),
        Ok(metadata) if metadata.file_type().is_symlink() => {
            return Err(FlowError::progress(format!(
                "root {} was replaced by a symlink; refusing to unlink",
                root.path.display()
            )))
        }
        Ok(_) => {}
    }
    if root.identity.is_none() {
        return Err(FlowError::progress(format!(
            "root {} has no recorded ownership; refusing to unlink {}",
            root.path.display(),
            file.display()
        )));
    }

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

/// Remove a root after pinning it by descriptor and verifying `(dev, ino)`.
///
/// The tree is emptied relative to that descriptor. If the path is renamed or
/// replaced after this point, the replacement is left alone.
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
        None => return Ok(()),
        Some(EntryKind::Symlink) => {
            return Err(FlowError::progress(format!(
                "{} was replaced by a symlink; refusing to follow it",
                root.path.display()
            )))
        }
        Some(EntryKind::Directory) => {}
        Some(_) => {
            return Err(FlowError::progress(format!(
                "{} was replaced by a non-directory; refusing to remove",
                root.path.display()
            )))
        }
    }

    let Some(dir) = open_child_dir(&parent, name)? else {
        return Err(FlowError::progress(format!(
            "{} changed while being opened",
            root.path.display()
        )));
    };
    let actual = RootIdentity::of(&dir.metadata()?);
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

    unmount_under(&root.path);
    clear_directory(&dir, 0)?;

    // Re-verify the name before removing it: the path may have been renamed or
    // replaced while the tree was being emptied. A replacement is never
    // removed, and an incomplete removal keeps the plan (and slot) alive.
    match kind_at(&parent, name)? {
        None => Ok(()),
        Some(EntryKind::Directory) => {
            let Some(check) = open_child_dir(&parent, name)? else {
                return Err(FlowError::progress(format!(
                    "{} changed while being removed",
                    root.path.display()
                )));
            };
            let actual = RootIdentity::of(&check.metadata()?);
            if let Some(expected) = root.identity {
                if actual != expected {
                    return Err(FlowError::progress(format!(
                        "{} was renamed or replaced during cleanup; slot retained",
                        root.path.display()
                    )));
                }
            }
            unlink_at_dir(&parent, name)
        }
        Some(_) => Err(FlowError::progress(format!(
            "{} was replaced by a non-directory during cleanup; slot retained",
            root.path.display()
        ))),
    }
}

/// Freeze and kill an adopted orphan and its descendants.
fn terminate_adopted(record: &ProcessRecord) -> Result<()> {
    if !process_record_is_alive(record) {
        return Ok(());
    }
    let mut targets = descendants_snapshot(record.pid);
    targets.push(record.clone());
    targets.sort_by_key(|item| item.pid);
    targets.dedup_by_key(|item| item.pid);
    targets.retain(process_record_is_alive);
    if targets.is_empty() {
        return Ok(());
    }
    for item in &targets {
        // SAFETY: pids are positive scalars; a dead pid is a no-op.
        unsafe {
            libc::kill(item.pid, libc::SIGSTOP);
        }
    }
    for item in &targets {
        // SAFETY: pids are positive scalars; a dead pid is a no-op.
        unsafe {
            libc::kill(item.pid, libc::SIGKILL);
        }
    }
    let deadline = std::time::Instant::now() + std::time::Duration::from_millis(2_000);
    loop {
        // These orphans were reparented to us, so we are also responsible for
        // reaping them; otherwise they linger as zombies and keep a pid.
        for item in &targets {
            // SAFETY: waitpid with WNOHANG takes scalars; a non-child returns
            // ECHILD and a still-running child returns 0.
            unsafe {
                libc::waitpid(item.pid, std::ptr::null_mut(), libc::WNOHANG);
            }
        }
        let survivors: Vec<i32> = targets
            .iter()
            .filter(|item| process_record_is_alive(item))
            .map(|item| item.pid)
            .collect();
        if survivors.is_empty() {
            return Ok(());
        }
        if std::time::Instant::now() >= deadline {
            return Err(FlowError::progress(format!(
                "adopted processes survived SIGKILL: {survivors:?}"
            )));
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
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
        return Ok(());
    };
    if !repo.exists() {
        return Ok(());
    }

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
