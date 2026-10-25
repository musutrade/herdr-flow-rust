//! Fresh control namespace and local operation permits for lifecycle instances.
use super::{registry, TaskEnvironment};
use crate::{
    lifecycle::{Owner, Permit},
    util, FlowError, Result,
};
use serde_json::json;
use std::{
    path::{Path, PathBuf},
    sync::Arc,
};

pub(super) fn data_root(root: &Path) -> Result<PathBuf> {
    if util::protected::is_document(&root.join("registry.json"))? {
        if !util::protected::is_namespace(&root.join("registry.json"))? {
            return Err(FlowError::guard("isolated registry namespace is missing"));
        }
        Ok(root.join("registry.json"))
    } else {
        Ok(root.to_path_buf())
    }
}
pub(super) fn enable(queue: &Path, id: &str) -> Result<()> {
    let root = registry::state_root()?;
    let _lock = registry::lock_registry()?;
    let doc = root.join("registry.json");
    initialize_control(&root, &doc, queue, id)?;
    let binding = util::read_state_json(&doc, "queue-binding.json")?;
    if binding != json!({"queue_root":queue,"queue_id":id}) {
        return Err(FlowError::guard(
            "environment registry belongs to another queue",
        ));
    }
    Ok(())
}
fn initialize_control(root: &Path, doc: &Path, queue: &Path, id: &str) -> Result<()> {
    if !util::protected::is_document(doc)? {
        if std::fs::read_dir(root)?.any(|e| e.map_or(true, |e| e.file_name() != "registry.lock")) {
            return Err(FlowError::guard(
                "isolated environment control must be empty; no migration",
            ));
        }
        util::protected::create(doc)?;
        util::protected::namespace(doc)?;
        util::atomic_write_json(doc, &json!({"entries":{}}))?;
        util::atomic_write_json(
            &doc.join("queue-binding.json"),
            &json!({"queue_root":queue,"queue_id":id}),
        )?;
    }
    Ok(())
}

pub fn owner(env: &TaskEnvironment) -> Option<Owner> {
    let i = env.creation.as_ref()?;
    let queue = i.lifecycle_queue.as_ref()?;
    Some(Owner {
        queue_root: queue.clone(),
        queue_id: i.binding.queue_id.clone(),
        task_id: i.binding.task_id.clone(),
        epoch: i.binding.epoch,
        instance_id: env.instance_id.clone(),
    })
}
pub(crate) fn isolated_document(env: &TaskEnvironment) -> Result<bool> {
    if util::protected::is_document(env.state_file())? {
        return Ok(true);
    }
    match env.state_file().parent() {
        Some(parent) => util::protected::is_namespace(parent),
        None => Ok(false),
    }
}
pub(crate) fn requires_managed_spawn(env: &TaskEnvironment) -> bool {
    owner(env).is_some() || isolated_document(env).unwrap_or(true)
}
pub(crate) fn mutation_permit(env: &TaskEnvironment) -> Result<Option<Arc<Permit>>> {
    let Some(owner) = owner(env) else {
        if isolated_document(env)? {
            return Err(FlowError::guard("isolated instance lost owner"));
        }
        return Ok(None);
    };
    verify_durable_owner(env, &owner)?;
    if !env.owns_lock() {
        return Err(FlowError::guard(
            "instance mutation requires environment ownership",
        ));
    }
    let held = env
        .creation_permit
        .lock()
        .map_err(|_| FlowError::guard("creation lease poisoned"))?;
    if let Some(lease) = held.as_ref() {
        verify_creation_lease(lease, &owner)?;
        return Ok(Some(lease.clone()));
    }
    Ok(Some(Arc::new(crate::lifecycle::write_permit(&owner)?)))
}
fn verify_creation_lease(lease: &Permit, owner: &Owner) -> Result<()> {
    let p = lease.owner();
    if !lease.exclusive()
        || p.queue_root != owner.queue_root
        || p.queue_id != owner.queue_id
        || p.task_id != owner.task_id
        || p.epoch != owner.epoch
        || (!p.instance_id.is_empty() && p.instance_id != owner.instance_id)
    {
        return Err(FlowError::guard("invalid instance creation lease"));
    }
    Ok(())
}
/// Terminal holds lifecycle EX and queue lock; this restore never requests SH.
pub fn restore_for_terminal(owner: &Owner, permit: &Permit) -> Result<TaskEnvironment> {
    if !permit.exclusive() || permit.owner() != owner {
        return Err(FlowError::guard(
            "terminal restore requires exact exclusive permit",
        ));
    }
    let env = super::restore_environment(&owner.task_id)?;
    if self::owner(&env).as_ref() != Some(owner) {
        return Err(FlowError::guard("terminal environment owner changed"));
    }
    let queue = crate::queue::load_queue(&owner.queue_root)?;
    permit.check(&queue, true)?;
    let binding = &env.creation.as_ref().unwrap().binding;
    super::instance::verify_terminal_identity(&env, binding, &owner.instance_id)?;
    Ok(env)
}

/// Initialize only a fresh runtime/receiver namespace for this active instance.
pub(crate) fn runtime_namespace(env: &TaskEnvironment, root: &Path) -> Result<()> {
    let Some(owner) = owner(env) else {
        return Ok(());
    };
    let _permit = mutation_permit(env)?;
    super::validate_path(env, root)?;
    if util::protected::is_namespace(root)? {
        return verify_runtime_owner(root, &owner);
    }
    // The parent belongs to the environment; the namespace itself cannot adopt
    // existing runtime data. Partial initialization is retained, never overwritten.
    let parent = root
        .parent()
        .ok_or_else(|| FlowError::guard("runtime has no parent"))?;
    util::ensure_private_dir(parent, true)?;
    util::protected::runtime_namespace(root)?;
    util::atomic_write_json(
        &root.join("lifecycle-owner.json"),
        &serde_json::to_value(owner)?,
    )
}

fn verify_runtime_owner(root: &Path, owner: &Owner) -> Result<()> {
    if !util::protected::is_runtime_namespace(root)? {
        return Err(FlowError::guard("runtime authority namespace is missing"));
    }
    let saved: Owner =
        serde_json::from_value(util::read_state_json(root, "lifecycle-owner.json")?)?;
    if &saved != owner {
        return Err(FlowError::guard("runtime namespace owner changed"));
    }
    Ok(())
}

fn verify_durable_owner(env: &TaskEnvironment, expected: &Owner) -> Result<()> {
    // During initial persist the EX creation lease owns an as-yet absent doc.
    if !env.state_file().exists() {
        return verify_initial_persist(env);
    }

    let value = util::read_json(env.state_file())?;
    let saved: TaskEnvironment = serde_json::from_value(value)?;
    if owner(&saved).as_ref() != Some(expected)
        || saved.workspace_root != env.workspace_root
        || saved.temp_root != env.temp_root
        || saved.state_file() != env.state_file()
    {
        return Err(FlowError::guard(
            "durable environment owner or roots changed",
        ));
    }
    Ok(())
}

fn verify_initial_persist(env: &TaskEnvironment) -> Result<()> {
    let lease = env
        .creation_permit
        .lock()
        .map_err(|_| FlowError::guard("creation lease poisoned"))?;
    if lease
        .as_ref()
        .is_some_and(|p| p.exclusive() && p.owner().instance_id.is_empty())
    {
        return Ok(());
    }
    Err(FlowError::guard(
        "environment authority disappeared; no recreation",
    ))
}
