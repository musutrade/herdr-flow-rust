//! Environment cleanup and destruction with idempotent operations.

use std::path::{Path, PathBuf};
use std::process::Command;
use serde::{Deserialize, Serialize};

use crate::error::{FlowError, Result};
use crate::util;
use super::env::TaskEnvironment;
use super::registry;

/// State tracking for cleanup operations.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CleanupState {
    /// Task ID being cleaned up.
    pub task_id: String,
    
    /// Remaining worktrees to clean.
    pub pending_worktrees: Vec<PathBuf>,
    
    /// Remaining processes to terminate.
    pub pending_processes: Vec<i32>,
    
    /// Remaining files to close/remove.
    pub pending_files: Vec<PathBuf>,
    
    /// Directories to remove.
    pub pending_directories: Vec<PathBuf>,
    
    /// Cleanup started at.
    pub started_at: f64,
    
    /// Last progress timestamp.
    pub updated_at: f64,
    
    /// Cleanup attempts count.
    pub attempts: u32,
    
    /// Last error message.
    pub last_error: Option<String>,
}

impl CleanupState {
    /// Create initial cleanup state from environment.
    fn from_environment(env: &TaskEnvironment) -> Self {
        let now = util::now();
        
        Self {
            task_id: env.task_id.clone(),
            pending_worktrees: env.worktree_registrations.clone(),
            pending_processes: env.child_processes.clone(),
            pending_files: env.open_files.clone(),
            pending_directories: vec![
                env.workspace_root.clone(),
                env.temp_root.clone(),
            ],
            started_at: now,
            updated_at: now,
            attempts: 0,
            last_error: None,
        }
    }
    
    /// Check if cleanup is complete.
    fn is_complete(&self) -> bool {
        self.pending_worktrees.is_empty() &&
        self.pending_processes.is_empty() &&
        self.pending_files.is_empty() &&
        self.pending_directories.is_empty()
    }
    
    /// Record a cleanup attempt.
    fn record_attempt(&mut self, error: Option<String>) {
        self.attempts += 1;
        self.updated_at = util::now();
        self.last_error = error;
    }
}

/// Terminate a process by PID.
fn terminate_process(pid: i32) -> Result<()> {
    // Send SIGTERM first
    unsafe {
        if libc::kill(pid, libc::SIGTERM) == 0 {
            // Wait a bit for graceful shutdown
            std::thread::sleep(std::time::Duration::from_millis(100));
            
            // Check if still alive
            if libc::kill(pid, 0) == 0 {
                // Still alive, send SIGKILL
                libc::kill(pid, libc::SIGKILL);
                std::thread::sleep(std::time::Duration::from_millis(50));
            }
            
            Ok(())
        } else {
            // Process might already be dead (ESRCH), which is fine
            let err = std::io::Error::last_os_error();
            if err.raw_os_error() == Some(libc::ESRCH) {
                Ok(())
            } else {
                Err(FlowError::progress(&format!(
                    "failed to terminate process {}: {}",
                    pid, err
                )))
            }
        }
    }
}

/// Remove a Git worktree registration.
fn remove_worktree(worktree_path: &Path) -> Result<()> {
    // Find the .git file or directory to determine the main repo
    if !worktree_path.exists() {
        // Already removed, that's fine
        return Ok(());
    }
    
    let git_file = worktree_path.join(".git");
    if !git_file.exists() {
        // Not a worktree or already cleaned
        return Ok(());
    }
    
    // Try to remove the worktree using git
    let output = Command::new("git")
        .args(&["worktree", "remove", "--force"])
        .arg(worktree_path)
        .output()
        .map_err(|e| FlowError::progress(&format!(
            "failed to run git worktree remove: {}", e
        )))?;
    
    if !output.status.success() {
        // If git command fails, try manual cleanup
        // Read .git file to find main repo
        if git_file.is_file() {
            let content = std::fs::read_to_string(&git_file).ok();
            if let Some(gitdir_line) = content {
                if let Some(gitdir) = gitdir_line.strip_prefix("gitdir: ") {
                    let gitdir = gitdir.trim();
                    let worktree_admin = Path::new(gitdir);
                    
                    // Remove the administrative files in main repo
                    if worktree_admin.exists() {
                        std::fs::remove_dir_all(worktree_admin).ok();
                    }
                }
            }
        }
    }
    
    Ok(())
}

