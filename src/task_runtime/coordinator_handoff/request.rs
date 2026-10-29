//! Operator requests install a queue fence before any external side effect.
//! Only EX acquisition is retried; a failed transaction is never replayed here.
use super::*;
mod recovery;
use crate::{lifecycle::Permit, queue};
use std::{
    sync::Arc,
    time::{Duration, Instant},
};

fn owner(root: &Path, task: &str) -> Result<Owner> {
    let permit = crate::lifecycle::runtime_observer_permit(root)?
        .ok_or_else(|| FlowError::guard("coordinator handoff requires an isolated Owner"))?;
    ensure(
        permit.owner().task_id == task,
        "coordinator handoff task changed",
    )?;
    Ok(permit.owner().clone())
}

fn exclusive(owner: &Owner) -> Result<Permit> {
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        match crate::lifecycle::acquire(owner, true) {
            Err(error) if error.kind == crate::ErrorKind::StateBusy => {
                if Instant::now() >= deadline {
                    return Err(FlowError::new(crate::ErrorKind::StateBusy,
                        "coordinator handoff Owner EX remained busy for two seconds; no transition committed; retry the same request"));
                }
                std::thread::sleep(Duration::from_millis(20));
            }
            result => return result,
        }
    }
}

pub(crate) fn status(root: &Path, task: &str) -> Result<J> {
    let permit = crate::lifecycle::runtime_observer_permit(root)?
        .ok_or_else(|| FlowError::guard("coordinator handoff requires an isolated Owner"))?;
    ensure(
        permit.owner().task_id == task,
        "coordinator handoff task changed",
    )?;
    let state = queue::load_queue(&permit.owner().queue_root)?;
    let record = &state["tasks"][task][KEY];
    if record.is_null() {
        return Ok(json!({"phase":"not_requested","automatic_rotation":false}));
    }
    Ok(record.clone())
}

pub(crate) fn action(root: &Path, task: &str, action: &str, spec: &J) -> Result<J> {
    // owner()'s SH is dropped before acquisition; no queue/guard/env lock held.
    let owner = owner(root, task)?;
    let permit = Arc::new(exclusive(&owner)?);
    let env = crate::task_env::isolation::restore_active(&owner, permit.clone())?;
    let _root_lock = crate::guard::locked_ex(root, &permit)?;
    let receiver = util::read_state_json_bounded(root, "receiver.json", RECORD_BYTES)?;
    let guard = path(&receiver, "guard_dir")?;
    ensure(
        guard.starts_with(root) && guard != root,
        "coordinator guard escapes runtime",
    )?;
    let _guard_lock = crate::guard::locked_ex(&guard, &permit)?;
    transact(&env, root, &guard, &receiver, &permit, action, spec)
}

fn transact(
    env: &TaskEnvironment,
    root: &Path,
    guard: &Path,
    receiver: &J,
    permit: &Permit,
    action: &str,
    spec: &J,
) -> Result<J> {
    let owner = permit.owner();
    let state = queue::load_queue(&owner.queue_root)?;
    permit.check(&state, false)?;
    let from = coordinator(env, root, guard, receiver, &state)?;
    let before = &state["tasks"][&owner.task_id][KEY];
    let next = match action {
        "receiver-handoff-request" => request(env, &state, &from, before, owner, spec)?,
        "receiver-handoff-cancel" => cancel(before, spec)?,
        _ => return Err(FlowError::usage("unknown coordinator handoff action")),
    };
    if &next != before {
        commit(permit, &state, &next)?;
    }
    start_draining(permit, action, spec, next)
}

fn start_draining(permit: &Permit, action: &str, spec: &J, mut next: J) -> Result<J> {
    let records = next["records"]
        .as_array_mut()
        .ok_or_else(|| FlowError::guard("handoff records missing"))?;
    let record = records
        .last_mut()
        .ok_or_else(|| FlowError::guard("handoff record missing"))?;
    if action != "receiver-handoff-request"
        || record["request_id"] != spec["request_id"]
        || record["phase"] != "requested"
    {
        return Ok(next);
    }
    let before = queue::load_queue(&permit.owner().queue_root)?;
    record["phase"] = json!("draining");
    commit(permit, &before, &next)?;
    Ok(next)
}

