//! Crash-safe, descriptor-anchored destruction of a task environment.
//!
//! Ownership is proven, never inferred:
//!
//! * every process the environment starts carries a unique marker in its
//!   environment ([`super::env::MARKER_ENV`]). The marker is inherited across
//!   `fork`/`exec`, survives `setsid()` and remains readable at
//!   `/proc/<pid>/environ` after the process is orphaned. Cleanup kills exactly
//!   the marked processes and their current descendants, so it cannot hit an
//!   unrelated child or another environment's orphan;
//! * registered files are unlinked relative to an `O_NOFOLLOW` walk, so a leaf
//!   swapped for a symlink loses only the link;
//! * a disposable root is pinned by descriptor and `(device, inode)`; the tree
//!   is emptied through that descriptor, permissions are widened with `fchmod`
//!   on directories only, and the final directory entry is removed through a
//!   random quarantine name whose identity is re-verified. A rename or
//!   replacement keeps the plan and slot instead of deleting unknown data or
//!   reporting a false success;
//! * worktrees are removed through the repository recorded at registration and
//!   verified with `git worktree list`.
//!
//! A plan is persisted before anything is touched and removed only after every
//! step succeeded. A failure keeps the plan and the resource slot; retries go
//! through [`cleanup_failed_environment`], and items whose resources are
//! already gone count as complete so a crash mid-retry can finish.

use std::ffi::{CString, OsStr};
use std::fs::File;
use std::os::fd::AsRawFd;
use std::os::unix::ffi::OsStrExt as _;
use std::os::unix::fs::MetadataExt as _;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::error::{FlowError, Result};
use crate::util;

use super::env::{
    clear_directory, descendants_snapshot, kind_at, open_beneath, open_child_dir,
    open_dir_nofollow, process_is_alive, process_record_is_alive, unlink_at, unlink_at_dir,
    EntryKind, EnvironmentState, ProcessRecord, RegisteredProcess, RootIdentity, TaskEnvironment,
    WorktreeRegistration,
};
use super::registry;

