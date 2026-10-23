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
//!   on directories only, and the final directory entry follows a durable,
//!   phased quarantine protocol. A rename, replacement or a missing quarantine
//!   at any phase keeps the plan and slot instead of reporting a false success;
//! * worktrees are removed through the repository recorded at registration and
//!   verified with `git worktree list`.
//!
//! A plan is persisted before anything is touched and removed only after every
//! step succeeded. A failure keeps the plan and the resource slot; retries go
//! through [`cleanup_failed_environment`], and items whose resources are
//! already gone count as complete so a crash mid-retry can finish.

mod observation;
mod signals;

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
    clear_directory, kind_at, open_beneath, open_child_dir, open_dir_nofollow, unlink_at,
    unlink_at_dir, EntryKind, EnvironmentState, ProcessRecord, RegisteredProcess, RootIdentity,
    TaskEnvironment, WorktreeRegistration,
};
use super::registry;

/// A disposable root that the environment owns.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PendingDirectory {
    pub path: PathBuf,
    pub identity: Option<RootIdentity>,
}

/// A root moved aside under an unpredictable name while it is being removed.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Quarantine {
    pub root: PathBuf,
    pub name: String,
    /// Set immediately before the unlink. A crash after the unlink is
    /// completed on retry; a missing quarantine without this flag is external
    /// interference and keeps the slot.
    #[serde(default)]
    pub unlink_committed: bool,
}

/// Durable cleanup plan.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CleanupState {
    pub task_id: String,
    pub instance_id: String,
    /// Minimal proof that a delivery-confirmed environment entered cleanup.
    #[serde(default)]
    pub delivery_confirmed: bool,
    /// Every root this environment owns.
    pub roots: Vec<PendingDirectory>,
    pub pending_processes: Vec<RegisteredProcess>,
    pub pending_worktrees: Vec<WorktreeRegistration>,
    pub pending_files: Vec<PathBuf>,
    /// Roots still to release; a subset of `roots`.
    pub pending_root_removals: Vec<PathBuf>,
    /// Recorded quarantine of the root currently being removed.
    #[serde(default)]
    pub quarantine: Option<Quarantine>,
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
        validate_environment_roots(env)?;
        let claimed = claim_missing_roots(env)?;
        let roots = build_roots(env, &claimed)?;
        let pending_root_removals = roots.iter().map(|root| root.path.clone()).collect();
        Ok(Self {
            task_id: env.task_id.clone(),
            instance_id: env.instance_id.clone(),
            delivery_confirmed: env.delivery["phase"] == "cleaning"
                && env.delivery["shared_removed"] == true,
            roots,
            pending_processes: env.child_processes.clone(),
            pending_worktrees: env.worktree_registrations.clone(),
            pending_files: env.open_files.clone(),
            pending_root_removals,
            quarantine: None,
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
            && self.quarantine.is_none()
    }

    fn record(&mut self, error: Option<String>) {
        self.attempts += 1;
        self.updated_at = util::now();
        self.last_error = error;
    }
}

fn validate_environment_roots(env: &TaskEnvironment) -> Result<()> {
    for (path, _) in env.roots() {
        registry::validate_outside_control(&path)?;
    }
    Ok(())
}

fn claim_missing_roots(env: &TaskEnvironment) -> Result<Vec<PathBuf>> {
    let unclaimed: Vec<PathBuf> = env
        .roots()
        .into_iter()
        .filter(|(_, identity)| identity.is_none())
        .map(|(path, _)| path)
        .collect();
    if unclaimed.is_empty() {
        return Ok(Vec::new());
    }
    registry::claim_roots_exclusively(&env.task_id, &env.instance_id, &unclaimed)
}

fn build_roots(env: &TaskEnvironment, claimed: &[PathBuf]) -> Result<Vec<PendingDirectory>> {
    let mut roots = Vec::new();
    for (path, identity) in env.roots() {
        ensure_outside_deliveries(&path, &env.delivery_targets)?;
        let Some(identity) = effective_identity(&path, identity, claimed)? else {
            continue;
        };
        roots.push(PendingDirectory {
            path,
            identity: Some(identity),
        });
    }
    Ok(roots)
}