fn coordinator(
    env: &TaskEnvironment,
    root: &Path,
    guard: &Path,
    receiver: &J,
    state: &J,
) -> Result<J> {
    let binding = util::read_state_json_bounded(guard, "state.json", RECORD_BYTES)?;
    let runtime = super::super::verify(env, root)?;
    ensure(
        runtime["configuration"] == "upstream" && runtime["adapter"] == "codex",
        "coordinator handoff requires the original upstream Codex runtime",
    )?;
    ensure(
        receiver["phase"] == "bound"
            && binding["runtime"] == json!(root)
            && binding["instance"] == env.instance_id
            && binding["control_protocol"] == "queue-tool-v2",
        "coordinator handoff receiver binding changed",
    )?;
    let scope = crate::guard::scope(&binding);
    ensure(
        state["guard_dir"] == json!(guard) && state["receiver"] == scope,
        "coordinator handoff current queue scope changed",
    )?;
    Ok(json!({"runtime":root,"guard":guard,"session":binding["session"],"scope":scope}))
}

fn request(
    env: &TaskEnvironment,
    state: &J,
    from: &J,
    before: &J,
    owner: &Owner,
    spec: &J,
) -> Result<J> {
    fields(spec, &["request_id", "reason"])?;
    super::super::validate_assignment(&text(spec, "request_id", 128)?)?;
    text(spec, "reason", 1024)?;
    let input_digest = util::pyjson::sha256_json_compact(spec);
    if let Some(existing) = repeated(before, spec, &input_digest)? {
        return Ok(existing);
    }
    new_request_ready(state, before, owner)?;
    let generation = before["records"].as_array().map_or(1, |r| r.len() + 1);
    let record = new_record(env, from, owner, spec, generation, &input_digest, before)?;
    let mut next = if before.is_null() {
        json!({"version":1,"owner":owner,"records":[]})
    } else {
        before.clone()
    };
    next["records"].as_array_mut().unwrap().push(record);
    next["next_generation"] = json!(generation + 1);
    next["active_generation"] = json!(generation);
    Ok(next)
}

fn repeated(before: &J, spec: &J, digest: &str) -> Result<Option<J>> {
    let Some(records) = before["records"].as_array() else {
        return Ok(None);
    };
    let Some(record) = records
        .iter()
        .find(|r| r["request_id"] == spec["request_id"])
    else {
        return Ok(None);
    };
    ensure(
        record["input_digest"] == digest && record["reason"] == spec["reason"],
        "coordinator handoff request ID was reused with different input",
    )?;
    Ok(Some(before.clone()))
}

fn new_request_ready(state: &J, before: &J, owner: &Owner) -> Result<()> {
    let task = &state["tasks"][&owner.task_id];
    ensure(
        crate::progress::lineage::is_parent(task),
        "handoff requires a versioned parent",
    )?;
    ensure(
        task["human_writer"].is_null() || task["human_writer"]["phase"] == "released",
        "finish the original human handoff before replacing the coordinator",
    )?;
    if let Some(records) = before["records"].as_array() {
        ensure(
            records.len() < GENERATIONS,
            "coordinator handoff capacity exhausted",
        )?;
        let previous = records.last().unwrap();
        ensure(
            previous["phase"] == "cancelled"
                || (previous["phase"] == "complete"
                    && previous["notices"]["ready"]["phase"] == "acknowledged"),
            "finish the original coordinator handoff first",
        )?;
    }
    single_owner(state, owner)
}

fn single_owner(state: &J, owner: &Owner) -> Result<()> {
    let tasks = state["tasks"]
        .as_object()
        .ok_or_else(|| FlowError::guard("queue tasks missing"))?;
    for (name, _) in tasks
        .iter()
        .filter(|(_, task)| task["final_outcome"].is_null())
    {
        ensure(
            crate::lifecycle::task_owner(state, name)?.as_ref() == Some(owner),
            "coordinator replacement cannot move an unfinished task of another Owner",
        )?;
    }
    Ok(())
}

fn new_record(
    env: &TaskEnvironment,
    from: &J,
    owner: &Owner,
    spec: &J,
    generation: usize,
    digest: &str,
    before: &J,
) -> Result<J> {
    let root = path(from, "runtime")?;
    let new_root = env
        .temp_root
        .join("rt")
        .join(format!("handoff-{generation}-{}", &digest[..24]));
    native::receiver::ensure_absent(&new_root)?;
    let to = json!({"runtime":new_root,"guard":new_root.join("guard"),"session":null,"scope":null});
    let configuration = frozen_configuration(&root, from)?;
    let recovery = recovery::reference(&root, owner, from, &to, generation, before)?;
    Ok(
        json!({"generation":generation,"request_id":spec["request_id"],"input_digest":digest,
        "reason":spec["reason"],"phase":"requested","from":from,"to":to,
        "configuration":configuration,"snapshot":null,
        "fence":{"generation":generation,"owner":owner,"old_runtime":root,"new_runtime":new_root},
        "binding":null,"notices":{"accept":notice(owner,generation,"accept"),"ready":notice(owner,generation,"ready")},
        "recovery":recovery,"retirement":null}),
    )
}

