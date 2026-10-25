//! The queue's immutable seed owns the fixed envelope slot before mkdir.
use super::*;
use crate::{
    lifecycle::{Owner, Permit},
    util, FlowError,
};

pub(crate) fn initialize_prepared(env: &mut TaskEnvironment) -> Result<()> {
    let permit = env
        .creation_permit
        .lock()
        .map_err(|_| FlowError::guard("creation lease poisoned"))?
        .clone()
        .ok_or_else(|| FlowError::guard("bootstrap requires creation lease"))?;
    initialize(env, &permit)?;
    super::super::super::env::reserve_environment(env, env.state_file())
}

fn initialize(env: &mut TaskEnvironment, permit: &Permit) -> Result<()> {
    validate_initialization(env, permit)?;
    #[cfg(test)]
    super::tests::checkpoint("seed-saved");
    super::super::super::env::lock_seed(env)?;
    let a = allocation(env)?;
    let parent = a.parent.verify()?;
    let directory = reserve_envelope(env, &parent)?;
    initialize_image(env, &directory)?;
    fs::sync_pair(&directory, &parent)?;
    #[cfg(test)]
    super::tests::checkpoint("envelope-initialized");
    verify(env)
}
fn validate_initialization(env: &TaskEnvironment, permit: &Permit) -> Result<()> {
    verify(env)?;
    let mapping = seed(env)?;
    verify_lease(env, permit)?;
    require(
        mapping["phase"] == "creating" && mapping["bootstrap"].is_null(),
        "bootstrap completed; refusing recreation",
    )?;
    validate_seed_image(env, &mapping)
}
fn validate_seed_image(env: &TaskEnvironment, mapping: &serde_json::Value) -> Result<()> {
    require(
        mapping["creation_seed"] == serde_json::to_value(env)?
            && allocation(env)?.envelope.is_none(),
        "bootstrap seed changed",
    )
}
fn reserve_envelope(env: &TaskEnvironment, parent: &std::fs::File) -> Result<std::fs::File> {
    let basename = env
        .state_file()
        .file_name()
        .ok_or_else(|| FlowError::guard("missing envelope slot"))?;
    let directory = fs::reserved_dir(parent, basename)?;
    fs::entries(&directory, &[".flow-document-v2", "authority.json"])?;
    Ok(directory)
}
fn initialize_image(env: &mut TaskEnvironment, directory: &std::fs::File) -> Result<()> {
    let proof = fs::Directory::capture(env.state_file())?;
    env.creation
        .as_mut()
        .unwrap()
        .allocation
        .as_mut()
        .unwrap()
        .envelope = Some(proof);
    fs::marker(directory, true, false)?;
    fs::initial_file(directory, "authority.json", &serde_json::to_vec(env)?)
}

fn verify_lease(env: &TaskEnvironment, permit: &Permit) -> Result<()> {
    let owner = crate::task_env::isolation::owner(env)
        .ok_or_else(|| FlowError::guard("seed owner absent"))?;
    let held = permit.owner();
    require(
        permit.exclusive()
            && held.queue_root == owner.queue_root
            && held.queue_id == owner.queue_id
            && held.task_id == owner.task_id
            && held.epoch == owner.epoch
            && (held.instance_id.is_empty() || held.instance_id == owner.instance_id),
        "bootstrap lease differs from owner",
    )?;
    Ok(())
}