fn ensure_outside_deliveries(
    path: &Path,
    targets: &[super::boundary::DeliveryTarget],
) -> Result<()> {
    for target in targets {
        if util::overlap(path, &target.path) {
            return Err(FlowError::progress(format!(
                "refusing to destroy: disposable root {} overlaps delivery target {}",
                path.display(),
                target.path.display()
            )));
        }
    }
    Ok(())
}

fn effective_identity(
    path: &Path,
    identity: Option<RootIdentity>,
    claimed: &[PathBuf],
) -> Result<Option<RootIdentity>> {
    match identity {
        Some(identity) => Ok(Some(identity)),
        None if claimed.iter().any(|root| root == path) => identity_of_existing(path),
        None => Ok(None),
    }
}

fn identity_of_existing(path: &Path) -> Result<Option<RootIdentity>> {
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
    let state: CleanupState = serde_json::from_str(&text).map_err(|error| {
        FlowError::progress(format!(
            "cannot parse cleanup plan {}: {error}",
            path.display()
        ))
    })?;
    // Existing plans bypass from_environment, including retries with no
    // environment document. Validate before any cleanup or completion side
    // effect, even when the plan's pending collections are already empty.
    for root in &state.roots {
        registry::validate_outside_control(&root.path)?;
    }
    Ok(state)
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
/// On Linux, signals require pidfd_open/pidfd_send_signal support and a known
/// process start time. Missing authority or syscall errors fail closed and keep
/// the cleanup plan/slot for retry; there is no numeric-PID signal fallback.
/// Already exited signal targets are idempotently complete. This function may
/// stop/kill owned processes before a later cleanup error; it is not a rollback.
pub fn destroy(env: &TaskEnvironment) -> Result<()> {
    registry::validate_task_id(&env.task_id)?;
    let transient_lock = acquire_destroy_lock(env)?;
    let state_file = registry::environment_state_path(&env.task_id)?;
    let mut document = load_document(&state_file)?;
    ensure_document_instance(&document, &env.instance_id)?;
    let Some(mut state) = load_or_build_plan(env, &document)? else {
        registry::finish_cleanup(&env.task_id)?;
        env.release_ownership();
        drop(transient_lock);
        return Ok(());
    };

    let result = destroy_plan(env, &mut document, &mut state);
    env.release_ownership();
    drop(transient_lock);
    result
}
fn destroy_plan(
    env: &TaskEnvironment,
    document: &mut Option<TaskEnvironment>,
    state: &mut CleanupState,
) -> Result<()> {
    if let Some(doc) = document.as_mut() {
        doc.mark_destroying()?;
    }
    env.release_file_handles();
    let outcome = run_cleanup(state);
    if state.is_complete() && outcome.is_ok() {
        return finish(&env.task_id, &env.instance_id);
    }
    persist_failure(state, document);
    outcome
}

fn acquire_destroy_lock(env: &TaskEnvironment) -> Result<Option<File>> {
    if env.owns_lock() {
        return Ok(None);
    }
    registry::try_lock_task(&env.task_id)?
        .map(Some)
        .ok_or_else(|| {
            FlowError::progress(format!(
                "environment for task {} is owned by another live handle",
                env.task_id
            ))
        })
}

fn load_document(state_file: &Path) -> Result<Option<TaskEnvironment>> {
    match std::fs::read_to_string(state_file) {
        Ok(text) => {
            let env = serde_json::from_str::<TaskEnvironment>(&text).map_err(|error| {
                FlowError::progress(format!("cannot parse {}: {error}", state_file.display()))
            })?;
            Ok(Some(env))
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error.into()),
    }
}

fn ensure_document_instance(document: &Option<TaskEnvironment>, instance_id: &str) -> Result<()> {
    let Some(doc) = document.as_ref() else {
        return Ok(());
    };
    if doc.instance_id == instance_id {
        return Ok(());
    }
    Err(FlowError::progress(format!(
        "task {} now belongs to environment instance {}; refusing to destroy it from a stale handle",
        doc.task_id, doc.instance_id
    )))
}

fn load_or_build_plan(
    env: &TaskEnvironment,
    document: &Option<TaskEnvironment>,
) -> Result<Option<CleanupState>> {
    if plan_exists(&env.task_id) {
        let plan = load_plan(&env.task_id)?;
        ensure_plan_instance(&plan, &env.instance_id)?;
        return Ok(Some(plan));
    }
    let Some(doc) = document.as_ref() else {
        return Ok(None);
    };
    let plan = CleanupState::from_environment(doc)?;
    save_plan(&plan)?;
    Ok(Some(plan))
}

fn ensure_plan_instance(plan: &CleanupState, instance_id: &str) -> Result<()> {
    if plan.instance_id == instance_id {
        return Ok(());
    }
    Err(FlowError::progress(format!(
        "cleanup plan for task {} belongs to another environment instance",
        plan.task_id
    )))
}

fn mark_document(document: &mut Option<TaskEnvironment>, state: EnvironmentState) {
    if let Some(doc) = document.as_mut() {
        doc.state = state;
        let _ = doc.persist();
    }
}

fn persist_failure(state: &CleanupState, document: &mut Option<TaskEnvironment>) {
    let _ = save_plan(state);
    mark_document(document, EnvironmentState::DestroyFailed);
}

/// Resume a failed cleanup for `task_id`.
pub fn cleanup_failed_environment(task_id: &str) -> Result<()> {
    resume_cleanup(task_id, false)
}

/// Resume the plan-only tail of a confirmed delivery, without redispatch.
pub fn resume_delivery_cleanup(task_id: &str) -> Result<()> {
    resume_cleanup(task_id, true)
}

fn resume_cleanup(task_id: &str, require_delivery: bool) -> Result<()> {
    registry::validate_task_id(task_id)?;
    let _lock = registry::try_lock_task(task_id)?.ok_or_else(|| {
        FlowError::progress(format!(
            "environment for task {task_id} is owned by another live handle"
        ))
    })?;

    let mut state = load_plan(task_id)?;
    validate_retry(&state, require_delivery)?;
    resume_plan(task_id, &mut state)
}
fn validate_retry(state: &CleanupState, require_delivery: bool) -> Result<()> {
    if require_delivery && !state.delivery_confirmed {
        return Err(FlowError::progress(
            "cleanup plan does not attest confirmed delivery",
        ));
    }
    if state.attempts >= MAX_ATTEMPTS {
        return Err(FlowError::progress(format!(
            "cleanup for task {task} already failed {} times; manual attention required",
            state.attempts,
            task = state.task_id
        )));
    }

    Ok(())
}
fn resume_plan(task_id: &str, state: &mut CleanupState) -> Result<()> {
    let outcome = run_cleanup(state);
    if state.is_complete() && outcome.is_ok() {
        finish(task_id, &state.instance_id)?;
        return Ok(());
    }
    save_plan(state)?;
    outcome
}

/// Remove the plan, the document and the index entry for this instance only.
fn finish(task_id: &str, instance_id: &str) -> Result<()> {
    registry::unregister_environment(task_id, instance_id)?;
    remove_document_if_current(task_id, instance_id)?;
    registry::finish_cleanup(task_id)
}

fn remove_document_if_current(task_id: &str, instance_id: &str) -> Result<()> {
    let state_file = registry::environment_state_path(task_id)?;
    let document = load_document(&state_file)?;
    ensure_document_instance(&document, instance_id)?;
    util::remove_file_if_exists(&state_file)
}

fn run_cleanup(state: &mut CleanupState) -> Result<()> {
    let mut errors = Vec::new();
    collect_process_errors(state, &mut errors);
    // A surviving writer can keep deleted files open or recreate paths while
    // cleanup removes them. Keep every filesystem obligation intact until the
    // process stage succeeds, including on plan-only retries.
    if !errors.is_empty() {
        return finish_cleanup(state, errors);
    }
    // A worktree removal also removes its directories. Unlink registered
    // files first while their descriptor-anchored parent walk still exists.
    collect_file_errors(state, &mut errors);
    collect_worktree_errors(state, &mut errors);
    if let Err(error) = release_pending_roots(state) {
        errors.push(format!("roots: {error}"));
    }
    finish_cleanup(state, errors)
}

fn collect_process_errors(state: &mut CleanupState, errors: &mut Vec<String>) {
    if let Err(error) = terminate_marked(&state.instance_id) {
        errors.push(format!("processes: {error}"));
    }
    state
        .pending_processes
        .retain(|entry| retain_live_process(entry, errors));
}

fn retain_live_process(entry: &RegisteredProcess, errors: &mut Vec<String>) -> bool {
    match registered_family_is_alive(entry) {
        Ok(false) => return false,
        Ok(true) => errors.push(format!(
            "registered process {} or a recorded descendant is still alive",
            entry.pid
        )),
        Err(error) => errors.push(error.to_string()),
    };
    true
}

/// Recorded descendants remain a cleanup obligation after their parent exits.
/// This only blocks deletion; it adds no authority to signal an unmarked PID.
fn registered_family_is_alive(entry: &RegisteredProcess) -> std::io::Result<bool> {
    let parent = ProcessRecord {
        pid: entry.pid,
        start_time: entry.start_time,
    };
    for record in std::iter::once(&parent).chain(&entry.descendants) {
        if observation::is_alive(record)? {
            return Ok(true);
        }
    }
    Ok(false)
}

fn collect_worktree_errors(state: &mut CleanupState, errors: &mut Vec<String>) {
    state
        .pending_worktrees
        .retain(|worktree| match remove_worktree(worktree, &state.roots) {
            Ok(()) => false,
            Err(error) => {
                errors.push(format!("worktree {}: {error}", worktree.path.display()));
                true
            }
        });
}

fn collect_file_errors(state: &mut CleanupState, errors: &mut Vec<String>) {
    state
        .pending_files
        .retain(|path| match remove_registered_file(path, &state.roots) {
            Ok(()) => false,
            Err(error) => {
                errors.push(format!("file {}: {error}", path.display()));
                true
            }
        });
}

fn release_pending_roots(state: &mut CleanupState) -> Result<()> {
    // Release one root at a time. The full pending set is persisted before
    // every destructive step, so a crash never loses a root from the plan.
    while let Some(path) = state.pending_root_removals.first().cloned() {
        let root = find_root(state, &path)?;
        cleanup_one_root(state, &root)?;
        state.pending_root_removals.retain(|item| item != &path);
        state.quarantine = None;
        save_plan(state)?;
    }
    Ok(())
}

fn find_root(state: &CleanupState, path: &Path) -> Result<PendingDirectory> {
    state
        .roots
        .iter()
        .find(|root| root.path == path)
        .cloned()
        .ok_or_else(|| {
            FlowError::progress(format!("root {} is not owned by this plan", path.display()))
        })
}

fn finish_cleanup(state: &mut CleanupState, errors: Vec<String>) -> Result<()> {
    if errors.is_empty() {
        state.record(None);
        return Ok(());
    }
    let message = errors.join("; ");
    state.record(Some(message.clone()));
    Err(FlowError::progress(format!(
        "cleanup of task {} incomplete: {message}",
        state.task_id
    )))
}

/// Repeatedly freeze and kill marked processes until a scan finds none.
///
/// A child forked between the scan and the freeze is missed by that pass; it
/// still carries the marker, so the next scan finds it. Each pass freezes the
/// whole set first, so the loop converges instead of racing.
fn terminate_marked(instance_id: &str) -> Result<()> {
    for _ in 0..8 {
        let mut targets = live_marked_targets(instance_id)?;
        if targets.is_empty() {
            return Ok(());
        }
        terminate_records(&mut targets)?;
    }
    ensure_no_marked(instance_id)
}

fn live_marked_targets(instance_id: &str) -> Result<Vec<ProcessRecord>> {
    let mut targets = super::discovery::marked_processes(instance_id)?;
    for record in targets.clone() {
        targets.extend(super::discovery::descendants_snapshot(record.pid)?);
    }
    retain_live_records(&mut targets)?;
    Ok(targets)
}

fn ensure_no_marked(instance_id: &str) -> Result<()> {
    let mut remaining = super::discovery::marked_processes(instance_id)?;
    retain_live_records(&mut remaining)?;
    if remaining.is_empty() {
        Ok(())
    } else {
        Err(FlowError::progress(format!(
            "marked processes keep reappearing: {:?}",
            remaining
                .iter()
                .map(|record| record.pid)
                .collect::<Vec<_>>()
        )))
    }
}

/// Freeze, kill and verify a set of positively-identified processes.
fn terminate_records(targets: &mut Vec<ProcessRecord>) -> Result<()> {
    targets.sort_by_key(|record| record.pid);
    targets.dedup_by_key(|record| record.pid);
    retain_live_records(targets)?;
    if targets.is_empty() {
        return Ok(());
    }
    freeze_targets(targets)?;
    kill_targets(targets)?;
    wait_for_targets(targets)
}

fn freeze_targets(targets: &[ProcessRecord]) -> std::io::Result<()> {
    targets
        .iter()
        .try_for_each(|record| signals::signal(record, libc::SIGSTOP))
}

fn kill_targets(targets: &[ProcessRecord]) -> std::io::Result<()> {
    targets
        .iter()
        .try_for_each(|record| signals::signal(record, libc::SIGKILL))
}

fn wait_for_targets(targets: &[ProcessRecord]) -> Result<()> {
    let deadline = std::time::Instant::now() + std::time::Duration::from_millis(2_000);
    loop {
        let survivors = live_survivors(targets)?;
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

fn live_survivors(targets: &[ProcessRecord]) -> std::io::Result<Vec<i32>> {
    let mut survivors = Vec::new();
    for record in targets {
        if observation::is_alive(record)? {
            survivors.push(record.pid);
        }
    }
    Ok(survivors)
}

fn retain_live_records(targets: &mut Vec<ProcessRecord>) -> std::io::Result<()> {
    let live = live_survivors(targets)?;
    targets.retain(|record| live.contains(&record.pid));
    Ok(())
}

/// Remove a registered file by unlinking its entry, never its symlink target.
fn remove_registered_file(file: &Path, roots: &[PendingDirectory]) -> Result<()> {
    let root = owner_of(file, roots).ok_or_else(|| {
        FlowError::progress(format!(
            "registered file {} is outside every owned root",
            file.display()
        ))
    })?;
    let expected = match prepare_file_root(root)? {
        PreparedRoot::Missing => return Ok(()),
        PreparedRoot::Present(identity) => identity,
    };
    let (parent_relative, name) = relative_leaf(file, &root.path)?;
    let root_dir = open_dir_nofollow(&root.path)?;
    ensure_identity(&root_dir, expected, &root.path)?;
    let parent = open_beneath(&root_dir, parent_relative)?;
    remove_leaf(&parent, name, file)
}

enum PreparedRoot {
    Missing,
    Present(RootIdentity),
}

fn prepare_file_root(root: &PendingDirectory) -> Result<PreparedRoot> {
    match std::fs::symlink_metadata(&root.path) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(PreparedRoot::Missing),
        Err(error) => Err(error.into()),
        Ok(metadata) if metadata.file_type().is_symlink() => Err(FlowError::progress(format!(
            "root {} was replaced by a symlink; refusing to unlink",
            root.path.display()
        ))),
        Ok(_) => match root.identity {
            Some(identity) => Ok(PreparedRoot::Present(identity)),
            None => Err(FlowError::progress(format!(
                "root {} has no recorded ownership; refusing to unlink",
                root.path.display()
            ))),
        },
    }
}

fn relative_leaf<'a>(file: &'a Path, root: &Path) -> Result<(&'a Path, &'a OsStr)> {
    let relative = file.strip_prefix(root).map_err(|_| {
        FlowError::progress(format!(
            "{} is not beneath {}",
            file.display(),
            root.display()
        ))
    })?;
    let name = relative.file_name().ok_or_else(|| {
        FlowError::progress(format!("refusing to unlink a root {}", file.display()))
    })?;
    Ok((relative.parent().unwrap_or_else(|| Path::new("")), name))
}

fn ensure_identity(dir: &File, expected: RootIdentity, path: &Path) -> Result<()> {
    if RootIdentity::of(&dir.metadata()?) == expected {
        return Ok(());
    }
    Err(FlowError::progress(format!(
        "{} identity changed; refusing to operate",
        path.display()
    )))
}

fn remove_leaf(parent: &File, name: &OsStr, file: &Path) -> Result<()> {
    match kind_at(parent, name)? {
        None => Ok(()),
        Some(EntryKind::Directory) => Err(FlowError::progress(format!(
            "registered file {} became a directory; refusing to unlink",
            file.display()
        ))),
        Some(_) => unlink_at(parent, name),
    }
}

fn owner_of<'a>(path: &Path, roots: &'a [PendingDirectory]) -> Option<&'a PendingDirectory> {
    roots
        .iter()
        .find(|root| util::is_relative_to(path, &root.path))
}

