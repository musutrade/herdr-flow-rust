//! Queue-side instance binding. Callers retain the queue lock before env ownership.
use super::{ensure, model, object, task, text, J};
use crate::{task_env::instance::InstanceBinding, task_env::TaskEnvironment, Result};
use serde_json::json;

/// Validate an explicit request against the current, idle task incarnation.
pub fn requested(s: &J, spec: &J) -> Result<InstanceBinding> {
    let t = task(s, spec.get("task"), spec.get("state_version"))?;
    let binding = request_binding(s, &t, spec)?;
    require_idle(s, &t)?;
    require_no_activity(&t)?;
    Ok(binding)
}

fn request_binding(s: &J, t: &J, spec: &J) -> Result<InstanceBinding> {
    let name = text(spec.get("task"), "task", 128)?;
    let queue_id = text(spec.get("queue_id"), "queue_id", 128)?;
    let epoch = spec["epoch"].as_u64().filter(|n| *n > 0);
    validate_request_tuple(s, t, spec, &queue_id, &name, epoch)?;
    Ok(InstanceBinding {
        queue_id,
        task_id: name,
        epoch: epoch.unwrap(),
    })
}

fn validate_request_tuple(
    s: &J,
    t: &J,
    spec: &J,
    queue: &str,
    name: &str,
    epoch: Option<u64>,
) -> Result<()> {
    ensure(
        s["id"] == queue && t["task"] == name && epoch.is_some() && t["epoch"] == spec["epoch"],
        "environment queue/task/epoch identity changed",
    )
}

fn require_idle(s: &J, t: &J) -> Result<()> {
    ensure(!model::bool_or(s, "stopped", false), "queue is stopped")?;
    ensure(
        t["control"] == "running"
            && matches!(t["phase"].as_str(), Some("waiting" | "coordinating")),
        "environment creation requires an idle running task",
    )
}

fn require_no_activity(t: &J) -> Result<()> {
    ensure(
        t["writer"].is_null(),
        "environment task already has a writer",
    )?;
    empty_collection(t, "pending", true)?;
    empty_collection(t, "operations", false)?;
    require_no_publication(t)
}

fn require_no_publication(t: &J) -> Result<()> {
    for key in ["lane", "pr", "batch_delivery"] {
        ensure(
            t[key].is_null(),
            "environment task has publication/lane ownership",
        )?;
    }
    Ok(())
}

fn empty_collection(t: &J, key: &str, array: bool) -> Result<()> {
    let empty = if t.get(key).is_none() {
        true // Requirement parents do not have activity operation collections.
    } else if array {
        t[key].as_array().is_some_and(Vec::is_empty)
    } else {
        t[key].as_object().is_some_and(serde_json::Map::is_empty)
    };
    ensure(empty, "environment task has pending results or operations")
}

/// Check an existing mapping without overwriting foreign or stale ownership.
pub fn mapping(s: &J, binding: &InstanceBinding, env: &TaskEnvironment) -> Result<()> {
    let record = &s["tasks"][&binding.task_id]["environment"];
    if record.is_null() {
        return Ok(());
    }
    object(record, "environment mapping")?;
    ensure(
        record["queue_id"] == binding.queue_id
            && record["task_id"] == binding.task_id
            && record["epoch"] == binding.epoch
            && record["instance_id"] == env.instance_id
            && record["state_file"] == json!(env.state_file())
            && record["checkout"] == json!(env.workspace_root.join("checkout"))
            && matches!(record["phase"].as_str(), Some("creating" | "active")),
        "queue environment mapping belongs to another instance",
    )
}

/// Publish a recoverable checkpoint; the queue caller persists before Git work.
pub fn checkpoint(
    s: &mut J,
    binding: &InstanceBinding,
    env: &TaskEnvironment,
    phase: &str,
    now: f64,
) -> Result<()> {
    mapping(s, binding, env)?;
    let t = &mut s["tasks"][&binding.task_id];
    let version = t["state_version"]
        .as_u64()
        .and_then(|n| n.checked_add(1))
        .ok_or_else(|| super::progress_error("invalid task state_version"))?;
    t["environment"] = json!({"queue_id":binding.queue_id,"task_id":binding.task_id,
        "epoch":binding.epoch,"instance_id":env.instance_id,"state_file":env.state_file(),
        "checkout":env.workspace_root.join("checkout"),"phase":phase});
    t["state_version"] = json!(version);
    t["updated_at"] = json!(now);
    Ok(())
}

/// Local tuple check for instance-owned native creation/recovery, not a closing barrier.
pub fn runtime_owner(s: &J, env: &TaskEnvironment, activity: &J) -> Result<()> {
    let Some(creation) = &env.creation else {
        return Ok(());
    };
    let binding = &creation.binding;
    let t = &s["tasks"][&binding.task_id];
    ensure(
        s["id"] == binding.queue_id
            && t["task"] == binding.task_id
            && t["epoch"] == binding.epoch
            && t["environment"]["phase"] == "active",
        "runtime owner queue/task/epoch is no longer current",
    )?;
    mapping(s, binding, env)?;
    ensure(
        activity["task"] == binding.task_id
            || activity["lineage"]["requirement"] == binding.task_id,
        "runtime activity belongs to another lifecycle task",
    )
}
