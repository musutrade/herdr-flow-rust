//! Versioned requirements and linked development/acceptance activities.
//! All mutations are staged on a copy and committed by the existing queue lock.
use serde_json::{json, Value};

use super::{
    artifact::{ArtifactAdapter, LocalArtifacts},
    *,
};

pub fn action(s: &mut J, spec: &J, now: f64) -> Result<J> {
    action_with_adapter(s, spec, now, &LocalArtifacts)
}

pub fn action_with_adapter(
    s: &mut J,
    spec: &J,
    now: f64,
    adapter: &dyn ArtifactAdapter,
) -> Result<J> {
    ensure_not_stopped(s)?;
    let mut staged = s.clone();
    let result = route(&mut staged, spec, now, adapter)?;
    *s = staged;
    Ok(result)
}

fn route(s: &mut J, spec: &J, now: f64, adapter: &dyn ArtifactAdapter) -> Result<J> {
    match model::string(spec, "kind").as_deref() {
        Some("plan") => plan(s, spec, now),
        Some("migrate") => migrate(s, spec, now),
        Some("revise") => revise(s, spec, now),
        Some("artifact") => artifact(s, spec, now, adapter),
        Some("candidate") => candidate(s, spec, now, adapter),
        Some("activity") => activity(s, spec, now, adapter),
        Some("checks") => checks(s, spec, now),
        Some("continue") => acceptance::resume(s, spec, now, adapter),
        _ => Err(progress_error("unknown lineage action")),
    }
}

fn field(spec: &J, key: &str) -> Result<String> {
    text(model::get(spec, key), key, 1000)
}

fn references(basis: &J, key: &str, required: bool) -> Result<()> {
    let values = model::array_field(basis, key)
        .ok_or_else(|| progress_error(format!("basis {key} must be an array")))?;
    ensure(values.len() <= 64, "too many basis references")?;
    ensure(
        !required || !values.is_empty(),
        "original requirement reference is required",
    )?;
    values
        .iter()
        .try_for_each(|value| text(Some(value), key, 1000).map(|_| ()))
}

fn requirement(spec: &J, revision: i64) -> Result<J> {
    let basis = object_owned(model::get(spec, "basis"), "requirement basis")?;
    references(&basis, "original", true)?;
    references(&basis, "changes", false)?;
    references(&basis, "specifications", false)?;
    field(&basis, "endpoint")?;
    field(&basis, "confirmation")?;
    let goal = field(spec, "goal")?;
    let criteria = completion_criteria(spec, &goal);
    Ok(json!({"revision": revision, "previous": revision - 1,
        "goal": goal, "basis": basis, "criteria": criteria,
        "acceptance": acceptance_checks(&json!({"acceptance": criteria}))?,
        "authorization": authorization(spec)?}))
}

fn completion_criteria(spec: &J, goal: &str) -> J {
    model::get(spec, "acceptance")
        .cloned()
        .unwrap_or_else(|| json!([{"id":"goal", "description":goal, "required":true}]))
}

fn plan(s: &mut J, spec: &J, now: f64) -> Result<J> {
    let name = prepare_registration_task(s, spec)?;
    let req = requirement(spec, 1)?;
    let t = json!({"task":name, "schema_version":2, "state_version":1,
        "epoch":1, "phase":"coordinating", "control":"running",
        "requirement_revision":1, "requirements":{"1":req},
        "artifacts":{}, "activities":{}, "acceptance_round":0,
        "candidate":null, "delivery_target":null, "approved":null,
        "writer":null, "pending":[], "operations":{}, "history":[],
        "created_at":now});
    s["tasks"][&name] = t.clone();
    Ok(t)
}

fn load_parent(s: &J, spec: &J) -> Result<J> {
    let t = task(
        s,
        model::get(spec, "task"),
        model::get(spec, "state_version"),
    )?;
    ensure(is_parent(&t), "requires a version 2 requirement task")?;
    ensure(task_running(&t), "requirement task is paused or cancelled")?;
    object(&t["requirements"], "requirements")?;
    object(req(&t), "current requirement")?;
    Ok(t)
}