/// Advance one root through the phased removal protocol.
///
/// Invariants:
/// * the plan always lists every pending root;
/// * the quarantine name and unlink commit are durable before the operation
///   they describe, so a crash is recoverable;
/// * nothing is deleted unless its recorded `(device, inode)` was verified;
/// * a replaced entry is never moved or deleted.
fn cleanup_one_root(state: &mut CleanupState, root: &PendingDirectory) -> Result<()> {
    let Some(expected) = root.identity else {
        return Err(FlowError::progress(format!(
            "root {} has no recorded identity; refusing to remove",
            root.path.display()
        )));
    };
    let (parent, root_name) = open_root_parent(root)?;
    if let Some(quarantine) = matching_quarantine(state, &root.path) {
        if resume_quarantine(state, &parent, root_name, &quarantine, expected, &root.path)?
            == ResumeOutcome::Done
        {
            return Ok(());
        }
    }
    start_quarantine(state, &parent, root_name, expected, &root.path)
}

#[derive(PartialEq, Eq)]
enum ResumeOutcome {
    Done,
    Restart,
}

fn open_root_parent(root: &PendingDirectory) -> Result<(File, &OsStr)> {
    let parent_path = root.path.parent().ok_or_else(|| {
        FlowError::progress(format!("{} has no parent directory", root.path.display()))
    })?;
    let name = root
        .path
        .file_name()
        .ok_or_else(|| FlowError::progress(format!("{} has no file name", root.path.display())))?;
    Ok((open_dir_nofollow(parent_path)?, name))
}

