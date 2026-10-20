//! Task progress stored inside `queue.json`, under the queue's existing lock.
//!
//! These functions do not send work, infer authorization, or retry external
//! actions. Callers persist the returned mutation before doing anything
//! outside the queue.

pub mod acceptance;
pub mod artifact;
pub mod lineage;

use std::collections::BTreeSet;
use std::path::Path;

use serde_json::{json, Map, Value};

use crate::error::{FlowError, Result};
use crate::model::{self, J};
use crate::util::pyjson::sha256_hex;
use crate::util::{is_relative_to, resolve_path};

pub fn progress_error(message: impl Into<String>) -> FlowError {
    FlowError::progress(message)
}

pub fn text(value: Option<&J>, name: &str, limit: usize) -> Result<String> {
    model::text(value, name, limit)
}

pub fn object<'a>(value: &'a J, name: &str) -> Result<&'a Map<String, Value>> {
    value
        .as_object()
        .ok_or_else(|| progress_error(format!("{name} must be an object")))
}

pub fn object_owned(value: Option<&J>, name: &str) -> Result<J> {
    value
        .cloned()
        .filter(Value::is_object)
        .ok_or_else(|| progress_error(format!("{name} must be an object")))
}

pub fn duration(value: Option<&J>, default: Option<f64>) -> Result<f64> {
    model::duration(value, default)
}

pub fn require_unowned(t: &J, allow_landed: bool) -> Result<()> {
    let delivery = model::get(t, "batch_delivery").filter(|value| model::truthy(Some(value)));
    if let Some(delivery) = delivery {
        if !(allow_landed && model::string(delivery, "phase").as_deref() == Some("landed")) {
            return Err(progress_error(
                "batch member is owned by its lane; use the lane delivery flow",
            ));
        }
    }
    Ok(())
}

fn workspace_conflict(t: &J, workspace: &Path, task_name: &str) -> Result<()> {
    if model::string(t, "task").as_deref() == Some(task_name) {
        return Ok(());
    }
    check_lane_conflict(t, workspace).and_then(|()| check_writer_conflict(t, workspace))
}

fn ensure(condition: bool, message: &str) -> Result<()> {
    if condition {
        Ok(())
    } else {
        Err(progress_error(message))
    }
}

fn paths_overlap(left: &Path, right: &Path) -> bool {
    is_relative_to(left, right) || is_relative_to(right, left)
}

fn check_lane_conflict(t: &J, workspace: &Path) -> Result<()> {
    let Some(lane) = model::get(t, "lane").filter(|value| model::truthy(Some(value))) else {
        return Ok(());
    };
    let Some(owned) = model::string(lane, "workspace_root") else {
        return Ok(());
    };
    ensure(
        !paths_overlap(workspace, Path::new(&owned)),
        "workspace is reserved by a batch lane; use an isolated checkout",
    )
}

fn check_writer_conflict(t: &J, workspace: &Path) -> Result<()> {
    let Some(writer) = model::get(t, "writer").filter(|value| model::truthy(Some(value))) else {
        return Ok(());
    };
    let Some(owned) = model::string(writer, "workspace") else {
        return Ok(());
    };
    ensure(
        !paths_overlap(workspace, Path::new(&owned)),
        "workspace already has a reserved/active writer; inspect and release it first",
    )
}

fn queue_running(s: &J) -> bool {
    !model::bool_or(s, "stopped", false)
}

fn task_running(t: &J) -> bool {
    model::string(t, "control").as_deref() == Some("running")
}

fn task_not_completed(t: &J) -> bool {
    model::string(t, "phase").as_deref() != Some("completed")
}

fn active_writer(t: &J) -> bool {
    model::get(t, "writer")
        .map(|value| !value.is_null())
        .unwrap_or(false)
}

fn has_lane(t: &J) -> bool {
    model::get(t, "lane")
        .filter(|value| model::truthy(Some(value)))
        .is_some()
}

fn operation_count(t: &J) -> usize {
    model::get(t, "operations")
        .and_then(Value::as_object)
        .map(Map::len)
        .unwrap_or(0)
}

fn pending_contains_event(t: &J, event_id: &str) -> bool {
    model::get(t, "pending")
        .and_then(Value::as_array)
        .map(|pending| pending.iter().any(|value| value.as_str() == Some(event_id)))
        .unwrap_or(false)
}

fn wait_has_events(s: &J, wait_id: &str) -> bool {
    model::get(s, "events")
        .and_then(Value::as_object)
        .map(|events| {
            events
                .values()
                .any(|event| model::string(event, "wait").as_deref() == Some(wait_id))
        })
        .unwrap_or(false)
}

fn ensure_wait_unconsumed(s: &J, wait_id: &str, message: &str) -> Result<()> {
    ensure(!wait_has_events(s, wait_id), message)
}

fn binding_current(t: &J, e: &J) -> bool {
    model::get(e, "binding")
        .map(|binding| same(t, binding))
        .unwrap_or(false)
}

fn lane_active(lane: &J) -> bool {
    model::get(lane, "active")
        .map(|value| model::truthy(Some(value)))
        .unwrap_or(false)
}

fn lane_pending(lane: &J) -> bool {
    model::array_field(lane, "pending")
        .map(|items| !items.is_empty())
        .unwrap_or(false)
}

pub fn reserve_writer(
    s: &J,
    workspace: &str,
    assignment: &str,
    session: &str,
    task_name: &str,
) -> Result<J> {
    let existing = model::dig(s, &["tasks", task_name])
        .cloned()
        .unwrap_or_else(|| json!({}));
    require_unowned(&existing, true)?;
    let workspace = text(Some(&json!(workspace)), "writer workspace", 1000)?;
    let path = Path::new(&workspace);
    if !path.is_absolute() {
        return Err(progress_error("writer workspace must be absolute"));
    }
    let workspace = resolve_path(path).display().to_string();
    if let Some(tasks) = model::get(s, "tasks").and_then(Value::as_object) {
        for task in tasks.values() {
            workspace_conflict(task, Path::new(&workspace), task_name)?;
        }
    }
    Ok(json!({
        "workspace": workspace,
        "assignment": assignment,
        "session": session
    }))
}

pub fn binding(w: &J) -> Result<J> {
    let assignment = text(model::get(w, "assignment"), "assignment", 128)?;
    let candidate = model::string(w, "candidate").unwrap_or_default();
    if model::get(w, "lineage").is_some() {
        lineage::validate_binding(w)?;
    } else if !model::is_full_sha(&candidate) {
        return Err(progress_error("progress requires a full candidate SHA"));
    }
    let task = model::get(w, "task")
        .cloned()
        .ok_or_else(|| progress_error("progress requires a task"))?;
    let epoch = model::get(w, "epoch")
        .cloned()
        .ok_or_else(|| progress_error("progress requires an epoch"))?;
    let mut result =
        json!({"task":task, "epoch":epoch, "assignment":assignment, "candidate":candidate});
    lineage::copy_binding(w, &mut result);
    Ok(result)
}

pub fn same(t: &J, b: &J) -> bool {
    [
        "task",
        "epoch",
        "assignment",
        "candidate",
        "lineage",
        "artifact_adapter",
    ]
    .iter()
    .all(|key| model::get(t, key) == model::get(b, key))
}

pub fn wait_for(s: &J, wait_id: Option<&J>, b: &J) -> Result<J> {
    let Some(wait_id) = wait_id.and_then(Value::as_str) else {
        return Err(progress_error(
            "successor requires an open wait bound to task/epoch/assignment/candidate",
        ));
    };
    let Some(w) = model::dig(s, &["waits", wait_id]) else {
        return Err(progress_error(
            "successor requires an open wait bound to task/epoch/assignment/candidate",
        ));
    };
    if model::bool_or(w, "closed", false) || !same(b, &binding(w)?) {
        return Err(progress_error(
            "successor requires an open wait bound to task/epoch/assignment/candidate",
        ));
    }
    if model::string(w, "kind").as_deref() == Some("guard") {
        return Err(progress_error(
            "a guard wait cannot be the assignment result",
        ));
    }
    Ok(w.clone())
}

pub fn audit(t: &mut J, now: f64, action: &str) {
    audit_fields(t, now, action, &[]);
}

pub fn audit_fields(t: &mut J, now: f64, action: &str, fields: &[(&str, J)]) {
    let Some(history) = model::get_mut(t, "history").and_then(Value::as_array_mut) else {
        return;
    };
    if history.len() >= 256 {
        let keep = history.split_off(history.len() - 255);
        *history = keep;
        let older = model::int(t, "older_transitions").unwrap_or(0) + 1;
        model::set(t, "older_transitions", json!(older));
    }
    let mut entry = Map::new();
    entry.insert("at".to_string(), json!(now));
    entry.insert(
        "version".to_string(),
        model::get(t, "state_version")
            .cloned()
            .unwrap_or(Value::Null),
    );
    entry.insert("action".to_string(), json!(action));
    for (key, value) in fields {
        entry.insert((*key).to_string(), value.clone());
    }
    let Some(history) = model::get_mut(t, "history").and_then(Value::as_array_mut) else {
        return;
    };
    history.push(Value::Object(entry));
}

fn acceptance_checks(spec: &J) -> Result<J> {
    acceptance_criteria(spec)
        .and_then(build_acceptance_checks)
        .and_then(require_required_criterion)
}

fn acceptance_criteria(spec: &J) -> Result<&[J]> {
    let Some(criteria) = model::get(spec, "acceptance").and_then(Value::as_array) else {
        return Err(progress_error(
            "original acceptance must contain 1..64 criteria",
        ));
    };
    ensure(
        (1..=64).contains(&criteria.len()),
        "original acceptance must contain 1..64 criteria",
    )
    .map(|()| criteria.as_slice())
}

fn build_acceptance_checks(criteria: &[J]) -> Result<J> {
    let mut checks = Map::new();
    criteria
        .iter()
        .try_for_each(|criterion| add_acceptance_check(&mut checks, criterion))?;
    Ok(Value::Object(checks))
}

fn add_acceptance_check(checks: &mut Map<String, Value>, criterion: &J) -> Result<()> {
    object(criterion, "acceptance criterion")
        .and_then(|_| text(model::get(criterion, "id"), "criterion ID", 64))
        .and_then(|key| insert_acceptance_check(checks, criterion, key))
}

fn insert_acceptance_check(
    checks: &mut Map<String, Value>,
    criterion: &J,
    key: String,
) -> Result<()> {
    ensure(
        !checks.contains_key(&key),
        "duplicate criterion or invalid required flag",
    )
    .and_then(|()| required_flag(criterion))
    .and_then(|required| {
        text(
            model::get(criterion, "description"),
            "acceptance description",
            1000,
        )
        .map(|description| (required, description))
    })
    .map(|(required, description)| {
        checks.insert(
            key,
            json!({
                "description": description,
                "required": required,
                "evidence": null
            }),
        );
    })
}

