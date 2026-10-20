//! Global registry of live task environments.
//!
//! The registry is a single JSON document guarded by a `flock` for the whole
//! read-modify-write operation, so concurrent registrations cannot lose
//! updates. Every live environment also occupies a resource slot: an
//! environment whose cleanup failed keeps its registry entry (or its cleanup
//! plan) until the resources are actually released, which prevents callers
//! from escaping a failed cleanup by creating fresh directories.

use std::collections::BTreeMap;
use std::fs::OpenOptions;
use std::os::fd::AsRawFd;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::error::{FlowError, Result};
use crate::util;

/// Default number of live task environments allowed at the same time.
pub const DEFAULT_LIMIT: usize = 32;

/// Maximum length accepted for a task identifier.
const MAX_TASK_ID: usize = 128;

/// A registry entry for one live task environment.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RegistryEntry {
    pub task_id: String,
    pub state_file: PathBuf,
    pub registered_at: f64,
}

/// The durable registry document.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Registry {
    entries: BTreeMap<String, RegistryEntry>,
}

impl Registry {
    fn add(&mut self, task_id: &str, state_file: &Path, now: f64) {
        self.entries.insert(
            task_id.to_string(),
            RegistryEntry {
                task_id: task_id.to_string(),
                state_file: state_file.to_path_buf(),
                registered_at: now,
            },
        );
    }

    fn remove(&mut self, task_id: &str) -> bool {
        self.entries.remove(task_id).is_some()
    }

    /// All registered entries, ordered by task identifier.
    pub fn entries(&self) -> impl Iterator<Item = &RegistryEntry> {
        self.entries.values()
    }

    pub fn contains(&self, task_id: &str) -> bool {
        self.entries.contains_key(task_id)
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

/// Reject task identifiers that could escape a directory or alias another.
pub fn validate_task_id(task_id: &str) -> Result<()> {
    let valid = !task_id.is_empty()
        && task_id.len() <= MAX_TASK_ID
        && task_id != "."
        && task_id != ".."
        && !task_id.starts_with('-')
        && task_id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'));
    if valid {
        Ok(())
    } else {
        Err(FlowError::progress(format!(
            "invalid task ID {task_id:?}: expected 1-{MAX_TASK_ID} characters of [A-Za-z0-9._-] without traversal"
        )))
    }
}

/// Root directory that holds environment state and cleanup plans.
pub fn state_root() -> Result<PathBuf> {
    if let Some(root) = std::env::var_os("HERDR_TASK_ENV_ROOT") {
        let root = PathBuf::from(root);
        ensure_private_dir(&root)?;
        return Ok(util::resolve_path(&root));
    }
    let home = std::env::var_os("HOME")
        .ok_or_else(|| FlowError::progress("HOME environment variable not set"))?;
    let herdr = PathBuf::from(home).join(".herdr");
    ensure_private_dir(&herdr)?;
    let flow = herdr.join("flow");
    ensure_private_dir(&flow)?;
    let root = flow.join("task_environments");
    ensure_private_dir(&root)?;
    Ok(util::resolve_path(&root))
}

/// Create `path` as a private directory when missing, then reject unsafe modes.
fn ensure_private_dir(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;

    if !path.exists() {
        std::fs::create_dir_all(path).map_err(|error| {
            FlowError::progress(format!("failed to create {}: {error}", path.display()))
        })?;
    }
    let metadata = std::fs::symlink_metadata(path)?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(FlowError::guard(format!(
            "{} must be a real directory",
            path.display()
        )));
    }
    let mut permissions = metadata.permissions();
    if permissions.mode() & 0o077 != 0 {
        permissions.set_mode(0o700);
        std::fs::set_permissions(path, permissions)?;
    }
    Ok(())
}

/// Path of the durable environment document for one task.
pub fn environment_state_path(task_id: &str) -> Result<PathBuf> {
    validate_task_id(task_id)?;
    Ok(state_root()?.join(format!("{task_id}.json")))
}

/// Path of the durable cleanup plan for one task.
pub fn cleanup_state_path(task_id: &str) -> Result<PathBuf> {
    validate_task_id(task_id)?;
    let dir = state_root()?.join("cleanup");
    ensure_private_dir(&dir)?;
    Ok(dir.join(format!("{task_id}.json")))
}

/// Path of the registry document.
pub fn registry_path() -> Result<PathBuf> {
    Ok(state_root()?.join("registry.json"))
}

fn registry_lock_path() -> Result<PathBuf> {
    Ok(state_root()?.join("registry.lock"))
}

/// Acquire the exclusive registry lock, releasing it when the handle drops.
pub fn lock_registry() -> Result<std::fs::File> {
    let path = registry_lock_path()?;
    let file = OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(&path)
        .map_err(|error| {
            FlowError::progress(format!("failed to open {}: {error}", path.display()))
        })?;
    util::flock_exclusive(file.as_raw_fd())?;
    Ok(file)
}

/// Load the registry without taking the lock (callers that mutate take it).
pub fn load_registry() -> Result<Registry> {
    let path = registry_path()?;
    if !path.exists() {
        return Ok(Registry::default());
    }
    let text = std::fs::read_to_string(&path)?;
    serde_json::from_str(&text).map_err(|error| {
        FlowError::progress(format!("failed to parse {}: {error}", path.display()))
    })
}

fn save_registry(registry: &Registry) -> Result<()> {
    let path = registry_path()?;
    let value = serde_json::to_value(registry)?;
    util::atomic_write_json(&path, &value)
}

/// Number of resource slots currently occupied.
///
/// A registry entry occupies a slot while the environment lives. A cleanup
/// plan also occupies a slot, so a failed cleanup keeps consuming capacity
/// until it succeeds instead of letting the caller allocate another root.
pub fn occupied_slots() -> Result<usize> {
    let registry = load_registry()?;
    let mut used = registry.len();
    for entry in std::fs::read_dir(state_root()?.join("cleanup"))
        .into_iter()
        .flatten()
    {
        let Ok(entry) = entry else { continue };
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }
        let task_id = path
            .file_stem()
            .and_then(|name| name.to_str())
            .unwrap_or_default();
        if !registry.contains(task_id) {
            used += 1;
        }
    }
    Ok(used)
}