fn matching_quarantine(state: &CleanupState, root: &Path) -> Option<Quarantine> {
    state.quarantine.clone().filter(|entry| entry.root == root)
}

fn resume_quarantine(
    state: &mut CleanupState,
    parent: &File,
    root_name: &OsStr,
    quarantine: &Quarantine,
    expected: RootIdentity,
    root_path: &Path,
) -> Result<ResumeOutcome> {
    let name = OsStr::new(&quarantine.name);
    match kind_at(parent, name)? {
        Some(EntryKind::Directory) => {
            resume_present_quarantine(state, parent, name, quarantine, expected, root_path)
                .map(|()| ResumeOutcome::Done)
        }
        None if quarantine.unlink_committed => {
            complete_committed(parent, root_name, root_path).map(|()| ResumeOutcome::Done)
        }
        None => recover_missing_quarantine(state, parent, root_name, root_path),
        Some(_) => Err(replaced_quarantine_error(root_path)),
    }
}

fn resume_present_quarantine(
    state: &mut CleanupState,
    parent: &File,
    name: &OsStr,
    quarantine: &Quarantine,
    expected: RootIdentity,
    root_path: &Path,
) -> Result<()> {
    let check = open_child_dir(parent, name)?
        .ok_or_else(|| FlowError::progress(format!("quarantine {} vanished", quarantine.name)))?;
    ensure_identity(&check, expected, root_path)?;
    commit_unlink(state, parent, name, quarantine, &check, root_path)
}

