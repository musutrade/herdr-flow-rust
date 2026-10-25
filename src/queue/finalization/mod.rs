//! Bounded continuation of terminal obligations, driven by existing entrypoints.
mod associations;
mod plan;
mod shared;

use super::{isolation, queue_error};
use crate::{
    lifecycle::{self, Owner, Permit},
    model::J,
    util, Result,
};
use plan::{object, Associations};
use serde::Serialize;
use serde_json::json;
use std::path::{Path, PathBuf};

fn require(ok: bool, message: &str) -> Result<()> {
    if ok {
        Ok(())
    } else {
        Err(queue_error(message))
    }
}

/// Diagnostic for this attempt only. The existing cleanup plan is the journal.
#[derive(Debug, Default, Serialize)]
pub struct CleanupReport {
    pub completed: usize,
    pub pending: usize,
    pub errors: Vec<String>,
}

/// Own the queue transaction and automatically continue any resulting terminal
/// obligation. Callers must not hold a queue lock. Failed cleanup remains pending.
pub fn task_action(
    root: &Path,
    action: &str,
    spec: &J,
    session: Option<&str>,
    factory: &crate::herdr::ReceiverFactory,
) -> Result<J> {
    {
        let _lock = super::locked(root, false)?;
        super::task_action(root, action, spec, session, factory)?;
    }
    let _report = drive(root)?;
    super::load_queue(root)
}

/// One bounded attempt per pending instance; no worker, scheduler or new ledger.
pub fn drive(root: &Path) -> Result<CleanupReport> {
    if !isolation::isolated(root)? {
        return Ok(CleanupReport::default());
    }
    let owners = pending(root)?;
    let mut report = CleanupReport::default();
    for owner in owners {
        match drive_owner(&owner) {
            Ok(()) => report.completed += 1,
            Err(error) => {
                report.pending += 1;
                report.errors.push(error.to_string());
            }
        }
    }
    Ok(report)
}

fn pending(root: &Path) -> Result<Vec<Owner>> {
    let s = super::load_queue(root)?;
    let mut owners = crate::task_env::terminal::pending_terminal_owners(root)?;
    for task in object(&s["tasks"], "tasks")?.values() {
        append_obligation(root, &s, task, &mut owners)?;
    }
    Ok(owners)
}

fn append_obligation(root: &Path, s: &J, task: &J, owners: &mut Vec<Owner>) -> Result<()> {
    if task["cleanup_obligation"].is_null() {
        return Ok(());
    }
    let owner: Owner = serde_json::from_value(task["cleanup_obligation"]["owner"].clone())?;
    require(
        owner.queue_root == util::resolve_path(root) && s["id"] == owner.queue_id,
        "pending cleanup queue identity changed",
    )?;
    if !owners.contains(&owner) {
        owners.push(owner);
    }
    Ok(())
}

fn drive_owner(owner: &Owner) -> Result<()> {
    let permit = cleanup_permit(owner)?;
    if finish_released(owner, &permit)? {
        return Ok(());
    }
    let plan = prepare(owner, &permit)?;
    crate::task_env::terminal::resume_terminal(owner, &permit)?;
    remove_associations(&plan, &permit)?;
    crate::task_env::terminal::release_terminal(owner, &permit)?;
    release_checkpoint(&plan, &permit)?;
    require(
        finish_released(owner, &permit)?,
        "cleanup release checkpoint missing",
    )
}

fn cleanup_permit(owner: &Owner) -> Result<Permit> {
    let _lock = super::locked(&owner.queue_root, false)?;
    let s = super::load_queue(&owner.queue_root)?;
    require(
        lifecycle::task_owner(&s, &owner.task_id)?.as_ref() == Some(owner),
        "pending cleanup incarnation changed",
    )?;
    let permit = lifecycle::acquire_terminal(owner)?;
    validate_terminal(&s, owner, &permit)?;
    Ok(permit)
}

fn prepare(owner: &Owner, permit: &Permit) -> Result<Associations> {
    let _lock = super::locked(&owner.queue_root, false)?;
    let s = super::load_queue(&owner.queue_root)?;
    let existing = crate::task_env::terminal::pending_terminal_owners(&owner.queue_root)?;
    if existing.contains(owner) {
        return restore_associations(&s, owner, permit);
    }
    freeze_associations(&s, owner, permit)
}

fn restore_associations(s: &J, owner: &Owner, permit: &Permit) -> Result<Associations> {
    validate_incarnation(s, owner, permit)?;
    plan::decode(
        crate::task_env::terminal::association_plan(owner, permit)?,
        owner,
    )
}

fn freeze_associations(s: &J, owner: &Owner, permit: &Permit) -> Result<Associations> {
    validate_terminal(s, owner, permit)?;
    let plan = plan::build(s, owner)?;
    shared::validate(s, &plan)?;
    crate::task_env::terminal::prepare(owner, permit, serde_json::to_value(&plan)?)?;
    Ok(plan)
}

fn validate_incarnation(s: &J, owner: &Owner, permit: &Permit) -> Result<()> {
    require(s["id"] == owner.queue_id, "cleanup queue identity changed")?;
    require(
        permit.exclusive() && permit.owner() == owner,
        "cleanup requires exact exclusive permit",
    )?;
    if !s["tasks"][&owner.task_id].is_null() {
        validate_terminal(s, owner, permit)?;
    }
    Ok(())
}

