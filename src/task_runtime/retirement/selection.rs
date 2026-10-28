use super::*;
use crate::progress;

pub(super) fn prepare(
    env: &TaskEnvironment,
    state: &J,
    owner: Owner,
    delivery: String,
) -> Result<Plan> {
    let epoch = env.delivery["stage"]["epoch"]
        .as_u64()
        .ok_or_else(|| FlowError::guard("stage resource scope missing"))?;
    let mut plan = Plan {
        owner,
        delivery,
        scope_epoch: epoch,
        phase: "pending".into(),
        entries: vec![],
        retained: vec![],
    };
    let waits = state["waits"]
        .as_object()
        .ok_or_else(|| FlowError::guard("waits missing"))?;
    let parent = plan.owner.task_id.clone();
    for (id, wait) in waits.iter().filter(|(_, w)| selected(w, &parent, epoch)) {
        ensure(
            plan.entries.len() + plan.retained.len() < 128,
            "stage resource inventory exceeds bound",
        )?;
        match entry(env, state, &plan, id, wait) {
            Ok(value) => plan.entries.push(value),
            Err(error) => plan
                .retained
                .push(json!({"wait":id,"reason":error.to_string()})),
        }
    }
    if plan.entries.is_empty() {
        plan.phase = "complete".into();
    }
    Ok(plan)
}

fn selected(wait: &J, parent: &str, epoch: u64) -> bool {
    wait["lineage"]["requirement"] == parent
        && wait["lineage"]["scope_epoch"] == epoch
        && matches!(
            wait["lineage"]["role"].as_str(),
            Some("development" | "acceptance")
        )
}

fn entry(env: &TaskEnvironment, state: &J, plan: &Plan, id: &str, wait: &J) -> Result<Entry> {
    let binding = progress::binding(wait)?;
    let (root, runtime) = original_runtime(env, wait, &binding, &plan.owner)?;
    original_result(state, &root, plan, id, &binding)?;
    let (journal, broker) = settled_execution(env, &root, &runtime, &binding)?;
    let (presentation, frontends) = super::super::tui::retirement_snapshot(env, &root)?;
    let (review, removal) = review(env, state, id, &binding)?;
    Ok(Entry {
        wait: id.into(),
        root,
        binding,
        runtime,
        broker,
        sessions: vec![journal],
        presentation,
        frontends,
        review,
        removal,
        phase: "planned".into(),
    })
}

fn original_runtime(
    env: &TaskEnvironment,
    wait: &J,
    binding: &J,
    owner: &Owner,
) -> Result<(PathBuf, J)> {
    let assignment = required(binding, "assignment")?;
    validate_assignment(&assignment)?;
    let root = env.temp_root.join("rt").join(&assignment);
    ensure(
        wait["runtime"] == json!(root),
        "original wait has no matching runtime",
    )?;
    crate::task_env::validate_path(env, &root)?;
    admit(&root)?;
    let runtime = read(&root, "runtime.json")?;
    runtime_identity(&runtime, &root, owner)?;
    Ok((root, runtime))
}

fn settled_execution(
    env: &TaskEnvironment,
    root: &Path,
    runtime: &J,
    binding: &J,
) -> Result<(J, ProcessRecord)> {
    let journal = read(root, "upstream-session.json")?;
    session_identity(root, runtime, &journal, binding)?;
    let broker = registered_broker(env, runtime)?;
    no_pending_tools(root)?;
    no_recovery_incident(env)?;
    Ok((journal, broker))
}

fn runtime_identity(runtime: &J, root: &Path, owner: &Owner) -> Result<()> {
    ensure(
        upstream::selected(runtime) && runtime["adapter"] == "codex",
        "runtime is not an upstream Codex worker",
    )?;
    ensure(
        runtime["lifecycle_owner"] == json!(owner)
            && runtime["instance"] == owner.instance_id
            && runtime["root"] == json!(root),
        "worker runtime Owner changed",
    )?;
    ensure(
        runtime["recovery_role"] != true && runtime["publication_role"] != true,
        "recovery and publication resources are retained",
    )?;
    ensure(
        !root.join("receiver.json").try_exists()?,
        "coordinator resources are retained",
    )
}

