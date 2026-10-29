//! Explicit checkout writer transfer. The protected queue is the authority;
//! viewing a task, a timeout, and a recovery verdict alone never transfer it.
use super::*;
use crate::{lifecycle::Owner, queue};
use std::os::unix::fs::MetadataExt;
use std::sync::Arc;
mod gate;
mod observe;
#[cfg(test)]
#[path = "../../../tests/common/human_writer.rs"]
pub(crate) mod tests;
pub(crate) use gate::{
    admit_launch, admit_native, admit_workspace, automatic_input_allowed, check_workspace,
};

const LIMIT: usize = 1024 * 1024;
const KEY: &str = "human_writer";

fn read(root: &Path, file: &str) -> Result<Option<J>> {
    util::read_state_json_bounded_if_exists(root, file, LIMIT)
}

fn owner(coordinator: &Path, task: &str) -> Result<Owner> {
    let permit = crate::lifecycle::runtime_observer_permit(coordinator)?
        .ok_or_else(|| FlowError::guard("human handoff requires an isolated Owner"))?;
    ensure(permit.owner().task_id == task, "human handoff task changed")?;
    Ok(permit.owner().clone())
}

pub fn status(coordinator: &Path, task: &str) -> Result<J> {
    let owner = owner(coordinator, task)?;
    let state = queue::load_queue(&owner.queue_root)?;
    let record = &state["tasks"][task][KEY];
    if record.is_null() {
        return Ok(json!({"phase":"not_requested","writer":"agent"}));
    }
    validate(record, &owner)?;
    Ok(record.clone())
}

pub fn action(coordinator: &Path, task: &str, action: &str, spec: &J) -> Result<J> {
    let owner = owner(coordinator, task)?;
    let permit = Arc::new(crate::lifecycle::acquire(&owner, true)?);
    let env = crate::task_env::isolation::restore_active(&owner, permit.clone())?;
    let (state, before) = action_state(&env, coordinator, &permit)?;
    let mut next = transition(&env, coordinator, &owner, &before, spec, &state, action)?;
    if next == before {
        return Ok(next);
    }
    next["updated_at"] = json!(util::now());
    commit(&owner, &before, &next, false)?;
    notice(coordinator, &next);
    Ok(next)
}

fn action_state(
    env: &TaskEnvironment,
    coordinator: &Path,
    permit: &crate::lifecycle::Permit,
) -> Result<(J, J)> {
    let owner = permit.owner();
    let state = queue::load_queue(&owner.queue_root)?;
    permit.check(&state, false)?;
    ensure(
        crate::progress::lineage::is_parent(&state["tasks"][&owner.task_id]),
        "human handoff requires a versioned requirement",
    )?;
    validate_coordinator(env, coordinator, &state)?;
    let before = state["tasks"][&owner.task_id][KEY].clone();
    if !before.is_null() {
        validate(&before, owner)?;
    }
    Ok((state, before))
}

fn transition(
    env: &TaskEnvironment,
    coordinator: &Path,
    owner: &Owner,
    before: &J,
    spec: &J,
    state: &J,
    action: &str,
) -> Result<J> {
    match action {
        "human-request" => request(env, coordinator, owner, before, spec),
        "human-acquire" => acquire(env, before, spec, state),
        "human-return" => give_back(env, before, spec, state),
        _ => Err(FlowError::usage("unknown human handoff action")),
    }
}

fn validate_coordinator(env: &TaskEnvironment, root: &Path, state: &J) -> Result<()> {
    crate::task_env::validate_path(env, root)?;
    let receiver = util::read_state_json(root, "receiver.json")?;
    let guard = PathBuf::from(required(&receiver, "guard_dir")?);
    let binding = util::read_state_json(&guard, "state.json")?;
    ensure(
        receiver["phase"] == "bound"
            && binding["runtime"] == json!(root)
            && binding["instance"] == env.instance_id
            && state["receiver"] == crate::guard::scope(&binding),
        "human handoff coordinator binding changed",
    )
}

fn fields(spec: &J, allowed: &[&str]) -> Result<()> {
    let object = spec
        .as_object()
        .ok_or_else(|| FlowError::guard("handoff spec must be an object"))?;
    ensure(
        object.keys().all(|k| allowed.contains(&k.as_str())),
        "unsupported handoff field",
    )
}

fn text(spec: &J, field: &str) -> Result<String> {
    let value = required(spec, field)?;
    ensure(value.len() <= 4096, "human handoff text exceeds bound")?;
    Ok(value)
}