fn required_flag(criterion: &J) -> Result<bool> {
    let Some(value) = model::get(criterion, "required") else {
        return Ok(true);
    };
    value
        .as_bool()
        .ok_or_else(|| progress_error("duplicate criterion or invalid required flag"))
}

fn require_required_criterion(checks: Value) -> Result<J> {
    let required = checks
        .as_object()
        .map(|values| {
            values
                .values()
                .any(|value| model::bool_or(value, "required", false))
        })
        .unwrap_or(false);
    ensure(
        required,
        "at least one original acceptance criterion must be required",
    )
    .map(|()| checks)
}

fn authorization(spec: &J) -> Result<J> {
    let Some(items) = model::get(spec, "authorization").and_then(Value::as_array) else {
        return Err(progress_error(
            "record existing authorization and its source",
        ));
    };
    if items.is_empty() || items.len() > 64 {
        return Err(progress_error(
            "record existing authorization and its source",
        ));
    }
    let mut permissions = Map::new();
    for item in items {
        let _ = object(item, "authorization")?;
        let key = text(model::get(item, "id"), "authorization ID", 64)?;
        if permissions.contains_key(&key) {
            return Err(progress_error("duplicate authorization ID"));
        }
        permissions.insert(
            key,
            json!({
                "action": text(model::get(item, "action"), "authorized action", 1000)?,
                "source": text(model::get(item, "source"), "authorization source", 1000)?
            }),
        );
    }
    Ok(Value::Object(permissions))
}

struct Registration {
    task: String,
    wait_id: String,
    wait: J,
    binding: J,
    checks: J,
    permissions: J,
    task_json: J,
    now: f64,
}

pub fn register(s: &mut J, spec: &J, now: f64) -> Result<J> {
    object(spec, "task spec")
        .and_then(|_| prepare_registration_task(s, spec))
        .and_then(|task| load_registration(s, spec, task, now))
        .and_then(|registration| build_registration(spec, registration))
        .and_then(|registration| attach_registration_writer(s, spec, registration))
        .and_then(|registration| finish_registration(s, registration, now))
}

fn prepare_registration_task(s: &mut J, spec: &J) -> Result<String> {
    ensure_not_stopped(s)
        .and_then(|()| text(model::get(spec, "task"), "task", 500))
        .and_then(|task| ensure_tasks_map(s).map(|()| task))
        .and_then(|task| ensure_task_slot(s, &task).map(|()| task))
}

fn ensure_not_stopped(s: &J) -> Result<()> {
    ensure(queue_running(s), "queue is stopped")
}

fn ensure_tasks_map(s: &mut J) -> Result<()> {
    if model::get(s, "tasks").is_none() {
        s["tasks"] = json!({});
    }
    ensure(
        model::get(s, "tasks")
            .map(Value::is_object)
            .unwrap_or(false),
        "task already exists or queue reached 100 tasks",
    )
}

fn task_count(s: &J) -> usize {
    model::get(s, "tasks")
        .and_then(Value::as_object)
        .map(Map::len)
        .unwrap_or(0)
}

fn task_slot_available(s: &J, task: &str) -> bool {
    model::dig(s, &["tasks", task]).is_none() && task_count(s) < 100
}

fn ensure_task_slot(s: &J, task: &str) -> Result<()> {
    ensure(
        task_slot_available(s, task),
        "task already exists or queue reached 100 tasks",
    )
}

fn load_registration(s: &J, spec: &J, task: String, now: f64) -> Result<Registration> {
    let wait_id = model::string(spec, "wait").unwrap_or_default();
    load_registration_wait(s, &task, &wait_id).and_then(|(wait, binding)| {
        registration_schemas(spec).map(|(checks, permissions)| Registration {
            task: task.clone(),
            wait_id: wait_id.clone(),
            wait,
            binding,
            checks,
            permissions,
            task_json: Value::Null,
            now,
        })
    })
}

fn load_registration_wait(s: &J, task: &str, wait_id: &str) -> Result<(J, J)> {
    let Some(wait) = model::dig(s, &["waits", wait_id]).cloned() else {
        return Err(progress_error(
            "register the bound result wait before the task",
        ));
    };
    ensure(
        model::string(&wait, "task").as_deref() == Some(task),
        "register the bound result wait before the task",
    )
    .and_then(|()| binding(&wait))
    .and_then(|binding| {
        ensure_wait_available(s, wait_id, &binding).map(|()| (wait.clone(), binding))
    })
}

fn ensure_wait_available(s: &J, wait_id: &str, binding: &J) -> Result<()> {
    wait_for(s, Some(&json!(wait_id)), binding).and_then(|_| {
        ensure(
            !wait_has_events(s, wait_id),
            "register task before result collection/ACK",
        )
    })
}

fn registration_schemas(spec: &J) -> Result<(J, J)> {
    acceptance_checks(spec)
        .and_then(|checks| authorization(spec).map(|permissions| (checks, permissions)))
}

fn build_registration(spec: &J, mut registration: Registration) -> Result<Registration> {
    let task_json = build_registration_task(spec, &registration)?;
    registration.task_json = task_json;
    Ok(registration)
}

fn build_registration_task(spec: &J, registration: &Registration) -> Result<J> {
    text(model::get(spec, "goal"), "goal", 1000)
        .and_then(|goal| {
            duration(model::get(spec, "apply_timeout_seconds"), Some(600.0))
                .map(|timeout| (goal, timeout))
        })
        .map(|(goal, timeout)| registration_task_json(registration, goal, timeout))
}

fn registration_task_json(registration: &Registration, goal: String, timeout: f64) -> J {
    let owner = model::string(&registration.wait, "owner").unwrap_or_default();
    let condition = model::string(&registration.wait, "condition").unwrap_or_default();
    json!({
        "task": registration.binding["task"],
        "epoch": registration.binding["epoch"],
        "assignment": registration.binding["assignment"],
        "candidate": registration.binding["candidate"],
        "goal": goal,
        "owner": owner,
        "state_version": 1,
        "phase": "waiting",
        "control": "running",
        "wait": registration.wait_id,
        "next_action": {
            "kind": "wait",
            "wait": registration.wait_id,
            "condition": condition,
            "owner": owner
        },
        "acceptance": registration.checks,
        "authorization": registration.permissions,
        "pending": [],
        "operations": {},
        "findings": {},
        "history": [],
        "created_at": registration.now,
        "apply_timeout": timeout,
        "writer": null
    })
}

fn workspace_requested(spec: &J) -> bool {
    model::truthy(model::get(spec, "workspace"))
}

fn attach_registration_writer(
    s: &J,
    spec: &J,
    mut registration: Registration,
) -> Result<Registration> {
    write_registration_writer(
        s,
        spec,
        &mut registration.task_json,
        &registration.wait,
        &registration.task,
    )?;
    Ok(registration)
}

fn write_registration_writer(
    s: &J,
    spec: &J,
    task_json: &mut J,
    wait: &J,
    task: &str,
) -> Result<()> {
    if !workspace_requested(spec) {
        return Ok(());
    }
    attach_registration_writer_steps(s, spec, task_json, wait, task)
}

fn attach_registration_writer_steps(
    s: &J,
    spec: &J,
    task_json: &mut J,
    wait: &J,
    task: &str,
) -> Result<()> {
    writer_session(wait)
        .and_then(|session| reserve_registration_writer(s, spec, task_json, task, &session))
        .and_then(|writer| writer_with_evidence(spec, writer))
        .and_then(|mut writer| {
            writer["producer"] = crate::identity::required_producer(wait)?;
            set_task_writer(task_json, writer)
        })
}

fn writer_session(wait: &J) -> Result<String> {
    Ok(crate::identity::required_producer(wait)?["session"]
        .as_str()
        .unwrap()
        .to_string())
}

fn reserve_registration_writer(
    s: &J,
    spec: &J,
    task_json: &J,
    task: &str,
    session: &str,
) -> Result<J> {
    reserve_writer(
        s,
        &model::string(spec, "workspace").unwrap_or_default(),
        &model::string(task_json, "assignment").unwrap_or_default(),
        session,
        task,
    )
}

fn writer_with_evidence(spec: &J, mut writer: J) -> Result<J> {
    text(
        model::get(spec, "execution_evidence"),
        "initial execution evidence",
        1000,
    )
    .map(|evidence| {
        writer["evidence"] = json!(evidence);
        writer
    })
}

fn set_task_writer(task_json: &mut J, writer: J) -> Result<()> {
    task_json["writer"] = writer;
    Ok(())
}

fn finish_registration(s: &mut J, mut registration: Registration, now: f64) -> Result<J> {
    lineage::copy_binding(&registration.binding, &mut registration.task_json);
    lineage::validate_registration(s, &registration.task_json)?;
    registration.task_json["schema_version"] =
        json!(
            if model::get(&registration.task_json, "lineage").is_some() {
                2
            } else {
                1
            }
        );
    audit(&mut registration.task_json, now, "registered");
    s["tasks"][&registration.task] = registration.task_json.clone();
    Ok(registration.task_json)
}

pub fn attach(s: &mut J, event: &mut J, w: &J, now: f64) -> Result<()> {
    let task_name = model::string(w, "task").unwrap_or_default();
    let Some(mut t) = model::dig(s, &["tasks", &task_name]).cloned() else {
        return Ok(());
    };
    if model::get(w, "assignment").is_none() {
        return Ok(());
    }
    event["binding"] = binding(w)?;
    event["applied"] = json!(false);
    let binding = event["binding"].clone();
    if !lineage::current(s, &t) || !attachment_current(&t, &binding, w) {
        event["archived"] = json!("stale assignment/candidate or terminal task");
        event["acknowledged"] = json!(true);
        event["withdrawn"] = json!(true);
        return Ok(());
    }
    let apply_timeout = model::number(&t, "apply_timeout").unwrap_or(600.0);
    event["apply_deadline"] = json!(now + apply_timeout);
    if let Some(pending) = model::get_mut(&mut t, "pending").and_then(Value::as_array_mut) {
        pending.push(event["id"].clone());
    }
    if model::string(&t, "phase").as_deref() != Some("next_pending") {
        t["phase"] = json!("result_pending");
    }
    let version = model::int(&t, "state_version").unwrap_or(0) + 1;
    t["state_version"] = json!(version);
    audit_fields(
        &mut t,
        now,
        "result_persisted",
        &[("event", event["id"].clone())],
    );
    s["tasks"][&task_name] = t;
    Ok(())
}

fn attachment_current(t: &J, binding: &J, wait: &J) -> bool {
    same(t, binding)
        && model::string(t, "wait") == model::string(wait, "id")
        && task_not_completed(t)
        && model::string(t, "control").as_deref() != Some("cancelled")
}

