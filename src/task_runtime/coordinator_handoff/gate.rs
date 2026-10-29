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
    if records.iter().any(|record| record["phase"] != "cancelled") {
        return Ok(true);
    }
    super::mirror::cancelled(task)?;
    Ok(false)
}

pub(crate) fn check_workspace(state: &J, path: &Path) -> Result<()> {
    validate_queue(state)?;
    receiver_workspace(state, path)?;
    let Some(tasks) = state["tasks"].as_object() else {
        return Ok(());
    };
    for task in tasks.values().filter(|task| !task[KEY].is_null()) {
        let checkout = super::path(&task["environment"], "checkout")?;
        if crate::task_env::paths_overlap(path, &checkout) {
            ensure(
                !blocked(task)?,
                "coordinator handoff requires verified continuation; new checkout work is fenced",
            )?;
            super::mirror::workspace(task)?;
        }
    }
    Ok(())
}

fn receiver_workspace(state: &J, workspace: &Path) -> Result<()> {
    let receiver = &state["receiver"]["codex_runtime"];
    if receiver.is_null() {
        return Ok(());
    }
    let checkout = super::path(receiver, "cwd")?;
    if crate::task_env::paths_overlap(workspace, &checkout) {
        unregistered_receiver(state)?;
    }
    Ok(())
}

fn unregistered_receiver(state: &J) -> Result<()> {
    let receiver = &state["receiver"];
    if receiver["codex_runtime"].is_null() {
        return Ok(());
    }
    let task = required(receiver, "task")?;
    if state["tasks"][task][KEY].is_null() {
        super::mirror::unregistered(&super::path(&receiver["codex_runtime"], "runtime")?)?;
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
    let task = &state["tasks"][&owner.task_id];
    if blocked(task)? {
        return Ok(true);
    }
    super::mirror::runtime(root, task)?;
    Ok(false)
}

pub(crate) fn admit_control(state: &J, owner: &Owner, action: &str) -> Result<()> {
    validate_queue(state)?;
    let task = &state["tasks"][&owner.task_id];
    if matches!(action, "inspect" | "ack") {
        return Ok(());
    }
    unregistered_receiver(state)?;
    if !blocked(task)? {
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
