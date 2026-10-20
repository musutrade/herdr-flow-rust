//! Durable index of live task environments.
//!
//! Directory layout under the state root:
//!
//! ```text
//! envs/<task>.json      environment documents
//! plans/<task>.json     pending cleanup plans
//! locks/<task>.lock     per-task exclusive ownership lock
//! registry.json         index of claimed roots, guarded by registry.lock
//! registry.lock
//! ```
//!
//! Task ids therefore never collide with internal file names, and one task can
//! never claim a root that another live task already owns.

use std::collections::BTreeMap;
use std::fs::OpenOptions;
use std::os::fd::AsRawFd;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::error::{FlowError, Result};
use crate::util;

/// Default number of live environments allowed at once.
pub const DEFAULT_LIMIT: usize = 32;

const MAX_TASK_ID: usize = 128;

/// One live environment claim.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RegistryEntry {
    pub task_id: String,
    pub instance_id: String,
    pub workspace_root: PathBuf,
    pub temp_root: PathBuf,
    /// Every root claimed by this instance. A recovery can claim the roots one
    /// at a time without a later claim replacing an earlier one.
    #[serde(default)]
    pub roots: Vec<PathBuf>,
    pub state_file: PathBuf,
    pub registered_at: f64,
}

impl RegistryEntry {
    /// All roots claimed by this entry, including legacy documents.
    pub fn claimed_roots(&self) -> Vec<PathBuf> {
        if self.roots.is_empty() {
            vec![self.workspace_root.clone(), self.temp_root.clone()]
        } else {
            self.roots.clone()
        }
    }
}

/// The durable index document.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Registry {
    entries: BTreeMap<String, RegistryEntry>,
}

impl Registry {
    pub fn entries(&self) -> impl Iterator<Item = &RegistryEntry> {
        self.entries.values()
    }