pub fn task(s: &J, name: Option<&J>, version: Option<&J>) -> Result<J> {
    let Some(name) = name.and_then(Value::as_str) else {
        return Err(progress_error(
            "task state version changed; inspect current state before acting",
        ));
    };
    let Some(t) = model::dig(s, &["tasks", name]) else {
        return Err(progress_error(
            "task state version changed; inspect current state before acting",
        ));
    };
    let Some(version) = version.and_then(Value::as_i64) else {
        return Err(progress_error(
            "task state version changed; inspect current state before acting",
        ));
    };
    if model::int(t, "state_version") != Some(version) {
        return Err(progress_error(
            "task state version changed; inspect current state before acting",
        ));
    }
    ensure(
        lineage::supported_schema(t),
        "unsupported task schema version",
    )?;
    Ok(t.clone())
}

fn record_acceptance(t: &mut J, spec: &J, e: &J) -> Result<()> {
    let Some(evidence) = model::get(spec, "acceptance") else {
        return Ok(());
    };
    let Some(evidence) = evidence.as_object() else {
        return Err(progress_error("acceptance evidence must be an object"));
    };
    let candidate = model::string(t, "candidate").unwrap_or_default();
    let event_id = model::string(e, "id").unwrap_or_default();
    for (key, value) in evidence {
        if model::dig(t, &["acceptance", key]).is_none() {
            return Err(progress_error(
                "cannot replace or extend original acceptance during result application",
            ));
        }
        let path = text(Some(value), "acceptance evidence", 1000)?;
        t["acceptance"][key]["evidence"] = json!({
            "path": path,
            "candidate": candidate,
            "event": event_id
        });
    }
    Ok(())
}

fn add_findings(t: &mut J, findings: Option<&J>) -> Result<()> {
    let Some(findings) = findings else {
        return Ok(());
    };
    let Some(findings) = findings.as_array() else {
        return Err(progress_error("finding must be an object"));
    };
    findings
        .iter()
        .try_for_each(|finding| add_finding(t, finding))
}

fn finding_count(t: &J) -> usize {
    model::get(t, "findings")
        .and_then(Value::as_object)
        .map(Map::len)
        .unwrap_or(0)
}

fn finding_slot_available(t: &J, key: &str) -> bool {
    model::dig(t, &["findings", key]).is_none() && finding_count(t) < 64
}

fn add_finding(t: &mut J, finding: &J) -> Result<()> {
    object(finding, "finding")
        .and_then(|_| text(model::get(finding, "id"), "finding ID", 64))
        .and_then(|key| insert_finding(t, finding, key))
}

fn insert_finding(t: &mut J, finding: &J, key: String) -> Result<()> {
    ensure(
        finding_slot_available(t, &key),
        "duplicate finding or finding limit reached",
    )
    .and_then(|()| blocking_flag(finding))
    .and_then(|blocking| build_finding(finding, blocking))
    .map(|value| {
        t["findings"][key] = value;
    })
}

fn blocking_flag(finding: &J) -> Result<bool> {
    let Some(value) = model::get(finding, "blocking") else {
        return Err(progress_error(
            "finding must explicitly classify blocking/nonblocking",
        ));
    };
    value
        .as_bool()
        .ok_or_else(|| progress_error("finding must explicitly classify blocking/nonblocking"))
}

fn build_finding(finding: &J, blocking: bool) -> Result<J> {
    text(model::get(finding, "evidence"), "finding evidence", 1000)
        .and_then(|evidence| {
            text(
                model::get(finding, "clears_when"),
                "finding release condition",
                1000,
            )
            .map(|clears_when| (evidence, clears_when))
        })
        .map(|(evidence, clears_when)| {
            json!({
                "blocking": blocking,
                "evidence": evidence,
                "clears_when": clears_when,
                "resolved": null
            })
        })
}

fn resolve_findings(t: &mut J, resolutions: Option<&J>) -> Result<()> {
    let Some(resolutions) = resolutions else {
        return Ok(());
    };
    let Some(resolutions) = resolutions.as_object() else {
        return Err(progress_error("finding resolutions must be an object"));
    };
    for (key, value) in resolutions {
        if model::dig(t, &["findings", key]).is_none() {
            return Err(progress_error("unknown finding"));
        }
        let evidence = text(Some(value), "finding resolution evidence", 1000)?;
        t["findings"][key]["resolved"] = json!(evidence);
    }
    Ok(())
}

#[derive(Default)]
struct DispatchContext {
    opid: String,
    assignment: String,
    session: String,
    owner: String,
    wait_id: String,
    seconds: f64,
    writes: bool,
}

fn apply_dispatch(s: &J, t: &mut J, a: &mut J, e: &J, now: f64) -> Result<()> {
    let mut context = DispatchContext::default();
    dispatch_validate(s, t, a, &mut context)
        .and_then(|()| dispatch_wait(s, t, a, &mut context))
        .and_then(|()| dispatch_finish(s, t, a, e, now, &context))
}

fn dispatch_validate(s: &J, t: &J, a: &J, context: &mut DispatchContext) -> Result<()> {
    require_unowned(t, true)
        .and_then(|()| ensure_no_active_writer(t))
        .and_then(|()| set_dispatch_opid(context, a))
        .and_then(|()| ensure_operation_unused(s, &context.opid))
        .and_then(|()| ensure_operation_room(t))
        .and_then(|()| ensure_dispatch_authorized(t, a))
        .and_then(|()| set_dispatch_assignment(context, t, a))
        .and_then(|()| set_dispatch_session(context, a))
        .and_then(|()| set_dispatch_owner(context, a))
        .and_then(|()| require_dispatch_instruction(a))
}

fn ensure_no_active_writer(t: &J) -> Result<()> {
    ensure(
        !active_writer(t),
        "release the previous writer with evidence before successor dispatch",
    )
}

fn set_dispatch_opid(context: &mut DispatchContext, a: &J) -> Result<()> {
    text(model::get(a, "operation"), "operation ID", 128).map(|opid| context.opid = opid)
}

fn operation_id_used(s: &J, opid: &str) -> bool {
    model::get(s, "tasks")
        .and_then(Value::as_object)
        .map(|tasks| {
            tasks
                .values()
                .any(|task| model::dig(task, &["operations", opid]).is_some())
        })
        .unwrap_or(false)
}

fn ensure_operation_unused(s: &J, opid: &str) -> Result<()> {
    ensure(
        !operation_id_used(s, opid),
        "operation ID already used; inspect it instead of replaying",
    )
}

fn ensure_operation_room(t: &J) -> Result<()> {
    ensure(
        operation_count(t) < 128,
        "task reached 128 operations; preserve this task and use a new bounded task contract",
    )
}

fn dispatch_authorized(t: &J, a: &J) -> bool {
    let auth_key = model::string(a, "authorization").unwrap_or_default();
    model::dig(t, &["authorization", &auth_key])
        .map(|permission| model::string(a, "action") == model::string(permission, "action"))
        .unwrap_or(false)
}

fn ensure_dispatch_authorized(t: &J, a: &J) -> Result<()> {
    ensure(
        dispatch_authorized(t, a),
        "successor must reference an existing exact authorized action",
    )
}

fn assignment_reused(t: &J, assignment: &str) -> bool {
    model::get(t, "operations")
        .and_then(Value::as_object)
        .map(|operations| {
            operations.values().any(|operation| {
                model::string(operation, "assignment").as_deref() == Some(assignment)
            })
        })
        .unwrap_or(false)
}

fn successor_assignment_new(t: &J, assignment: &str) -> bool {
    assignment != model::string(t, "assignment").unwrap_or_default()
        && !assignment_reused(t, assignment)
}

fn set_dispatch_assignment(context: &mut DispatchContext, t: &J, a: &J) -> Result<()> {
    text(model::get(a, "assignment"), "successor assignment", 128)
        .and_then(|assignment| {
            ensure(
                successor_assignment_new(t, &assignment),
                "successor needs a new assignment ID",
            )
            .map(|()| assignment)
        })
        .map(|assignment| context.assignment = assignment)
}

fn set_dispatch_session(context: &mut DispatchContext, a: &J) -> Result<()> {
    text(model::get(a, "session"), "successor session", 128)
        .map(|session| context.session = session)
}

fn set_dispatch_owner(context: &mut DispatchContext, a: &J) -> Result<()> {
    text(model::get(a, "owner"), "successor owner", 500).map(|owner| context.owner = owner)
}

fn require_dispatch_instruction(a: &J) -> Result<()> {
    text(
        model::get(a, "instruction"),
        "concrete successor instruction",
        1000,
    )
    .map(|_| ())
}

fn dispatch_wait(s: &J, t: &J, a: &J, context: &mut DispatchContext) -> Result<()> {
    dispatch_probe_wait(s, t, a, &context.assignment)
        .and_then(|wait| ensure_producer_session(&wait, a, &context.session).map(|()| wait))
        .and_then(|wait| set_dispatch_wait_id(context, &wait))
        .and_then(|()| {
            ensure_wait_unconsumed(s, &context.wait_id, "successor wait already has events")
        })
        .and_then(|()| set_dispatch_seconds(context, a))
        .and_then(|()| set_dispatch_writes(context, a))
}

fn dispatch_probe_wait(s: &J, t: &J, a: &J, assignment: &str) -> Result<J> {
    let mut probe = t.clone();
    probe["assignment"] = json!(assignment);
    lineage::advance_session(&mut probe);
    wait_for(s, model::get(a, "wait"), &probe)
}

fn ensure_producer_session(wait: &J, action: &J, session: &str) -> Result<()> {
    let actual = crate::identity::required_producer(wait)?;
    let expected = crate::identity::validate(
        &json!({"adapter":action.get("adapter").cloned().unwrap_or(json!("codex")),"session":session}),
    )?;
    ensure(
        actual == expected,
        "successor result wait must pin the receiving producer session",
    )
}

fn set_dispatch_wait_id(context: &mut DispatchContext, wait: &J) -> Result<()> {
    context.wait_id = model::string(wait, "id").unwrap_or_default();
    Ok(())
}

fn set_dispatch_seconds(context: &mut DispatchContext, a: &J) -> Result<()> {
    duration(model::get(a, "accept_timeout_seconds"), Some(600.0))
        .map(|seconds| context.seconds = seconds)
}

fn dispatch_writes(a: &J) -> Result<bool> {
    let Some(value) = model::get(a, "writes") else {
        return Ok(true);
    };
    value
        .as_bool()
        .ok_or_else(|| progress_error("writes must be a boolean"))
}

fn set_dispatch_writes(context: &mut DispatchContext, a: &J) -> Result<()> {
    dispatch_writes(a).map(|writes| context.writes = writes)
}

fn dispatch_finish(
    s: &J,
    t: &mut J,
    a: &J,
    e: &J,
    now: f64,
    context: &DispatchContext,
) -> Result<()> {
    repair_dispatch_intent(t, a, e, now)
        .and_then(|()| reserve_dispatch_writer(s, t, a, context))
        .map(|()| record_dispatch_operation(t, a, now, context))
}