fn notice(owner: &Owner, generation: usize, kind: &str) -> J {
    let id = util::pyjson::sha256_json_compact(&json!([owner, generation, kind]));
    json!({"wait":format!("handoff-{kind}-{}",&id[..24]),"event":null,"delivery":null,"turn":null,"phase":"pending"})
}

fn frozen_configuration(root: &Path, from: &J) -> Result<J> {
    let runtime = util::read_state_json_bounded(root, "runtime.json", RECORD_BYTES)?;
    let journal = util::read_state_json_bounded(root, "upstream-session.json", RECORD_BYTES)?;
    upstream::endpoint(&runtime)?;
    role_models::validate(&runtime)?;
    ensure(
        journal["phase"] == "created" && journal["session"] == from["session"],
        "coordinator original creation identity changed",
    )?;
    let business = &journal["session_configuration"];
    let approval = runtime
        .get("approval_policy")
        .cloned()
        .unwrap_or(json!("untrusted"));
    ensure(
        business["approvalPolicy"] == approval && business["cwd"] == runtime["cwd"],
        "coordinator original permissions/directory changed",
    )?;
    let driver = runtime
        .get("driver_binary")
        .cloned()
        .unwrap_or(json!(std::env::current_exe()?));
    ensure(
        business["model"] == runtime["model"]["id"]
            && business["modelProvider"] == runtime["model"]["providerID"],
        "coordinator original session configuration changed",
    )?;
    Ok(
        json!({"provider":runtime["model"]["providerID"],"model":runtime["model"],
        "effort":runtime["model"]["reasoning_effort"],"approval":business["approvalPolicy"],"sandbox":business["sandbox"],
        "cwd":runtime["cwd"],"upstream_endpoint":runtime["upstream_socket"],"role_models":runtime["role_models"],
        "driver":driver,"business_session_configuration":business}),
    )
}

fn cancel(before: &J, spec: &J) -> Result<J> {
    fields(spec, &["request_id"])?;
    let mut next = before.clone();
    let records = next["records"]
        .as_array_mut()
        .ok_or_else(|| FlowError::guard("handoff was not requested"))?;
    let record = records
        .last_mut()
        .ok_or_else(|| FlowError::guard("handoff record missing"))?;
    ensure(
        record["request_id"] == spec["request_id"],
        "cancel must name the original handoff request",
    )?;
    if record["phase"] == "cancelled" {
        return Ok(before.clone());
    }
    ensure(
        record["phase"] == "draining",
        "only an unstarted draining handoff may be cancelled",
    )?;
    let reserved = path(&record["to"], "runtime")?;
    native::receiver::ensure_absent(&reserved)?;
    ensure(
        record["snapshot"].is_null() && record["retirement"].is_null(),
        "handoff already has an external effect",
    )?;
    record["phase"] = json!("cancelled");
    next["active_generation"] = J::Null;
    Ok(next)
}

fn commit(permit: &Permit, before: &J, next: &J) -> Result<()> {
    let owner = permit.owner();
    let _lock = queue::locked(&owner.queue_root, false)?;
    let mut state = queue::load_queue(&owner.queue_root)?;
    permit.check(&state, false)?;
    ensure(
        state["receiver"] == before["receiver"] && state["guard_dir"] == before["guard_dir"],
        "coordinator scope changed before handoff transaction",
    )?;
    single_owner(&state, owner)?;
    let task = &mut state["tasks"][&owner.task_id];
    ensure(
        task == &before["tasks"][&owner.task_id],
        "parent changed before handoff transaction",
    )?;
    task[KEY] = next.clone();
    task["state_version"] = json!(task["state_version"]
        .as_u64()
        .and_then(|n| n.checked_add(1))
        .ok_or_else(|| FlowError::guard("invalid parent state version"))?);
    queue::save(&owner.queue_root, &mut state)
}

#[cfg(test)]
#[path = "../../../tests/common/coordinator_handoff_request.rs"]
mod tests;
