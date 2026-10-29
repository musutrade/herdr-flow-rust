use super::*;

/// Compare persisted prefixes as well as validating the new shape. Null is
/// optional only before first use; it cannot erase an already installed fence.
pub(crate) fn validate_save(before: &J, after: &J) -> Result<()> {
    let Some(tasks) = before["tasks"].as_object() else {
        return initial_records(before, after);
    };
    for (name, task) in tasks.iter().filter(|(_, t)| !t[KEY].is_null()) {
        let next = &after["tasks"][name];
        if next.is_null() {
            removed(task)?;
        } else {
            if task[KEY] != next[KEY] {
                super::anchor::open(task)?;
                super::anchor::open(next)?;
            }
            state(&task[KEY], &next[KEY])?;
        }
    }
    initial_records(before, after)
}

fn initial_records(before: &J, after: &J) -> Result<()> {
    let Some(tasks) = after["tasks"].as_object() else {
        return Ok(());
    };
    for (name, task) in tasks.iter().filter(|(_, t)| !t[KEY].is_null()) {
        if before["tasks"][name][KEY].is_null() {
            let records = entries(&task[KEY])?;
            ensure(
                records.len() == 1 && records[0]["phase"] == "requested",
                "new handoff must begin at its original request",
            )?;
            super::anchor::initial(before, after, name, &records[0])?;
        }
    }
    Ok(())
}

fn removed(task: &J) -> Result<()> {
    ensure(
        matches!(task["final_outcome"].as_str(), Some("success" | "failure"))
            && task["cleanup_obligation"]["status"] == "released"
            && task["cleanup_obligation"]["owner"] == task[KEY]["owner"],
        "coordinator history may only retire with its completed terminal obligation",
    )
}

fn entries(state: &J) -> Result<&Vec<J>> {
    state["records"]
        .as_array()
        .ok_or_else(|| FlowError::guard("coordinator history cannot be erased"))
}

fn state(before: &J, after: &J) -> Result<()> {
    let old = entries(before)?;
    let new = entries(after)?;
    state_identity(before, after, old.len(), new.len())?;
    for (old, new) in old.iter().zip(new) {
        record(old, new)?;
    }
    if new.len() > old.len() {
        append(old.last(), new.last().unwrap())?;
    }
    Ok(())
}

fn state_identity(before: &J, after: &J, old_len: usize, new_len: usize) -> Result<()> {
    ensure(
        before["owner"] == after["owner"] && before["version"] == after["version"],
        "coordinator handoff authority changed",
    )?;
    ensure(
        new_len >= old_len && new_len <= old_len + 1,
        "coordinator handoff history changed",
    )
}

fn append(previous: Option<&J>, next: &J) -> Result<()> {
    let previous =
        previous.ok_or_else(|| FlowError::guard("missing original handoff generation"))?;
    ensure(
        previous["phase"] == "cancelled"
            || (previous["phase"] == "complete"
                && previous["notices"]["ready"]["phase"] == "acknowledged"),
        "finish and acknowledge the original coordinator handoff before another request",
    )?;
    ensure(
        next["phase"] == "requested",
        "successor handoff skips its original request",
    )
}

fn record(before: &J, after: &J) -> Result<()> {
    if before == after {
        return Ok(());
    }
    ensure(
        before["phase"] != "cancelled",
        "cancelled coordinator handoff is immutable",
    )?;
    frozen_identity(before, after)?;
    phase_transition(before, after)?;
    target(&before["to"], &after["to"])?;
    evidence(before, after)?;
    notices(before, after)?;
    recovery(&before["recovery"], &after["recovery"])?;
    retirement(&before["retirement"], &after["retirement"])
}

fn frozen_identity(before: &J, after: &J) -> Result<()> {
    for key in [
        "generation",
        "request_id",
        "input_digest",
        "reason",
        "from",
        "configuration",
        "fence",
    ] {
        ensure(
            before[key] == after[key],
            "frozen coordinator handoff identity changed",
        )?;
    }
    Ok(())
}
fn notices(before: &J, after: &J) -> Result<()> {
    for name in ["accept", "ready"] {
        notice(&before["notices"][name], &after["notices"][name])?;
    }
    Ok(())
}

fn once(before: &J, after: &J) -> Result<()> {
    ensure(
        before.is_null() || before == after,
        "persisted coordinator handoff evidence changed",
    )
}

fn evidence(before: &J, after: &J) -> Result<()> {
    once(&before["snapshot"], &after["snapshot"])?;
    once(&before["binding"], &after["binding"])
}

fn phase_transition(before: &J, after: &J) -> Result<()> {
    let old = phase(before)?;
    let new = phase(after)?;
    ensure(
        old == new || (old < 7 && new == old + 1) || (old == 1 && new == 8),
        "coordinator handoff phase cannot skip or move backwards",
    )
}

fn target(before: &J, after: &J) -> Result<()> {
    for key in ["runtime", "guard"] {
        ensure(
            before[key] == after[key],
            "coordinator successor reservation changed",
        )?;
    }
    once(&before["session"], &after["session"])?;
    once(&before["scope"], &after["scope"])
}

fn notice(before: &J, after: &J) -> Result<()> {
    for key in ["wait", "event", "delivery", "turn"] {
        once(&before[key], &after[key])?;
    }
    ensure(
        record::notice_phase(after)? >= record::notice_phase(before)?,
        "coordinator notice phase moved backwards",
    )
}

fn recovery(before: &J, after: &J) -> Result<()> {
    if before.is_null() {
        return ensure(after.is_null(), "handoff cannot add a recovery role");
    }
    for key in ["observer", "origin_coordinator", "from_target"] {
        once(&before[key], &after[key])?;
    }
    for key in ["generation", "runtime", "scope", "transaction_id"] {
        once(&before["to_target"][key], &after["to_target"][key])?;
    }
    ensure(
        before["phase"] != "bound" || after["phase"] == "bound",
        "recovery target moved backwards",
    )
}

fn retirement(before: &J, after: &J) -> Result<()> {
    if before.is_null() {
        return Ok(());
    }
    once(&before["path"], &after["path"])?;
    once(&before["sha256"], &after["sha256"])?;
    if before["phase"] == "complete" {
        once(before, after)?;
    }
    let phases = ["planned", "retiring", "sessions", "complete"];
    let old = phases.iter().position(|p| before["phase"] == *p);
    let new = phases.iter().position(|p| after["phase"] == *p);
    ensure(
        new.is_some() && new >= old,
        "coordinator retirement phase moved backwards",
    )
}