fn request(
    env: &TaskEnvironment,
    coordinator: &Path,
    owner: &Owner,
    before: &J,
    spec: &J,
) -> Result<J> {
    validate_request(spec)?;
    if !before.is_null() && before["id"] == spec["id"] {
        ensure(
            before["actor"] == spec["actor"] && before["reason"] == spec["reason"],
            "human request identity changed",
        )?;
        return Ok(before.clone());
    }
    ensure(
        before.is_null() || before["phase"] == "released",
        "finish the original human handoff first",
    )?;
    recovery_ready(coordinator, owner)?;
    request_record(env, coordinator, owner, spec)
}

fn validate_request(spec: &J) -> Result<()> {
    fields(spec, &["id", "actor", "reason"])?;
    validate_assignment(&text(spec, "id")?)?;
    text(spec, "actor")?;
    text(spec, "reason")?;
    Ok(())
}

fn recovery_ready(coordinator: &Path, owner: &Owner) -> Result<()> {
    let recovery = coordinator.join("recovery-observer");
    let context = util::read_state_json(&recovery, "observer.json")?;
    ensure(
        context["owner"] == json!(owner) && context["coordinator"] == json!(coordinator),
        "configure the original recovery observer before handoff",
    )?;
    ensure(
        util::read_state_json(&recovery, "agent.json")?["phase"] == "ready",
        "recovery session is unavailable",
    )?;
    observe::settled_recovery(&recovery)
}

fn request_record(env: &TaskEnvironment, coordinator: &Path, owner: &Owner, spec: &J) -> Result<J> {
    let checkout = env.workspace_root.join("checkout");
    let identity = directory(&checkout)?;
    ensure(
        env.worktree_registrations
            .iter()
            .any(|w| w.path == checkout),
        "human checkout is not registered",
    )?;
    Ok(
        json!({"version":1,"owner":owner,"id":spec["id"],"actor":spec["actor"],"reason":spec["reason"],
        "coordinator":coordinator,"checkout":checkout,"identity":identity,
        "phase":"draining","writer":"agent","requested_at":util::now(),
        "next_step":"Wait for the original execution to settle, then human-acquire with this exact id and actor. No human writes are authorized yet."}),
    )
}

fn bound(before: &J, spec: &J) -> Result<()> {
    ensure(
        !before.is_null() && spec["id"] == before["id"] && spec["actor"] == before["actor"],
        "human handoff id or actor changed",
    )
}

fn acquire(env: &TaskEnvironment, before: &J, spec: &J, state: &J) -> Result<J> {
    fields(spec, &["id", "actor"])?;
    bound(before, spec)?;
    if before["phase"] == "human" {
        return Ok(before.clone());
    }
    ensure(
        matches!(before["phase"].as_str(), Some("draining" | "blocked")),
        "human writer cannot be acquired in this phase",
    )?;
    observe::settled_recovery(
        &PathBuf::from(required(before, "coordinator")?).join("recovery-observer"),
    )?;
    let mut next = before.clone();
    match observe::quiescent(env, before, state) {
        Ok(proof) => {
            next["proof"] = proof;
            next["phase"] = json!("human");
            next["writer"] = next["actor"].clone();
            next["before"] = json!(fingerprint(before)?);
            next["next_step"] = json!("Only this checkout is transferred. Finish all edits and child commands before human-return; do not edit after returning.");
            next.as_object_mut().unwrap().remove("blocked_reason");
        }
        Err(error) => {
            next["blocked_reason"] = json!(error.to_string());
            next["next_step"] = json!("Inspect the original execution or unknown operation. Retain this request and retry acquisition only after the condition changes; never replay work.");
        }
    }
    Ok(next)
}

fn give_back(env: &TaskEnvironment, before: &J, spec: &J, state: &J) -> Result<J> {
    fields(spec, &["id", "actor", "note"])?;
    bound(before, spec)?;
    let note = text(spec, "note")?;
    if before["phase"] == "verifying" {
        ensure(
            before["return"]["note"] == note,
            "human return already recorded with another note",
        )?;
        return Ok(before.clone());
    }
    ensure(
        before["phase"] == "human",
        "only the current human writer can return",
    )?;
    let proof = observe::quiescent(env, before, state)?;
    let mut next = before.clone();
    next["return"] = json!({"id":util::uuid_hex(),"note":note,"fingerprint":fingerprint(before)?,"proof":proof,"at":util::now()});
    next["phase"] = json!("verifying");
    next["writer"] = json!("none");
    next["next_step"] = json!("Wait for the existing Recovery Agent to verify this return and notify the original coordinator. Neither human nor automated checkout writes are admitted.");
    Ok(next)
}

fn directory(path: &Path) -> Result<J> {
    let metadata = std::fs::symlink_metadata(path)?;
    ensure(
        metadata.is_dir() && path.is_absolute() && path.canonicalize()? == path,
        "human checkout must be a canonical directory",
    )?;
    Ok(json!({"dev":metadata.dev(),"ino":metadata.ino()}))
}