/// Only a queue-bound bootstrap may reconstruct an incomplete control document.
/// Once bootstrap identity is checkpointed, absence/replacement is an error.
pub(crate) fn restore(owner: &Owner, permit: &Permit) -> Result<TaskEnvironment> {
    let mapping = restore_mapping(owner)?;
    if mapping["creation_seed"].is_null() {
        return crate::task_env::restore_environment(&owner.task_id);
    }
    let initial = restore_seed(owner, &mapping)?;
    if !mapping["bootstrap"].is_null() {
        return restore_known(&initial, &mapping["bootstrap"]);
    }
    require(
        mapping["phase"] == "creating",
        "active environment has no bootstrap identity",
    )?;
    restore_initial(initial, permit)
}
fn restore_mapping(owner: &Owner) -> Result<J> {
    control::verify_current(&owner.queue_root)?;
    let q = crate::queue::load_queue(&owner.queue_root)?;
    require(
        crate::lifecycle::task_owner(&q, &owner.task_id)?.as_ref() == Some(owner),
        "bootstrap restore owner changed",
    )?;
    Ok(q["tasks"][&owner.task_id]["environment"].clone())
}
fn restore_seed(owner: &Owner, mapping: &J) -> Result<TaskEnvironment> {
    let initial: TaskEnvironment = serde_json::from_value(mapping["creation_seed"].clone())?;
    require(
        crate::task_env::isolation::owner(&initial).as_ref() == Some(owner),
        "bootstrap seed belongs to another owner",
    )?;
    verify(&initial)?;
    Ok(initial)
}
fn restore_initial(initial: TaskEnvironment, permit: &Permit) -> Result<TaskEnvironment> {
    // Only the exact initial-image prefix is recoverable; no IO error is absence.
    if let Some(value) = initialized_image(&initial)? {
        let saved: TaskEnvironment = serde_json::from_value(value)?;
        let proof = allocation(&saved)?.envelope.as_ref().unwrap();
        return restore_known(&saved, &serde_json::to_value(proof)?);
    }
    let mut env = initial;
    initialize(&mut env, permit)?;
    Ok(env)
}

fn restore_known(initial: &TaskEnvironment, value: &J) -> Result<TaskEnvironment> {
    let proof: fs::Directory = serde_json::from_value(value.clone())?;
    require(
        proof.path == initial.state_file(),
        "bootstrap proof path changed",
    )?;
    proof.verify()?;
    let env = crate::task_env::restore_environment(&initial.task_id)?;
    require(
        crate::task_env::isolation::owner(&env) == crate::task_env::isolation::owner(initial)
            && allocation(&env)?.envelope.as_ref() == Some(&proof),
        "envelope identity or owner changed",
    )?;
    verify(&env)?;
    Ok(env)
}

fn initialized_image(initial: &TaskEnvironment) -> Result<Option<J>> {
    if fs::absent(initial.state_file())? {
        return Ok(None);
    }
    let (_parent, directory) = initial_directory(initial)?;
    let Some(bytes) = fs::read_initial(&directory, "authority.json")? else {
        return Ok(None);
    };
    compare_initial_image(initial, &bytes)
}
fn initial_directory(initial: &TaskEnvironment) -> Result<(std::fs::File, std::fs::File)> {
    let parent = allocation(initial)?.parent.verify()?;
    let directory = fs::open_child(&parent, initial.state_file().file_name().unwrap())?;
    fs::entries(&directory, &[".flow-document-v2", "authority.json"])?;
    Ok((parent, directory))
}
fn expected_image(initial: &TaskEnvironment) -> Result<(J, Vec<u8>)> {
    let proof = fs::Directory::capture(initial.state_file())?;
    let mut expected = serde_json::to_value(initial)?;
    expected["creation"]["allocation"]["envelope"] = serde_json::to_value(proof)?;
    // Typed serialization preserves exactly the original writer's field order.
    let typed: TaskEnvironment = serde_json::from_value(expected.clone())?;
    let encoded = serde_json::to_vec(&typed)?;
    Ok((expected, encoded))
}
fn compare_initial_image(initial: &TaskEnvironment, bytes: &[u8]) -> Result<Option<J>> {
    let (expected, encoded) = expected_image(initial)?;
    require(
        encoded.starts_with(bytes),
        "bootstrap payload differs from immutable seed",
    )?;
    if bytes.len() != encoded.len() {
        return Ok(None);
    }
    require(
        util::read_json(initial.state_file())? == expected,
        "bootstrap image changed",
    )?;
    Ok(Some(expected))
}
