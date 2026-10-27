//! References, not record age, decide whether a completed turn can be retired.
use super::*;

pub(super) fn tools(root: &Path, active: &J, journals: &J) -> Result<Vec<String>> {
    let mut keys = Vec::new();
    let control = journals["control-tools.json"].as_object().unwrap();
    for (key, entry) in control {
        if pair_complete(entry, &journals["broker-tools.json"][key])
            && old_turn(entry, active, &journals["broker-events.json"])
            && obligations_consumed(root, entry)?
        {
            keys.push(key.clone());
        }
    }
    Ok(keys)
}

fn pair_complete(control: &J, broker: &J) -> bool {
    control["phase"] == "complete"
        && broker["phase"] == "responded"
        && control["request"] == broker["request"]
        && control["result"] == broker["result"]
}

fn old_turn(entry: &J, active: &J, events: &J) -> bool {
    let params = &entry["request"]["params"];
    let Some(turn) = params["turnId"].as_str() else {
        return false;
    };
    active["turn"].is_string()
        && params["threadId"] == active["thread"]
        && params["turnId"] != active["turn"]
        && events["turns"][turn]["thread"] == active["thread"]
        && events["turns"][turn]["turn"]["status"] == "completed"
}

fn obligations_consumed(root: &Path, entry: &J) -> Result<bool> {
    match entry["request"]["params"]["arguments"]["action"].as_str() {
        Some("finalize") => Ok(false), // commit_terminal still consumes this pair.
        Some("run") if entry["result"]["success"] == true => worker_applied(root, entry),
        Some("run") => Ok(false), // A failed reply may follow a partial dispatch.
        Some("deliver") => Ok(entry["result"]["success"] == true),
        _ => Ok(true),
    }
}

fn worker_applied(root: &Path, entry: &J) -> Result<bool> {
    if entry["collection"]["phase"] != "signalled" {
        return Ok(false);
    }
    let runtime = read(root, "runtime.json")?;
    let Some(queue_root) = runtime["lifecycle_owner"]["queue_root"].as_str() else {
        return Ok(false);
    };
    let spec: J =
        serde_json::from_str(&required(&entry["request"]["params"]["arguments"], "spec")?)?;
    let wait = required(&spec, "wait")?;
    let q = crate::queue::load_queue(Path::new(queue_root))?;
    ensure(
        q["id"] == runtime["lifecycle_owner"]["queue_id"],
        "checkpoint queue changed",
    )?;
    applied_binding(&q, &wait, &entry["collection"]["binding"])
}

fn applied_binding(q: &J, wait: &str, binding: &J) -> Result<bool> {
    let events = q["events"]
        .as_object()
        .ok_or_else(|| FlowError::guard("queue events missing"))?;
    Ok(binding.is_object()
        && events
            .values()
            .any(|e| e["wait"] == wait && e["applied"] == true && e["binding"] == *binding))
}

pub(super) fn turns(root: &Path, active: &J, journals: &J) -> Result<Vec<String>> {
    let events = &journals["broker-events.json"];
    let Some(turns) = events["turns"].as_object() else {
        return Ok(Vec::new());
    };
    let worker = read(root, "turn.json")?;
    Ok(turns
        .iter()
        .filter(|(id, entry)| {
            entry["turn"]["status"] == "completed"
                && json!(id) != active["turn"]
                && json!(id) != worker["turn"]
                && json!(id) != events["latest"]
                && !referenced(id, journals)
        })
        .map(|(id, _)| id.clone())
        .collect())
}

fn referenced(id: &str, journals: &J) -> bool {
    for name in ["broker-tools.json", "control-tools.json"] {
        if journals[name]
            .as_object()
            .unwrap()
            .values()
            .any(|entry| entry["request"]["params"]["turnId"] == id)
        {
            return true;
        }
    }
    journals["broker-events.json"]["pending"]
        .as_object()
        .is_some_and(|m| m.values().any(|e| e["params"]["turnId"] == id))
}