fn repair_dispatch_intent(t: &mut J, a: &J, e: &J, now: f64) -> Result<()> {
    if has_lane(t) {
        crate::batch::repair_intent(t, a, e, now)
    } else {
        crate::pr::repair_intent(t, a, e, now)
    }
}

fn reserve_dispatch_writer(s: &J, t: &mut J, a: &J, context: &DispatchContext) -> Result<()> {
    if !context.writes {
        return Ok(());
    }
    reserve_dispatch_writer_now(s, t, a, context)
}

fn reserve_dispatch_writer_now(s: &J, t: &mut J, a: &J, context: &DispatchContext) -> Result<()> {
    text(model::get(a, "workspace"), "writer workspace", 1000)
        .and_then(|workspace| {
            reserve_writer(
                s,
                &workspace,
                &context.assignment,
                &context.session,
                &model::string(t, "task").unwrap_or_default(),
            )
        })
        .map(|mut writer| {
            writer["producer"] = json!({"adapter":a.get("adapter").cloned().unwrap_or(json!("codex")),"session":context.session});
            t["writer"] = writer;
        })
}

fn record_dispatch_operation(t: &mut J, a: &J, now: f64, context: &DispatchContext) {
    lineage::advance_session(t);
    t["operations"][&context.opid] = json!({
        "id": context.opid,
        "adapter": a.get("adapter").cloned().unwrap_or(json!("codex")),
        "assignment": context.assignment,
        "session": context.session,
        "owner": context.owner,
        "phase": "prepared",
        "prepared_at": now,
        "deadline": now + context.seconds,
        "intent": a
    });
    t["assignment"] = json!(context.assignment);
    t["owner"] = json!(context.owner);
    t["wait"] = json!(context.wait_id);
    t["phase"] = json!("next_pending");
}

fn apply_wait(s: &J, t: &mut J, a: &mut J) -> Result<()> {
    wait_for(s, model::get(a, "wait"), t)
        .and_then(|wait| {
            let wait_id = model::string(&wait, "id").unwrap_or_default();
            ensure_wait_unconsumed(
                s,
                &wait_id,
                "next wait already has collected events; register a fresh concrete dependency",
            )
            .map(|()| wait)
        })
        .map(|wait| attach_wait_action(t, a, &wait))
}

fn attach_wait_action(t: &mut J, a: &mut J, wait: &J) {
    let wait_id = model::string(wait, "id").unwrap_or_default();
    let owner = model::string(wait, "owner").unwrap_or_default();
    let condition = model::string(wait, "condition").unwrap_or_default();
    a["owner"] = json!(owner);
    a["condition"] = json!(condition);
    t["wait"] = json!(wait_id);
    t["owner"] = json!(owner);
    t["phase"] = json!("waiting");
}

fn apply_blocked(t: &mut J, a: &J) -> Result<()> {
    blocker_owner(a)
        .and_then(|owner| require_blocker_fields(a).map(|()| owner))
        .map(|owner| set_blocked(t, owner))
}

fn blocker_owner(a: &J) -> Result<String> {
    text(model::get(a, "owner"), "blocker owner", 500)
}

fn require_blocker_fields(a: &J) -> Result<()> {
    text(model::get(a, "reason"), "blocking reason", 1000)
        .and_then(|_| text(model::get(a, "condition"), "recovery condition", 1000).map(|_| ()))
}

fn set_blocked(t: &mut J, owner: String) {
    t["owner"] = json!(owner);
    t["phase"] = json!("blocked");
}

fn apply_complete(t: &mut J, now: f64) -> Result<()> {
    ensure_batch_landed(t)
        .and_then(|()| ensure_lane_settled(t))
        .and_then(|()| ensure_pr_merged(t))
        .and_then(|()| ensure_writer_released(t))
        .and_then(|()| ensure_acceptance_met(t))
        .and_then(|()| ensure_findings_resolved(t))
        .map(|()| mark_task_complete(t, now))
}

fn batch_landed(t: &J) -> bool {
    let Some(delivery) = model::get(t, "batch_delivery").filter(|value| model::truthy(Some(value)))
    else {
        return true;
    };
    model::string(delivery, "phase").as_deref() == Some("landed")
}

fn ensure_batch_landed(t: &J) -> Result<()> {
    ensure(
        batch_landed(t),
        "batch member delivery requires confirmed landing before task completion",
    )
}

fn lane_settled(t: &J) -> bool {
    let Some(lane) = model::get(t, "lane").filter(|value| model::truthy(Some(value))) else {
        return true;
    };
    !lane_active(lane) && !lane_pending(lane)
}

fn ensure_lane_settled(t: &J) -> Result<()> {
    ensure(
        lane_settled(t),
        "deliver or explicitly disposition owned batch members before completing the lane",
    )
}

fn pr_merged(t: &J) -> bool {
    let Some(pull_request) = model::get(t, "pr").filter(|value| model::truthy(Some(value))) else {
        return true;
    };
    model::string(pull_request, "phase").as_deref() == Some("merged")
}

fn ensure_pr_merged(t: &J) -> Result<()> {
    ensure(
        pr_merged(t),
        "PR delivery requires confirmed remote merge and landing before task completion",
    )
}

fn ensure_writer_released(t: &J) -> Result<()> {
    ensure(
        !active_writer(t),
        "release the active writer with evidence before completing the task",
    )
}

fn required_acceptance_missing(criterion: &J) -> bool {
    model::bool_or(criterion, "required", false)
        && criterion
            .get("evidence")
            .map(Value::is_null)
            .unwrap_or(true)
}

fn acceptance_met(t: &J) -> bool {
    let Some(acceptance) = model::get(t, "acceptance").and_then(Value::as_object) else {
        return true;
    };
    acceptance
        .values()
        .all(|criterion| !required_acceptance_missing(criterion))
}

fn ensure_acceptance_met(t: &J) -> Result<()> {
    ensure(
        acceptance_met(t),
        "original required acceptance is still missing",
    )
}

fn blocking_finding_unresolved(finding: &J) -> bool {
    model::bool_or(finding, "blocking", false)
        && finding.get("resolved").map(Value::is_null).unwrap_or(true)
}

fn findings_resolved(t: &J) -> bool {
    let Some(findings) = model::get(t, "findings").and_then(Value::as_object) else {
        return true;
    };
    findings
        .values()
        .all(|finding| !blocking_finding_unresolved(finding))
}

fn ensure_findings_resolved(t: &J) -> Result<()> {
    ensure(
        findings_resolved(t),
        "blocking review findings are unresolved",
    )
}

fn mark_task_complete(t: &mut J, now: f64) {
    t["phase"] = json!("completed");
    t["completed_at"] = json!(now);
}

struct ApplyState {
    task: J,
    event: J,
    event_key: String,
    event_id: String,
    dismiss: Vec<String>,
    action: J,
    kind: String,
    version: i64,
    now: f64,
}

/// Atomically apply a received result and record a concrete successor.
pub fn apply(s: &mut J, spec: &J, now: f64) -> Result<J> {
    apply_with_adapter(s, spec, now, &artifact::LocalArtifacts)
}

pub fn apply_with_adapter(
    s: &mut J,
    spec: &J,
    now: f64,
    adapter: &dyn artifact::ArtifactAdapter,
) -> Result<J> {
    let mut staged = s.clone();
    let prepared = acceptance::prepare(&staged, spec)?;
    let child = apply_inner(&mut staged, &prepared, now)?;
    lineage::applied(&mut staged, &prepared, &child, now, adapter)?;
    *s = staged;
    Ok(child)
}

fn apply_inner(s: &mut J, spec: &J, now: f64) -> Result<J> {
    object(spec, "application spec")
        .and_then(|_| load_apply_state(s, spec, now))
        .and_then(|state| collect_apply_state(spec, state))
        .and_then(|state| apply_successor(s, state))
        .map(|state| commit_apply_state(s, state))
}

fn load_apply_state(s: &J, spec: &J, now: f64) -> Result<ApplyState> {
    task(
        s,
        model::get(spec, "task"),
        model::get(spec, "state_version"),
    )
    .and_then(|task| lineage::ensure_current(s, &task).map(|()| task))
    .and_then(|task| ensure_applyable(s, &task).map(|()| task))
    .and_then(|task| load_apply_event(s, spec, task, now))
}

fn applyable(s: &J, t: &J) -> bool {
    queue_running(s) && task_applyable(t)
}

fn task_applyable(t: &J) -> bool {
    task_running(t) && task_not_completed(t)
}

fn ensure_applyable(s: &J, t: &J) -> Result<()> {
    ensure(
        applyable(s, t),
        "task is stopped, paused, cancelled or completed",
    )
}

fn load_apply_event(s: &J, spec: &J, task: J, now: f64) -> Result<ApplyState> {
    let event_key = model::string(spec, "event").unwrap_or_default();
    let Some(event) = model::dig(s, &["events", &event_key]).cloned() else {
        return Err(progress_error(
            "apply requires a received, current, unapplied result",
        ));
    };
    let event_id = model::string(&event, "id").unwrap_or_default();
    Ok(ApplyState {
        task,
        event,
        event_key,
        event_id,
        dismiss: Vec::new(),
        action: json!({}),
        kind: String::new(),
        version: 0,
        now,
    })
}

fn collect_apply_state(spec: &J, mut state: ApplyState) -> Result<ApplyState> {
    ensure(
        apply_current(&state),
        "apply requires a received, current, unapplied result",
    )
    .and_then(|()| set_apply_dismiss(&mut state, spec))
    .and_then(|()| record_acceptance(&mut state.task, spec, &state.event))
    .and_then(|()| add_findings(&mut state.task, model::get(spec, "findings")))
    .and_then(|()| resolve_findings(&mut state.task, model::get(spec, "resolve_findings")))
    .and_then(|()| set_apply_action(&mut state, spec))
    .and_then(|()| apply_release_evidence(&mut state, spec))
    .map(|()| state)
}

fn result_pending_phase(t: &J) -> bool {
    model::string(t, "phase").as_deref() == Some("result_pending")
}

fn receipt_current(e: &J) -> bool {
    model::bool_or(e, "acknowledged", false) && !model::bool_or(e, "archived", false)
}

fn apply_pending_pair(state: &ApplyState) -> bool {
    result_pending_phase(&state.task) && pending_contains_event(&state.task, &state.event_id)
}

fn apply_pending_current(state: &ApplyState) -> bool {
    apply_pending_pair(state) && receipt_current(&state.event)
}

fn apply_current(state: &ApplyState) -> bool {
    apply_pending_current(state) && binding_current(&state.task, &state.event)
}