/// Remove a directory and all its contents, handling common issues.
fn remove_directory(dir: &Path) -> Result<()> {
    if !dir.exists() {
        return Ok(());
    }
    
    // First, try to make everything writable
    let _ = make_writable_recursive(dir);
    
    // Try normal removal
    match std::fs::remove_dir_all(dir) {
        Ok(()) => Ok(()),
        Err(e) => {
            // If removal fails, try harder
            
            // On Linux, check if there are any mounts
            if cfg!(target_os = "linux") {
                let _ = unmount_any(dir);
            }
            
            // Try again
            std::fs::remove_dir_all(dir)
                .map_err(|e2| FlowError::progress(&format!(
                    "failed to remove directory {:?}: {} (retry: {})",
                    dir, e, e2
                )))
        }
    }
}

/// Make a directory and all its contents writable.
fn make_writable_recursive(dir: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    
    if dir.is_dir() {
        // Make directory writable
        if let Ok(metadata) = std::fs::metadata(dir) {
            let mut perms = metadata.permissions();
            perms.set_mode(perms.mode() | 0o700);
            let _ = std::fs::set_permissions(dir, perms);
        }
        
        // Recurse into children
        if let Ok(entries) = std::fs::read_dir(dir) {
            for entry in entries.flatten() {
                let path = entry.path();
                if path.is_dir() {
                    let _ = make_writable_recursive(&path);
                } else {
                    // Make file writable
                    if let Ok(metadata) = std::fs::metadata(&path) {
                        let mut perms = metadata.permissions();
                        perms.set_mode(perms.mode() | 0o600);
                        let _ = std::fs::set_permissions(&path, perms);
                    }
                }
            }
        }
    }
    
    Ok(())
}

/// Unmount any mounts under a directory (Linux only).
#[cfg(target_os = "linux")]
fn unmount_any(dir: &Path) -> Result<()> {
    // Read /proc/mounts to find any mounts under this directory
    let mounts = std::fs::read_to_string("/proc/mounts").ok();
    if let Some(mounts) = mounts {
        let dir_str = dir.to_string_lossy();
        for line in mounts.lines() {
            let parts: Vec<&str> = line.split_whitespace().collect();
            if parts.len() >= 2 {
                let mount_point = parts[1];
                if mount_point.starts_with(dir_str.as_ref()) {
                    // Try to unmount
                    let _ = Command::new("umount")
                        .arg("-f")
                        .arg(mount_point)
                        .output();
                }
            }
        }
    }
    Ok(())
}

#[cfg(not(target_os = "linux"))]
fn unmount_any(_dir: &Path) -> Result<()> {
    Ok(())
}

/// Destroy a task environment, releasing all resources.
pub fn destroy(env: &TaskEnvironment) -> Result<()> {
    let mut state = CleanupState::from_environment(env);
    
    // Perform cleanup
    let result = cleanup_resources(&mut state);
    
    // If cleanup succeeded, remove from registry
    if state.is_complete() {
        // Try to unregister (ignore error if already unregistered)
        let _ = registry::unregister_environment(&env.task_id);
        
        // Remove state file
        if env.state_file().exists() {
            std::fs::remove_file(env.state_file())
                .map_err(|e| FlowError::progress(&format!(
                    "failed to remove state file: {}", e
                )))?;
        }
        
        Ok(())
    } else {
        // Cleanup failed, save cleanup state for retry
        save_cleanup_state(&env.task_id, &state)?;
        result
    }
}

