//! Keep the current verdict intact for idempotent recovery delivery. Historical
//! attempts retain correlation/dispositions and bounded excerpts with digests.
use super::*;

pub(super) fn digest(value: &J) -> Result<String> {
    Ok(util::pyjson::sha256_hex(&serde_json::to_vec(value)?))
}

fn excerpt(value: &J) -> J {
    let Some(text) = value.as_str() else {
        return J::Null;
    };
    let end = text.floor_char_boundary(text.len().min(256));
    json!(&text[..end])
}

fn conclusion(value: &J) -> Result<J> {
    Ok(json!({"sha256":digest(value)?,"incident":value["incident"],
        "disposition":value["disposition"],"requested_action":value["requested_action"],
        "facts":excerpt(&value["facts"]),"actions":excerpt(&value["actions"]),
        "current_state":excerpt(&value["current_state"]),"next_step":excerpt(&value["next_step"])}))
}

fn summary(entry: &J) -> Result<J> {
    if entry["summary_version"] == 1 {
        return Ok(entry.clone());
    }
    let returned = &entry["return"];
    let verification = &entry["verification"];
    Ok(json!({"summary_version":1,"sha256":digest(entry)?,
        "return":{"id":returned["id"],"fingerprint":returned["fingerprint"],
            "note":excerpt(&returned["note"]),"at":returned["at"]},
        "verification":{"incident":verification["incident"],"turn":verification["turn"],
            "agent":verification["agent"],"at":verification["at"],
            "model_result":conclusion(&verification["model_result"])?,
            "result":conclusion(&verification["result"])?}}))
}

pub(super) fn append_verification(next: &mut J) -> Result<()> {
    let entry = summary(&json!({"return":next["return"],"verification":next["verification"]}))?;
    if next["verifications"].is_null() {
        next["verifications"] = json!([]);
    }
    history(next)?.push(entry);
    fit(next)
}

fn history(next: &mut J) -> Result<&mut Vec<J>> {
    next["verifications"]
        .as_array_mut()
        .ok_or_else(|| FlowError::guard("invalid verification history"))
}

pub(super) fn fit(next: &mut J) -> Result<()> {
    if next["verifications"].is_null() {
        return Ok(());
    }
    for entry in history(next)?.iter_mut() {
        *entry = summary(entry)?;
    }
    let sizes = history(next)?
        .iter()
        .map(|entry| Ok(serde_json::to_vec(entry)?.len() + 1))
        .collect::<Result<Vec<usize>>>()?;
    let bytes: usize = sizes.iter().sum();
    let base = serde_json::to_vec(next)?.len().saturating_sub(bytes);
    // Leave room for the pruning receipt and status warning, measured in bytes.
    let budget = (LIMIT / 2).min(LIMIT.saturating_sub(base + 4096));
    prune(next, &sizes, bytes.saturating_sub(budget))
}

fn prune(next: &mut J, sizes: &[usize], excess: usize) -> Result<()> {
    let mut removed = 0;
    let mut count = 0;
    while removed < excess {
        removed += sizes[count];
        count += 1;
    }
    let entries: Vec<J> = history(next)?.drain(..count).collect();
    for entry in entries {
        pruning_receipt(next, &entry)?;
    }
    Ok(())
}

fn pruning_receipt(next: &mut J, entry: &J) -> Result<()> {
    let previous = &next["verification_history_pruned"];
    let count = previous["count"]
        .as_u64()
        .unwrap_or(0)
        .checked_add(1)
        .ok_or_else(|| FlowError::guard("verification pruning count overflow"))?;
    next["verification_history_pruned"] = json!({"count":count,
        "sha256":digest(&json!({"previous":previous["sha256"],"entry":entry}))?,
        "last_incident":entry["verification"]["incident"],"last_turn":entry["verification"]["turn"]});
    Ok(())
}

pub(super) fn status(record: &J) -> Result<J> {
    let bytes = serde_json::to_vec(record)?.len();
    if bytes <= LIMIT {
        return Ok(record.clone());
    }
    let mut view = record.clone();
    if fit(&mut view).is_err() || serde_json::to_vec(&view)?.len() > LIMIT {
        view = json!({"id":record["id"],"owner":record["owner"],"phase":record["phase"],
            "actor":record["actor"],"writer":record["writer"],"checkout":record["checkout"]});
    }
    view["warning"] = json!({"kind":"oversized_handoff","bytes":bytes,"sha256":digest(record)?,
        "message":"Stored handoff exceeds the byte bound; admission remains blocked. This is a compact diagnostic view. A transition compacts verification history; human-abandon permits failure finalization."});
    Ok(view)
}
