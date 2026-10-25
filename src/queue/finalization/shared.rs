//! Typed references in surviving tasks; unrelated strings never convey ownership.
use super::*;

pub(super) fn permits(s: &J, plan: &Associations) -> Result<Vec<Permit>> {
    let mut after = s.clone();
    prune(&mut after, plan)?;
    let mut permits = Vec::new();
    for (id, before) in object(&s["tasks"], "tasks")? {
        retain_shared_permit(s, id, before, &after, plan, &mut permits)?;
    }
    Ok(permits)
}

fn retain_shared_permit(
    s: &J,
    id: &str,
    before: &J,
    after: &J,
    plan: &Associations,
    permits: &mut Vec<Permit>,
) -> Result<()> {
    if plan.tasks.contains_key(id) || before == &after["tasks"][id] {
        return Ok(());
    }
    if let Some(owner) = lifecycle::task_owner(s, id)? {
        permits.push(lifecycle::write_permit(&owner)?);
    }
    require(
        before["final_outcome"].is_null(),
        "foreign terminal task retains cleanup reference",
    )
}

pub(super) fn validate(s: &J, plan: &Associations) -> Result<()> {
    for task in plan.tasks.values() {
        validate_owned_claim(task, plan)?;
    }
    for (id, task) in object(&s["tasks"], "tasks")? {
        if !plan.tasks.contains_key(id) {
            validate_task(id, task, plan)?;
        }
    }
    Ok(())
}

fn validate_task(id: &str, task: &J, plan: &Associations) -> Result<()> {
    validate_claim_chain(task, plan)?;
    let lane = &task["lane"];
    if lane.is_null() {
        return Ok(());
    }
    validate_members(&lane["pending"], plan, id)?;
    if !lane["active"].is_null() {
        validate_members(&lane["active"]["members"], plan, id)?;
    }
    if let Some(history) = lane["history"].as_array() {
        for batch in history {
            validate_members(&batch["members"], plan, id)?;
        }
    }
    Ok(())
}

fn validate_claim_chain(task: &J, plan: &Associations) -> Result<()> {
    let claim = &task["claim"];
    for reference in [&claim["root"], &claim["previous"]["task"]] {
        require(
            !plan.tasks.contains_key(reference.as_str().unwrap_or("")),
            "foreign live claim authority retains this instance; unsupported isolated sharing",
        )?;
    }
    // claimed_by is live claim authority, not a historical lane annotation.
    require(
        !owned(&task["batch_delivery"]["claimed_by"], plan),
        "foreign receipt claim authority retains this instance",
    )
}

fn validate_owned_claim(task: &J, plan: &Associations) -> Result<()> {
    let claim = &task["claim"];
    for reference in [
        &claim["root"],
        &claim["previous"]["task"],
        &task["batch_delivery"]["claimed_by"]["task"],
    ] {
        if let Some(id) = reference.as_str() {
            require(
                plan.tasks.contains_key(id),
                "owned task retains cross-owner live claim authority",
            )?;
        }
    }
    Ok(())
}

fn validate_members(members: &J, plan: &Associations, lane: &str) -> Result<()> {
    let Some(members) = members.as_array() else {
        return Ok(());
    };
    for member in members {
        if let Some(task) = plan.tasks.get(member["task"].as_str().unwrap_or("")) {
            validate_member(member, task, lane)?;
        }
    }
    Ok(())
}

fn validate_member(member: &J, task: &J, lane: &str) -> Result<()> {
    require(
        member["epoch"] == task["epoch"] && member["head"] == task["candidate"],
        "cleanup lane member incarnation changed",
    )?;
    require(
        member["receipt_id"]
            .as_str()
            .is_some_and(|id| !id.is_empty())
            && member["receipt_id"] == task["batch_delivery"]["receipt_id"],
        "cleanup lane receipt changed",
    )?;
    require(
        task["batch_delivery"]["owner"] == lane
            && task["batch_delivery"]["phase"] == "accepted"
            && task["batch_delivery"]["head"] == member["head"],
        "shared lane reference is not an accepted Git receipt",
    )
}

pub(super) fn prune(s: &mut J, plan: &Associations) -> Result<()> {
    validate(s, plan)?;
    if let Some(tasks) = s["tasks"].as_object_mut() {
        for (id, task) in tasks {
            if !plan.tasks.contains_key(id) {
                let before = task.clone();
                prune_task(task, plan);
                if task != &before {
                    advance_task_version(task)?;
                }
            }
        }
    }
    Ok(())
}

fn advance_task_version(task: &mut J) -> Result<()> {
    let version = task["state_version"]
        .as_u64()
        .and_then(|v| v.checked_add(1))
        .ok_or_else(|| queue_error("shared task state version missing or exhausted"))?;
    task["state_version"] = json!(version);
    Ok(())
}

fn prune_task(task: &mut J, plan: &Associations) {
    let Some(lane) = task.get_mut("lane") else {
        return;
    };
    if lane.is_null() {
        return;
    }
    prune_group(lane, "pending", plan);
    if let Some(active) = lane.get_mut("active") {
        prune_group(active, "members", plan);
    }
    if let Some(history) = lane.get_mut("history").and_then(J::as_array_mut) {
        for batch in history {
            prune_group(batch, "members", plan);
        }
    }
}

fn owned(value: &J, plan: &Associations) -> bool {
    plan.tasks
        .contains_key(value["task"].as_str().unwrap_or(""))
}

fn prune_group(value: &mut J, key: &str, plan: &Associations) {
    if let Some(members) = value.get_mut(key).and_then(J::as_array_mut) {
        members.retain(|m| !owned(m, plan));
    }
}
