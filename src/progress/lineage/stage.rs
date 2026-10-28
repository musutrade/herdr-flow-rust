//! A bounded current scope and last delivery checkpoint within one requirement.
use super::*;

pub(super) fn begin(s: &mut J, spec: &J, now: f64) -> Result<J> {
    fields(spec)?;
    let mut parent = load_parent(s, spec)?;
    ensure(
        parent["stage"].is_null() && parent["delivery"].is_null(),
        "finish the existing stage delivery before starting another stage",
    )?;
    quiescent(s, &parent)?;
    parent["stage"] = descriptor(&mut parent, spec)?;
    requirement(&parent)?;
    replace_candidate(&mut parent, J::Null);
    parent["continuation"] = J::Null;
    Ok(save_parent(s, parent, now, "stage_started"))
}

fn descriptor(parent: &mut J, spec: &J) -> Result<J> {
    let id = field(spec, "id")?;
    let scope = field(spec, "scope")?;
    let epoch = advance_scope(parent)?;
    Ok(json!({"id":id,"scope":scope,"epoch":epoch,
        "revision":parent["requirement_revision"],"criteria":spec["criteria"]}))
}

fn fields(spec: &J) -> Result<()> {
    let fields = object(spec, "stage specification")?;
    ensure(
        fields.keys().all(|k| {
            ["kind", "task", "state_version", "id", "scope", "criteria"].contains(&k.as_str())
        }),
        "stage cannot revise requirements or authorization",
    )
}

fn epoch(value: &J) -> Result<u64> {
    match value.get("scope_epoch") {
        None => Ok(0),
        Some(value) => value
            .as_u64()
            .ok_or_else(|| progress_error("invalid scope epoch")),
    }
}

pub(crate) fn current(parent: &J, link: &J) -> bool {
    epoch(parent)
        .ok()
        .zip(epoch(link).ok())
        .is_some_and(|(a, b)| a == b)
}

pub(crate) fn copy_scope(source: &J, target: &mut J) -> Result<()> {
    if source.get("scope_epoch").is_some() {
        target["scope_epoch"] = json!(epoch(source)?);
    }
    Ok(())
}

fn advance_scope(parent: &mut J) -> Result<u64> {
    let next = epoch(parent)?
        .checked_add(1)
        .ok_or_else(|| progress_error("scope epoch exhausted"))?;
    parent["scope_epoch"] = json!(next);
    Ok(next)
}

pub(super) fn supersede(parent: &mut J) -> Result<()> {
    if !parent["stage"].is_null() {
        advance_scope(parent)?;
        parent["stage"] = J::Null;
    }
    Ok(())
}

/// Inherit actual criteria and authority; a stage may only select existing IDs.
pub(crate) fn requirement(parent: &J) -> Result<J> {
    let mut scoped = req(parent).clone();
    object(&scoped, "current requirement")?;
    if parent["stage"].is_null() {
        return Ok(scoped);
    }
    let stage = &parent["stage"];
    ensure(
        stage["revision"] == parent["requirement_revision"]
            && stage["epoch"] == json!(epoch(parent)?),
        "stage scope changed",
    )?;
    scoped["criteria"] = select_criteria(&scoped, &stage["criteria"])?;
    scoped["acceptance"] = acceptance_checks(&json!({"acceptance":scoped["criteria"]}))?;
    scoped["goal"] = json!(field(stage, "scope")?);
    scoped["stage"] = stage.clone();
    Ok(scoped)
}

fn select_criteria(requirement: &J, selection: &J) -> Result<J> {
    let ids = selection
        .as_array()
        .ok_or_else(|| progress_error("stage criteria must be an array"))?;
    ensure(
        (1..=64).contains(&ids.len()),
        "stage requires 1..64 existing criteria",
    )?;
    let criteria = requirement["criteria"]
        .as_array()
        .ok_or_else(|| progress_error("requirement criteria missing"))?;
    let mut seen = BTreeSet::new();
    let selected = ids
        .iter()
        .map(|id| select_criterion(criteria, id, &mut seen))
        .collect::<Result<Vec<_>>>()?;
    Ok(json!(selected))
}

fn select_criterion(criteria: &[J], id: &J, seen: &mut BTreeSet<String>) -> Result<J> {
    let name = text(Some(id), "stage criterion", 64)?;
    ensure(seen.insert(name), "duplicate stage criterion")?;
    criteria
        .iter()
        .find(|item| item["id"] == *id)
        .cloned()
        .ok_or_else(|| progress_error("stage criterion is not in the agreed requirement"))
}

/// Only applied, terminal executions may be superseded by a stage.
pub(crate) fn quiescent(s: &J, parent: &J) -> Result<()> {
    for child in object(&s["tasks"], "tasks")?
        .values()
        .filter(|t| t["lineage"]["requirement"] == parent["task"])
    {
        released(child)?;
        applied_execution(s, child)?;
    }
    Ok(())
}

fn released(child: &J) -> Result<()> {
    ensure_writer_released(child)?;
    ensure(
        child["pending"].as_array().is_some_and(|p| p.is_empty()),
        "apply pending activity results before changing stage",
    )?;
    let operations = object(&child["operations"], "activity operations")?;
    ensure(
        operations.values().all(|op| op["phase"] == "accepted"),
        "reconcile original activity operations before changing stage",
    )
}

fn applied_execution(s: &J, child: &J) -> Result<()> {
    ensure(
        matches!(child["phase"].as_str(), Some("completed" | "blocked")),
        "stage transition requires settled activities",
    )?;
    let binding = binding(child)?;
    let events = object(&s["events"], "events")?;
    ensure(
        events.values().any(|e| {
            e["applied"] == true
                && matches!(e["outcome"].as_str(), Some("complete" | "failed"))
                && e["binding"] == binding
        }),
        "stage transition requires an applied result of the original execution",
    )
}

pub(crate) fn continue_parent(s: &mut J, name: &str, now: f64) -> Result<J> {
    let mut parent = s["tasks"][name].clone();
    advance_scope(&mut parent)?;
    for field in [
        "stage",
        "delivery",
        "continuation",
        "publication_preparation",
    ] {
        parent[field] = J::Null;
    }
    replace_candidate(&mut parent, J::Null);
    parent["control"] = json!("running");
    parent["stage_delivery"]["phase"] = json!("complete");
    Ok(save_parent(s, parent, now, "stage_delivered"))
}
