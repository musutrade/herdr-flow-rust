//! Reclaim completed turns without forgetting an operation in the current turn.
//! The broker owns this transaction; the control service must leave a prepared
//! checkpoint alone until the broker finishes it under the service lock.
use super::*;
mod retirement;
const FILE: &str = "record-checkpoint.json";
const FILES: [&str; 3] = [
    "control-tools.json",
    "broker-tools.json",
    "broker-events.json",
];

fn read(root: &Path, name: &str) -> Result<J> {
    let limit = if name == "broker-events.json" {
        16 * 1024 * 1024
    } else {
        1024 * 1024
    };
    let value = util::read_state_json_bounded_if_exists(root, name, limit)?.unwrap_or(json!({}));
    ensure(value.is_object(), "invalid checkpoint journal")?;
    Ok(value)
}

pub(super) fn advance(root: &Path, thread: &J, turn: &J) -> Result<()> {
    ensure(
        thread.as_str().is_some_and(|s| !s.is_empty())
            && turn.as_str().is_some_and(|s| !s.is_empty()),
        "checkpoint turn identity missing",
    )?;
    util::atomic_write_json(
        &root.join("broker-turn.json"),
        &json!({"thread":thread,"turn":turn}),
    )
}

pub(super) fn native_turn(root: &Path, event: &J) -> Result<()> {
    if event["method"] != "turn/started" {
        return Ok(());
    }
    let active = read(root, "receiver-turn.json")?;
    let params = &event["params"];
    if active["thread"] == params["threadId"] && active["turn"] == params["turn"]["id"] {
        advance(root, &active["thread"], &active["turn"])?;
    }
    Ok(())
}

pub(in crate::task_runtime) fn current_request(root: &Path, request: &J) -> Result<()> {
    let active = read(root, "broker-turn.json")?;
    if active.is_object() && !active.as_object().unwrap().is_empty() {
        ensure(
            active["thread"] == request["params"]["threadId"]
                && active["turn"] == request["params"]["turnId"],
            "control request belongs to a previous turn; never replay",
        )?;
    }
    Ok(())
}

pub(in crate::task_runtime) fn ready(root: &Path) -> Result<()> {
    if !available(root)? {
        return Err(FlowError::new(
            crate::ErrorKind::StateBusy,
            "record checkpoint pending; wait for broker",
        ));
    }
    Ok(())
}

pub(in crate::task_runtime) fn available(root: &Path) -> Result<bool> {
    Ok(read(root, FILE)?["phase"] != "pruning")
}

/// False means a control operation still owns the lock. Do not drain native
/// events or accept another operation until the prepared transaction completes.
pub(super) fn resume(root: &Path) -> Result<bool> {
    let plan = read(root, FILE)?;
    if plan["phase"] != "pruning" {
        return Ok(true);
    }
    let _permit = match crate::lifecycle::runtime_permit(root) {
        Err(error) if error.kind == crate::ErrorKind::StateBusy => return Ok(false),
        result => result?,
    };
    let Some(_lock) = util::FileLock::acquire(&root.join("control-service.lock"), false)? else {
        return Ok(false);
    };
    finish(root, plan)?;
    Ok(true)
}

pub(super) fn compact(root: &Path) -> Result<()> {
    let Some(_lock) = util::FileLock::acquire(&root.join("control-service.lock"), false)? else {
        return Ok(());
    };
    let previous = read(root, FILE)?;
    if previous["phase"] == "pruning" {
        return finish(root, previous);
    }
    let plan = prepare(root, &previous)?;
    if plan["files"].as_object().unwrap().is_empty() {
        return Ok(());
    }
    // The bounded manifest precedes every deletion. A crash may leave any
    // prefix applied; each remaining entry must still match its exact digest.
    util::atomic_write_json(&root.join(FILE), &plan)?;
    finish(root, plan)
}

