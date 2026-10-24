//! Record an existing publication authorization at a quiescent V1 checkpoint.
//! This is not an authorization grant or a publication/recovery executor.
use super::*;

pub fn append(s: &mut J, spec: &J, now: f64) -> Result<J> {
    validate_fields(spec)?;
    let t = task(s, spec.get("task"), spec.get("state_version"))?;
    ensure(t["task"] == spec["task"], "task map identity mismatch")?;
    checkpoint(s, &t)?;
    let id = text(spec.get("id"), "authorization ID", 64)?;
    let entry = json!({"action":"publication", "source":text(spec.get("source"), "authorization source", 1000)?});
    if existing_entry(&t, &id, &entry)? {
        return Ok(t);
    }
    commit(s, t, id, entry, now)
}

fn existing_entry(t: &J, id: &str, entry: &J) -> Result<bool> {
    let permissions = object(&t["authorization"], "task authorization")?;
    ensure(permissions.len() <= 64, "too many authorization entries")?;
    if let Some(existing) = permissions.get(id) {
        ensure(
            existing == entry,
            "authorization ID already has a different value",
        )?;
        return Ok(true);
    }
    ensure(permissions.len() < 64, "too many authorization entries")?;
    Ok(false)
}

fn validate_fields(spec: &J) -> Result<()> {
    let fields = object(spec, "authorization spec")?;
    ensure(
        fields.keys().all(|k| {
            matches!(
                k.as_str(),
                "task" | "state_version" | "id" | "action" | "source"
            )
        }),
        "unknown authorization spec field",
    )?;
    ensure(
        spec["action"] == "publication",
        "only publication authorization may be appended",
    )
}

fn checkpoint(s: &J, t: &J) -> Result<()> {
    validate_task_state(s, t)?;
    ensure(
        t["writer"].is_null() && t["pending"] == json!([]),
        "release writer and apply pending results before authorization",
    )?;
    ensure(
        t["delivery"].is_null() && t["batch_delivery"].is_null(),
        "delivery-owned tasks cannot append authorization",
    )?;
    ensure(
        t["history"].is_array(),
        "authorization requires task audit history",
    )?;
    validate_operations(s, t)?;
    validate_lane(t)?;
    validate_pr(t)
}

fn validate_task_state(s: &J, t: &J) -> Result<()> {
    ensure(
        s["stopped"] == false,
        "authorization requires a running queue",
    )?;
    ensure(
        matches!(model::int(t, "schema_version"), Some(1 | 3)) && t.get("lineage").is_none(),
        "authorization append supports V1 schema 1 or 3 only",
    )?;
    ensure(
        t["control"] == "running" && t["phase"] == "blocked",
        "authorization requires a running blocked task",
    )
}

fn validate_operations(s: &J, t: &J) -> Result<()> {
    let operations = object(&t["operations"], "task operations")?;
    for op in operations.values() {
        ensure(
            op["phase"] == "accepted",
            "unfinished operation blocks authorization",
        )?;
        let assignment = text(op.get("assignment"), "operation assignment", 200)?;
        let wait = text(op["intent"].get("wait"), "operation wait", 200)?;
        let events = object(&s["events"], "queue events")?;
        ensure(
            events
                .values()
                .any(|e| applied_result(e, t, &assignment, &wait)),
            "accepted operation needs an applied result before authorization",
        )?;
    }
    Ok(())
}

fn applied_result(e: &J, t: &J, assignment: &str, wait: &str) -> bool {
    e["applied"] == true
        && e["outcome"] == "complete"
        && e["binding"]["task"] == t["task"]
        && e["binding"]["epoch"] == t["epoch"]
        && e["binding"]["assignment"] == assignment
        && e["wait"] == wait
}

fn validate_lane(t: &J) -> Result<()> {
    if t["lane"].is_null() {
        return Ok(());
    }
    ensure(
        t["lane"]["active"]["phase"] == "snapshot_frozen",
        "lane authorization requires a frozen unsent snapshot",
    )
}

fn validate_pr(t: &J) -> Result<()> {
    if t["pr"].is_null() {
        return Ok(());
    }
    let p = &t["pr"];
    ensure(
        p["phase"] == "publication_prepared"
            && p["publication"]["phase"] == "prepared"
            && p["publication"]["kind"] == "create",
        "authorization requires an unsent create publication",
    )?;
    ensure(
        p["number"].is_null() && p["observation"].is_null() && p["landing"].is_null(),
        "observed publication cannot append authorization",
    )
}

fn commit(s: &mut J, mut t: J, id: String, entry: J, now: f64) -> Result<J> {
    let version = model::int(&t, "state_version")
        .and_then(|n| n.checked_add(1))
        .ok_or_else(|| progress_error("task state version overflow"))?;
    let name = text(t.get("task"), "task ID", 200)?;
    ensure(s["tasks"][&name] == t, "task map identity mismatch")?;
    t["authorization"][&id] = entry.clone();
    t["state_version"] = json!(version);
    audit_fields(
        &mut t,
        now,
        "authorization_added",
        &[("id", json!(id)), ("authorization", entry)],
    );
    let mut staged = s.clone();
    staged["tasks"][&name] = t.clone();
    *s = staged;
    Ok(t)
}