fn validate_terminal(s: &J, owner: &Owner, permit: &Permit) -> Result<()> {
    permit.check(s, true)?;
    let task = &s["tasks"][&owner.task_id];
    require(
        matches!(task["final_outcome"].as_str(), Some("success" | "failure")),
        "cleanup requires explicit final outcome",
    )?;
    require(
        task["cleanup_obligation"]["owner"] == json!(owner),
        "cleanup obligation identity changed",
    )
}

fn remove_associations(plan: &Associations, permit: &Permit) -> Result<()> {
    let root = &plan.owner.queue_root;
    let _lock = super::locked(root, false)?;
    let mut s = super::load_queue(root)?;
    validate_incarnation(&s, &plan.owner, permit)?;
    if associations_removed(&s, plan) {
        return Ok(());
    }
    let _shared_permits = shared::permits(&s, plan)?;
    let before = s.clone();
    associations::remove(root, &mut s, plan)?;
    save_cleanup(root, &before, &mut s, plan, permit)
}

fn release_checkpoint(plan: &Associations, permit: &Permit) -> Result<()> {
    let root = &plan.owner.queue_root;
    let _lock = super::locked(root, false)?;
    let mut s = super::load_queue(root)?;
    validate_incarnation(&s, &plan.owner, permit)?;
    require(
        associations_removed(&s, plan),
        "cleanup associations remain",
    )?;
    let proof = crate::task_env::terminal::plan_removal_proof(&plan.owner, permit)?;
    let epochs: std::collections::BTreeMap<_, _> = plan
        .tasks
        .iter()
        .map(|(id, task)| (id.clone(), task["epoch"].clone()))
        .collect();
    s["tasks"][&plan.owner.task_id]["cleanup_obligation"]["status"] = json!("released");
    s["tasks"][&plan.owner.task_id]["cleanup_obligation"]["plan_removal"] =
        serde_json::to_value(proof)?;
    s["tasks"][&plan.owner.task_id]["cleanup_obligation"]["cleanup_tasks"] = json!(epochs);
    write_versioned(root, &mut s)
}

fn finish_released(owner: &Owner, permit: &Permit) -> Result<bool> {
    let root = &owner.queue_root;
    let _lock = super::locked(root, false)?;
    let mut s = super::load_queue(root)?;
    validate_terminal(&s, owner, permit)?;
    let obligation = &s["tasks"][&owner.task_id]["cleanup_obligation"];
    if obligation["status"] != "released" {
        return Ok(false);
    }
    finish_released_state(root, &mut s, owner, permit)?;
    Ok(true)
}

fn finish_released_state(root: &Path, s: &mut J, owner: &Owner, permit: &Permit) -> Result<()> {
    let obligation = &s["tasks"][&owner.task_id]["cleanup_obligation"];
    let tasks = object(&obligation["cleanup_tasks"], "released cleanup tasks")?.clone();
    validate_tail_tasks(s, owner, &tasks)?;
    crate::task_env::terminal::forget_terminal(owner, permit)?;
    lifecycle::retire(owner, permit)?;
    for id in tasks.keys() {
        s["tasks"].as_object_mut().unwrap().remove(id);
    }
    write_versioned(root, s)
}

fn validate_tail_tasks(s: &J, owner: &Owner, tasks: &serde_json::Map<String, J>) -> Result<()> {
    require(
        tasks.get(&owner.task_id) == Some(&json!(owner.epoch)),
        "released cleanup owner missing",
    )?;
    validate_activity_epochs(s, owner, tasks)?;
    for (id, epoch) in tasks {
        validate_tail_task(s, owner, id, epoch)?;
    }
    Ok(())
}

fn validate_activity_epochs(
    s: &J,
    owner: &Owner,
    tasks: &serde_json::Map<String, J>,
) -> Result<()> {
    for (id, task) in object(&s["tasks"], "tasks")? {
        if id == &owner.task_id || task["lineage"]["requirement"] == owner.task_id {
            require(
                tasks.get(id) == Some(&task["epoch"]),
                "released cleanup activity changed",
            )?;
        }
    }
    Ok(())
}

fn validate_tail_task(s: &J, owner: &Owner, id: &str, epoch: &J) -> Result<()> {
    require(
        s["tasks"][id]["epoch"] == *epoch && lifecycle::task_owner(s, id)?.as_ref() == Some(owner),
        "released task belongs to another instance",
    )
}

fn associations_removed(s: &J, plan: &Associations) -> bool {
    let task = &s["tasks"][&plan.owner.task_id];
    task.is_null() || task["cleanup_obligation"]["status"] == "associations_removed"
}

fn save_cleanup(
    root: &Path,
    before: &J,
    s: &mut J,
    plan: &Associations,
    permit: &Permit,
) -> Result<()> {
    isolation::validate_layout(root, s)?;
    validate_incarnation(before, &plan.owner, permit)?;
    let mut expected = before.clone();
    associations::prune(&mut expected, plan)?;
    require(
        &expected == s,
        "cleanup save exceeds its frozen associations",
    )?;
    write_versioned(root, s)
}

fn write_versioned(root: &Path, s: &mut J) -> Result<()> {
    isolation::validate_layout(root, s)?;
    isolation::validate_version(&super::load_queue(root)?, s)?;
    isolation::advance_version(s)?;
    s["updated_at"] = json!(util::now());
    util::atomic_write_json(&root.join("queue.json"), s)
}
