//! Global environment registry for tracking active task environments.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use serde::{Deserialize, Serialize};

use crate::error::{FlowError, Result};
use crate::util;

/// Registry entry for a task environment.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct RegistryEntry {
    task_id: String,
    state_file: PathBuf,
    registered_at: f64,
}

/// Global registry of task environments.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct Registry {
    entries: BTreeMap<String, RegistryEntry>,
}

impl Registry {
    fn new() -> Self {
        Self {
            entries: BTreeMap::new(),
        }
    }
    
    fn add(&mut self, task_id: String, state_file: PathBuf, now: f64) {
        self.entries.insert(task_id.clone(), RegistryEntry {
            task_id,
            state_file,
            registered_at: now,
        });
    }
    
    fn remove(&mut self, task_id: &str) -> bool {
        self.entries.remove(task_id).is_some()
    }
    
    fn get(&self, task_id: &str) -> Option<&RegistryEntry> {
        self.entries.get(task_id)
    }
    
    fn contains(&self, task_id: &str) -> bool {
        self.entries.contains_key(task_id)
    }
}

/// Get the registry file path.
fn registry_path() -> Result<PathBuf> {
    // Store in ~/.herdr/flow/task_environments/registry.json
    let home = std::env::var("HOME")
        .map_err(|_| FlowError::progress("HOME environment variable not set"))?;
    
    // Ensure parent directories exist with secure permissions
    let herdr_dir = Path::new(&home).join(".herdr");
    ensure_secure_dir(&herdr_dir)?;
    
    let flow_dir = herdr_dir.join("flow");
    ensure_secure_dir(&flow_dir)?;
    
    let registry_dir = flow_dir.join("task_environments");
    ensure_secure_dir(&registry_dir)?;
    
    Ok(registry_dir.join("registry.json"))
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

/// Load the registry from disk.
fn load_registry() -> Result<Registry> {
    let path = registry_path()?;
    
    if !path.exists() {
        return Ok(Registry::new());
    }
    
    let content = std::fs::read_to_string(&path)
        .map_err(|e| FlowError::progress(&format!("failed to read registry: {}", e)))?;
    
    serde_json::from_str(&content)
        .map_err(|e| FlowError::progress(&format!("failed to parse registry: {}", e)))
}

/// Save the registry to disk.
fn save_registry(registry: &Registry) -> Result<()> {
    let path = registry_path()?;
    let value = serde_json::to_value(registry)
        .map_err(|e| FlowError::progress(&format!("failed to serialize registry: {}", e)))?;
    
    util::atomic_write_json(&path, &value)
}

/// Register a new task environment.
pub fn register_environment(task_id: &str, state_file: &Path) -> Result<()> {
    let mut registry = load_registry()?;
    
    if registry.contains(task_id) {
        return Err(FlowError::progress(&format!(
            "task {} is already registered",
            task_id
        )));
    }
    
    let now = util::now();
    registry.add(task_id.to_string(), state_file.to_path_buf(), now);
    
    save_registry(&registry)
}

/// Unregister a task environment.
pub fn unregister_environment(task_id: &str) -> Result<()> {
    let mut registry = load_registry()?;
    
    if !registry.remove(task_id) {
        return Err(FlowError::progress(&format!(
            "task {} is not registered",
            task_id
        )));
    }
    
    save_registry(&registry)
}

/// Get the state file path for a task environment.
pub fn environment_state_path(task_id: &str) -> Result<PathBuf> {
    let home = std::env::var("HOME")
        .map_err(|_| FlowError::progress("HOME environment variable not set"))?;
    
    // Ensure parent directories exist with secure permissions
    let herdr_dir = Path::new(&home).join(".herdr");
    ensure_secure_dir(&herdr_dir)?;
    
    let flow_dir = herdr_dir.join("flow");
    ensure_secure_dir(&flow_dir)?;
    
    let state_dir = flow_dir.join("task_environments");
    ensure_secure_dir(&state_dir)?;
    
    Ok(state_dir.join(format!("{}.json", task_id)))
}

/// Check if a task environment is registered.
#[allow(dead_code)]
pub fn is_registered(task_id: &str) -> Result<bool> {
    let registry = load_registry()?;
    Ok(registry.contains(task_id))
}

/// Get all registered task IDs.
#[allow(dead_code)]
pub fn list_registered() -> Result<Vec<String>> {
    let registry = load_registry()?;
    Ok(registry.entries.keys().cloned().collect())
}

/// A registry handle that provides transactional access.
pub struct EnvironmentRegistry {
    registry: Registry,
}

impl EnvironmentRegistry {
    /// Load the registry.
    pub fn load() -> Result<Self> {
        Ok(Self {
            registry: load_registry()?,
        })
    }
    
    /// Get an environment's state file path.
    pub fn get_state_file(&self, task_id: &str) -> Option<PathBuf> {
        self.registry.get(task_id).map(|e| e.state_file.clone())
    }
    
    /// Check if a task is registered.
    pub fn contains(&self, task_id: &str) -> bool {
        self.registry.contains(task_id)
    }
    
    /// List all registered tasks.
    pub fn list(&self) -> Vec<String> {
        self.registry.entries.keys().cloned().collect()
    }
}