fn complete_committed(parent: &File, root_name: &OsStr, root_path: &Path) -> Result<()> {
    if kind_at(parent, root_name)?.is_some() {
        return Err(FlowError::progress(format!(
            "{} reappeared after its committed removal; slot retained",
            root_path.display()
        )));
    }
    Ok(())
}

fn recover_missing_quarantine(
    state: &mut CleanupState,
    parent: &File,
    root_name: &OsStr,
    root_path: &Path,
) -> Result<ResumeOutcome> {
    if kind_at(parent, root_name)?.is_none() {
        return Err(FlowError::progress(format!(
            "quarantine for {} vanished; slot retained",
            root_path.display()
        )));
    }
    state.quarantine = None;
    save_plan(state)?;
    Ok(ResumeOutcome::Restart)
}

fn replaced_quarantine_error(root_path: &Path) -> FlowError {
    FlowError::progress(format!(
        "quarantine for {} was replaced; slot retained",
        root_path.display()
    ))
}

fn start_quarantine(
    state: &mut CleanupState,
    parent: &File,
    root_name: &OsStr,
    expected: RootIdentity,
    root_path: &Path,
) -> Result<()> {
    let dir = open_verified_root(parent, root_name, expected, root_path)?;
    if pinned_root_is_empty(&dir, root_path)? {
        return Ok(());
    }
    ensure_entry_identity(parent, root_name, expected, root_path)?;
    let quarantine = begin_quarantine(state, root_path)?;
    rename_into_quarantine(parent, root_name, &quarantine, root_path)?;
    finish_quarantine(state, parent, &quarantine, expected, root_path)
}