fn set_apply_dismiss(state: &mut ApplyState, spec: &J) -> Result<()> {
    dismiss_values(spec)
        .and_then(|dismiss| {
            ensure(
                archived_pending(state, &dismiss),
                "explicitly account for every other pending result with archive_events",
            )
            .map(|()| dismiss)
        })
        .map(|dismiss| state.dismiss = dismiss)
}

fn dismiss_values(spec: &J) -> Result<Vec<String>> {
    let Some(value) = model::get(spec, "archive_events") else {
        return Ok(Vec::new());
    };
    let Some(items) = value.as_array() else {
        return Err(progress_error(
            "explicitly account for every other pending result with archive_events",
        ));
    };
    items
        .iter()
        .map(|item| {
            item.as_str().map(ToString::to_string).ok_or_else(|| {
                progress_error(
                    "explicitly account for every other pending result with archive_events",
                )
            })
        })
        .collect()
}

fn archived_pending(state: &ApplyState, dismiss: &[String]) -> bool {
    let others: BTreeSet<String> = model::array_field(&state.task, "pending")
        .map(|pending| {
            pending
                .iter()
                .filter_map(Value::as_str)
                .filter(|id| *id != state.event_id)
                .map(ToString::to_string)
                .collect()
        })
        .unwrap_or_default();
    let dismissed: BTreeSet<String> = dismiss.iter().cloned().collect();
    others == dismissed
}

fn set_apply_action(state: &mut ApplyState, spec: &J) -> Result<()> {
    object_owned(model::get(spec, "next_action"), "next action").map(|action| state.action = action)
}

fn apply_release_evidence(state: &mut ApplyState, spec: &J) -> Result<()> {
    if !model::truthy(model::get(spec, "release_evidence")) {
        return Ok(());
    }
    write_release_evidence(state, spec)
}

fn write_release_evidence(state: &mut ApplyState, spec: &J) -> Result<()> {
    text(
        model::get(spec, "release_evidence"),
        "writer release evidence",
        1000,
    )
    .map(|evidence| {
        state.task["writer"] = Value::Null;
        state.task["last_release"] = json!({
            "evidence": evidence,
            "at": state.now,
            "assignment": model::string(&state.task, "assignment")
        });
    })
}

fn apply_successor(s: &J, mut state: ApplyState) -> Result<ApplyState> {
    let kind = model::string(&state.action, "kind").unwrap_or_default();
    apply_action_kind(
        s,
        &mut state.task,
        &mut state.action,
        &state.event,
        state.now,
        &kind,
    )
    .map(|()| {
        state.kind = kind;
        state
    })
}

fn apply_action_kind(s: &J, t: &mut J, a: &mut J, e: &J, now: f64, kind: &str) -> Result<()> {
    if kind == "dispatch" {
        return apply_dispatch(s, t, a, e, now);
    }
    apply_non_dispatch_kind(s, t, a, now, kind)
}

fn apply_non_dispatch_kind(s: &J, t: &mut J, a: &mut J, now: f64, kind: &str) -> Result<()> {
    if kind == "wait" {
        return apply_wait(s, t, a);
    }
    apply_static_kind(t, a, now, kind)
}

fn apply_static_kind(t: &mut J, a: &mut J, now: f64, kind: &str) -> Result<()> {
    if kind == "blocked" {
        return apply_blocked(t, a);
    }
    apply_complete_kind(t, now, kind)
}

fn apply_complete_kind(t: &mut J, now: f64, kind: &str) -> Result<()> {
    if kind == "complete" {
        return apply_complete(t, now);
    }
    Err(progress_error(
        "successor must be dispatch, registered wait, explicit blocker, or acceptance complete",
    ))
}

fn commit_apply_state(s: &mut J, mut state: ApplyState) -> J {
    commit_apply_task(s, &mut state);
    commit_apply_event(s, &state);
    state.task
}

fn commit_apply_task(s: &mut J, state: &mut ApplyState) {
    let version = model::int(&state.task, "state_version").unwrap_or(0) + 1;
    state.task["next_action"] = state.action.clone();
    state.task["pending"] = json!([]);
    state.task["state_version"] = json!(version);
    audit_fields(
        &mut state.task,
        state.now,
        "applied",
        &[
            ("event", json!(state.event_id)),
            ("next", json!(state.kind)),
        ],
    );
    let task_name = model::string(&state.task, "task").unwrap_or_default();
    s["tasks"][&task_name] = state.task.clone();
    state.version = version;
}

fn commit_apply_event(s: &mut J, state: &ApplyState) {
    mark_event_applied(s, &state.event_key, state.version, state.now);
    state
        .dismiss
        .iter()
        .for_each(|dismissed| archive_event(s, dismissed, &state.event_id));
}

fn mark_event_applied(s: &mut J, event_key: &str, version: i64, now: f64) {
    if let Some(event) = model::get_mut(s, "events")
        .and_then(Value::as_object_mut)
        .and_then(|events| events.get_mut(event_key))
    {
        event["applied"] = json!(true);
        event["applied_at"] = json!(now);
        event["applied_version"] = json!(version);
    }
}

fn archive_event(s: &mut J, dismissed: &str, event_id: &str) {
    if let Some(event) = model::get_mut(s, "events")
        .and_then(Value::as_object_mut)
        .and_then(|events| events.get_mut(dismissed))
    {
        event["archived"] = json!(format!("explicitly superseded by {event_id}"));
    }
}

struct BeginState {
    task: J,
    operation: J,
    operation_id: String,
}

pub fn begin(
    s: &mut J,
    name: Option<&J>,
    version: Option<&J>,
    operation: Option<&J>,
    now: f64,
) -> Result<J> {
    task(s, name, version)
        .and_then(|task| require_unowned(&task, true).map(|()| task))
        .and_then(|task| load_begin_operation(task, operation))
        .and_then(|state| validate_begin(s, &state, now).map(|()| state))
        .map(|state| commit_begin(s, state, now))
}

fn load_begin_operation(task: J, operation: Option<&J>) -> Result<BeginState> {
    let Some(operation) = operation.and_then(Value::as_str) else {
        return Err(progress_error(
            "operation may already have executed; reconcile instead of resending",
        ));
    };
    let Some(op) = model::dig(&task, &["operations", operation]).cloned() else {
        return Err(progress_error(
            "operation may already have executed; reconcile instead of resending",
        ));
    };
    Ok(BeginState {
        task,
        operation: op,
        operation_id: operation.to_string(),
    })
}

fn validate_begin(s: &J, state: &BeginState, now: f64) -> Result<()> {
    lineage::ensure_current(s, &state.task)?;
    ensure(
        begin_state_ok(s, &state.task, &state.operation),
        "operation may already have executed; reconcile instead of resending",
    )
    .and_then(|()| check_begin_repair(s, &state.task, &state.operation, now))
}

fn begin_queue_ok(s: &J) -> bool {
    queue_running(s)
}

fn begin_control_ok(t: &J) -> bool {
    task_running(t)
}

fn begin_phase_ok(t: &J) -> bool {
    model::string(t, "phase").as_deref() == Some("next_pending")
}

fn begin_operation_ok(operation: &J) -> bool {
    model::string(operation, "phase").as_deref() == Some("prepared")
}

fn begin_task_ok(s: &J, t: &J) -> bool {
    begin_queue_ok(s) && begin_control_ok(t)
}

fn begin_task_phase_ok(s: &J, t: &J) -> bool {
    begin_task_ok(s, t) && begin_phase_ok(t)
}

fn begin_state_ok(s: &J, t: &J, operation: &J) -> bool {
    begin_task_phase_ok(s, t) && begin_operation_ok(operation)
}

fn check_begin_repair(s: &J, t: &J, operation: &J, now: f64) -> Result<()> {
    if has_lane(t) {
        crate::batch::check_repair(s, t, operation, now, true)
    } else {
        crate::pr::check_repair(s, t, operation, now, true)
    }
}

fn commit_begin(s: &mut J, mut state: BeginState, now: f64) -> J {
    let task_name = model::string(&state.task, "task").unwrap_or_default();
    state.task["operations"][&state.operation_id]["phase"] = json!("sending");
    state.task["operations"][&state.operation_id]["started_at"] = json!(now);
    let version = model::int(&state.task, "state_version").unwrap_or(0) + 1;
    state.task["state_version"] = json!(version);
    audit_fields(
        &mut state.task,
        now,
        "dispatch_started",
        &[("operation", json!(state.operation_id))],
    );
    s["tasks"][&task_name] = state.task.clone();
    state.task["operations"][&state.operation_id].clone()
}

pub fn sent(s: &mut J, spec: &J, now: f64) -> Result<J> {
    object(spec, "transport response spec")
        .and_then(|_| load_sent_task(s, spec))
        .and_then(|task| commit_sent(s, spec, task, now))
}

fn load_sent_task(s: &J, spec: &J) -> Result<J> {
    let operation = spec_operation(spec);
    task(
        s,
        model::get(spec, "task"),
        model::get(spec, "state_version"),
    )
    .and_then(|task| ensure_sent_state(&task, &operation).map(|()| task))
}

fn operation_phase(t: &J, operation: &str) -> Option<String> {
    model::dig_str(t, &["operations", operation, "phase"])
}

fn spec_operation(spec: &J) -> String {
    model::string(spec, "operation").unwrap_or_default()
}

fn sent_state_ok(t: &J, operation: &str) -> bool {
    operation_phase(t, operation).as_deref() == Some("sending")
        && model::string(t, "phase").as_deref() == Some("next_pending")
}

fn ensure_sent_state(t: &J, operation: &str) -> Result<()> {
    ensure(
        sent_state_ok(t, operation),
        "only a sending operation can record a transport response",
    )
}

fn commit_sent(s: &mut J, spec: &J, mut task: J, now: f64) -> Result<J> {
    let operation = spec_operation(spec);
    let response = text(model::get(spec, "response"), "transport response", 1000)?;
    task["operations"][&operation]["phase"] = json!("submitted");
    task["operations"][&operation]["response"] = json!(response);
    task["operations"][&operation]["submitted_at"] = json!(now);
    let version = model::int(&task, "state_version").unwrap_or(0) + 1;
    task["state_version"] = json!(version);
    audit_fields(
        &mut task,
        now,
        "dispatch_response",
        &[("operation", json!(operation))],
    );
    let task_name = model::string(&task, "task").unwrap_or_default();
    s["tasks"][&task_name] = task.clone();
    Ok(task)
}

pub fn accept(s: &mut J, spec: &J, session: &str, now: f64) -> Result<J> {
    accept_identity(s, spec, &json!({"adapter":"codex","session":session}), now)
}

pub fn accept_identity(s: &mut J, spec: &J, identity: &J, now: f64) -> Result<J> {
    let identity = crate::identity::validate(identity)?;
    let name = model::string(spec, "task").unwrap_or_default();
    let operation = spec_operation(spec);
    let expected = s["tasks"][name]["operations"][operation]
        .get("adapter")
        .cloned()
        .unwrap_or(json!("codex"));
    ensure(identity["adapter"] == expected, "takeover adapter mismatch")?;
    let session = identity["session"].as_str().unwrap();
    object(spec, "takeover spec")
        .and_then(|_| accept_load(s, spec, session, now))
        .map(|(task, evidence)| accept_commit(s, spec, session, evidence, task, now))
}