/// Configured environment limit (`HERDR_TASK_ENV_LIMIT`, default 32).
pub fn limit() -> usize {
    std::env::var("HERDR_TASK_ENV_LIMIT")
        .ok()
        .and_then(|value| value.parse::<usize>().ok())
        .filter(|value| *value > 0)
        .unwrap_or(DEFAULT_LIMIT)
}

/// Reserve a resource slot for `task_id`, failing when the limit is reached.
pub fn register_environment(task_id: &str, state_file: &Path) -> Result<()> {
    validate_task_id(task_id)?;
    let _lock = lock_registry()?;
    let mut registry = load_registry()?;
    if registry.contains(task_id) {
        return Err(FlowError::progress(format!(
            "environment for task {task_id} is already registered"
        )));
    }
    let used = occupied_slots()?;
    let cap = limit();
    if used >= cap {
        return Err(FlowError::progress(format!(
            "task environment limit reached ({used}/{cap}); finish or clean up pending environments first"
        )));
    }
    registry.add(task_id, state_file, util::now());
    save_registry(&registry)
}

/// Release the resource slot held by `task_id`. Missing entries are ignored so
/// destroy remains idempotent across crash recovery.
pub fn unregister_environment(task_id: &str) -> Result<()> {
    validate_task_id(task_id)?;
    let _lock = lock_registry()?;
    let mut registry = load_registry()?;
    if !registry.remove(task_id) {
        return Ok(());
    }
    save_registry(&registry)
}

/// Check whether a live environment is registered.
pub fn is_registered(task_id: &str) -> Result<bool> {
    validate_task_id(task_id)?;
    Ok(load_registry()?.contains(task_id))
}

/// All registered task identifiers, ordered.
pub fn list_registered() -> Result<Vec<String>> {
    Ok(load_registry()?
        .entries()
        .map(|entry| entry.task_id.clone())
        .collect())
}

/// A read-only snapshot of the registry.
pub struct EnvironmentRegistry {
    registry: Registry,
}

impl EnvironmentRegistry {
    pub fn load() -> Result<Self> {
        Ok(Self {
            registry: load_registry()?,
        })
    }

    pub fn contains(&self, task_id: &str) -> bool {
        self.registry.contains(task_id)
    }

    pub fn list(&self) -> Vec<String> {
        self.registry
            .entries()
            .map(|entry| entry.task_id.clone())
            .collect()
    }

    pub fn get_state_file(&self, task_id: &str) -> Option<PathBuf> {
        self.registry
            .entries()
            .find(|entry| entry.task_id == task_id)
            .map(|entry| entry.state_file.clone())
    }
}