fn pinned_root_is_empty(dir: &File, root_path: &Path) -> Result<bool> {
    unmount_under(root_path);
    clear_directory(dir, 0)?;
    Ok(dir.metadata()?.nlink() == 0)
}

fn finish_quarantine(
    state: &mut CleanupState,
    parent: &File,
    quarantine: &Quarantine,
    expected: RootIdentity,
    root_path: &Path,
) -> Result<()> {
    let name = OsStr::new(&quarantine.name);
    let check = open_child_dir(parent, name)?
        .ok_or_else(|| FlowError::progress(format!("quarantine {} vanished", quarantine.name)))?;
    ensure_identity(&check, expected, root_path)?;
    commit_unlink(state, parent, name, quarantine, &check, root_path)
}

fn open_verified_root(
    parent: &File,
    root_name: &OsStr,
    expected: RootIdentity,
    root_path: &Path,
) -> Result<File> {
    let dir = open_child_dir(parent, root_name)?.ok_or_else(|| {
        FlowError::progress(format!(
            "{} changed while being opened",
            root_path.display()
        ))
    })?;
    ensure_identity(&dir, expected, root_path)?;
    if matches!(kind_at(parent, root_name)?, Some(EntryKind::Directory)) {
        return Ok(dir);
    }
    Err(FlowError::progress(format!(
        "{} was replaced by a non-directory; refusing to remove",
        root_path.display()
    )))
}

