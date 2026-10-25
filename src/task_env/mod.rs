//! Task environment lifecycle: identity, disposable boundary, delivery targets
//! and crash-safe destruction.
//!
//! An environment owns two disposable roots (workspace and temporary), keeps
//! formal delivery targets outside them, occupies one resource slot while it
//! lives, and is destroyed as a unit. Adapters receive the environment and use
//! [`validate_path`] before writing, so work cannot escape into unmanaged
//! directories or into a delivery target.

mod boundary;
mod cleanup;
pub mod cli;
pub(crate) mod discovery;
mod env;
pub mod instance;
pub(crate) use env::PreparedConfinement;
mod registry;
mod status;
pub use status::status;

pub use boundary::{
    is_delivery, is_within_disposable, paths_overlap, validate_delivery_targets, validate_path,
    DeliveryTarget, ResourceBoundary,
};
pub use cleanup::{
    cleanup_failed_environment, resume_delivery_cleanup, CleanupState, Quarantine, MAX_ATTEMPTS,
};
pub use cleanup::{quiesce_environment, PendingDirectory};
pub use env::{
    discover_repository, marked_processes, process_has_marker, process_is_alive,
    process_record_is_alive, process_start_time, recover_interrupted, restore, validate_ancestors,
    EnvironmentState, ProcessRecord, RegisteredProcess, RootIdentity, TaskEnvironment,
    WorktreeRegistration, MARKER_ENV,
};
pub use registry::{
    claim_roots_exclusively, cleanup_state_path, environment_state_path, interrupted_tasks,
    is_registered, limit, list_registered, load_registry, occupied_slots, register_environment,
    registry_path, state_root, unregister_environment, validate_task_id, EnvironmentRegistry,
    Registry, RegistryEntry, DEFAULT_LIMIT,
};

use crate::error::Result;

/// Create a new task environment and reserve its resource slot.
///
/// `workspace_root` and `temp_root` must be absent or empty and non-overlapping.
/// Accepted roots, including pre-existing empty directories, are claimed as
/// disposable and may be removed by destruction. Delivery targets must resolve
/// outside both roots and roots outside the control tree. The returned handle
/// holds the exclusive task lock. Failure may leave recovery state; use
/// [`recover_interrupted`] before retrying creation. Spawning/confinement and
/// cleanup require the supported Linux /proc, Landlock and pidfd facilities.
pub fn create_environment(
    task_id: &str,
    workspace_root: &std::path::Path,
    delivery_targets: Vec<DeliveryTarget>,
    temp_root: &std::path::Path,
) -> Result<TaskEnvironment> {
    env::create(task_id, workspace_root, delivery_targets, temp_root)
}

#[cfg(test)]
static TEST_ENV_ROOT_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[cfg(test)]
pub(crate) struct TestEnvRootGuard {
    previous: Option<std::ffi::OsString>,
    _lock: std::sync::MutexGuard<'static, ()>,
}

#[cfg(test)]
pub(crate) fn test_env_root(path: &std::path::Path) -> TestEnvRootGuard {
    let lock = TEST_ENV_ROOT_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let previous = std::env::var_os("HERDR_TASK_ENV_ROOT");
    std::env::set_var("HERDR_TASK_ENV_ROOT", path);
    TestEnvRootGuard {
        previous,
        _lock: lock,
    }
}

#[cfg(test)]
impl Drop for TestEnvRootGuard {
    fn drop(&mut self) {
        if let Some(previous) = self.previous.take() {
            std::env::set_var("HERDR_TASK_ENV_ROOT", previous);
        } else {
            std::env::remove_var("HERDR_TASK_ENV_ROOT");
        }
    }
}

/// Reopen the environment belonging to `task_id`.
///
/// Restoring never allocates a second environment: it returns the registered
/// document, including one left in `creating` or `destroying` by a crash.
/// Acquires exclusive ownership and fails while another handle owns the task.
/// Does not restart processes, reopen persisted file paths, or finish cleanup;
/// use [`recover_interrupted`] when destruction/recovery is intended.
pub fn restore_environment(task_id: &str) -> Result<TaskEnvironment> {
    restore(task_id)
}

/// Destroy an environment: release every resource, then free its slot.
///
/// Idempotent. A failed attempt keeps the plan and the slot, and can be
/// resumed with [`cleanup_failed_environment`]. Loads durable registration so a
/// stale handle cannot omit later resources. May stop/kill owned processes,
/// close local descriptors, remove disposable trees and unregister worktrees;
/// failures do not roll back those effects. Process-stage errors defer worktree,
/// file and root removal, preserving their cleanup obligations for retry.
/// Call from an external supervisor,
/// not a task-owned process. Do not free claims or delete journals after errors.
pub fn destroy_environment(environment: &TaskEnvironment) -> Result<()> {
    cleanup::destroy(environment)
}