pub fn is_parent(t: &J) -> bool {
    model::int(t, "schema_version") == Some(2) && model::get(t, "requirements").is_some()
}

fn req(t: &J) -> &J {
    let revision = model::int(t, "requirement_revision")
        .unwrap_or(0)
        .to_string();
    &t["requirements"][revision]
}

fn save_parent(s: &mut J, mut t: J, now: f64, reason: &str) -> J {
    t["state_version"] = json!(model::int(&t, "state_version").unwrap_or(0) + 1);
    audit(&mut t, now, reason);
    let name = model::string(&t, "task").unwrap_or_default();
    s["tasks"][name] = t.clone();
    t
}

fn migrate(s: &mut J, spec: &J, now: f64) -> Result<J> {
    let old = task(
        s,
        model::get(spec, "task"),
        model::get(spec, "state_version"),
    )?;
    migration_allowed(&old)?;
    let name = field(spec, "task")?;
    s["tasks"].as_object_mut().unwrap().remove(&name);
    let mut t = plan(s, spec, now)?;
    t["legacy"] = old.clone();
    t["epoch"] = old["epoch"].clone();
    t["state_version"] = old["state_version"].clone();
    Ok(save_parent(s, t, now, "migrated"))
}

fn migration_allowed(old: &J) -> Result<()> {
    claim::forbid_migration(old)?;
    ensure(legacy_task(old), "only legacy tasks can migrate")?;
    require_unowned(old, false)?;
    ensure(!has_lane(old), "batch lanes cannot migrate")?;
    ensure(model::get(old, "pr").is_none(), "PR tasks cannot migrate")?;
    migration_quiescent(old)
}

fn legacy_task(t: &J) -> bool {
    model::int(t, "schema_version").unwrap_or(1) == 1 && model::get(t, "lineage").is_none()
}

fn migration_quiescent(t: &J) -> Result<()> {
    ensure(
        model::string(t, "phase").as_deref() == Some("blocked")
            || model::string(t, "control").as_deref() == Some("cancelled"),
        "migration requires blocked/cancelled task",
    )?;
    ensure_writer_released(t)?;
    ensure(
        model::array_field(t, "pending").is_some_and(|values| values.is_empty()),
        "resolve pending results first",
    )?;
    let ops = object(&t["operations"], "operations")?;
    ensure(
        ops.values().all(|op| op["phase"] == "accepted"),
        "reconcile uncertain operations before migration",
    )
}

fn revise(s: &mut J, spec: &J, now: f64) -> Result<J> {
    let mut t = load_parent(s, spec)?;
    let revision = model::int(&t, "requirement_revision").unwrap_or(0) + 1;
    ensure(revision <= 128, "requirement revision capacity reached")?;
    let inherited = inherited_revision(&t, spec)?;
    let next = requirement(&inherited, revision)?;
    t["requirements"][revision.to_string()] = next;
    t["requirement_revision"] = json!(revision);
    replace_candidate(&mut t, Value::Null);
    Ok(save_parent(s, t, now, "requirements_revised"))
}

fn inherited_revision(t: &J, spec: &J) -> Result<J> {
    object(spec, "revision specification")?;
    let mut next = spec.clone();
    if next.get("authorization").is_none() {
        next["authorization"] = json!(inherited_permissions(t)?);
    }
    if next.get("acceptance").is_none() {
        next["acceptance"] = req(t)["criteria"].clone();
    }
    Ok(next)
}

fn artifact(s: &mut J, spec: &J, now: f64, adapter: &dyn ArtifactAdapter) -> Result<J> {
    let mut t = load_parent(s, spec)?;
    let value = object_owned(model::get(spec, "artifact"), "artifact")?;
    insert_artifact(&mut t, &value, adapter)?;
    Ok(save_parent(s, t, now, "artifact_registered"))
}

