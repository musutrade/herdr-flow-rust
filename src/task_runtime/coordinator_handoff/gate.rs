//! A recorded intention is never permission to create a successor. Until the
//! delivery/configuration driver supplies its exact proof, only original work
//! can drain. These checks also protect readers of a partially written record.
use super::*;

fn blocked(task: &J) -> Result<bool> {
    if task[KEY].is_null() {
        return Ok(false);
    }
    let records = task[KEY]["records"]
        .as_array()
        .ok_or_else(|| FlowError::guard("coordinator handoff records missing"))?;
    // This foundation exposes no replacement/restore operation. A later phase
    // label or ready ACK alone cannot prove the native permissions were restored.
    Ok(records.iter().any(|record| record["phase"] != "cancelled"))
}

pub(crate) fn check_workspace(state: &J, path: &Path) -> Result<()> {
    validate_queue(state)?;
    let Some(tasks) = state["tasks"].as_object() else {
        return Ok(());
    };
    for task in tasks.values().filter(|task| !task[KEY].is_null()) {
        if blocked(task)? {
            let checkout = super::path(&task["environment"], "checkout")?;
            ensure(
                !crate::task_env::paths_overlap(path, &checkout),
                "coordinator handoff requires verified continuation; new checkout work is fenced",
            )?;
        }
    }
    Ok(())
}

pub(crate) fn runtime_blocked(root: &Path, runtime: &J) -> Result<bool> {
    let Some(value) = runtime.get("lifecycle_owner").filter(|v| !v.is_null()) else {
        return util::protected::is_runtime_namespace(root);
    };
    let owner: Owner = serde_json::from_value(value.clone())?;
    let state = crate::queue::load_queue(&owner.queue_root)?;
    ensure(
        crate::lifecycle::task_owner(&state, &owner.task_id)?.as_ref() == Some(&owner),
        "coordinator handoff runtime Owner changed",
    )?;
    blocked(&state["tasks"][&owner.task_id])
}

pub(crate) fn admit_control(state: &J, owner: &Owner, action: &str) -> Result<()> {
    validate_queue(state)?;
    let task = &state["tasks"][&owner.task_id];
    if !blocked(task)? {
        return Ok(());
    }
    if matches!(action, "inspect" | "ack") {
        return Ok(());
    }
    let phase = task[KEY]["records"]
        .as_array()
        .and_then(|r| r.last())
        .and_then(|r| r["phase"].as_str());
    ensure(
        action == "apply" && matches!(phase, Some("requested" | "draining")),
        "coordinator handoff permits only inspection and original result/notification settlement",
    )
}
