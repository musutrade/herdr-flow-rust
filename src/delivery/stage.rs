//! Retire a confirmed stage intent without completing the parent environment.
//! The checkpoint precedes clearing the environment journal and unfreezing the
//! queue, so interruption can only finish that transition, never redeliver.
use super::*;

pub fn complete(env: &mut TaskEnvironment, queue: &Path, delivery: &str) -> Result<J> {
    let permit = crate::task_env::isolation::mutation_permit(env)?;
    ensure(
        env.owns_lock() && env.state == EnvironmentState::Active,
        "stage continuation requires owned active environment",
    )?;
    let _lock = crate::queue::locked(queue, false)?;
    let mut state = crate::queue::load_queue(queue)?;
    if let Some(permit) = permit.as_ref() {
        permit.check(&state, false)?;
    }
    checkpoint(env, queue, delivery, &mut state)?;
    resume(env, queue, &mut state)
}

fn checkpoint(env: &mut TaskEnvironment, queue: &Path, id: &str, state: &mut J) -> Result<()> {
    let previous = &state["tasks"][&env.task_id]["stage_delivery"];
    if previous["delivery"]["id"] == id {
        return validate_checkpoint(env, queue, previous);
    }
    ensure(
        previous["phase"] != "continuing",
        "finish the recorded stage transition first",
    )?;
    validate_confirmed(env, queue, id, state)?;
    let resource_cleanup = crate::task_runtime::retirement::prepare(env, state)?;
    state["tasks"][&env.task_id]["stage_delivery"] =
        json!({"phase":"continuing","delivery":env.delivery,"resource_cleanup":resource_cleanup});
    crate::queue::save(queue, state)
}

fn validate_confirmed(env: &TaskEnvironment, queue: &Path, id: &str, state: &J) -> Result<()> {
    ensure(env.delivery["id"] == id, "stage delivery ID changed")?;
    validate_identity(env, queue, &env.delivery)?;
    validate_freeze(state, &env.delivery)?;
    let parent = &state["tasks"][&env.task_id];
    ensure(
        parent["control"] == "delivering" && parent["delivery"] == id,
        "stage intent has not frozen its parent",
    )?;
    lineage::stage::quiescent(state, parent)
}

fn validate_identity(env: &TaskEnvironment, queue: &Path, intent: &J) -> Result<()> {
    ensure(
        intent["phase"] == "confirmed" && intent["stage"].is_object(),
        "stage continuation requires an actually confirmed stage delivery",
    )?;
    ensure(
        intent["task"] == env.task_id
            && intent["instance"] == env.instance_id
            && intent["queue"] == json!(queue),
        "stage delivery owner changed",
    )?;
    ensure(
        intent["receipt"].as_object().is_some_and(|r| !r.is_empty()),
        "stage delivery receipt missing",
    )
}

fn validate_checkpoint(env: &TaskEnvironment, queue: &Path, checkpoint: &J) -> Result<()> {
    ensure(
        matches!(
            checkpoint["phase"].as_str(),
            Some("continuing" | "complete")
        ),
        "invalid stage checkpoint phase",
    )?;
    validate_identity(env, queue, &checkpoint["delivery"])
}

fn resume(env: &mut TaskEnvironment, queue: &Path, state: &mut J) -> Result<J> {
    let checkpoint = state["tasks"][&env.task_id]["stage_delivery"].clone();
    if checkpoint["phase"] == "complete" {
        return Ok(checkpoint);
    }
    validate_resume(state, &checkpoint["delivery"])?;
    clear_intent(env, &checkpoint["delivery"])?;
    lineage::stage::continue_parent(state, &env.task_id, util::now())?;
    crate::queue::save(queue, state)?;
    Ok(state["tasks"][&env.task_id]["stage_delivery"].clone())
}

fn validate_resume(state: &J, intent: &J) -> Result<()> {
    let parent = &state["tasks"][field(intent, "task")?];
    ensure(
        parent["control"] == "delivering" && parent["delivery"] == intent["id"],
        "stage checkpoint no longer owns the parent",
    )?;
    validate_freeze(state, intent)?;
    lineage::stage::quiescent(state, parent)
}

fn clear_intent(env: &mut TaskEnvironment, expected: &J) -> Result<()> {
    ensure(
        env.delivery.is_null() || env.delivery == *expected,
        "stage journal changed after checkpoint",
    )?;
    // Re-persist even an empty in-memory journal: an earlier persist error may
    // have left this handle cleared while the durable environment still has it.
    env.delivery = J::Null;
    env.persist()
}