fn prepare(root: &Path, previous: &J) -> Result<J> {
    let mut active = read(root, "broker-turn.json")?;
    if active.as_object().unwrap().is_empty() {
        active = read(root, "receiver-turn.json")?;
    }
    let mut journals = journals(root)?;
    let keys = retirement::tools(root, &active, &journals)?;
    let mut files = json!({});
    for name in ["control-tools.json", "broker-tools.json"] {
        files[name] = proofs(&journals[name], &keys);
        remove(&mut journals[name], &files[name])?;
    }
    let turns = retirement::turns(root, &active, &journals)?;
    files["broker-events.json"] = proofs(&journals["broker-events.json"]["turns"], &turns);
    files
        .as_object_mut()
        .unwrap()
        .retain(|_, v| !v.as_object().unwrap().is_empty());
    let generation = previous["generation"]
        .as_u64()
        .unwrap_or(0)
        .saturating_add(1);
    Ok(json!({"phase":"pruning","generation":generation,"active":active,"files":files}))
}

fn journals(root: &Path) -> Result<J> {
    let mut journals = json!({});
    for name in FILES {
        journals[name] = read(root, name)?;
    }
    Ok(journals)
}

fn proofs(journal: &J, keys: &[String]) -> J {
    let proofs: serde_json::Map<String, J> = keys
        .iter()
        .map(|key| (key.clone(), json!(digest(&journal[key]))))
        .collect();
    json!(proofs)
}

fn digest(value: &J) -> String {
    util::pyjson::sha256_hex(value.to_string().as_bytes())
}

fn remove(journal: &mut J, proofs: &J) -> Result<()> {
    let entries = journal
        .as_object_mut()
        .ok_or_else(|| FlowError::guard("invalid retirement entries"))?;
    let proofs = proofs
        .as_object()
        .ok_or_else(|| FlowError::guard("invalid retirement proofs"))?;
    ensure(proofs.len() <= 64, "checkpoint manifest exceeds bound")?;
    for (key, proof) in proofs {
        if let Some(entry) = entries.get(key) {
            ensure(
                json!(digest(entry)) == *proof,
                "checkpoint entry changed; preserve evidence",
            )?;
        }
        entries.remove(key);
    }
    Ok(())
}

fn finish(root: &Path, mut plan: J) -> Result<()> {
    let files = plan["files"]
        .as_object()
        .ok_or_else(|| FlowError::guard("checkpoint files missing"))?;
    ensure(
        files.keys().all(|name| FILES.contains(&name.as_str())),
        "foreign checkpoint file",
    )?;
    // Validate every remaining record before writing even the first file.
    let replacements = replacements(root, files)?;
    for (name, journal) in replacements {
        util::atomic_write_json(&root.join(name), &journal)?;
    }
    plan["retired"] = json!({});
    for name in FILES {
        plan["retired"][name] = json!(plan["files"][name].as_object().map_or(0, |m| m.len()));
    }
    plan.as_object_mut().unwrap().remove("files");
    plan["phase"] = json!("complete");
    util::atomic_write_json(&root.join(FILE), &plan)
}

fn replacements(root: &Path, files: &serde_json::Map<String, J>) -> Result<Vec<(&'static str, J)>> {
    let mut replacements = Vec::new();
    for name in FILES {
        if let Some(proofs) = files.get(name) {
            replacements.push((name, retired_journal(root, name, proofs)?));
        }
    }
    Ok(replacements)
}

fn retired_journal(root: &Path, name: &str, proofs: &J) -> Result<J> {
    // A missing file is storage loss, not an already-applied deletion.
    // Atomic replacement always leaves the journal itself present.
    let limit = if name == "broker-events.json" {
        16 * 1024 * 1024
    } else {
        1024 * 1024
    };
    let mut journal = util::read_state_json_bounded(root, name, limit)?;
    let entries = if name == "broker-events.json" {
        &mut journal["turns"]
    } else {
        &mut journal
    };
    remove(entries, proofs)?;
    Ok(journal)
}

#[cfg(test)]
#[path = "../../../tests/common/record_checkpoint.rs"]
mod tests;