/// Perform resource cleanup operations.
fn cleanup_resources(state: &mut CleanupState) -> Result<()> {
    let mut errors = Vec::new();
    
    // Terminate processes
    state.pending_processes.retain(|&pid| {
        match terminate_process(pid) {
            Ok(()) => false,
            Err(e) => {
                errors.push(format!("process {}: {}", pid, e));
                true
            }
        }
    });
    
    // Remove worktrees
    state.pending_worktrees.retain(|worktree| {
        match remove_worktree(worktree) {
            Ok(()) => false,
            Err(e) => {
                errors.push(format!("worktree {:?}: {}", worktree, e));
                true
            }
        }
    });
    
    // Remove directories
    state.pending_directories.retain(|dir| {
        match remove_directory(dir) {
            Ok(()) => false,
            Err(e) => {
                errors.push(format!("directory {:?}: {}", dir, e));
                true
            }
        }
    });
    
    // Clear pending files (they should be gone with directories)
    state.pending_files.clear();
    
    if errors.is_empty() {
        state.record_attempt(None);
        Ok(())
    } else {
        let error_msg = format!("cleanup failures: {}", errors.join("; "));
        state.record_attempt(Some(error_msg.clone()));
        Err(FlowError::progress(&error_msg))
    }
}

/// Save cleanup state for later retry.
fn save_cleanup_state(task_id: &str, state: &CleanupState) -> Result<()> {
    let path = cleanup_state_path(task_id)?;
    let value = serde_json::to_value(state)
        .map_err(|e| FlowError::progress(&format!("failed to serialize cleanup state: {}", e)))?;
    
    util::atomic_write_json(&path, &value)
}

/// Load cleanup state for retry.
fn load_cleanup_state(task_id: &str) -> Result<CleanupState> {
    let path = cleanup_state_path(task_id)?;
    
    if !path.exists() {
        return Err(FlowError::progress(&format!(
            "no cleanup state for task {}",
            task_id
        )));
    }
    
    let content = std::fs::read_to_string(&path)
        .map_err(|e| FlowError::progress(&format!("failed to read cleanup state: {}", e)))?;
    
    serde_json::from_str(&content)
        .map_err(|e| FlowError::progress(&format!("failed to parse cleanup state: {}", e)))
}

/// Get cleanup state file path.
fn cleanup_state_path(task_id: &str) -> Result<PathBuf> {
    let home = std::env::var("HOME")
        .map_err(|_| FlowError::progress("HOME environment variable not set"))?;
    
    // Ensure parent directories exist with secure permissions
    let herdr_dir = Path::new(&home).join(".herdr");
    ensure_secure_dir(&herdr_dir)?;
    
    let flow_dir = herdr_dir.join("flow");
    ensure_secure_dir(&flow_dir)?;
    
    let cleanup_dir = flow_dir.join("cleanup");
    ensure_secure_dir(&cleanup_dir)?;
    
    Ok(cleanup_dir.join(format!("{}.json", task_id)))
}

/// Ensure a directory exists with secure permissions (0700).
fn ensure_secure_dir(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    
    if !path.exists() {
        std::fs::create_dir(path)
            .map_err(|e| FlowError::progress(&format!(
                "failed to create directory: {}", e
            )))?;
    }
    
    let metadata = std::fs::metadata(path)
        .map_err(|e| FlowError::progress(&format!("failed to get metadata: {}", e)))?;
    
    let mut perms = metadata.permissions();
    perms.set_mode(0o700);
    
    std::fs::set_permissions(path, perms)
        .map_err(|e| FlowError::progress(&format!("failed to set permissions: {}", e)))
}

/// Retry cleanup for a failed environment.
pub fn cleanup_failed_environment(task_id: &str) -> Result<()> {
    let mut state = load_cleanup_state(task_id)?;
    
    // Check if too many attempts
    if state.attempts >= 5 {
        return Err(FlowError::progress(&format!(
            "cleanup for task {} failed after {} attempts",
            task_id, state.attempts
        )));
    }
    
    // Perform cleanup
    let result = cleanup_resources(&mut state);
    
    if state.is_complete() {
        // Success, remove cleanup state
        let path = cleanup_state_path(task_id)?;
        if path.exists() {
            std::fs::remove_file(&path)
                .map_err(|e| FlowError::progress(&format!(
                    "failed to remove cleanup state: {}", e
                )))?;
        }
        
        // Try to unregister (ignore error if already unregistered)
        let _ = registry::unregister_environment(task_id);
        
        Ok(())
    } else {
        // Still failed, save updated state
        save_cleanup_state(task_id, &state)?;
        result
    }
}