fn original_result(state: &J, root: &Path, plan: &Plan, id: &str, binding: &J) -> Result<()> {
    let receipt = read(root, RECEIPT)?;
    receipt_identity(&receipt, root, plan, id, binding)?;
    let guard = PathBuf::from(required(state, "guard_dir")?);
    let coordinator = read(&guard, "state.json")?["runtime"].clone();
    ensure(
        receipt["coordinator"] == coordinator,
        "collection coordinator changed",
    )?;
    let child = &state["tasks"][required(binding, "task")?];
    ensure(progress::same(child, binding), "activity binding changed")?;
    applied_result(state, id, binding)
}

fn receipt_identity(receipt: &J, root: &Path, plan: &Plan, id: &str, binding: &J) -> Result<()> {
    ensure(
        receipt["owner"] == json!(plan.owner)
            && receipt["runtime"] == json!(root)
            && receipt["wait"] == id
            && receipt["binding"] == *binding,
        "original persisted collection is not established",
    )
}

fn applied_result(state: &J, id: &str, binding: &J) -> Result<()> {
    let events = state["events"]
        .as_object()
        .ok_or_else(|| FlowError::guard("events missing"))?;
    ensure(
        events.values().any(|e| {
            e["wait"] == id
                && e["binding"] == *binding
                && e["acknowledged"] == true
                && e["applied"] == true
        }),
        "original result is not ACKed/applied",
    )
}

fn session_identity(root: &Path, runtime: &J, session: &J, binding: &J) -> Result<()> {
    let creation = read(root, "creation.json")?;
    ensure(
        creation["phase"] == "created"
            && creation["packet"]["binding"] == *binding
            && creation["session"] == session["session"],
        "original worker creation is unknown or changed",
    )?;
    settled_turn(root, session, binding)?;
    ensure(
        session["phase"] == "created"
            && session["instance"] == runtime["instance"]
            && session["runtime"]["upstream_socket"] == runtime["upstream_socket"],
        "upstream session ownership is unknown or changed",
    )?;
    crate::identity::validate(&json!({"adapter":"codex","session":session["session"]})).map(|_| ())
}

fn settled_turn(root: &Path, session: &J, binding: &J) -> Result<()> {
    let turn = read(root, "turn.json")?;
    ensure(
        turn["phase"] == "signalled"
            && turn["binding"] == *binding
            && turn["session"] == session["session"]
            && turn["outcome"].is_string(),
        "original worker turn has not settled",
    )
}

fn registered_broker(env: &TaskEnvironment, runtime: &J) -> Result<ProcessRecord> {
    let pid = runtime["pid"]
        .as_i64()
        .and_then(|p| i32::try_from(p).ok())
        .filter(|p| *p > 1)
        .ok_or_else(|| FlowError::guard("broker PID missing"))?;
    let start_time = runtime["start_time"]
        .as_u64()
        .ok_or_else(|| FlowError::guard("broker start identity missing"))?;
    let registration = env
        .child_processes
        .iter()
        .find(|p| p.pid == pid && p.start_time == Some(start_time))
        .ok_or_else(|| FlowError::guard("broker is not registered by this environment"))?;
    ensure(
        registration.descendants.is_empty(),
        "broker has recorded descendants; retain for terminal ownership scan",
    )?;
    Ok(ProcessRecord {
        pid,
        start_time: Some(start_time),
    })
}