fn ensure_entry_identity(
    parent: &File,
    root_name: &OsStr,
    expected: RootIdentity,
    root_path: &Path,
) -> Result<()> {
    let Some(check) = open_child_dir(parent, root_name)? else {
        return Err(FlowError::progress(format!(
            "{} vanished before quarantine; slot retained",
            root_path.display()
        )));
    };
    ensure_identity(&check, expected, root_path)
}

fn begin_quarantine(state: &mut CleanupState, root_path: &Path) -> Result<Quarantine> {
    let quarantine = Quarantine {
        root: root_path.to_path_buf(),
        name: format!(".herdr-rm-{}", util::uuid_hex()),
        unlink_committed: false,
    };
    state.quarantine = Some(quarantine.clone());
    save_plan(state)?;
    Ok(quarantine)
}

fn rename_into_quarantine(
    parent: &File,
    root_name: &OsStr,
    quarantine: &Quarantine,
    root_path: &Path,
) -> Result<()> {
    match rename_noreplace(parent, root_name, OsStr::new(&quarantine.name)) {
        Ok(()) => Ok(()),
        Err(error) => Err(FlowError::progress(format!(
            "cannot quarantine {}: {error}",
            root_path.display()
        ))),
    }
}

fn commit_unlink(
    state: &mut CleanupState,
    parent: &File,
    name: &OsStr,
    quarantine: &Quarantine,
    check: &File,
    root_path: &Path,
) -> Result<()> {
    if !quarantine.unlink_committed {
        state
            .quarantine
            .as_mut()
            .expect("quarantine")
            .unlink_committed = true;
        save_plan(state)?;
    }
    unlink_at_dir(parent, name)?;
    if check.metadata()?.nlink() != 0 {
        return Err(FlowError::progress(format!(
            "{} is still linked elsewhere; slot retained",
            root_path.display()
        )));
    }
    Ok(())
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
    ensure_worktree_owned(worktree, roots)?;
    let Some(repo) = worktree.repo.as_ref() else {
        return Ok(());
    };
    if !repo.exists() {
        return Ok(());
    }
    run_worktree_removal(repo, &worktree.path);
    if worktree_registered(repo, &worktree.path)? {
        return Err(FlowError::progress(format!(
            "repository {} still lists worktree {}",
            repo.display(),
            worktree.path.display()
        )));
    }
    Ok(())
}

