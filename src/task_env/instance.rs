//! Creation-only task ownership. Queue checkpoint precedes Git side effects.
//! This is not the terminal writer barrier or the finalizer (separate slices).
pub(super) mod git;
mod roots;

use super::{
    DeliveryTarget, EnvironmentState, RootIdentity, TaskEnvironment, WorktreeRegistration,
};
use crate::{util, FlowError, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::path::PathBuf;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InstanceBinding {
    pub queue_id: String,
    pub task_id: String,
    pub epoch: u64,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CreateSpec {
    pub workspace_root: PathBuf,
    pub temp_root: PathBuf,
    pub delivery_targets: Vec<DeliveryTarget>,
    pub repository: PathBuf,
    pub commit: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CreationPhase {
    Prepared,
    GitPending,
    GitRegistered,
    Ready,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CreationIntent {
    pub binding: InstanceBinding,
    #[serde(default)]
    pub process_boundary: Option<ProcessBoundary>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lifecycle_queue: Option<PathBuf>,
    pub phase: CreationPhase,
    pub repository: PathBuf,
    pub repository_identity: RootIdentity,
    pub common_dir: PathBuf,
    pub common_identity: RootIdentity,
    pub commit: String,
    pub checkout: PathBuf,
    pub checkout_identity: Option<RootIdentity>,
    pub git_identity: Option<GitIdentity>,
    pub git_admin_before: Option<std::collections::BTreeMap<String, RootIdentity>>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GitIdentity {
    pub git_file_identity: RootIdentity,
    pub admin_dir: PathBuf,
    pub admin_identity: RootIdentity,
}

pub(super) fn require(ok: bool, message: &str) -> Result<()> {
    if ok {
        Ok(())
    } else {
        Err(FlowError::progress(message))
    }
}

pub fn owner(env: &TaskEnvironment) -> Option<Value> {
    env.creation.as_ref().map(|intent| {
        json!({
            "binding":intent.binding,"instance_id":env.instance_id
        })
    })
}

/// Persist an immutable creation intent and reserve existing environment roots.
/// Does not create a worktree, launch a process, or adopt an existing instance.
/// Caller holds the queue lock and must save the matching queue association
/// before materialize_instance. An error may leave an intent; never blind-retry.
pub fn begin_instance(binding: &InstanceBinding, spec: &CreateSpec) -> Result<TaskEnvironment> {
    validate_binding(binding)?;
    validate_spec(spec)?;
    let intent = git::intent(binding, spec)?;
    super::env::create_with_intent(
        &binding.task_id,
        &spec.workspace_root,
        spec.delivery_targets.clone(),
        &spec.temp_root,
        Some(intent),
    )
}

/// Isolated creation requires a prospective EX lease; no legacy state is migrated.
pub fn begin_isolated(
    binding: &InstanceBinding,
    spec: &CreateSpec,
    queue_root: &std::path::Path,
    permit: std::sync::Arc<crate::lifecycle::Permit>,
) -> Result<TaskEnvironment> {
    validate_binding(binding)?;
    validate_spec(spec)?;
    verify_prospective_lease(binding, queue_root, &permit)?;
    let mut intent = git::intent(binding, spec)?;
    intent.lifecycle_queue = Some(queue_root.to_path_buf());
    super::isolation::enable(queue_root, &binding.queue_id)?;
    super::env::create_with_lease(
        &binding.task_id,
        &spec.workspace_root,
        spec.delivery_targets.clone(),
        &spec.temp_root,
        Some(intent),
        Some(permit),
    )
}
fn verify_prospective_lease(
    binding: &InstanceBinding,
    queue_root: &std::path::Path,
    permit: &crate::lifecycle::Permit,
) -> Result<()> {
    let p = permit.owner();
    require(
        permit.exclusive()
            && p.queue_root == queue_root
            && p.queue_id == binding.queue_id
            && p.task_id == binding.task_id
            && p.epoch == binding.epoch
            && p.instance_id.is_empty(),
        "isolated create requires prospective exclusive lease",
    )?;
    Ok(())
}

pub fn resume_isolated(
    binding: &InstanceBinding,
    expected: &str,
    permit: std::sync::Arc<crate::lifecycle::Permit>,
) -> Result<TaskEnvironment> {
    let env = super::restore_environment(&binding.task_id)?;
    *env.creation_permit.lock().unwrap() = Some(permit);
    super::isolation::mutation_permit(&env)?;
    verify_creation_identity(&env, binding, expected)?;
    Ok(env)
}

fn validate_binding(binding: &InstanceBinding) -> Result<()> {
    super::validate_task_id(&binding.task_id)?;
    require(
        !binding.queue_id.is_empty() && binding.queue_id.len() <= 128,
        "invalid queue identity",
    )?;
    require(binding.epoch > 0, "instance epoch must be positive")
}

fn validate_spec(spec: &CreateSpec) -> Result<()> {
    require(
        spec.workspace_root.is_absolute() && spec.temp_root.is_absolute(),
        "instance roots must be absolute",
    )?;
    require(
        spec.repository.is_absolute(),
        "instance repository must be absolute",
    )?;
    require(
        spec.delivery_targets.iter().all(|t| t.path.is_absolute()),
        "instance delivery targets must be absolute",
    )?;
    require(
        spec.commit.len() == 40 && spec.commit.bytes().all(|c| c.is_ascii_hexdigit()),
        "instance commit must be a full SHA",
    )?;
    for root in [&spec.workspace_root, &spec.temp_root] {
        require(
            !util::overlap(root, &spec.repository),
            "instance roots must not overlap repository",
        )?;
    }
    Ok(())
}

/// Resume only the recorded tuple; never allocate a replacement instance.
pub fn resume_instance(binding: &InstanceBinding, expected: &str) -> Result<TaskEnvironment> {
    validate_binding(binding)?;
    let env = super::restore_environment(&binding.task_id)?;
    verify_binding(&env, binding, expected)?;
    ensure_registered(&env)?;
    roots::verify_recorded(&env)?;
    git::verify_recorded(intent(&env)?)?;
    Ok(env)
}

pub(super) fn verify_creation_identity(
    env: &TaskEnvironment,
    binding: &InstanceBinding,
    expected: &str,
) -> Result<()> {
    verify_binding(env, binding, expected)?;
    ensure_registered(env)?;
    roots::verify_recorded(env)?;
    git::verify_recorded(intent(env)?)
}

fn verify_binding(env: &TaskEnvironment, binding: &InstanceBinding, expected: &str) -> Result<()> {
    require(env.owns_lock(), "instance handle has no ownership lock")?;
    verify_document(env)?;
    require(
        env.task_id == binding.task_id && env.instance_id == expected,
        "task instance identity changed",
    )?;
    require(
        &intent(env)?.binding == binding,
        "queue/task/epoch binding changed",
    )?;
    require(
        matches!(
            env.state,
            EnvironmentState::Creating | EnvironmentState::Active
        ),
        "instance is not creating/active",
    )
}

pub(super) fn intent(env: &TaskEnvironment) -> Result<&CreationIntent> {
    env.creation
        .as_ref()
        .ok_or_else(|| FlowError::progress("legacy environment has no queue instance binding"))
}

/// Finish this intent, including a successful Git add whose reply was lost.
/// Unknown objects are retained as errors, never adopted, removed or recreated.
pub fn materialize_instance(env: &mut TaskEnvironment) -> Result<()> {
    let _permit = super::isolation::mutation_permit(env)?;
    let binding = intent(env)?.binding.clone();
    verify_binding(env, &binding, &env.instance_id)?;
    if intent(env)?.phase == CreationPhase::Ready {
        verify_owned(env)?;
        return require(
            env.state == EnvironmentState::Active,
            "ready instance is not active",
        );
    }
    finish_materialization(env)
}

fn finish_materialization(env: &mut TaskEnvironment) -> Result<()> {
    require(
        env.state == EnvironmentState::Creating,
        "unfinished creation must remain creating",
    )?;
    require(
        super::discovery::marked_processes(&env.instance_id)?.is_empty(),
        "instance creation still has live owned execution; no replay",
    )?;
    roots::materialize(env)?;
    git::materialize(env)?;
    finish(env)
}

fn finish(env: &mut TaskEnvironment) -> Result<()> {
    let i = intent(env)?;
    let registration = WorktreeRegistration {
        path: i.checkout.clone(),
        repo: Some(i.common_dir.clone()),
    };
    if env.worktree_registrations.is_empty() {
        env.worktree_registrations.push(registration);
    } else {
        require(
            env.worktree_registrations == [registration],
            "instance worktree registration changed",
        )?;
    }
    env.creation.as_mut().unwrap().phase = CreationPhase::Ready;
    env.state = EnvironmentState::Active;
    env.updated_at = util::now();
    env.persist()
}

/// Identity check for already-created resources; HEAD may advance during work.
pub fn verify_instance(
    env: &TaskEnvironment,
    binding: &InstanceBinding,
    expected: &str,
) -> Result<()> {
    verify_binding(env, binding, expected)?;
    verify_owned(env)
}

pub fn verify_owned(env: &TaskEnvironment) -> Result<()> {
    verify_document(env)?;
    ensure_registered(env)?;
    verify_ready_handle(env)?;
    roots::verify_recorded(env)?;
    git::verify_ready(intent(env)?)?;
    verify_registration(env)
}

fn verify_ready_handle(env: &TaskEnvironment) -> Result<()> {
    require(env.owns_lock(), "instance handle has no ownership lock")?;
    require(
        intent(env)?.phase == CreationPhase::Ready && env.state == EnvironmentState::Active,
        "instance creation is incomplete",
    )
}

fn verify_registration(env: &TaskEnvironment) -> Result<()> {
    let i = intent(env)?;
    require(
        env.worktree_registrations
            == [WorktreeRegistration {
                path: i.checkout.clone(),
                repo: Some(i.common_dir.clone()),
            }],
        "instance worktree registration changed",
    )
}

// Identity changes are checked against the durable authority as well as paths.
fn verify_document(env: &TaskEnvironment) -> Result<()> {
    let root = env
        .state_file()
        .parent()
        .ok_or_else(|| FlowError::progress("missing environment parent"))?;
    let name = env
        .state_file()
        .file_name()
        .and_then(|s| s.to_str())
        .ok_or_else(|| FlowError::progress("invalid environment filename"))?;
    let saved = util::read_state_json_bounded(root, name, 1024 * 1024)?;
    require(
        saved == serde_json::to_value(env)?,
        "durable task instance changed; reload before continuing",
    )
}

fn ensure_registered(env: &TaskEnvironment) -> Result<()> {
    let registry = super::load_registry()?;
    if let Some(entry) = registry.get(&env.task_id) {
        require(
            entry.task_id == env.task_id
                && entry.instance_id == env.instance_id
                && entry
                    .claimed_roots()
                    .into_iter()
                    .collect::<std::collections::BTreeSet<_>>()
                    == [env.workspace_root.clone(), env.temp_root.clone()]
                        .into_iter()
                        .collect()
                && entry.workspace_root == env.workspace_root
                && entry.temp_root == env.temp_root
                && entry.state_file == env.state_file,
            "instance registry ownership changed",
        )
    } else {
        require(
            env.state == EnvironmentState::Creating,
            "active instance lost its registry ownership",
        )?;
        super::env::reserve_environment(env, env.state_file())
    }
}

/// Terminal records an obligation even when creation cannot safely continue.
/// It checks the durable owner, not whether every partial resource is deletable.
pub(super) fn verify_terminal_identity(
    env: &TaskEnvironment,
    binding: &InstanceBinding,
    expected: &str,
) -> Result<()> {
    verify_binding(env, binding, expected)?;
    if let Some(entry) = super::load_registry()?.get(&env.task_id) {
        require(
            entry.instance_id == env.instance_id
                && entry.task_id == env.task_id
                && entry.state_file == env.state_file(),
            "terminal registry identity changed",
        )?;
    }
    Ok(())
}

/// Positive exclusion of processes that predate creation, within this boot.
/// Captured before the random instance is allocated or any managed child starts.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProcessBoundary {
    pub boot_id: String,
    pub start_ticks: u64,
    pub pid_namespace: RootIdentity,
    pub time_namespace: RootIdentity,
}
impl ProcessBoundary {
    pub(crate) fn capture() -> Result<Self> {
        let boot_id = std::fs::read_to_string("/proc/sys/kernel/random/boot_id")?
            .trim()
            .to_owned();
        require(!boot_id.is_empty(), "boot identity missing")?;
        let start_ticks = boot_ticks()?;
        Ok(Self {
            boot_id,
            start_ticks,
            pid_namespace: RootIdentity::of(&std::fs::metadata("/proc/self/ns/pid")?),
            time_namespace: RootIdentity::of(&std::fs::metadata("/proc/self/ns/time")?),
        })
    }
    pub(crate) fn verify(&self) -> Result<()> {
        let current = Self::capture()?;
        require(
            self.boot_id == current.boot_id
                && self.start_ticks <= current.start_ticks
                && self.pid_namespace == current.pid_namespace
                && self.time_namespace == current.time_namespace,
            "process boot boundary changed",
        )
    }
}

fn boot_ticks() -> Result<u64> {
    let mut time = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    // SAFETY: valid writable timespec and scalar supported clock ID.
    require(
        unsafe { libc::clock_gettime(libc::CLOCK_BOOTTIME, &mut time) } == 0,
        "boot clock unavailable",
    )?;
    // SAFETY: sysconf takes a scalar selector, no borrowed memory.
    let hz = unsafe { libc::sysconf(libc::_SC_CLK_TCK) };
    require(
        hz > 0 && time.tv_sec >= 0 && time.tv_nsec >= 0,
        "invalid boot clock",
    )?;
    let ticks = (time.tv_sec as u128) * (hz as u128)
        + (time.tv_nsec as u128) * (hz as u128) / 1_000_000_000;
    let start_ticks = u64::try_from(ticks).map_err(|_| FlowError::guard("boot ticks overflow"))?;
    Ok(start_ticks)
}