fn accept_load(s: &mut J, spec: &J, session: &str, now: f64) -> Result<(J, String)> {
    task(
        s,
        model::get(spec, "task"),
        model::get(spec, "state_version"),
    )
    .and_then(|task| lineage::ensure_current(s, &task).map(|()| task))
    .and_then(|task| require_unowned(&task, true).map(|()| task))
    .and_then(|task| ensure_takeover(s, &task, spec, session).map(|()| task))
    .and_then(|task| takeover_evidence(spec).map(|evidence| (task, evidence)))
    .and_then(|(task, evidence)| {
        accept_check_repair(s, spec, &task, now).map(|()| (task, evidence))
    })
}

fn phase_next_pending(t: &J) -> bool {
    model::string(t, "phase").as_deref() == Some("next_pending")
}

fn takeover_queue_ok(s: &J) -> bool {
    queue_running(s)
}

fn takeover_task_ok(t: &J) -> bool {
    task_running(t) && phase_next_pending(t)
}

fn takeover_state_ok(s: &J, t: &J) -> bool {
    takeover_queue_ok(s) && takeover_task_ok(t)
}

fn operation_accepting(t: &J, spec: &J) -> bool {
    let operation = spec_operation(spec);
    matches!(
        operation_phase(t, &operation).as_deref(),
        Some("sending" | "submitted")
    )
}

fn operation_session_ready(t: &J, spec: &J, session: &str) -> bool {
    let operation = spec_operation(spec);
    model::dig_str(t, &["operations", &operation, "session"]).as_deref() == Some(session)
}

fn takeover_operation_ok(t: &J, spec: &J, session: &str) -> bool {
    operation_accepting(t, spec) && operation_session_ready(t, spec, session)
}

fn spec_assignment_matches(t: &J, spec: &J) -> bool {
    let operation = spec_operation(spec);
    model::string(spec, "assignment")
        == model::dig_str(t, &["operations", &operation, "assignment"])
}

fn task_assignment_matches(t: &J, spec: &J) -> bool {
    let operation = spec_operation(spec);
    model::string(t, "assignment") == model::dig_str(t, &["operations", &operation, "assignment"])
}

fn takeover_assignments_ok(t: &J, spec: &J) -> bool {
    spec_assignment_matches(t, spec) && task_assignment_matches(t, spec)
}

fn takeover_action_ok(s: &J, t: &J, spec: &J, session: &str) -> bool {
    takeover_state_ok(s, t) && takeover_operation_ok(t, spec, session)
}

fn takeover_ok(s: &J, t: &J, spec: &J, session: &str) -> bool {
    takeover_action_ok(s, t, spec, session) && takeover_assignments_ok(t, spec)
}

fn ensure_takeover(s: &J, t: &J, spec: &J, session: &str) -> Result<()> {
    ensure(
        takeover_ok(s, t, spec, session),
        "takeover must match the exact pending operation, assignment and receiving session",
    )
}

fn takeover_evidence(spec: &J) -> Result<String> {
    text(model::get(spec, "evidence"), "takeover evidence", 1000)
}

fn accept_check_repair(s: &J, spec: &J, t: &J, now: f64) -> Result<()> {
    let operation = spec_operation(spec);
    let op = model::dig(t, &["operations", &operation])
        .cloned()
        .unwrap_or(Value::Null);
    if has_lane(t) {
        crate::batch::check_repair(s, t, &op, now, true)
    } else {
        crate::pr::check_repair(s, t, &op, now, true)
    }
}

fn accept_commit(s: &mut J, spec: &J, session: &str, evidence: String, mut task: J, now: f64) -> J {
    let operation = spec_operation(spec);
    task["operations"][&operation]["phase"] = json!("accepted");
    task["operations"][&operation]["accepted_at"] = json!(now);
    task["operations"][&operation]["evidence"] = json!(evidence);
    let has_pending = model::array_field(&task, "pending")
        .map(|items| !items.is_empty())
        .unwrap_or(false);
    task["phase"] = json!(if has_pending {
        "result_pending"
    } else {
        "executing"
    });
    let version = model::int(&task, "state_version").unwrap_or(0) + 1;
    task["state_version"] = json!(version);
    audit_fields(
        &mut task,
        now,
        "accepted",
        &[("operation", json!(operation)), ("session", json!(session))],
    );
    let task_name = model::string(&task, "task").unwrap_or_default();
    s["tasks"][&task_name] = task.clone();
    task
}

pub fn control(
    s: &mut J,
    name: Option<&J>,
    version: Option<&J>,
    value: Option<&J>,
    reason: Option<&J>,
    now: f64,
) -> Result<J> {
    control_value(value)
        .and_then(|value| control_task(s, name, version).map(|task| (value, task)))
        .and_then(|(value, task)| control_commit(s, value, reason, task, now))
}

fn control_value(value: Option<&J>) -> Result<String> {
    let Some(value) = value.and_then(Value::as_str) else {
        return Err(progress_error(
            "control must be running, paused or cancelled",
        ));
    };
    ensure(
        matches!(value, "running" | "paused" | "cancelled"),
        "control must be running, paused or cancelled",
    )
    .map(|()| value.to_string())
}

fn control_task(s: &J, name: Option<&J>, version: Option<&J>) -> Result<J> {
    task(s, name, version).and_then(|task| {
        ensure(control_allowed(&task), "terminal tasks cannot be resumed").map(|()| task)
    })
}

fn control_allowed(t: &J) -> bool {
    model::string(t, "control").as_deref() != Some("cancelled")
        && model::string(t, "phase").as_deref() != Some("completed")
}

fn control_commit(
    s: &mut J,
    value: String,
    reason: Option<&J>,
    mut task: J,
    now: f64,
) -> Result<J> {
    let reason = text(reason, "control reason", 1000)?;
    task["control"] = json!(value);
    task["control_reason"] = json!(reason);
    let version = model::int(&task, "state_version").unwrap_or(0) + 1;
    task["state_version"] = json!(version);
    audit_fields(&mut task, now, "control", &[("value", json!(value))]);
    let task_name = model::string(&task, "task").unwrap_or_default();
    s["tasks"][&task_name] = task.clone();
    Ok(task)
}

pub fn resume(s: &mut J, spec: &J, now: f64) -> Result<J> {
    object(spec, "recovery spec")
        .and_then(|_| resume_load(s, spec))
        .and_then(|(task, wait)| resume_commit(s, spec, task, wait, now))
}

fn resume_load(s: &mut J, spec: &J) -> Result<(J, J)> {
    task(
        s,
        model::get(spec, "task"),
        model::get(spec, "state_version"),
    )
    .and_then(|task| lineage::ensure_current(s, &task).map(|()| task))
    .and_then(|task| require_unowned(&task, true).map(|()| task))
    .and_then(|task| ensure_recovery_state(s, &task).map(|()| task))
    .and_then(|task| resume_wait(s, spec, &task).map(|wait| (task, wait)))
    .and_then(|(task, wait)| {
        ensure(
            !wait_has_events(s, &model::string(&wait, "id").unwrap_or_default()),
            "recovery needs an unconsumed registered wait",
        )
        .map(|()| (task, wait))
    })
}

fn recovery_control_ok(t: &J) -> bool {
    task_running(t)
}

fn recovery_phase_ok(t: &J) -> bool {
    model::string(t, "phase").as_deref() == Some("blocked")
}

fn recovery_task_ok(t: &J) -> bool {
    recovery_control_ok(t) && recovery_phase_ok(t)
}

fn recovery_no_pending(t: &J) -> bool {
    !model::array_field(t, "pending")
        .map(|items| !items.is_empty())
        .unwrap_or(false)
}

fn recovery_queue_task_ok(s: &J, t: &J) -> bool {
    queue_running(s) && recovery_task_ok(t)
}

fn recovery_state_ok(s: &J, t: &J) -> bool {
    recovery_queue_task_ok(s, t) && recovery_no_pending(t)
}

fn ensure_recovery_state(s: &J, t: &J) -> Result<()> {
    ensure(
        recovery_state_ok(s, t),
        "only a running blocked task with no pending results can register recovery",
    )
}

fn resume_wait(s: &J, spec: &J, t: &J) -> Result<J> {
    wait_for(s, model::get(spec, "wait"), t)
}

fn resume_commit(s: &mut J, spec: &J, mut task: J, wait: J, now: f64) -> Result<J> {
    let evidence = text(model::get(spec, "evidence"), "recovery evidence", 1000)?;
    let wait_id = model::string(&wait, "id").unwrap_or_default();
    let owner = model::string(&wait, "owner").unwrap_or_default();
    let condition = model::string(&wait, "condition").unwrap_or_default();
    task["wait"] = json!(wait_id);
    task["owner"] = json!(owner);
    task["phase"] = json!("waiting");
    task["next_action"] = json!({
        "kind": "wait",
        "wait": wait_id,
        "owner": owner,
        "condition": condition
    });
    let version = model::int(&task, "state_version").unwrap_or(0) + 1;
    task["state_version"] = json!(version);
    audit_fields(
        &mut task,
        now,
        "recovery_wait",
        &[("evidence", json!(evidence)), ("wait", json!(wait_id))],
    );
    let task_name = model::string(&task, "task").unwrap_or_default();
    s["tasks"][&task_name] = task.clone();
    Ok(task)
}

pub fn release(s: &mut J, spec: &J, now: f64) -> Result<J> {
    object(spec, "release spec")
        .and_then(|_| release_load(s, spec))
        .map(|(task, evidence)| release_commit(s, task, evidence, now))
}

fn release_load(s: &J, spec: &J) -> Result<(J, String)> {
    task(
        s,
        model::get(spec, "task"),
        model::get(spec, "state_version"),
    )
    .and_then(|task| release_evidence(spec).map(|evidence| (task, evidence)))
    .and_then(|(task, evidence)| {
        ensure(
            release_allowed(&task),
            "use result application to release a running writer",
        )
        .map(|()| (task, evidence))
    })
}

fn release_evidence(spec: &J) -> Result<String> {
    text(
        model::get(spec, "evidence"),
        "inspected writer release evidence",
        1000,
    )
}

fn release_allowed(t: &J) -> bool {
    matches!(
        model::string(t, "phase").as_deref(),
        Some("blocked" | "completed")
    ) || model::string(t, "control").as_deref() == Some("cancelled")
}