fn insert_artifact(t: &mut J, value: &J, adapter: &dyn ArtifactAdapter) -> Result<()> {
    let id = text(model::get(value, "id"), "artifact id", 256)?;
    field(value, "adapter")?;
    let objects = object(&t["artifacts"], "artifacts")?;
    ensure(!objects.contains_key(&id), "artifact id already registered")?;
    ensure(objects.len() < 256, "artifact capacity reached")?;
    validate_input(t, model::get(value, "input"))?;
    adapter.verify(value)?;
    t["artifacts"][id] = json!({"object":value, "revision":t["requirement_revision"]});
    Ok(())
}

fn validate_input(t: &J, input: Option<&J>) -> Result<()> {
    if let Some(input) = input.filter(|v| !v.is_null()) {
        artifact_record(t, input)?;
    }
    Ok(())
}

fn artifact_record<'a>(t: &'a J, id: &J) -> Result<&'a J> {
    let id = text(Some(id), "artifact id", 256)?;
    model::dig(t, &["artifacts", &id, "object"])
        .ok_or_else(|| progress_error("artifact is not registered"))
}

fn candidate(s: &mut J, spec: &J, now: f64, adapter: &dyn ArtifactAdapter) -> Result<J> {
    let mut t = load_parent(s, spec)?;
    let id = json!(field(spec, "candidate")?);
    adapter.verify(artifact_record(&t, &id)?)?;
    field(spec, "reason")?;
    replace_candidate(&mut t, id);
    Ok(save_parent(s, t, now, "candidate_replaced"))
}

fn replace_candidate(t: &mut J, id: J) {
    t["candidate"] = id.clone();
    t["delivery_target"] = id;
    t["approved"] = Value::Null;
    t["acceptance_round"] = json!(model::int(t, "acceptance_round").unwrap_or(0) + 1);
}

fn activity(s: &mut J, spec: &J, now: f64, adapter: &dyn ArtifactAdapter) -> Result<J> {
    let parent = load_parent(s, spec)?;
    let name = field(spec, "activity")?;
    let role = activity_role(spec)?;
    let input = json!(field(spec, "input")?);
    let object = artifact_record(&parent, &input)?;
    adapter.verify(object)?;
    validate_acceptance_input(&parent, &role, &input)?;
    let link = activity_link(&parent, spec, &role, &input)?;
    let wait = activity_wait(spec, &name, object, &link)?;
    register_activity(s, spec, parent, wait, link, now)
}

fn activity_role(spec: &J) -> Result<String> {
    let role = field(spec, "role")?;
    ensure(
        ["development", "acceptance"].contains(&role.as_str()),
        "invalid activity role",
    )?;
    Ok(role)
}

fn register_activity(s: &mut J, spec: &J, mut parent: J, wait: J, link: J, now: f64) -> Result<J> {
    let name = field(spec, "activity")?;
    let child_spec = activity_spec(spec, &parent, &name)?;
    let wait = crate::queue::register_wait(s, &wait, now)?;
    let mut child_spec = child_spec;
    child_spec["wait"] = wait["id"].clone();
    parent["activities"][&name] = link;
    let parent_name = field(&parent, "task")?;
    s["tasks"][parent_name] = parent.clone();
    let child = register(s, &child_spec, now)?;
    save_parent(s, parent, now, "activity_registered");
    Ok(child)
}

fn validate_acceptance_input(t: &J, role: &str, input: &J) -> Result<()> {
    if role == "acceptance" {
        ensure(
            t["candidate"] == *input,
            "acceptance must pin the selected candidate",
        )?;
    }
    Ok(())
}

fn activity_link(t: &J, spec: &J, role: &str, input: &J) -> Result<J> {
    let epoch = model::int(spec, "session_epoch").unwrap_or(0);
    ensure(epoch > 0, "session_epoch must be a positive integer")?;
    let round = if role == "acceptance" {
        t["acceptance_round"].clone()
    } else {
        json!(0)
    };
    Ok(
        json!({"requirement":t["task"], "revision":t["requirement_revision"],
        "role":role, "input":input, "round":round, "session_epoch":epoch}),
    )
}

