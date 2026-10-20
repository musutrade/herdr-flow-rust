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
mod env;
pub(crate) use env::confine_renames_to;
mod registry;

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
/// `workspace_root` and `temp_root` must be absent or empty: the environment
/// only ever destroys directories it created. Delivery targets must resolve
/// outside both roots.
pub fn create_environment(
    task_id: &str,
    workspace_root: &std::path::Path,
    delivery_targets: Vec<DeliveryTarget>,
    temp_root: &std::path::Path,
) -> Result<TaskEnvironment> {
    env::create(task_id, workspace_root, delivery_targets, temp_root)
}

/// Reopen the environment belonging to `task_id`.
///
/// Restoring never allocates a second environment: it returns the registered
/// document, including one left in `creating` or `destroying` by a crash.
pub fn restore_environment(task_id: &str) -> Result<TaskEnvironment> {
    restore(task_id)
}

/// Destroy an environment: release every resource, then free its slot.
///
/// Idempotent. A failed attempt keeps the plan and the slot, and can be
/// resumed with [`cleanup_failed_environment`].
pub fn destroy_environment(environment: &TaskEnvironment) -> Result<()> {
    cleanup::destroy(environment)
}