fn release_commit(s: &mut J, mut task: J, evidence: String, now: f64) -> J {
    task["writer"] = Value::Null;
    let version = model::int(&task, "state_version").unwrap_or(0) + 1;
    task["state_version"] = json!(version);
    audit_fields(
        &mut task,
        now,
        "writer_released",
        &[("evidence", json!(evidence))],
    );
    let task_name = model::string(&task, "task").unwrap_or_default();
    s["tasks"][&task_name] = task.clone();
    task
}

/// The queue watcher calls this even when there are no new source events.
pub fn stalls(s: &mut J, now: f64) -> Result<()> {
    let queue_id = model::string(s, "id").unwrap_or_default();
    let task_names: Vec<String> = model::get(s, "tasks")
        .and_then(Value::as_object)
        .map(|tasks| tasks.keys().cloned().collect())
        .unwrap_or_default();
    task_names
        .iter()
        .try_for_each(|name| stall_task(s, &queue_id, name, now))
}

fn stall_task(s: &mut J, queue_id: &str, name: &str, now: f64) -> Result<()> {
    let Some(task) = model::dig(s, &["tasks", name]).cloned() else {
        return Ok(());
    };
    if !task_running(&task) || !lineage::actionable(s, &task) {
        return Ok(());
    }
    stall_reason(s, &task, now).map_or(Ok(()), |reason| {
        add_stall_event(s, queue_id, &task, now, reason)
    })
}

fn stall_reason(s: &J, task: &J, now: f64) -> Option<String> {
    phase_stall(s, task, now).or_else(|| lane_stall(task, now))
}

fn phase_stall(s: &J, task: &J, now: f64) -> Option<String> {
    if model::string(task, "phase").as_deref() == Some("result_pending") {
        return pending_stall(s, task, now);
    }
    next_pending_stall(task, now)
}

fn pending_stall(s: &J, task: &J, now: f64) -> Option<String> {
    model::array_field(task, "pending").and_then(|pending| {
        pending
            .iter()
            .find_map(|event| overdue_pending(s, event, now))
    })
}

fn pending_deadline(s: &J, event: &J) -> Option<f64> {
    let id = event.as_str()?;
    model::dig_f64(s, &["events", id, "apply_deadline"])
}

fn overdue_pending(s: &J, event: &J, now: f64) -> Option<String> {
    pending_deadline(s, event)
        .and_then(|deadline| (now >= deadline).then(|| "result_unapplied".to_string()))
}

fn next_pending_stall(task: &J, now: f64) -> Option<String> {
    if model::string(task, "phase").as_deref() != Some("next_pending") {
        return None;
    }
    successor_overdue(task, now)
}

fn successor_overdue(task: &J, now: f64) -> Option<String> {
    let operation = model::dig_str(task, &["next_action", "operation"]).unwrap_or_default();
    model::dig_f64(task, &["operations", &operation, "deadline"])
        .and_then(|deadline| (now >= deadline).then(|| "successor_unaccepted".to_string()))
}

fn lane_stall(task: &J, now: f64) -> Option<String> {
    if !has_lane(task) {
        return None;
    }
    crate::batch::stall(task, now)
}

fn add_stall_event(s: &mut J, queue_id: &str, task: &J, now: f64, reason: String) -> Result<()> {
    let event_id = stall_event_id(queue_id, task, &reason);
    if model::dig(s, &["events", &event_id]).is_some() {
        return Ok(());
    }
    ensure_stall_capacity(s).map(|()| {
        insert_stall_event(s, &event_id, task, now, &reason);
    })
}

fn stall_event_id(queue_id: &str, task: &J, reason: &str) -> String {
    let key = format!(
        "{queue_id}:{}:{}:{}:{}:{reason}",
        model::string(task, "task").unwrap_or_default(),
        model::get(task, "epoch")
            .map(|value| value.to_string())
            .unwrap_or_default(),
        model::string(task, "assignment").unwrap_or_default(),
        model::int(task, "state_version").unwrap_or(0)
    );
    sha256_hex(key.as_bytes())[..24].to_string()
}

fn ensure_stall_capacity(s: &J) -> Result<()> {
    let event_count = model::get(s, "events")
        .and_then(Value::as_object)
        .map(Map::len)
        .unwrap_or(0);
    ensure(
        event_count < 10000,
        "queue event capacity reached; preserve this queue and start a new one",
    )
}

fn insert_stall_event(s: &mut J, event_id: &str, task: &J, now: f64, reason: &str) {
    let mut binding = json!({
        "task": task.get("task").cloned().unwrap_or(Value::Null),
        "epoch": task.get("epoch").cloned().unwrap_or(Value::Null),
        "assignment": task.get("assignment").cloned().unwrap_or(Value::Null),
        "candidate": task.get("candidate").cloned().unwrap_or(Value::Null)
    });
    lineage::copy_binding(task, &mut binding);
    let event = json!({
        "id": event_id,
        "wait": task.get("wait").cloned().unwrap_or(Value::Null),
        "task": task.get("task").cloned().unwrap_or(Value::Null),
        "epoch": task.get("epoch").cloned().unwrap_or(Value::Null),
        "owner": task.get("owner").cloned().unwrap_or(Value::Null),
        "outcome": "decision",
        "kind": "progress_stall",
        "reason": reason,
        "state_version": task.get("state_version").cloned().unwrap_or(Value::Null),
        "binding": binding,
        "summary": format!("{reason}: inspect persisted result/operation; do not replay dispatch"),
        "evidence": "queue.json",
        "observed_at": now,
        "acknowledged": false
    });
    s["events"][event_id] = event;
}

pub fn current_event(s: &J, e: &J) -> bool {
    event_task(s, e)
        .map(|task| lineage::actionable(s, task) && event_is_current(task, e))
        .unwrap_or(false)
}

fn event_task<'a>(s: &'a J, e: &J) -> Option<&'a J> {
    let task_name = model::string(e, "task")?;
    model::dig(s, &["tasks", &task_name])
}

fn event_receipt_current(task: &J, e: &J) -> bool {
    binding_current(task, e) && task_running(task)
}

fn event_is_current(task: &J, e: &J) -> bool {
    if !event_receipt_current(task, e) {
        return false;
    }
    event_stall_or_pending(task, e)
}

fn event_stall_or_pending(task: &J, e: &J) -> bool {
    if model::string(e, "kind").as_deref() == Some("progress_stall") {
        return stall_event_current(task, e);
    }
    pending_event_current(task, e)
}

fn stall_event_current(task: &J, e: &J) -> bool {
    if model::get(e, "state_version") != model::get(task, "state_version") {
        return false;
    }
    stall_reason_current(task, e)
}

fn stall_reason_current(task: &J, e: &J) -> bool {
    let reason = model::string(e, "reason").unwrap_or_default();
    if reason.starts_with("batch_") {
        return batch_stall_current(task, e, &reason);
    }
    matches!(
        model::string(task, "phase").as_deref(),
        Some("result_pending" | "next_pending")
    )
}

fn batch_stall_current(task: &J, e: &J, reason: &str) -> bool {
    let observed = model::number(e, "observed_at").unwrap_or(0.0);
    crate::batch::stall(task, observed).as_deref() == Some(reason)
}

fn pending_event_current(task: &J, e: &J) -> bool {
    pending_contains_event(task, &model::string(e, "id").unwrap_or_default())
        && !model::bool_or(e, "archived", false)
}

fn result_pending_diagnosis(events: &[J], deadlines: &[f64], now: f64) -> (String, String, String) {
    if events.is_empty() {
        return result_pending_without_event();
    }
    result_pending_detail(events.len(), deadlines, now)
}

fn result_pending_without_event() -> (String, String, String) {
    (
        "result_pending_without_event".to_string(),
        "task is result_pending but no current event is linked".to_string(),
        "inspect queue.json and event records; do not infer a result or dispatch".to_string(),
    )
}

fn any_deadline_passed(deadlines: &[f64], now: f64) -> bool {
    deadlines.iter().any(|deadline| now >= *deadline)
}

fn result_pending_detail(count: usize, deadlines: &[f64], now: f64) -> (String, String, String) {
    let overdue = any_deadline_passed(deadlines, now);
    let kind = if overdue {
        "result_unapplied_overdue"
    } else {
        "result_pending_application"
    };
    (
        kind.to_string(),
        result_pending_reason(count, overdue),
        "read queue.json and the delivery, then apply each current event with task-apply at the current state_version; do not resend the original assignment".to_string(),
    )
}

fn result_pending_reason(count: usize, overdue: bool) -> String {
    let mut reason = format!("{count} received result(s) are not applied; ACK is receipt only");
    if overdue {
        reason.push_str("; apply deadline passed");
    }
    reason
}

fn next_pending_diagnosis(operation: Option<&J>, now: f64) -> (String, String, String) {
    let phase = operation.and_then(|op| model::string(op, "phase"));
    if phase.as_deref() == Some("prepared") {
        return prepared_successor_diagnosis(operation, now);
    }
    successor_uncertain_or_default(operation, now, phase)
}

fn successor_uncertain_or_default(
    operation: Option<&J>,
    now: f64,
    phase: Option<String>,
) -> (String, String, String) {
    if matches!(phase.as_deref(), Some("sending" | "submitted")) {
        return sending_successor_diagnosis(operation, now, phase.unwrap_or_default());
    }
    fallback_successor_diagnosis()
}

fn deadline_passed(operation: &J, now: f64) -> bool {
    model::number(operation, "deadline")
        .map(|deadline| now >= deadline)
        .unwrap_or(false)
}

fn prepared_successor_diagnosis(operation: Option<&J>, now: f64) -> (String, String, String) {
    let op = operation.unwrap_or(&Value::Null);
    let overdue = deadline_passed(op, now);
    let kind = if overdue {
        "successor_prepared_overdue"
    } else {
        "successor_prepared"
    };
    (
        kind.to_string(),
        prepared_successor_reason(op, overdue),
        "dispatch only through the existing task-begin path, then require exact task-accept; do not create a second operation".to_string(),
    )
}

fn prepared_successor_reason(operation: &J, overdue: bool) -> String {
    let mut reason = format!(
        "operation {} is prepared but not dispatched",
        model::string(operation, "id").unwrap_or_default()
    );
    if overdue {
        reason.push_str("; takeover deadline passed");
    }
    reason
}

fn sending_successor_diagnosis(
    operation: Option<&J>,
    now: f64,
    phase: String,
) -> (String, String, String) {
    let op = operation.unwrap_or(&Value::Null);
    let overdue = deadline_passed(op, now);
    let kind = if overdue {
        "successor_takeover_overdue"
    } else {
        "successor_uncertain"
    };
    (
        kind.to_string(),
        sending_successor_reason(op, &phase, overdue),
        "inspect the existing operation and exact recipient session; accept only the matching takeover, otherwise record a blocker and never replay the dispatch".to_string(),
    )
}

fn sending_successor_reason(operation: &J, phase: &str, overdue: bool) -> String {
    let mut reason = format!(
        "operation {} is {phase}; takeover is not confirmed",
        model::string(operation, "id").unwrap_or_default()
    );
    if overdue {
        reason.push_str("; takeover deadline passed");
    }
    reason
}

