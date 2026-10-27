//! Durable, explicit recovery conclusions; result delivery is not task replay.
use super::*;
use crate::herdr::ReceiverAdapter;

pub(super) fn schema(id: &str) -> J {
    json!({"type":"object","additionalProperties":false,
        "required":["incident","disposition","facts","actions","current_state","next_step"],
        "properties":{
            "incident":{"type":"string","enum":[id]},
            "disposition":{"type":"string","enum":["recovered","needs_human","insufficient_evidence"]},
            "facts":{"type":"string"},"actions":{"type":"string"},
            "current_state":{"type":"string"},"next_step":{"type":"string"}}})
}

pub(super) fn parse(turn: &J, id: &str) -> Result<J> {
    let items = turn["items"]
        .as_array()
        .ok_or_else(|| FlowError::guard("recovery output missing"))?;
    let text = items
        .iter()
        .rev()
        .find(|item| item["type"] == "agentMessage")
        .and_then(|item| item["text"].as_str())
        .ok_or_else(|| FlowError::guard("recovery conclusion missing"))?;
    ensure(
        text.len() <= 65536,
        "recovery conclusion exceeds message bound",
    )?;
    let result: J = serde_json::from_str(text)?;
    validate(&result, id)?;
    Ok(result)
}

fn validate(result: &J, id: &str) -> Result<()> {
    let fields = result
        .as_object()
        .ok_or_else(|| FlowError::guard("recovery result must be an object"))?;
    ensure(
        fields.len() == 6 && result["incident"] == id,
        "recovery conclusion belongs to another incident",
    )?;
    ensure(
        matches!(
            result["disposition"].as_str(),
            Some("recovered" | "needs_human" | "insufficient_evidence")
        ),
        "recovery disposition missing",
    )?;
    for field in ["facts", "actions", "current_state", "next_step"] {
        required(result, field)?;
    }
    Ok(())
}

pub(super) fn notice(context: &J, text: &str) {
    let coordinator = Path::new(context["coordinator"].as_str().unwrap_or_default());
    // Failure remains in the durable status. No recursive Agent is launched to
    // repair a failed display, and no unrelated Herdr pane is touched.
    let _ = tui::recovery_notice(coordinator, &context["runtime"], text);
}

pub(super) fn handoff(root: &Path, context: &J, incident: &mut J, q: &J) -> Result<()> {
    if incident["handoff"]["phase"] == "sending" {
        incident["phase"] = json!("unavailable");
        incident["next_step"] = json!("Recovery result delivery is uncertain; inspect the original coordinator session before retrying.");
        return save(root, "incident.json", incident);
    }
    let state = receiver_state(context, q)?;
    let adapter = match native::receiver::active::Adapter::new(&state) {
        Ok(adapter) => adapter,
        Err(_) => return Ok(()), // The result remains visible while control is unavailable.
    };
    if adapter.agent()?["interactive_ready"] != true {
        return Ok(());
    }
    deliver(root, incident, &state, &adapter)
}

fn receiver_state(context: &J, q: &J) -> Result<J> {
    let coordinator = PathBuf::from(required(context, "coordinator")?);
    let receiver = util::read_state_json(&coordinator, "receiver.json")?;
    let guard = PathBuf::from(required(&receiver, "guard_dir")?);
    let state = util::read_state_json(&guard, "state.json")?;
    ensure(
        q["receiver"] == crate::guard::scope(&state)
            && state["instance"] == context["owner"]["instance_id"],
        "recovery handoff receiver changed",
    )?;
    Ok(state)
}

fn deliver(root: &Path, incident: &mut J, state: &J, adapter: &dyn ReceiverAdapter) -> Result<()> {
    incident["handoff"] = json!({"phase":"sending","session":state["session"]});
    save(root, "incident.json", incident)?;
    let message = format!("Recovery result for your existing task (not a new assignment): {}. Original observation: {}. Verify the current original operation and existing authorization before continuing. An unknown send/Git result must be inspected, never blindly replayed. needs_human/insufficient_evidence mean report the waiting condition; do not mark the requirement complete or destroy its resources.", incident["result"], incident["fault"]);
    let turn = adapter.prompt(&message)?;
    incident["handoff"]["phase"] = json!("submitted");
    incident["handoff"]["turn"] = json!(turn);
    incident["phase"] = json!("handled");
    save(root, "incident.json", incident)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn only_bound_complete_conclusions_are_accepted() {
        for disposition in ["recovered", "needs_human", "insufficient_evidence"] {
            let result = json!({"incident":"a","disposition":disposition,"facts":"observed",
                "actions":"read only","current_state":"known","next_step":"verify original operation"});
            let turn = json!({"items":[{"type":"agentMessage","text":result.to_string()}]});
            assert_eq!(parse(&turn, "a").unwrap(), result);
            assert!(parse(&turn, "b").is_err());
            let mut missing = result.clone();
            missing["next_step"] = J::Null;
            assert!(validate(&missing, "a").is_err());
        }
        assert!(parse(&json!({"items":[]}), "a").is_err());
        assert!(parse(
            &json!({"items":[{"type":"agentMessage","text":"not json"}]}),
            "a"
        )
        .is_err());
    }
}
