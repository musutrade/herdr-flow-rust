//! Release settled stage execution resources through the existing queue watcher.
//! Runtime evidence stays readable; the original parent and its gate survive.
use super::*;
use crate::{lifecycle::Owner, queue, task_env::ProcessRecord};
use serde::{Deserialize, Serialize};
use std::sync::Arc;
mod execution;
mod selection;

const LIMIT: usize = 8 * 1024 * 1024;
const MARKER: &str = "stage-retirement.json";
const RECEIPT: &str = "collection-receipt.json";

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Plan {
    owner: Owner,
    delivery: String,
    scope_epoch: u64,
    phase: String,
    entries: Vec<Entry>,
    retained: Vec<J>,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Entry {
    wait: String,
    root: PathBuf,
    binding: J,
    runtime: J,
    broker: ProcessRecord,
    sessions: Vec<J>,
    presentation: Option<J>,
    frontends: Vec<ProcessRecord>,
    phase: String,
}

fn read(root: &Path, name: &str) -> Result<J> {
    util::read_state_json_bounded(root, name, LIMIT)
}

/// A retained authority marker prevents old prepare/open/restart/dispatch paths
/// from reviving execution. There is deliberately no directory deletion here.
pub(crate) fn admit(root: &Path) -> Result<()> {
    if !root.try_exists()? {
        return Ok(());
    }
    ensure(
        util::read_state_json_bounded_if_exists(root, MARKER, LIMIT)?.is_none(),
        "runtime is retired by its original stage; inspect the retained evidence, do not restart",
    )
}

/// Called only after the coordinator's signalled collection was persisted while
/// holding its control-service lock. The receipt survives ordinary log pruning.
pub(crate) fn collection(coordinator: &Path, owner: &Owner, entry: &J) -> Result<()> {
    if entry["collection"]["phase"] != "signalled" {
        return Ok(());
    }
    let value: J = serde_json::from_str(&required(&entry["result"]["contentItems"][0], "text")?)?;
    let binding = &entry["collection"]["binding"];
    let root = PathBuf::from(required(&value, "runtime")?);
    validate_collection_runtime(coordinator, owner, &root, binding)?;
    let receipt = json!({"owner":owner,"coordinator":coordinator,"runtime":root,
        "wait":value["wait"],"binding":binding});
    save_collection(&root, &receipt)
}

fn validate_collection_runtime(
    coordinator: &Path,
    owner: &Owner,
    root: &Path,
    binding: &J,
) -> Result<()> {
    let assignment = required(binding, "assignment")?;
    validate_assignment(&assignment)?;
    ensure(
        root == coordinator.parent().unwrap().join(assignment)
            && read(root, "runtime.json")?["lifecycle_owner"] == json!(owner),
        "collection receipt runtime changed",
    )
}

fn save_collection(root: &Path, receipt: &J) -> Result<()> {
    let old = util::read_state_json_bounded_if_exists(root, RECEIPT, LIMIT)?;
    ensure(
        old.as_ref().is_none_or(|v| v == receipt),
        "collection receipt changed",
    )?;
    if old.is_none() {
        util::atomic_write_json(&root.join(RECEIPT), receipt)?;
    }
    Ok(())
}

/// Freeze selection before stage continuation clears the old scope. Filesystem
/// reads only: no native RPC, process signalling or directory deletion here.
pub(crate) fn prepare(env: &mut TaskEnvironment, state: &mut J) -> Result<J> {
    let Some(owner) = crate::task_env::isolation::owner(env) else {
        return Ok(J::Null);
    };
    let delivery = required(&env.delivery, "id")?;
    if !env.stage_resources.is_null() {
        let old = load(env)?;
        if old.delivery == delivery {
            register_notification(state, &old)?;
            return Ok(summary(&old));
        }
        ensure(
            old.phase == "complete",
            "finish the original stage resource plan first",
        )?;
    }
    let plan = selection::prepare(env, state, owner, delivery)?;
    save(env, &plan)?;
    register_notification(state, &plan)?;
    Ok(summary(&plan))
}

fn notification_id(plan: &Plan) -> String {
    format!(
        "stage-{}",
        &util::pyjson::sha256_hex(plan.delivery.as_bytes())[..32]
    )
}

fn register_notification(state: &mut J, plan: &Plan) -> Result<()> {
    if plan.entries.is_empty() {
        return Ok(());
    }
    for entry in &plan.entries {
        let wait = &mut state["waits"][&entry.wait];
        ensure(
            crate::progress::binding(wait)? == entry.binding,
            "retired result wait binding changed",
        )?;
        wait["closed"] = json!(true);
    }
    let id = notification_id(plan);
    let old = &state["waits"][&id];
    if !old.is_null() {
        return ensure(
            old["task"] == plan.owner.task_id
                && old["epoch"] == plan.owner.epoch
                && old["stage_delivery"] == plan.delivery
                && old["kind"] == "signal",
            "stage cleanup notification identity changed",
        );
    }
    queue::register_wait(
        state,
        &json!({"id":id,"kind":"signal","task":plan.owner.task_id,
        "epoch":plan.owner.epoch,"owner":"coordinator","reason":"stage resource cleanup",
        "condition":"original stage resource plan settled; ACK notice without task-apply",
        "timeout_seconds":604800,"stage_delivery":plan.delivery}),
        util::now(),
    )
    .map(|_| ())
}

fn load(env: &TaskEnvironment) -> Result<Plan> {
    let plan: Plan = serde_json::from_value(env.stage_resources.clone())?;
    ensure(
        crate::task_env::isolation::owner(env).as_ref() == Some(&plan.owner)
            && plan.entries.len() <= 128
            && plan.retained.len() <= 128,
        "stage resource plan Owner or bounds changed",
    )?;
    Ok(plan)
}

fn save(env: &mut TaskEnvironment, plan: &Plan) -> Result<()> {
    let value = serde_json::to_value(plan)?;
    ensure(
        serde_json::to_vec(&value)?.len() <= LIMIT,
        "stage resource plan exceeds bound",
    )?;
    env.stage_resources = value;
    env.persist()
}

fn summary(plan: &Plan) -> J {
    json!({"phase":plan.phase,"delivery":plan.delivery,"selected":plan.entries.len(),
        "completed":plan.entries.iter().filter(|e| e.phase == "complete").count(),
        "retained":plan.retained})
}

/// The watcher invokes this outside every control/queue/environment permit.
/// One bounded attempt per pending parent; finalization always takes precedence.
pub(crate) fn drive(queue_root: &Path) -> Result<()> {
    if !queue::isolation::isolated(queue_root)? {
        return Ok(());
    }
    let state = queue::load_queue(queue_root)?;
    drive_tasks(&state)
}

fn drive_tasks(state: &J) -> Result<()> {
    let Some(tasks) = state.get("tasks") else {
        return Ok(());
    };
    let tasks = tasks
        .as_object()
        .ok_or_else(|| FlowError::guard("tasks missing"))?;
    for task in tasks.values().filter(|t| pending(t)) {
        let owner = crate::lifecycle::task_owner(state, &required(task, "task")?)?
            .ok_or_else(|| FlowError::guard("stage cleanup Owner missing"))?;
        if let Err(error) = drive_owner(&owner) {
            if error.kind != crate::ErrorKind::StateBusy {
                eprintln!("stage resource cleanup retained: {error}");
            }
        }
    }
    Ok(())
}

fn pending(task: &J) -> bool {
    task["stage_delivery"]["phase"] == "complete"
        && task["stage_delivery"]["resource_cleanup"]["phase"] == "pending"
        && task["final_outcome"].is_null()
        && task["cleanup_obligation"].is_null()
}

fn drive_owner(owner: &Owner) -> Result<()> {
    let permit = Arc::new(crate::lifecycle::acquire(owner, true)?);
    let mut env = crate::task_env::isolation::restore_active(owner, permit.clone())?;
    let mut plan = load(&env)?;
    validate_current(&plan, &permit)?;
    let result = execution::drive(&mut env, &mut plan, &permit);
    publish_summary(&plan, &permit, result.as_ref().err())?;
    result
}

fn validate_current(plan: &Plan, permit: &crate::lifecycle::Permit) -> Result<()> {
    let _lock = queue::locked(&plan.owner.queue_root, false)?;
    let state = queue::load_queue(&plan.owner.queue_root)?;
    permit.check(&state, false)?;
    ensure(
        pending(&state["tasks"][&plan.owner.task_id])
            && state["tasks"][&plan.owner.task_id]["stage_delivery"]["delivery"]["id"]
                == plan.delivery,
        "stage resource checkpoint changed",
    )
}

fn publish_summary(
    plan: &Plan,
    permit: &crate::lifecycle::Permit,
    error: Option<&FlowError>,
) -> Result<()> {
    let _lock = queue::locked(&plan.owner.queue_root, false)?;
    let mut state = queue::load_queue(&plan.owner.queue_root)?;
    permit.check(&state, false)?;
    let parent = &mut state["tasks"][&plan.owner.task_id];
    ensure(
        parent["stage_delivery"]["delivery"]["id"] == plan.delivery,
        "stage receipt changed",
    )?;
    let mut result = summary(plan);
    result["error"] = error.map(|e| json!(e.to_string())).unwrap_or(J::Null);
    parent["stage_delivery"]["resource_cleanup"] = result;
    if plan.phase == "complete" || error.is_some_and(|e| e.kind != crate::ErrorKind::StateBusy) {
        notify(&mut state, plan, permit, error)?;
    }
    queue::save(&plan.owner.queue_root, &mut state)
}

fn notify(
    state: &mut J,
    plan: &Plan,
    permit: &crate::lifecycle::Permit,
    error: Option<&FlowError>,
) -> Result<()> {
    let complete = plan.phase == "complete";
    let summary = if complete {
        format!("Stage {} resource cleanup completed. ACK this host notice, inspect the original parent, then continue with fresh development and acceptance. This notice requires no task-apply and grants no new authorization.", plan.delivery)
    } else {
        format!("Stage {} cleanup retains its original obligations: {}. ACK this host notice; inspect/recover the original resources without replaying delivery or applying a business result.", plan.delivery, error.unwrap())
    };
    let observed = queue::Observed {
        outcome: if complete { "complete" } else { "decision" }.into(),
        summary,
        evidence: Some(plan.owner.queue_root.to_string_lossy().into_owned()),
        suffix: Some(
            if complete {
                "resources_complete"
            } else {
                "resources_retained"
            }
            .into(),
        ),
    };
    queue::record_stage_observation(state, &notification_id(plan), permit, observed, complete)
}

/// Terminal cleanup includes frozen descendants even after their parent session
/// was deleted. Runtime directories are retained, so ordinary scanning still works.
pub(crate) fn sessions(env: &TaskEnvironment) -> Result<Vec<J>> {
    if env.stage_resources.is_null() {
        return Ok(vec![]);
    }
    Ok(load(env)?
        .entries
        .into_iter()
        .flat_map(|e| e.sessions)
        .collect())
}

/// The frozen persistence identity survives a crash after Herdr closes its pane
/// but before the ordinary presentation journal records that close.
pub(crate) fn presentation(env: &TaskEnvironment, root: &Path) -> Result<Option<J>> {
    if env.stage_resources.is_null() {
        return Ok(None);
    }
    Ok(load(env)?
        .entries
        .into_iter()
        .find(|e| e.root == root && e.phase != "planned")
        .and_then(|e| e.presentation))
}

#[cfg(test)]
#[path = "../../tests/common/stage_retirement_selection.rs"]
mod selection_tests;
#[cfg(test)]
#[path = "../../tests/common/stage_retirement.rs"]
mod tests;