fn activity_spec(spec: &J, t: &J, name: &str) -> Result<J> {
    object(spec, "activity specification")?;
    let mut child = spec.clone();
    child["task"] = json!(name);
    child["goal"] = req(t)["goal"].clone();
    child["acceptance"] = req(t)["criteria"].clone();
    child["authorization"] = json!(inherited_permissions(t)?);
    Ok(child)
}

fn inherited_permissions(t: &J) -> Result<Vec<J>> {
    let permissions = object(&req(t)["authorization"], "requirement.authorization")?;
    permissions
        .iter()
        .map(|(id, value)| {
            object(value, "requirement.authorization entry")?;
            let mut permission = value.clone();
            permission["id"] = json!(id);
            Ok(permission)
        })
        .collect()
}

fn activity_wait(spec: &J, name: &str, artifact: &J, link: &J) -> Result<J> {
    let mut wait = object_owned(model::get(spec, "result_wait"), "result wait")?;
    wait["task"] = json!(name);
    wait["candidate"] = artifact["id"].clone();
    wait["artifact_adapter"] = artifact["adapter"].clone();
    wait["lineage"] = link.clone();
    Ok(wait)
}

/// Legacy tasks have no lineage. Linked activities must match the live parent.
pub fn current(s: &J, t: &J) -> bool {
    if !supported_schema(t) {
        return false;
    }
    let Some(link) = model::get(t, "lineage") else {
        return true;
    };
    let parent = &s["tasks"][link["requirement"].as_str().unwrap_or("")];
    parent_revision_current(parent, link) && candidate_current(parent, t, link)
}

fn parent_revision_current(parent: &J, link: &J) -> bool {
    is_parent(parent) && parent["requirement_revision"] == link["revision"]
}

fn candidate_current(parent: &J, t: &J, link: &J) -> bool {
    if link["role"] != "acceptance" {
        return true;
    }
    parent["candidate"] == t["candidate"] && parent["acceptance_round"] == link["round"]
}

pub fn ensure_current(s: &J, t: &J) -> Result<()> {
    ensure(
        actionable(s, t),
        "obsolete requirement, candidate or acceptance round, or inactive parent",
    )
}

pub fn copy_binding(source: &J, target: &mut J) {
    for key in ["lineage", "artifact_adapter"] {
        if let Some(value) = model::get(source, key) {
            target[key] = value.clone();
        }
    }
}

pub fn validate_binding(w: &J) -> Result<()> {
    let link = object_owned(model::get(w, "lineage"), "lineage binding")?;
    field(&link, "requirement")?;
    field(&link, "role")?;
    field(w, "artifact_adapter")?;
    text(model::get(w, "candidate"), "artifact identity", 256)?;
    ensure(
        model::int(&link, "revision").unwrap_or(0) > 0,
        "invalid requirement revision",
    )?;
    ensure(
        model::int(&link, "session_epoch").unwrap_or(0) > 0,
        "invalid session epoch",
    )
}

pub fn validate_registration(s: &J, t: &J) -> Result<()> {
    let Some(link) = model::get(t, "lineage") else {
        return Ok(());
    };
    let parent = &s["tasks"][link["requirement"].as_str().unwrap_or("")];
    let name = field(t, "task")?;
    ensure(
        parent["activities"][name] == *link,
        "register linked activities with task-lineage",
    )?;
    let artifact = artifact_record(parent, &t["candidate"])?;
    ensure(
        artifact["adapter"] == t["artifact_adapter"],
        "artifact adapter mismatch",
    )?;
    ensure(link["input"] == t["candidate"], "activity input mismatch")?;
    ensure_current(s, t)
}

fn checks(s: &mut J, spec: &J, now: f64) -> Result<J> {
    let mut t = task(
        s,
        model::get(spec, "task"),
        model::get(spec, "state_version"),
    )?;
    ensure_current(s, &t)?;
    ensure(
        t["lineage"]["role"] == "acceptance",
        "checks require an acceptance activity",
    )?;
    ensure(task_applyable(&t), "activity is not running")?;
    let items = model::array_field(spec, "checks")
        .ok_or_else(|| progress_error("checks must be an array"))?;
    ensure(items.len() <= 64, "too many derived checks")?;
    let mut derived = json!({});
    for item in items {
        insert_check(&mut derived, &t, item)?;
    }
    t["derived_checks"] = derived;
    Ok(save_parent(s, t, now, "checks_derived"))
}