fn fingerprint(record: &J) -> Result<String> {
    let checkout = PathBuf::from(required(record, "checkout")?);
    ensure(
        directory(&checkout)? == record["identity"],
        "human checkout identity changed",
    )?;
    util::fingerprint(&checkout)
}

fn validate(record: &J, owner: &Owner) -> Result<()> {
    validate_identity(record, owner)?;
    validate_paths(record)?;
    validate_phase(record)?;
    ensure(
        serde_json::to_vec(record)?.len() <= LIMIT,
        "human handoff exceeds bound",
    )
}

fn validate_identity(record: &J, owner: &Owner) -> Result<()> {
    ensure(
        record["version"] == 1 && record["owner"] == json!(owner),
        "human handoff Owner changed",
    )?;
    validate_assignment(&required(record, "id")?)?;
    text(record, "actor")?;
    text(record, "reason")?;
    Ok(())
}

fn validate_paths(record: &J) -> Result<()> {
    for field in ["coordinator", "checkout"] {
        ensure(
            Path::new(&required(record, field)?).is_absolute(),
            "human handoff path is not absolute",
        )?;
    }
    ensure(
        record["identity"]["dev"].is_u64() && record["identity"]["ino"].is_u64(),
        "human checkout identity missing",
    )
}

fn validate_phase(record: &J) -> Result<()> {
    ensure(
        matches!(
            record["phase"].as_str(),
            Some("draining" | "human" | "verifying" | "blocked" | "released")
        ),
        "unknown human handoff phase",
    )?;
    let expected = match record["phase"].as_str() {
        Some("human") => record["actor"].clone(),
        Some("draining" | "released") => json!("agent"),
        _ => json!("none"),
    };
    ensure(
        record["writer"] == expected,
        "human writer phase is inconsistent",
    )?;
    if matches!(
        record["phase"].as_str(),
        Some("verifying" | "blocked" | "released")
    ) {
        validate_assignment(&required(&record["return"], "id")?)?;
        required(&record["return"], "fingerprint")?;
    }
    Ok(())
}

fn commit(owner: &Owner, before: &J, next: &J, invalidate: bool) -> Result<()> {
    validate(next, owner)?;
    let _lock = queue::locked(&owner.queue_root, false)?;
    let mut state = queue::load_queue(&owner.queue_root)?;
    ensure(
        crate::lifecycle::task_owner(&state, &owner.task_id)?.as_ref() == Some(owner),
        "human handoff Owner changed",
    )?;
    commit_task(
        &mut state["tasks"][&owner.task_id],
        before,
        next,
        invalidate,
    )?;
    queue::save(&owner.queue_root, &mut state)
}

fn commit_task(task: &mut J, before: &J, next: &J, invalidate: bool) -> Result<()> {
    ensure(
        task[KEY] == *before && task["final_outcome"].is_null(),
        "human handoff changed or task ended",
    )?;
    archive(task, before, next)?;
    task[KEY] = next.clone();
    if invalidate {
        crate::progress::lineage::invalidate_human_candidate(task)?;
    }
    task["state_version"] = json!(task["state_version"]
        .as_u64()
        .and_then(|v| v.checked_add(1))
        .ok_or_else(|| FlowError::guard("invalid task version"))?);
    Ok(())
}

fn archive(task: &mut J, before: &J, next: &J) -> Result<()> {
    if before.is_null() || before["id"] == next["id"] {
        return Ok(());
    }
    if task["human_handoffs"].is_null() {
        task["human_handoffs"] = json!([]);
    }
    let history = task["human_handoffs"]
        .as_array_mut()
        .ok_or_else(|| FlowError::guard("invalid human history"))?;
    ensure(
        history.len() < 64 && history.iter().all(|v| v["id"] != next["id"]),
        "human request id already used or history bound reached",
    )?;
    history.push(before.clone());
    ensure(
        serde_json::to_vec(history)?.len() <= LIMIT,
        "human handoff history exceeds bound",
    )
}

fn notice(coordinator: &Path, record: &J) {
    if let Ok(runtime) = util::read_state_json(coordinator, "runtime.json") {
        let message = format!(
            "human writer: {} · {}",
            record["phase"].as_str().unwrap_or("unknown"),
            record["actor"].as_str().unwrap_or("unknown")
        );
        let _ = tui::recovery_notice(coordinator, &runtime, &message);
    }
}

