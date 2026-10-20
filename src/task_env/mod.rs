//! Task environment lifecycle management.
//!
//! Establishes clear resource ownership from task creation: agreements on formal
//! deliverables in target locations, and all task work within disposable environments.
//! Provides create, restore, stop, and destroy operations with crash recovery.

mod boundary;
mod cleanup;
mod env;
mod registry;

pub use boundary::{validate_path, DeliveryTarget, ResourceBoundary};
pub use cleanup::{cleanup_failed_environment, CleanupState};
pub use env::{TaskEnvironment, EnvironmentState};
pub use registry::{EnvironmentRegistry, register_environment, unregister_environment};

use crate::error::Result;

/// Create a new task environment with the given identity and targets.
pub fn create_environment(
    task_id: &str,
    workspace_root: &std::path::Path,
    delivery_targets: Vec<DeliveryTarget>,
    temp_root: &std::path::Path,
) -> Result<TaskEnvironment> {
    env::create(task_id, workspace_root, delivery_targets, temp_root)
}

/// Restore an existing task environment from its persistent state.
pub fn restore_environment(task_id: &str, state_root: &std::path::Path) -> Result<TaskEnvironment> {
    env::restore(task_id, state_root)
}

/// Destroy a task environment, releasing all resources.
pub fn destroy_environment(env: &TaskEnvironment) -> Result<()> {
    cleanup::destroy(env)
}

/// Check if a path is within the environment's disposable boundary.
pub fn is_disposable(env: &TaskEnvironment, path: &std::path::Path) -> bool {
    boundary::is_within_disposable(env, path)
}

/// Check if a path is a valid delivery target.
pub fn is_delivery_target(env: &TaskEnvironment, path: &std::path::Path) -> bool {
    boundary::is_delivery(env, path)
}