fn fallback_successor_diagnosis() -> (String, String, String) {
    (
        "successor_pending".to_string(),
        "a successor is recorded but its exact operation is unavailable".to_string(),
        "inspect queue.json; do not dispatch a replacement operation".to_string(),
    )
}

fn waiting_diagnosis(wait: Option<&J>) -> (String, String, String) {
    if let Some(wait) = wait.filter(|wait| !model::bool_or(wait, "closed", false)) {
        return (
            "waiting".to_string(),
            format!(
                "waiting for registered wait {}",
                model::string(wait, "id").unwrap_or_default()
            ),
            "observe the wait/event or its deadline; no model polling is needed".to_string(),
        );
    }
    (
        "no_open_next_action".to_string(),
        "task has no open wait".to_string(),
        "inspect queue.json and register a concrete wait before any dispatch".to_string(),
    )
}

/// Read-only account of what a task is waiting for and what next.
struct Diagnosis {
    result: J,
    events: Vec<J>,
    deadlines: Vec<f64>,
    operation: Option<J>,
    wait: Option<J>,
}

pub fn diagnose(s: &J, t: &J, now: Option<f64>) -> Result<J> {
    let now = now.unwrap_or_else(crate::util::now);
    let mut diagnosis = build_diagnosis(s, t);
    let detail = lineage::diagnosis(s, t).unwrap_or_else(|| diagnosis_detail(t, now, &diagnosis));
    diagnosis.result["classification"] = json!(detail.0);
    diagnosis.result["reason"] = json!(detail.1);
    diagnosis.result["next_step"] = json!(detail.2);
    Ok(diagnosis.result)
}

fn build_diagnosis(s: &J, t: &J) -> Diagnosis {
    let pending = pending_event_ids(s, t);
    let events = pending_events(s, &pending);
    let deadlines = pending_deadlines(&events);
    let operation = diagnosis_operation(t);
    let wait = diagnosis_wait(s, t);
    let mut result = diagnosis_json(t, pending);
    add_pending_deadlines(&mut result, &deadlines);
    add_diagnosis_writer(&mut result, t);
    add_diagnosis_operation(&mut result, operation.as_ref());
    add_diagnosis_wait(&mut result, wait.as_ref());
    Diagnosis {
        result,
        events,
        deadlines,
        operation,
        wait,
    }
}

fn pending_event_ids(s: &J, t: &J) -> Vec<String> {
    model::array_field(t, "pending")
        .map(|items| {
            items
                .iter()
                .filter_map(Value::as_str)
                .filter(|id| model::dig(s, &["events", id]).is_some())
                .map(ToString::to_string)
                .collect()
        })
        .unwrap_or_default()
}

fn pending_events(s: &J, pending: &[String]) -> Vec<J> {
    pending
        .iter()
        .filter_map(|id| model::dig(s, &["events", id]).cloned())
        .collect()
}

fn pending_deadlines(events: &[J]) -> Vec<f64> {
    events
        .iter()
        .filter_map(|event| model::number(event, "apply_deadline"))
        .collect()
}

fn diagnosis_operation(t: &J) -> Option<J> {
    let action = next_action(t);
    if model::string(&action, "kind").as_deref() != Some("dispatch") {
        return None;
    }
    model::string(&action, "operation").and_then(|id| model::dig(t, &["operations", &id]).cloned())
}

fn next_action(t: &J) -> J {
    model::get(t, "next_action")
        .cloned()
        .unwrap_or_else(|| json!({}))
}

fn diagnosis_wait(s: &J, t: &J) -> Option<J> {
    model::string(t, "wait").and_then(|id| model::dig(s, &["waits", &id]).cloned())
}

fn diagnosis_json(t: &J, pending: Vec<String>) -> J {
    json!({
        "task": t.get("task").cloned().unwrap_or(Value::Null),
        "epoch": t.get("epoch").cloned().unwrap_or(Value::Null),
        "assignment": t.get("assignment").cloned().unwrap_or(Value::Null),
        "candidate": t.get("candidate").cloned().unwrap_or(Value::Null),
        "phase": model::string(t, "phase").unwrap_or_else(|| "unknown".to_string()),
        "control": model::string(t, "control").unwrap_or_else(|| "running".to_string()),
        "state_version": t.get("state_version").cloned().unwrap_or(Value::Null),
        "owner": t.get("owner").cloned().unwrap_or(Value::Null),
        "wait": t.get("wait").cloned().unwrap_or(Value::Null),
        "pending": pending,
        "evidence": "queue.json"
    })
}

fn add_pending_deadlines(result: &mut J, deadlines: &[f64]) {
    if !deadlines.is_empty() {
        result["pending_deadlines"] = json!(deadlines);
    }
}

fn add_diagnosis_writer(result: &mut J, t: &J) {
    if let Some(writer) = model::get(t, "writer").filter(|value| !value.is_null()) {
        result["writer"] = json!({
            "workspace": writer.get("workspace").cloned().unwrap_or(Value::Null),
            "assignment": writer.get("assignment").cloned().unwrap_or(Value::Null),
            "session": writer.get("session").cloned().unwrap_or(Value::Null),
            "evidence": writer.get("evidence").cloned().unwrap_or(Value::Null)
        });
    }
}

fn add_diagnosis_operation(result: &mut J, operation: Option<&J>) {
    if let Some(operation) = operation {
        result["operation"] = json!({
            "id": operation.get("id").cloned().unwrap_or(Value::Null),
            "phase": operation.get("phase").cloned().unwrap_or(Value::Null),
            "assignment": operation.get("assignment").cloned().unwrap_or(Value::Null),
            "session": operation.get("session").cloned().unwrap_or(Value::Null),
            "owner": operation.get("owner").cloned().unwrap_or(Value::Null),
            "deadline": operation.get("deadline").cloned().unwrap_or(Value::Null)
        });
    }
}

fn add_diagnosis_wait(result: &mut J, wait: Option<&J>) {
    if let Some(wait) = wait {
        result["wait_condition"] = wait.get("condition").cloned().unwrap_or(Value::Null);
        result["wait_deadline"] = wait.get("deadline").cloned().unwrap_or(Value::Null);
    }
}

fn diagnosis_detail(t: &J, now: f64, diagnosis: &Diagnosis) -> (String, String, String) {
    let control = model::string(t, "control").unwrap_or_else(|| "running".to_string());
    if control == "paused" {
        return paused_detail();
    }
    nonpaused_diagnosis_detail(t, now, diagnosis, &control)
}

fn nonpaused_diagnosis_detail(
    t: &J,
    now: f64,
    diagnosis: &Diagnosis,
    control: &str,
) -> (String, String, String) {
    if control == "cancelled" {
        return cancelled_detail();
    }
    phase_detail(t, now, diagnosis)
}

fn paused_detail() -> (String, String, String) {
    (
        "paused".to_string(),
        "task is paused; no automatic continuation".to_string(),
        "resume only after an explicit user instruction; do not dispatch or apply a new result"
            .to_string(),
    )
}

fn cancelled_detail() -> (String, String, String) {
    (
        "cancelled".to_string(),
        "task is cancelled; retain evidence and the current writer until explicitly released"
            .to_string(),
        "do not continue automatically; inspect release evidence before any replacement work"
            .to_string(),
    )
}

fn phase_detail(t: &J, now: f64, diagnosis: &Diagnosis) -> (String, String, String) {
    let phase = model::string(t, "phase").unwrap_or_else(|| "unknown".to_string());
    completed_phase_detail(&phase)
        .or_else(|| blocked_phase_detail(&phase, &next_action(t)))
        .or_else(|| {
            result_pending_phase_detail(&phase, &diagnosis.events, &diagnosis.deadlines, now)
        })
        .or_else(|| next_pending_phase_detail(&phase, diagnosis.operation.as_ref(), now))
        .or_else(|| executing_phase_detail(&phase, t))
        .or_else(|| waiting_phase_detail(&phase, diagnosis.wait.as_ref()))
        .unwrap_or_else(|| unknown_phase_detail(&phase))
}

fn completed_phase_detail(phase: &str) -> Option<(String, String, String)> {
    if phase != "completed" {
        return None;
    }
    Some((
        "completed".to_string(),
        "all required acceptance is complete".to_string(),
        "no further task action; retain the completion evidence".to_string(),
    ))
}

fn blocked_phase_detail(phase: &str, action: &J) -> Option<(String, String, String)> {
    if phase != "blocked" {
        return None;
    }
    Some(blocked_phase_text(action))
}

fn blocked_phase_text(action: &J) -> (String, String, String) {
    (
        "blocked".to_string(),
        format!(
            "blocked: {}",
            model::string(action, "reason")
                .unwrap_or_else(|| "reason not recorded".to_string())
        ),
        format!(
            "recover only when {} is true: register a fresh matching wait and use task-resume with inspected evidence",
            model::string(action, "condition")
                .unwrap_or_else(|| "the recorded condition".to_string())
        ),
    )
}

fn result_pending_phase_detail(
    phase: &str,
    events: &[J],
    deadlines: &[f64],
    now: f64,
) -> Option<(String, String, String)> {
    if phase != "result_pending" {
        return None;
    }
    Some(result_pending_diagnosis(events, deadlines, now))
}

fn next_pending_phase_detail(
    phase: &str,
    operation: Option<&J>,
    now: f64,
) -> Option<(String, String, String)> {
    if phase != "next_pending" {
        return None;
    }
    Some(next_pending_diagnosis(operation, now))
}

fn executing_phase_detail(phase: &str, t: &J) -> Option<(String, String, String)> {
    if phase != "executing" {
        return None;
    }
    Some((
        "executing".to_string(),
        format!(
            "owner {} is executing; wait for the registered result",
            model::string(t, "owner").unwrap_or_default()
        ),
        "check the registered wait and its deadline; do not kill, reassign or duplicate the writer without inspected evidence".to_string(),
    ))
}

fn waiting_phase_detail(phase: &str, wait: Option<&J>) -> Option<(String, String, String)> {
    if phase != "waiting" {
        return None;
    }
    Some(waiting_diagnosis(wait))
}

fn unknown_phase_detail(phase: &str) -> (String, String, String) {
    (
        phase.to_string(),
        "task state is not recognized".to_string(),
        "inspect queue.json; do not dispatch or apply based on an unknown state".to_string(),
    )
}

pub fn diagnostics(s: &J, now: Option<f64>) -> Result<J> {
    let output = model::get(s, "tasks")
        .and_then(Value::as_object)
        .into_iter()
        .flat_map(|tasks| tasks.iter())
        .map(|(name, task)| diagnose(s, task, now).map(|diagnosis| (name.clone(), diagnosis)))
        .collect::<Result<Map<String, Value>>>()?;
    Ok(Value::Object(output))
}
