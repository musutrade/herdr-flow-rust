//! Receiver retirement is transient cleanup metadata, never inferred from absence.
use super::*;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Dependency {
    owner: Owner,
    guard_dir: PathBuf,
    scope: J,
    retired_by: Option<Owner>,
}

pub(super) struct Retirement {
    owner: Owner,
    dependency: Dependency,
}

pub(super) type Retirements = BTreeMap<String, Retirement>;

fn descriptor(plan: &Associations) -> Result<Option<Dependency>> {
    if plan.owned_receiver {
        return Ok(None);
    }
    let Some(owner) = plan.receiver_owner.clone() else {
        return Ok(None);
    };
    let guard_dir = plan
        .guard_dir
        .clone()
        .ok_or_else(|| queue_error("receiver dependency path missing"))?;
    Ok(Some(Dependency {
        owner,
        guard_dir,
        scope: plan.receiver.clone(),
        retired_by: None,
    }))
}

fn stored(task: &J) -> Result<Option<Dependency>> {
    let value = &task["cleanup_obligation"]["receiver_dependency"];
    if value.is_null() {
        return Ok(None);
    }
    Ok(Some(serde_json::from_value(value.clone())?))
}

fn validate(actual: &Dependency, expected: &Dependency) -> Result<()> {
    require(
        actual.owner == expected.owner
            && actual.guard_dir == expected.guard_dir
            && actual.scope == expected.scope,
        "frozen receiver dependency changed",
    )?;
    require(
        actual
            .retired_by
            .as_ref()
            .is_none_or(|owner| owner == &expected.owner),
        "receiver retirement owner changed",
    )
}

/// Caller holds A's EX and queue lock. Root plan persistence precedes this write.
pub(super) fn checkpoint(s: &mut J, plan: &Associations, permit: &Permit) -> Result<()> {
    let Some(expected) = descriptor(plan)? else {
        return Ok(());
    };
    permit.check(s, true)?;
    require(
        permit.owner() == &plan.owner,
        "receiver dependency requires exact owner",
    )?;
    if let Some(current) = stored(&s["tasks"][&plan.owner.task_id])? {
        return validate(&current, &expected);
    }
    set_dependency(&mut s["tasks"][&plan.owner.task_id], &expected)?;
    super::write_versioned(&plan.owner.queue_root, s)
}

fn set_dependency(task: &mut J, dependency: &Dependency) -> Result<()> {
    task["cleanup_obligation"]["receiver_dependency"] = serde_json::to_value(dependency)?;
    Ok(())
}

/// The only permitted change to A's frozen row is its exact receiver descriptor
/// and a retirement bit naming that descriptor's complete Owner.
pub(super) fn normalize_task(expected: &mut J, current: &J, plan: &Associations) -> Result<()> {
    let Some(actual) = stored(current)? else {
        return Ok(());
    };
    let original = descriptor(plan)?.ok_or_else(|| queue_error("unplanned receiver dependency"))?;
    validate(&actual, &original)?;
    set_dependency(expected, &actual)
}

pub(super) fn retired(s: &J, plan: &Associations) -> Result<bool> {
    let Some(expected) = descriptor(plan)? else {
        return Ok(false);
    };
    let Some(current) = stored(&s["tasks"][&plan.owner.task_id])? else {
        return Ok(false);
    };
    validate(&current, &expected)?;
    Ok(current.retired_by.as_ref() == Some(&expected.owner))
}

/// Snapshot immutable plans once under the queue lock, before side effects.
/// Reuse this exact map for actual and expected prune; A's plan never changes.
pub(super) fn retirement_targets(s: &J, retiring: &Associations) -> Result<Retirements> {
    let mut targets = Retirements::new();
    if !retiring.owned_receiver {
        return Ok(targets);
    }
    require(
        retiring.receiver_owner.as_ref() == Some(&retiring.owner),
        "retiring receiver lacks owner proof",
    )?;
    for (id, task) in object(&s["tasks"], "tasks")? {
        if let Some(target) = retirement_target(s, id, task, retiring)? {
            targets.insert(id.clone(), target);
        }
    }
    Ok(targets)
}

fn retirement_target(
    s: &J,
    id: &str,
    task: &J,
    retiring: &Associations,
) -> Result<Option<Retirement>> {
    if id == retiring.owner.task_id || task["cleanup_obligation"]["status"] != "pending" {
        return Ok(None);
    }
    let owner = dependent_owner(s, id, task, retiring)?;
    let Some(dependency) = frozen_dependency(&owner, task, retiring)? else {
        return Ok(None);
    };
    Ok(Some(Retirement { owner, dependency }))
}

fn dependent_owner(s: &J, id: &str, task: &J, retiring: &Associations) -> Result<Owner> {
    let owner: Owner = serde_json::from_value(task["cleanup_obligation"]["owner"].clone())?;
    require(
        owner.queue_root == retiring.owner.queue_root && owner.queue_id == retiring.owner.queue_id,
        "receiver dependency queue changed",
    )?;
    require(
        lifecycle::task_owner(s, id)?.as_ref() == Some(&owner),
        "receiver dependent owner changed",
    )?;
    Ok(owner)
}

fn frozen_dependency(
    owner: &Owner,
    task: &J,
    retiring: &Associations,
) -> Result<Option<Dependency>> {
    let Some(value) = crate::task_env::terminal::frozen_association_plan(owner)? else {
        return Ok(None);
    };
    let frozen = plan::decode(value, owner)?;
    let Some(mut dependency) = descriptor(&frozen)? else {
        return Ok(None);
    };
    if !matches_retiring(&dependency, retiring) {
        return Ok(None);
    }
    validate_stored(task, &dependency)?;
    dependency.retired_by = Some(retiring.owner.clone());
    Ok(Some(dependency))
}

fn validate_stored(task: &J, expected: &Dependency) -> Result<()> {
    if let Some(actual) = stored(task)? {
        validate(&actual, expected)?;
    }
    Ok(())
}

fn matches_retiring(dependency: &Dependency, retiring: &Associations) -> bool {
    dependency.owner == retiring.owner
        && Some(&dependency.guard_dir) == retiring.guard_dir.as_ref()
        && dependency.scope == retiring.receiver
}

/// B's resources are absent; this metadata is committed atomically with clearing
/// B's receiver association. No A business fields or ordinary permits are used.
pub(super) fn apply(s: &mut J, retirements: &Retirements) -> Result<()> {
    for (id, retirement) in retirements {
        require(
            lifecycle::task_owner(s, id)?.as_ref() == Some(&retirement.owner)
                && s["tasks"][id]["cleanup_obligation"]["status"] == "pending",
            "receiver dependent incarnation changed",
        )?;
        set_dependency(&mut s["tasks"][id], &retirement.dependency)?;
    }
    Ok(())
}