    pub fn get(&self, task_id: &str) -> Option<&RegistryEntry> {
        self.entries.get(task_id)
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

    fn insert(&mut self, entry: RegistryEntry) {
        self.entries.insert(entry.task_id.clone(), entry);
    }
}

/// Reject ids that could escape a directory or alias an internal file.
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
    // SAFETY: geteuid takes no pointers and has no caller preconditions.
    if metadata.uid() != unsafe { libc::geteuid() } {
        return Err(FlowError::guard(format!(
            "{} must be owned by this user",
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

/// Root directory that holds environment state and cleanup plans.
pub fn state_root() -> Result<PathBuf> {
    let root = match std::env::var_os("HERDR_TASK_ENV_ROOT") {
        Some(root) => PathBuf::from(root),
        None => {
            let home = std::env::var_os("HOME")
                .ok_or_else(|| FlowError::progress("HOME environment variable not set"))?;
            PathBuf::from(home)
                .join(".herdr")
                .join("flow")
                .join("task_environments")
        }
    };
    ensure_private_dir(&root)?;
    Ok(util::resolve_path(&root))
}

fn subdir(name: &str) -> Result<PathBuf> {
    let dir = state_root()?.join(name);
    ensure_private_dir(&dir)?;
    Ok(dir)
}

/// Environment documents directory.
pub fn env_dir() -> Result<PathBuf> {
    subdir("envs")
}

/// Cleanup plans directory.
pub fn plan_dir() -> Result<PathBuf> {
    subdir("plans")
}

fn lock_dir() -> Result<PathBuf> {
    subdir("locks")
}

/// Reject a path that overlaps the control-state root.
///
/// The control root holds the documents that authorize cleanup. A task root
/// inside it (or containing it) would let a confined task rewrite its own
/// authorization, so the relationship is refused in both directions.
pub fn validate_outside_control(path: &Path) -> Result<()> {
    let control = state_root()?;
    if util::overlap(path, &control) {
        return Err(FlowError::progress(format!(
            "{} overlaps the task-environment control root {}",
            path.display(),
            control.display()
        )));
    }
    Ok(())
}

/// Document path for one environment. Never collides with internal files.
pub fn environment_state_path(task_id: &str) -> Result<PathBuf> {
    validate_task_id(task_id)?;
    Ok(env_dir()?.join(format!("{task_id}.json")))
}

/// Cleanup plan path for one environment.
pub fn cleanup_state_path(task_id: &str) -> Result<PathBuf> {
    validate_task_id(task_id)?;
    Ok(plan_dir()?.join(format!("{task_id}.json")))
}

pub fn registry_path() -> Result<PathBuf> {
    Ok(state_root()?.join("registry.json"))
}

/// Acquire the exclusive index lock.
fn lock_registry() -> Result<std::fs::File> {
    let path = state_root()?.join("registry.lock");
    let file = OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(&path)
        .map_err(|error| {
            FlowError::progress(format!("failed to open {}: {error}", path.display()))
        })?;
    util::flock_exclusive(file.as_raw_fd())?;
    Ok(file)
}

/// Acquire the per-task ownership lock.
///
/// The lock is held by the live [`crate::task_env::TaskEnvironment`] handle, so
/// a second handle (including one produced by `restore_environment`) cannot
/// own the same environment at the same time. Returns `Ok(None)` when the task
/// is already owned.
pub fn try_lock_task(task_id: &str) -> Result<Option<std::fs::File>> {
    validate_task_id(task_id)?;
    let path = lock_dir()?.join(format!("{task_id}.lock"));
    let file = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(&path)
        .map_err(|error| {
            FlowError::progress(format!("failed to open {}: {error}", path.display()))
        })?;
    if util::flock_try_exclusive(file.as_raw_fd())? {
        Ok(Some(file))
    } else {
        Ok(None)
    }
}

/// Load the index without locking; writers hold [`lock_registry`] first.
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
    let value = serde_json::to_value(registry)?;
    util::atomic_write_json(&registry_path()?, &value)
}

/// Number of live environments plus pending cleanup plans.
pub fn occupied_slots() -> Result<usize> {
    Ok(count_json_files(&env_dir()?)? + count_json_files(&plan_dir()?)?)
}

fn count_json_files(dir: &Path) -> Result<usize> {
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(0),
        Err(error) => return Err(error.into()),
    };
    let mut count = 0;
    for entry in entries.flatten() {
        if entry.path().extension().and_then(|ext| ext.to_str()) == Some("json") {
            count += 1;
        }
    }
    Ok(count)
}

/// Configured limit (`HERDR_TASK_ENV_LIMIT`, default 32).
pub fn limit() -> usize {
    std::env::var("HERDR_TASK_ENV_LIMIT")
        .ok()
        .and_then(|value| value.parse::<usize>().ok())
        .filter(|value| *value > 0)
        .unwrap_or(DEFAULT_LIMIT)
}

/// Claim the environment's roots in the index.
///
/// Fails when another live entry already claims an overlapping root, and when
/// the resource limit is reached. The caller holds the per-task lock, so no
/// concurrent claim for this task can interleave.
pub fn register_environment(entry: RegistryEntry) -> Result<()> {
    validate_task_id(&entry.task_id)?;
    let _lock = lock_registry()?;
    let mut registry = load_registry()?;
    ensure_same_instance(&registry, &entry)?;
    ensure_roots_unclaimed(&registry, &entry)?;
    ensure_capacity()?;
    registry.insert(entry);
    save_registry(&registry)
}

fn ensure_same_instance(registry: &Registry, entry: &RegistryEntry) -> Result<()> {
    match registry.get(&entry.task_id) {
        Some(existing) if existing.instance_id != entry.instance_id => {
            Err(FlowError::progress(format!(
                "task {} is already claimed by environment instance {}",
                entry.task_id, existing.instance_id
            )))
        }
        _ => Ok(()),
    }
}

fn ensure_roots_unclaimed(registry: &Registry, entry: &RegistryEntry) -> Result<()> {
    for other in registry.entries() {
        if other.task_id == entry.task_id {
            continue;
        }
        find_root_conflict(entry, other)?;
    }
    Ok(())
}

fn find_root_conflict(entry: &RegistryEntry, other: &RegistryEntry) -> Result<()> {
    for mine in entry.claimed_roots() {
        for theirs in other.claimed_roots() {
            if util::overlap(&mine, &theirs) {
                return Err(FlowError::progress(format!(
                    "root {} overlaps with root {} claimed by task {}",
                    mine.display(),
                    theirs.display(),
                    other.task_id
                )));
            }
        }
    }
    Ok(())
}

fn ensure_capacity() -> Result<()> {
    let used = occupied_slots()?;
    let cap = limit();
    if used > cap {
        return Err(FlowError::progress(format!(
            "task environment limit reached ({used}/{cap}); clean up pending environments first"
        )));
    }
    Ok(())
}

/// Release the index entry, but only when it belongs to `instance_id`.
pub fn unregister_environment(task_id: &str, instance_id: &str) -> Result<()> {
    validate_task_id(task_id)?;
    let _lock = lock_registry()?;
    let mut registry = load_registry()?;
    match registry.get(task_id) {
        Some(entry) if entry.instance_id == instance_id => {
            registry.entries.remove(task_id);
            save_registry(&registry)
        }
        // Already released, or a different instance now owns the id: never
        // touch the newer owner.
        _ => Ok(()),
    }
}

/// Atomically claim a set of empty, unclaimed roots for `task_id`.
///
/// The emptiness checks and the claim happen under the registry lock, and the
/// claims are merged into one entry, so claiming the second root cannot
/// replace the first. Returns the roots that were actually claimed.
pub fn claim_roots_exclusively(
    task_id: &str,
    instance_id: &str,
    candidates: &[PathBuf],
) -> Result<Vec<PathBuf>> {
    validate_task_id(task_id)?;
    if candidates.is_empty() {
        return Ok(Vec::new());
    }
    let _lock = lock_registry()?;
    let mut registry = load_registry()?;
    let claimed = collect_claims(&registry, instance_id, candidates)?;
    commit_claims(&mut registry, task_id, instance_id, &claimed)
}

fn collect_claims(
    registry: &Registry,
    instance_id: &str,
    candidates: &[PathBuf],
) -> Result<Vec<PathBuf>> {
    let mut claimed = Vec::new();
    for candidate in candidates {
        consider_candidate(registry, instance_id, candidate, &mut claimed)?;
    }
    Ok(claimed)
}

fn commit_claims(
    registry: &mut Registry,
    task_id: &str,
    instance_id: &str,
    claimed: &[PathBuf],
) -> Result<Vec<PathBuf>> {
    if claimed.is_empty() {
        return Ok(Vec::new());
    }
    merge_claim(registry, task_id, instance_id, claimed)?;
    save_registry(registry)?;
    Ok(claimed.to_vec())
}

fn consider_candidate(
    registry: &Registry,
    instance_id: &str,
    candidate: &PathBuf,
    claimed: &mut Vec<PathBuf>,
) -> Result<()> {
    if claimed.iter().any(|root| root == candidate) {
        return Ok(());
    }
    if !claimable_root(registry, instance_id, candidate)? {
        return Ok(());
    }
    claimed.push(candidate.clone());
    Ok(())
}

fn claimable_root(registry: &Registry, instance_id: &str, candidate: &Path) -> Result<bool> {
    let metadata = match std::fs::symlink_metadata(candidate) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(error.into()),
    };
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Ok(false);
    }
    if std::fs::read_dir(candidate)?.next().is_some() {
        return Ok(false);
    }
    Ok(!has_foreign_claim(registry, instance_id, candidate))
}

fn has_foreign_claim(registry: &Registry, instance_id: &str, candidate: &Path) -> bool {
    registry.entries().any(|other| {
        other.instance_id != instance_id
            && other
                .claimed_roots()
                .iter()
                .any(|root| util::overlap(root, candidate))
    })
}

fn merge_claim(
    registry: &mut Registry,
    task_id: &str,
    instance_id: &str,
    claimed: &[PathBuf],
) -> Result<()> {
    let mut entry = resolve_claim_entry(registry, task_id, instance_id, claimed)?;
    for root in claimed {
        if !entry.roots.contains(root) {
            entry.roots.push(root.clone());
        }
    }
    entry.workspace_root = entry.roots[0].clone();
    entry.temp_root = entry.roots.last().unwrap().clone();
    registry.insert(entry);
    Ok(())
}

fn resolve_claim_entry(
    registry: &Registry,
    task_id: &str,
    instance_id: &str,
    claimed: &[PathBuf],
) -> Result<RegistryEntry> {
    if let Some(entry) = registry.entries.get(task_id) {
        if entry.instance_id != instance_id {
            return Err(FlowError::progress(format!(
                "task {task_id} is claimed by another environment instance"
            )));
        }
        return Ok(entry.clone());
    }
    Ok(RegistryEntry {
        task_id: task_id.to_string(),
        instance_id: instance_id.to_string(),
        workspace_root: claimed[0].clone(),
        temp_root: claimed.last().unwrap().clone(),
        roots: Vec::new(),
        state_file: environment_state_path(task_id)?,
        registered_at: util::now(),
    })
}

pub fn is_registered(task_id: &str) -> Result<bool> {
    validate_task_id(task_id)?;
    Ok(load_registry()?.contains(task_id))
}

pub fn list_registered() -> Result<Vec<String>> {
    Ok(load_registry()?
        .entries()
        .map(|entry| entry.task_id.clone())
        .collect())
}

/// Tasks whose document shows an interrupted create or destroy.
pub fn interrupted_tasks() -> Result<Vec<String>> {
    let mut tasks = json_task_ids(&plan_dir()?)?;
    for task_id in interrupted_env_ids(&env_dir()?)? {
        if !tasks.contains(&task_id) {
            tasks.push(task_id);
        }
    }
    tasks.sort();
    Ok(tasks)
}

fn json_task_ids(dir: &Path) -> Result<Vec<String>> {
    let mut tasks = Vec::new();
    for entry in std::fs::read_dir(dir)?.flatten() {
        let path = entry.path();
        if path.extension().and_then(|ext| ext.to_str()) == Some("json") {
            if let Some(task_id) = path.file_stem().and_then(|stem| stem.to_str()) {
                tasks.push(task_id.to_string());
            }
        }
    }
    Ok(tasks)
}

fn interrupted_env_ids(dir: &Path) -> Result<Vec<String>> {
    let mut tasks = Vec::new();
    for entry in std::fs::read_dir(dir)?.flatten() {
        let path = entry.path();
        if document_is_interrupted(&path) {
            if let Some(task_id) = path.file_stem().and_then(|stem| stem.to_str()) {
                tasks.push(task_id.to_string());
            }
        }
    }
    Ok(tasks)
}

fn document_is_interrupted(path: &Path) -> bool {
    if path.extension().and_then(|ext| ext.to_str()) != Some("json") {
        return false;
    }
    let Ok(text) = std::fs::read_to_string(path) else {
        return false;
    };
    let Ok(value) = serde_json::from_str::<serde_json::Value>(&text) else {
        return false;
    };
    matches!(
        value.get("state").and_then(|value| value.as_str()),
        Some("creating" | "destroying" | "destroy_failed")
    )
}

/// Read-only snapshot of the index.
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
            .get(task_id)
            .map(|entry| entry.state_file.clone())
    }
}