fn insert_check(checks: &mut J, t: &J, item: &J) -> Result<()> {
    let id = field(item, "id")?;
    let basis = field(item, "criterion")?;
    ensure(
        model::dig(t, &["acceptance", &basis]).is_some(),
        "check must reference a completion criterion",
    )?;
    field(item, "description")?;
    field(item, "source")?;
    ensure(model::get(checks, &id).is_none(), "duplicate derived check")?;
    checks[&id] = item.clone();
    Ok(())
}

/// Append a development output in the same transaction as applying its result.
pub fn applied(
    s: &mut J,
    spec: &J,
    child: &J,
    now: f64,
    adapter: &dyn ArtifactAdapter,
) -> Result<()> {
    let Some(link) = model::get(child, "lineage") else {
        return ensure(
            model::get(spec, "output").is_none(),
            "output requires a linked activity",
        );
    };
    let name = field(link, "requirement")?;
    let mut parent = s["tasks"][&name].clone();
    apply_output(&mut parent, child, spec, adapter)?;
    if child["lineage"]["role"] == "acceptance" {
        adapter.verify(artifact_record(&parent, &child["candidate"])?)?;
    }
    acceptance::record(&mut parent, child, spec)?;
    save_parent(s, parent, now, "activity_result_applied");
    Ok(())
}

fn apply_output(parent: &mut J, child: &J, spec: &J, adapter: &dyn ArtifactAdapter) -> Result<()> {
    let Some(output) = model::get(spec, "output") else {
        return Ok(());
    };
    ensure(
        child["lineage"]["role"] == "development",
        "only development produces new artifacts",
    )?;
    ensure(
        output["input"] == child["lineage"]["input"],
        "output must link the development input",
    )?;
    insert_artifact(parent, output, adapter)?;
    let id = field(output, "id")?;
    parent["artifacts"][id]["activity"] = child["task"].clone();
    let name = field(child, "task")?;
    parent["activities"][name]["output"] = output["id"].clone();
    Ok(())
}

/// A successor producer session has a new epoch even within one activity.
pub fn advance_session(t: &mut J) {
    if let Some(link) = model::get_mut(t, "lineage") {
        link["session_epoch"] = json!(model::int(link, "session_epoch").unwrap_or(0) + 1);
    }
}

pub fn supported_schema(t: &J) -> bool {
    match model::get(t, "schema_version") {
        None => true,
        Some(value) => matches!(value.as_i64(), Some(1 | 2 | claim::SCHEMA)),
    }
}

pub fn diagnosis(s: &J, t: &J) -> Option<(String, String, String)> {
    if !current(s, t) {
        return Some(("obsolete_activity".into(), "requirement, candidate, round or schema is no longer current".into(),
            "inspect the old operation and writer, cancel/release with evidence; do not replay or apply the old result".into()));
    }
    if !parent_running(s, t) {
        return Some(("parent_inactive".into(), "parent requirement is paused or cancelled".into(),
            "inspect parent control; keep pending results and do not dispatch or apply until authorized recovery".into()));
    }
    if is_parent(t) && task_running(t) {
        return Some((
            "coordinating".into(),
            "requirement and linked activities have independent versions".into(),
            "inspect activities and the fixed candidate; new development outputs do not replace it"
                .into(),
        ));
    }
    None
}

/// A pause suspends continuation without discarding a valid arriving result.
pub fn actionable(s: &J, t: &J) -> bool {
    current(s, t) && parent_running(s, t)
}

fn parent_running(s: &J, t: &J) -> bool {
    let Some(link) = model::get(t, "lineage") else {
        return true;
    };
    let parent = &s["tasks"][link["requirement"].as_str().unwrap_or("")];
    task_running(parent)
}
