//! Immutable independent verdicts, separate from execution outcomes and receipts.
use super::*;
mod flow;
pub(crate) mod native;
mod validation;

pub fn submit(s: &mut J, wait_id: &str, reviewer: &J, report: &J) -> Result<J> {
    let wait = &s["waits"][wait_id];
    let name = text(model::get(wait, "task"), "activity", 1000)?;
    let child = &s["tasks"][&name];
    let reviewer = validate_submitter(s, wait, child, reviewer)?;
    let envelope = json!({"binding":binding(wait)?, "reviewer":reviewer, "report":report});
    validation::report(s, child, report)?;
    let id = crate::util::pyjson::sha256_hex(serde_json::to_string(&envelope)?.as_bytes());
    let value = json!({"id":id,"envelope":envelope});
    if !child["verdict"].is_null() {
        ensure(
            child["verdict"] == value,
            "independent verdict is immutable",
        )?;
        return Ok(value);
    }
    s["tasks"][name]["verdict"] = value.clone();
    Ok(value)
}

fn validate_submitter(s: &J, wait: &J, child: &J, reviewer: &J) -> Result<J> {
    ensure(lineage::current(s, child), "obsolete verdict binding")?;
    ensure(
        child["lineage"]["role"] == "acceptance",
        "verdict requires acceptance role",
    )?;
    ensure(
        task_not_completed(child) && child["control"] != "cancelled",
        "acceptance activity is inactive",
    )?;
    let reviewer = crate::identity::validate(reviewer)?;
    ensure(
        crate::identity::required_producer(wait)? == reviewer,
        "verdict reviewer mismatch",
    )?;
    validation::independent(s, child, &reviewer)?;
    Ok(reviewer)
}

/// Convert the role's chosen continuation to an ordinary atomic result action.
/// The coordinator names the immutable report; it cannot supply new conclusions.
pub(super) fn prepare(s: &J, spec: &J) -> Result<J> {
    let child = &s["tasks"][spec["task"].as_str().unwrap_or_default()];
    if child["lineage"]["role"] != "acceptance" {
        return Ok(spec.clone());
    }
    ensure(
        ["acceptance", "findings", "resolve_findings"]
            .iter()
            .all(|k| spec.get(*k).is_none()),
        "coordinator cannot rewrite independent acceptance evidence",
    )?;
    let event = &s["events"][spec["event"].as_str().unwrap_or_default()];
    let verdict = selected(s, child, spec, event)?;
    flow::validate_next(&verdict, &spec["next_action"])?;
    let mut prepared = spec.clone();
    prepared["acceptance_continuation"] = spec["next_action"].clone();
    if verdict == "passed" {
        prepared["acceptance"] =
            validation::completion_evidence(&child["verdict"]["envelope"]["report"])?;
        prepared["next_action"] = json!({"kind":"complete"});
    } else {
        prepared["next_action"]["kind"] = json!("blocked");
    }
    Ok(prepared)
}

fn selected(s: &J, child: &J, spec: &J, event: &J) -> Result<String> {
    if spec["verdict"].is_null() {
        return execution_conclusion(child, spec, event);
    }
    validate_selection(s, child, spec, event)?;
    text(
        model::get(&child["verdict"]["envelope"]["report"], "conclusion"),
        "conclusion",
        64,
    )
}
fn execution_conclusion(child: &J, spec: &J, event: &J) -> Result<String> {
    text(
        model::get(spec, "execution_problem"),
        "execution problem",
        1000,
    )?;
    ensure(
        child["verdict"].is_null() || event["outcome"] != "complete",
        "apply the registered independent verdict",
    )?;
    Ok("execution_unavailable".into())
}
fn validate_selection(s: &J, child: &J, spec: &J, event: &J) -> Result<()> {
    ensure(
        event["outcome"] == "complete",
        "execution failure is not a formal verdict",
    )?;
    let value = &child["verdict"];
    let wait = &s["waits"][child["wait"].as_str().unwrap_or_default()];
    ensure(
        crate::identity::required_producer(wait)? == value["envelope"]["reviewer"],
        "verdict reviewer no longer owns activity",
    )?;
    ensure(
        !value.is_null() && spec["verdict"] == value["id"],
        "unknown independent verdict",
    )?;
    ensure(
        value["envelope"]["binding"] == binding(child)?,
        "verdict binding is obsolete",
    )?;
    Ok(())
}

pub(super) fn record(parent: &mut J, child: &J, spec: &J) -> Result<()> {
    if child["lineage"]["role"] != "acceptance" {
        return Ok(());
    }
    let next = &spec["acceptance_continuation"];
    ensure(
        next.is_object(),
        "acceptance requires a structured continuation",
    )?;
    if let Some(verdicts) = parent.get("verdicts") {
        object(verdicts, "verdicts")?;
    }
    parent["approved"] = Value::Null;
    let id = child["task"].as_str().unwrap_or_default();
    if spec["verdict"].is_null() {
        parent["verdicts"][id] = Value::Null;
    } else {
        parent["verdicts"][id] = child["verdict"].clone();
        validation::record_gaps(parent, child)?;
    }
    parent["continuation"] = json!({"id":id,"phase":"planned","binding":binding(child)?,
        "verdict":spec["verdict"],"action":next,"execution_problem":spec["execution_problem"]});
    if next["kind"] == "delivery" {
        parent["approved"] = json!({"candidate":child["candidate"],"revision":child["lineage"]["revision"],
            "round":child["lineage"]["round"],"activity":child["task"],"verdict":spec["verdict"]});
    }
    Ok(())
}

/// Consume a saved intent by registering a linked activity, never by sending it.
/// Existing dispatch journals retain responsibility for exactly-once submission.
pub(super) fn resume(
    s: &mut J,
    spec: &J,
    now: f64,
    adapter: &dyn artifact::ArtifactAdapter,
) -> Result<J> {
    let name = text(model::get(spec, "task"), "task", 1000)?;
    let parent = s["tasks"][&name].clone();
    let intent = &parent["continuation"];
    ensure(
        intent["id"] == spec["intent"] && !intent.is_null(),
        "continuation changed",
    )?;
    if intent["phase"] == "registered" {
        ensure(
            intent["request"] == spec["activity"],
            "continuation already consumed differently",
        )?;
        return Ok(s["tasks"][intent["activity"].as_str().unwrap_or_default()].clone());
    }
    flow::validate_resume(&parent, spec)?;
    let mut request = spec["activity"].clone();
    request["task"] = json!(name);
    request["state_version"] = parent["state_version"].clone();
    request["kind"] = json!("activity");
    let child = lineage::action_with_adapter(s, &request, now, adapter)?;
    s["tasks"][&name]["continuation"]["phase"] = json!("registered");
    s["tasks"][&name]["continuation"]["activity"] = child["task"].clone();
    s["tasks"][&name]["continuation"]["request"] = spec["activity"].clone();
    let child_name = child["task"].as_str().unwrap();
    s["tasks"][child_name]["acceptance_origin"] = intent.clone();
    Ok(s["tasks"][child_name].clone())
}