pub(super) fn verification_fault(context: &J, state: &J) -> Result<Option<J>> {
    let owner: Owner = serde_json::from_value(context["owner"].clone())?;
    let record = &state["tasks"][&owner.task_id][KEY];
    if record.is_null() {
        return Ok(None);
    }
    validate(record, &owner)?;
    if record["phase"] != "verifying" {
        return Ok(None);
    }
    ensure(
        record["coordinator"] == context["coordinator"],
        "human verification coordinator changed",
    )?;
    Ok(Some(
        json!({"source":format!("human-writer:{}", owner.task_id),"id":record["return"]["id"],
        "kind":"human_writer_return","handoff":record,
        "instruction":"Verify the returned checkout and actual task state. This is neither independent acceptance nor permission to replay an old operation."}),
    ))
}

pub(super) fn pending(context: &J, state: &J) -> Result<bool> {
    let task = required(&context["owner"], "task_id")?;
    let record = &state["tasks"][task][KEY];
    Ok(!record.is_null() && record["phase"] != "released")
}

pub(super) fn finish_verification(context: &J, incident: &J, result: &J) -> Result<J> {
    let owner: Owner = serde_json::from_value(context["owner"].clone())?;
    let permit = crate::lifecycle::write_permit(&owner)?;
    let env = crate::task_env::instance::allocation::restore(&owner, &permit)?;
    let state = queue::load_queue(&owner.queue_root)?;
    let before = &state["tasks"][&owner.task_id][KEY];
    validate_verification(before, &owner, context, incident)?;
    if same_verification(before, incident, result) {
        return Ok(before["verification"]["result"].clone());
    }
    commit_verification(&env, &owner, &state, incident, result)
}

fn validate_verification(before: &J, owner: &Owner, context: &J, incident: &J) -> Result<()> {
    validate(before, owner)?;
    ensure(
        before["coordinator"] == context["coordinator"]
            && incident["fault"]["handoff"]["owner"] == json!(owner),
        "human verification binding changed",
    )?;
    ensure(
        before["return"]["id"] == incident["fault"]["id"],
        "human verification is stale",
    )
}

fn same_verification(before: &J, incident: &J, result: &J) -> bool {
    before["verification"]["incident"] == incident["id"]
        && before["verification"]["turn"] == incident["turn"]
        && before["verification"]["model_result"] == *result
}

fn commit_verification(
    env: &TaskEnvironment,
    owner: &Owner,
    state: &J,
    incident: &J,
    result: &J,
) -> Result<J> {
    let before = &state["tasks"][&owner.task_id][KEY];
    ensure(
        before["phase"] == "verifying",
        "human verification is stale",
    )?;
    ensure(
        result["disposition"] != "repair_requested",
        "human return cannot request another watcher repair",
    )?;
    let mut next = before.clone();
    let verdict = verify_return(env, &mut next, state, result);
    next["updated_at"] = json!(util::now());
    next["verification"] = json!({"incident":incident["id"],"agent":incident["agent"],"turn":incident["turn"],"model_result":result,"result":verdict,"at":next["updated_at"]});
    append_verification(&mut next)?;
    commit(owner, before, &next, next["phase"] == "released")?;
    notice(Path::new(&required(&next, "coordinator")?), &next);
    Ok(verdict)
}

fn append_verification(next: &mut J) -> Result<()> {
    if next["verifications"].is_null() {
        next["verifications"] = json!([]);
    }
    let entry = json!({"return":next["return"],"verification":next["verification"]});
    let history = next["verifications"]
        .as_array_mut()
        .ok_or_else(|| FlowError::guard("invalid verification history"))?;
    ensure(
        history.len() < 64,
        "human verification history exceeds bound",
    )?;
    history.push(entry);
    Ok(())
}

fn verify_return(env: &TaskEnvironment, next: &mut J, state: &J, result: &J) -> J {
    let mut verdict = result.clone();
    next["phase"] = json!("blocked");
    if result["disposition"] == "recovered" {
        match release_proof(env, next, state) {
            Ok(proof) => {
                next["verification_proof"] = proof;
                next["phase"] = json!("released");
                next["writer"] = json!("agent");
                next["next_step"] = json!("Original coordinator may continue. Register the current candidate and obtain fresh independent acceptance; prior verdicts remain historical.");
                return verdict;
            }
            Err(error) => {
                verdict["disposition"] = json!("insufficient_evidence");
                verdict["current_state"] = json!(format!("Host verification blocked: {error}"));
                verdict["next_step"] = json!("Inspect human-status, settle the blocking condition, then explicitly reacquire and return the same handoff. Do not replay an old operation.");
            }
        }
    }
    next["next_step"] = verdict["next_step"].clone();
    verdict
}

fn release_proof(env: &TaskEnvironment, record: &J, state: &J) -> Result<J> {
    let proof = observe::quiescent(env, record, state)?;
    ensure(
        record["return"]["fingerprint"] == json!(fingerprint(record)?),
        "checkout changed after human return; retain the write fence",
    )?;
    Ok(proof)
}
