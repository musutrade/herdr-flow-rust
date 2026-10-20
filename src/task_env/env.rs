//! Task environment core data structures and operations.

use std::path::{Path, PathBuf};
use serde::{Deserialize, Serialize};

use crate::error::{FlowError, Result};
use crate::util;
use super::boundary::DeliveryTarget;
use super::registry;

/// The lifecycle state of a task environment.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EnvironmentState {
    /// Environment is being created.
    Creating,
    /// Environment is active and usable.
    Active,
    /// Environment is being destroyed.
    Destroying,
    /// Environment destruction failed, needs cleanup.
    DestroyFailed,
}

/// A task environment with clear resource boundaries and lifecycle management.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskEnvironment {
    /// Unique task identifier.
    pub task_id: String,
    
    /// Root directory for task workspace (disposable).
    pub workspace_root: PathBuf,
    
    /// Root directory for temporary files (disposable).
    pub temp_root: PathBuf,
    
    /// Formal delivery targets (protected, outside disposable boundary).
    pub delivery_targets: Vec<DeliveryTarget>,
    
    /// Current lifecycle state.
    pub state: EnvironmentState,
    
    /// Creation timestamp.
    pub created_at: f64,
    
    /// Last state change timestamp.
    pub updated_at: f64,
    
    /// Git worktree registrations that need cleanup.
    pub worktree_registrations: Vec<PathBuf>,
    
    /// Active child process PIDs that need termination.
    pub child_processes: Vec<i32>,
    
    /// Open file handles that need closing (by path).
    pub open_files: Vec<PathBuf>,
    
    /// State file location.
    state_file: PathBuf,
}

impl TaskEnvironment {
    /// Get the state file path for this environment.
    pub fn state_file(&self) -> &Path {
        &self.state_file
    }
    
    /// Mark the environment as active.
    pub fn mark_active(&mut self, now: f64) -> Result<()> {
        self.state = EnvironmentState::Active;
        self.updated_at = now;
        self.persist()
    }
    
    /// Mark the environment as destroying.
    pub fn mark_destroying(&mut self, now: f64) -> Result<()> {
        self.state = EnvironmentState::Destroying;
        self.updated_at = now;
        self.persist()
    }
    
    /// Mark the environment destruction as failed.
    pub fn mark_destroy_failed(&mut self, now: f64) -> Result<()> {
        self.state = EnvironmentState::DestroyFailed;
        self.updated_at = now;
        self.persist()
    }
    
    /// Register a worktree that needs cleanup.
    pub fn register_worktree(&mut self, path: PathBuf) -> Result<()> {
        if !self.worktree_registrations.contains(&path) {
            self.worktree_registrations.push(path);
            self.persist()?;
        }
        Ok(())
    }
    
    /// Register a child process that needs termination.
    pub fn register_process(&mut self, pid: i32) -> Result<()> {
        if !self.child_processes.contains(&pid) {
            self.child_processes.push(pid);
            self.persist()?;
        }
        Ok(())
    }
    
    /// Register an open file that needs closing.
    pub fn register_file(&mut self, path: PathBuf) -> Result<()> {
        if !self.open_files.contains(&path) {
            self.open_files.push(path);
            self.persist()?;
        }
        Ok(())
    }
    
    /// Unregister a worktree after successful cleanup.
    pub fn unregister_worktree(&mut self, path: &Path) -> Result<()> {
        self.worktree_registrations.retain(|p| p != path);
        self.persist()
    }
    
    /// Unregister a process after termination.
    pub fn unregister_process(&mut self, pid: i32) -> Result<()> {
        self.child_processes.retain(|&p| p != pid);
        self.persist()
    }
    
    /// Unregister a file after closing.
    pub fn unregister_file(&mut self, path: &Path) -> Result<()> {
        self.open_files.retain(|p| p != path);
        self.persist()
    }
    
    /// Persist the environment state to disk.
    fn persist(&self) -> Result<()> {
        let value = serde_json::to_value(self)
            .map_err(|e| FlowError::progress(&format!("failed to serialize environment: {}", e)))?;
        
        util::atomic_write_json(&self.state_file, &value)
            .map_err(|e| FlowError::progress(&format!("failed to persist environment: {}", e)))
    }
    
    /// Load an environment from its state file.
    fn load(state_file: &Path) -> Result<Self> {
        let content = std::fs::read_to_string(state_file)
            .map_err(|e| FlowError::progress(&format!("failed to read environment state: {}", e)))?;
        
        let mut env: TaskEnvironment = serde_json::from_str(&content)
            .map_err(|e| FlowError::progress(&format!("failed to parse environment state: {}", e)))?;
        
        env.state_file = state_file.to_path_buf();
        Ok(env)
    }
}

/// Create a new task environment.
pub fn create(
    task_id: &str,
    workspace_root: &Path,
    delivery_targets: Vec<DeliveryTarget>,
    temp_root: &Path,
) -> Result<TaskEnvironment> {
    let now = util::now();
    
    // Validate delivery targets are outside disposable boundaries
    for target in &delivery_targets {
        if util::is_relative_to(&target.path, workspace_root) {
            return Err(FlowError::progress(&format!(
                "delivery target {:?} is inside disposable workspace {:?}",
                target.path, workspace_root
            )));
        }
        if util::is_relative_to(&target.path, temp_root) {
            return Err(FlowError::progress(&format!(
                "delivery target {:?} is inside disposable temp {:?}",
                target.path, temp_root
            )));
        }
    }
    
    // Create workspace directory
    if !workspace_root.exists() {
        std::fs::create_dir_all(workspace_root)
            .map_err(|e| FlowError::progress(&format!(
                "failed to create workspace root: {}", e
            )))?;
    }
    
    // Create temp directory
    if !temp_root.exists() {
        std::fs::create_dir_all(temp_root)
            .map_err(|e| FlowError::progress(&format!(
                "failed to create temp root: {}", e
            )))?;
    }
    
    // Determine state file location (in a persistent location outside disposable areas)
    let state_file = registry::environment_state_path(task_id)?;
    
    // Check if environment already exists
    if state_file.exists() {
        return Err(FlowError::progress(&format!(
            "environment for task {} already exists",
            task_id
        )));
    }
    
    let mut env = TaskEnvironment {
        task_id: task_id.to_string(),
        workspace_root: workspace_root.to_path_buf(),
        temp_root: temp_root.to_path_buf(),
        delivery_targets,
        state: EnvironmentState::Creating,
        created_at: now,
        updated_at: now,
        worktree_registrations: Vec::new(),
        child_processes: Vec::new(),
        open_files: Vec::new(),
        state_file: state_file.clone(),
    };
    
    // Persist initial state
    env.persist()?;
    
    // Register in global registry
    registry::register_environment(task_id, &state_file)?;
    
    // Mark as active
    env.mark_active(now)?;
    
    Ok(env)
}

/// Restore an existing task environment.
pub fn restore(task_id: &str, _state_root: &Path) -> Result<TaskEnvironment> {
    let state_file = registry::environment_state_path(task_id)?;
    
    if !state_file.exists() {
        return Err(FlowError::progress(&format!(
            "environment for task {} does not exist",
            task_id
        )));
    }
    
    TaskEnvironment::load(&state_file)
}