fn no_pending_tools(root: &Path) -> Result<()> {
    let events =
        util::read_state_json_bounded_if_exists(root, "broker-events.json", 16 * 1024 * 1024)?;
    if let Some(events) = events {
        ensure(
            events["pending"].as_object().is_some_and(|v| v.is_empty()),
            "worker has unresolved native tool requests",
        )?;
    }
    for name in ["broker-tools.json", "control-tools.json"] {
        if let Some(value) = util::read_state_json_bounded_if_exists(root, name, LIMIT)? {
            ensure(
                value.as_object().is_some_and(|m| {
                    m.values()
                        .all(|e| matches!(e["phase"].as_str(), Some("complete" | "responded")))
                }),
                "worker has an unresolved control operation",
            )?;
        }
    }
    Ok(())
}

pub(super) fn no_recovery_incident(env: &TaskEnvironment) -> Result<()> {
    let directory = env.temp_root.join("rt");
    for (index, entry) in std::fs::read_dir(directory)?.enumerate() {
        ensure(index < 128, "runtime inventory exceeds bound")?;
        let root = entry?.path().join("recovery-observer");
        if !root.try_exists()? {
            continue;
        }
        if let Some(incident) =
            util::read_state_json_bounded_if_exists(&root, "incident.json", LIMIT)?
        {
            ensure(
                incident["phase"] == "handled",
                "unresolved recovery incident retains stage evidence",
            )?;
        }
    }
    Ok(())
}

fn review(
    env: &TaskEnvironment,
    state: &J,
    wait: &str,
    binding: &J,
) -> Result<(
    Option<crate::task_env::review::ReviewWorkspace>,
    Option<crate::task_env::DirectoryRemoval>,
)> {
    let assignment = required(binding, "assignment")?;
    let Some(record) = env.review_workspaces.get(&assignment) else {
        return Ok((None, None));
    };
    crate::task_env::review::validate(env, record)?;
    ensure(
        record.binding == *binding && record.wait == wait,
        "review original wait or assignment binding changed",
    )?;
    ensure(
        record.phase == crate::task_env::review::Phase::Ready,
        "review clone has an unsettled materialization",
    )?;
    if !unreferenced(env, state, record)? {
        return Ok((Some(record.clone()), None));
    }
    let identity = record
        .identity
        .ok_or_else(|| FlowError::guard("review clone identity missing"))?;
    Ok((
        Some(record.clone()),
        Some(crate::task_env::DirectoryRemoval::new(
            record.path.clone(),
            identity,
        )?),
    ))
}

pub(super) fn unreferenced(
    env: &TaskEnvironment,
    state: &J,
    record: &crate::task_env::review::ReviewWorkspace,
) -> Result<bool> {
    if registered_references(env, state, record) {
        return Ok(false);
    }
    for (index, entry) in std::fs::read_dir(env.temp_root.join("rt"))?.enumerate() {
        ensure(index < 128, "runtime reference inventory exceeds bound")?;
        let root = entry?.path();
        if root == record.runtime {
            continue;
        }
        if runtime_references(&root, &record.path)? {
            return Ok(false);
        }
    }
    Ok(true)
}

fn registered_references(
    env: &TaskEnvironment,
    state: &J,
    record: &crate::task_env::review::ReviewWorkspace,
) -> bool {
    references(state, &record.path)
        || env
            .review_workspaces
            .values()
            .any(|other| other.path != record.path && other.source.starts_with(&record.path))
}

fn runtime_references(root: &Path, path: &Path) -> Result<bool> {
    for name in [
        "runtime.json",
        "creation.json",
        "turn.json",
        "receiver.json",
    ] {
        if let Some(value) = util::read_state_json_bounded_if_exists(root, name, LIMIT)? {
            if references(&value, path) {
                return Ok(true);
            }
        }
    }
    Ok(false)
}

fn references(value: &J, path: &Path) -> bool {
    match value {
        J::String(text) => path.to_str().is_none_or(|p| text.contains(p)),
        J::Array(values) => values.iter().any(|v| references(v, path)),
        J::Object(values) => values.values().any(|v| references(v, path)),
        _ => false,
    }
}