fn ensure_worktree_owned(
    worktree: &WorktreeRegistration,
    roots: &[PendingDirectory],
) -> Result<()> {
    if roots
        .iter()
        .any(|root| util::is_relative_to(&worktree.path, &root.path))
    {
        return Ok(());
    }
    Err(FlowError::progress(format!(
        "worktree {} is outside every owned root",
        worktree.path.display()
    )))
}

fn run_worktree_removal(repo: &Path, worktree: &Path) {
    let _ = std::process::Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["worktree", "remove", "--force", "--force"])
        .arg(worktree)
        .output();
    let _ = std::process::Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["worktree", "prune"])
        .output();
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
    Ok(listing_contains(&output.stdout, &expected))
}

fn listing_contains(stdout: &[u8], expected: &Path) -> bool {
    String::from_utf8_lossy(stdout)
        .lines()
        .filter_map(|line| line.strip_prefix("worktree "))
        .any(|listed| util::resolve_path(Path::new(listed.trim())) == expected)
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

/// Freeze writes and confirm task-exclusive runtimes have exited before shared
/// records are removed. Errors retain the environment and its occupied slot.
pub fn quiesce_environment(env: &mut TaskEnvironment) -> Result<()> {
    if !env.owns_lock() {
        return Err(FlowError::progress(
            "quiesce requires environment ownership",
        ));
    }
    env.mark_destroying()?;
    terminate_marked(&env.instance_id)?;
    for process in &env.child_processes {
        if registered_family_is_alive(process)? {
            return Err(FlowError::progress(
                "registered task process or a recorded descendant is still alive",
            ));
        }
    }
    env.release_file_handles();
    Ok(())
}

#[cfg(test)]
mod signal_identity_tests {
    use super::*;

    #[test]
    fn recorded_family_checks_descendants_without_claiming_supervisor_children() {
        let mut child = std::process::Command::new("sleep")
            .arg("30")
            .spawn()
            .unwrap();
        let pid = child.id() as i32;
        let start = super::super::env::process_start_time(pid).unwrap();
        let live = ProcessRecord {
            pid,
            start_time: Some(start),
        };
        let mut family = RegisteredProcess {
            pid: i32::MAX,
            pgid: i32::MAX,
            start_time: Some(1),
            descendants: vec![ProcessRecord {
                pid,
                start_time: Some(start.wrapping_add(1)),
            }],
            direct_children: vec![live.clone()],
        };
        let stale_or_unrelated = registered_family_is_alive(&family).unwrap();
        family.descendants[0] = live.clone();
        let recorded_live = registered_family_is_alive(&family).unwrap();
        family.descendants[0].start_time = None;
        let missing_identity_blocks = registered_family_is_alive(&family).unwrap();
        family.descendants[0] = live;
        let was_running = child.try_wait().unwrap().is_none();
        let _ = child.kill();
        child.wait().unwrap();
        assert!(!stale_or_unrelated);
        assert!(recorded_live);
        assert!(missing_identity_blocks);
        assert!(was_running);
        assert!(!registered_family_is_alive(&family).unwrap());
    }

    #[test]
    fn signal_stage_rejects_stale_process_identity() {
        // Model an identity becoming stale after the earlier discovery/filter
        // stage. Only a child owned by this test is ever signalled.
        let mut child = std::process::Command::new("sleep")
            .arg("30")
            .spawn()
            .unwrap();
        let pid = child.id() as i32;
        let start = super::super::env::process_start_time(pid).unwrap();
        let stale = ProcessRecord {
            pid,
            start_time: Some(start.wrapping_add(1)),
        };
        assert!(!observation::is_alive(&stale).unwrap());
        kill_targets(&[stale]).unwrap();
        let deadline = std::time::Instant::now() + std::time::Duration::from_millis(100);
        let mut exited = false;
        while std::time::Instant::now() < deadline {
            if child.try_wait().unwrap().is_some() {
                exited = true;
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        let _ = child.kill();
        child.wait().unwrap();
        assert!(
            !exited,
            "signal stage killed a process with a mismatched start time"
        );
    }
}