/// A disposable root that the environment owns.
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
    /// Every root this environment owns.
    pub roots: Vec<PendingDirectory>,
    pub pending_processes: Vec<RegisteredProcess>,
    pub pending_worktrees: Vec<WorktreeRegistration>,
    pub pending_files: Vec<PathBuf>,
    /// Roots still to release; a subset of `roots`.
    pub pending_root_removals: Vec<PathBuf>,
    /// Root whose removal was committed to disk before unlinking. A crash
    /// between unlink and the next save is completed on retry; a missing root
    /// without this marker was renamed externally and keeps the slot.
    #[serde(default)]
    pub removal_in_progress: Option<PathBuf>,
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

        // Roots without a recorded identity come from an interrupted create.
        // Claim them atomically in the registry before considering removal, so
        // a concurrent task cannot take them in between.
        let unclaimed: Vec<PathBuf> = env
            .roots()
            .into_iter()
            .filter(|(_, identity)| identity.is_none())
            .map(|(path, _)| path)
            .collect();
        let claimed = if unclaimed.is_empty() {
            Vec::new()
        } else {
            registry::claim_roots_exclusively(&env.task_id, &env.instance_id, &unclaimed)?
        };

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

            let identity = match identity {
                Some(identity) => identity,
                None if claimed.iter().any(|root| root == &path) => {
                    match std::fs::symlink_metadata(&path) {
                        Ok(metadata) => RootIdentity::of(&metadata),
                        Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
                        Err(error) => return Err(error.into()),
                    }
                }
                // Never recorded and never claimed: not ours, leave it alone.
                None => continue,
            };
            pending_root_removals.push(path.clone());
            roots.push(PendingDirectory {
                path,
                identity: Some(identity),
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
            removal_in_progress: None,
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
/// The plan is built from the durable document, not from the caller's handle,
/// so a stale handle cannot omit resources registered later.
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

    // Positive attribution: every process started by this environment carries
    // its marker, including descendants that detached or were orphaned.
    let mut targets = super::env::marked_processes(&state.instance_id);
    for record in targets.clone() {
        targets.extend(descendants_snapshot(record.pid));
    }
    if let Err(error) = terminate_records(&mut targets) {
        errors.push(format!("processes: {error}"));
    }
    // A registered process that is still alive but unmarked cannot be proven
    // ours; never signal it, and never report the slot as clean.
    state.pending_processes.retain(|entry| {
        if process_is_alive(entry) {
            errors.push(format!(
                "registered process {} is alive but not marked by this environment",
                entry.pid
            ));
            true
        } else {
            false
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

    let pending = std::mem::take(&mut state.pending_root_removals);
    let mut remaining = Vec::new();
    for path in pending {
        let Some(root) = state.roots.iter().find(|root| root.path == path) else {
            errors.push(format!("root {} is not owned by this plan", path.display()));
            remaining.push(path);
            continue;
        };
        let committed = state.removal_in_progress.as_deref() == Some(path.as_path());
        if committed && !path.exists() {
            // We committed to removing this root before the crash; the name is
            // gone, so the removal completed.
            state.removal_in_progress = None;
            continue;
        }
        // Persist the commitment before deleting so a crash here is provable.
        state.removal_in_progress = Some(path.clone());
        if let Err(error) = save_plan(state) {
            errors.push(format!("root {}: {error}", path.display()));
            remaining.push(path);
            continue;
        }
        match remove_root(root) {
            Ok(()) => {
                state.removal_in_progress = None;
            }
            Err(error) => {
                errors.push(format!("root {}: {error}", path.display()));
                remaining.push(path);
            }
        }
    }
    state.pending_root_removals = remaining;

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

/// Freeze, kill and verify a set of positively-identified processes.
fn terminate_records(targets: &mut Vec<ProcessRecord>) -> Result<()> {
    targets.sort_by_key(|record| record.pid);
    targets.dedup_by_key(|record| record.pid);
    targets.retain(process_record_is_alive);
    if targets.is_empty() {
        return Ok(());
    }

    // Freeze everything first so no process can spawn or reparent a new child
    // between enumeration and termination.
    for record in targets.iter() {
        // SAFETY: pids are positive scalars; a dead pid is a no-op.
        unsafe {
            libc::kill(record.pid, libc::SIGSTOP);
        }
    }
    for record in targets.iter() {
        // SAFETY: pids are positive scalars; a dead pid is a no-op.
        unsafe {
            libc::kill(record.pid, libc::SIGKILL);
        }
    }

    let deadline = std::time::Instant::now() + std::time::Duration::from_millis(2_000);
    loop {
        let survivors: Vec<i32> = targets
            .iter()
            .filter(|record| process_record_is_alive(record))
            .map(|record| record.pid)
            .collect();
        if survivors.is_empty() {
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
    let Some(expected) = root.identity else {
        return Err(FlowError::progress(format!(
            "root {} has no recorded ownership; refusing to unlink {}",
            root.path.display(),
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
    let actual = RootIdentity::of(&root_dir.metadata()?);
    if actual != expected {
        return Err(FlowError::progress(format!(
            "{} identity changed; refusing to unlink",
            root.path.display()
        )));
    }
    let parent = open_beneath(&root_dir, parent_relative)?;
    match kind_at(&parent, name)? {
        None => Ok(()),
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

/// Remove a root through a pinned descriptor and a quarantine name.
///
/// The final directory entry is never unlinked by its original name. It is
/// renamed to an unpredictable name and its identity is verified there before
/// unlinking, so a path swapped at the last moment is moved aside and left
/// alone instead of being deleted. Any uncertainty keeps the slot.
fn remove_root(root: &PendingDirectory) -> Result<()> {
    let Some(expected) = root.identity else {
        return Err(FlowError::progress(format!(
            "root {} has no recorded identity; refusing to remove",
            root.path.display()
        )));
    };
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
        None => {
            return Err(FlowError::progress(format!(
                "{} is missing; ownership cannot be confirmed",
                root.path.display()
            )))
        }
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
    if actual != expected {
        return Err(FlowError::progress(format!(
            "{} was replaced by a different directory (inode {} -> {}); refusing to remove",
            root.path.display(),
            expected.ino,
            actual.ino
        )));
    }

    unmount_under(&root.path);
    clear_directory(&dir, 0)?;
    if dir.metadata()?.nlink() == 0 {
        return Ok(());
    }

    let quarantine = format!(".herdr-rm-{}", util::uuid_hex());
    let quarantine_os = OsStr::new(&quarantine);
    match rename_noreplace(&parent, name, quarantine_os) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return if dir.metadata()?.nlink() == 0 {
                Ok(())
            } else {
                Err(FlowError::progress(format!(
                    "{} was renamed during cleanup; slot retained",
                    root.path.display()
                )))
            };
        }
        Err(error) => {
            return Err(FlowError::progress(format!(
                "cannot quarantine {}: {error}",
                root.path.display()
            )))
        }
    }

    // Whatever landed under the quarantine name must be our inode before it
    // may be unlinked. A replacement is moved back untouched.
    match kind_at(&parent, quarantine_os)? {
        Some(EntryKind::Directory) => {
            let Some(check) = open_child_dir(&parent, quarantine_os)? else {
                return Err(FlowError::progress("quarantined entry vanished"));
            };
            if RootIdentity::of(&check.metadata()?) != expected {
                let _ = rename_noreplace(&parent, quarantine_os, name);
                return Err(FlowError::progress(format!(
                    "{} was replaced during cleanup; replacement preserved and slot retained",
                    root.path.display()
                )));
            }
            unlink_at_dir(&parent, quarantine_os)?;
            if dir.metadata()?.nlink() != 0 {
                return Err(FlowError::progress(format!(
                    "{} is still linked elsewhere; slot retained",
                    root.path.display()
                )));
            }
            Ok(())
        }
        Some(_) => {
            let _ = rename_noreplace(&parent, quarantine_os, name);
            Err(FlowError::progress(format!(
                "{} was replaced by a non-directory during cleanup; slot retained",
                root.path.display()
            )))
        }
        None => Err(FlowError::progress(format!(
            "quarantined copy of {} vanished; slot retained",
            root.path.display()
        ))),
    }
}

/// Atomically rename `from` to `to` in the same directory, never replacing `to`.
fn rename_noreplace(parent: &File, from: &OsStr, to: &OsStr) -> std::io::Result<()> {
    let from = CString::new(from.as_bytes())
        .map_err(|_| std::io::Error::from(std::io::ErrorKind::InvalidInput))?;
    let to = CString::new(to.as_bytes())
        .map_err(|_| std::io::Error::from(std::io::ErrorKind::InvalidInput))?;
    // SAFETY: both names are NUL-terminated and parent is a live directory fd.
    let result = unsafe {
        libc::syscall(
            libc::SYS_renameat2,
            parent.as_raw_fd(),
            from.as_ptr(),
            parent.as_raw_fd(),
            to.as_ptr(),
            libc::RENAME_NOREPLACE,
        )
    };
    if result == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
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
