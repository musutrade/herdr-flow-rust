//! Existing native and delivery operations, outside the queue transaction.
//! No model-supplied executable, endpoint, account, or new authority.
use super::*;
use crate::task_runtime as runtime;

pub(super) fn external(request: &J) -> bool {
    matches!(
        request["params"]["arguments"]["action"].as_str(),
        Some("run" | "deliver" | "stage-complete" | "finalize")
    )
}

/// The existing watcher calls this without holding the queue lock. Native
/// operations take their own queue transactions. The service lock serializes
/// request journals with the in-transaction ACK/apply service.
pub(crate) fn drive(queue_root: &Path) -> Result<bool> {
    super::super::recovery::if_needed(queue_root)?;
    let context = {
        let _lock = queue::locked(queue_root, false)?;
        let q = queue::load_queue(queue_root)?;
        if q["receiver"]["codex_runtime"].is_null() || q["stopped"] == true {
            return Ok(false);
        }
        let receiver_task = q["receiver"]["task"].as_str().unwrap_or_default();
        if !q["tasks"][receiver_task]["cleanup_obligation"].is_null() {
            return Ok(true);
        }
        let Some(context) = context(queue_root, &q)? else {
            return Ok(false);
        };
        context
    };
    drive_bound(&context.0, &context.1, &context.2)
}

fn context(queue_root: &Path, q: &J) -> Result<Option<(PathBuf, J, Owner)>> {
    let guard = PathBuf::from(required(q, "guard_dir")?);
    let state = util::read_state_json(&guard, "state.json")?;
    if state["control_protocol"] != "queue-tool-v2" {
        return Ok(None);
    }
    let root = PathBuf::from(required(&state, "runtime")?);
    let permit = bound_permit(queue_root, q, &root, &state)?;
    Ok(Some((root, state, permit.owner().clone())))
}

fn drive_bound(root: &Path, state: &J, owner: &Owner) -> Result<bool> {
    let permit = current(root, state, owner)?;
    let Some(_service) = util::FileLock::acquire(&root.join("control-service.lock"), false)? else {
        return Ok(false);
    };
    if !crate::task_runtime::broker::checkpoint::available(root)? {
        return Ok(false);
    }
    serve_external(root, state, owner)?;
    collect_workers(root, state, owner)?;
    drop(permit);
    commit_terminal(root, state, owner)
}

fn serve_external(root: &Path, state: &J, owner: &Owner) -> Result<()> {
    let requests = util::read_state_json_bounded_if_exists(root, "broker-tools.json", 1024 * 1024)?
        .unwrap_or(json!({}));
    let entries = requests
        .as_object()
        .ok_or_else(|| FlowError::guard("invalid tool requests"))?;
    for entry in entries
        .values()
        .filter(|e| e["phase"] == "pending" && external(&e["request"]))
    {
        serve_request(root, state, owner, &entry["request"])?;
    }
    Ok(())
}

fn current(root: &Path, state: &J, owner: &Owner) -> Result<crate::lifecycle::Permit> {
    let _lock = queue::locked(&owner.queue_root, false)?;
    let q = queue::load_queue(&owner.queue_root)?;
    let permit = bound_permit(&owner.queue_root, &q, root, state)?;
    ensure(permit.owner() == owner, "continuation owner changed")?;
    Ok(permit)
}

pub(super) fn lineage(q: &J, state: &J, owner: &Owner, spec: &J) -> Result<J> {
    ensure(
        owns(q, owner, &required(spec, "task")?),
        "foreign lineage task",
    )?;
    ensure(
        matches!(
            spec["kind"].as_str(),
            Some("artifact" | "candidate" | "activity" | "continue" | "stage")
        ),
        "control cannot create requirements or authorization",
    )?;
    let updated = queue::task_action(
        &owner.queue_root,
        "task-lineage",
        spec,
        state["session"].as_str(),
        &crate::herdr::receiver_factory,
    )?;
    inspect(&updated, owner)
}

pub(super) fn execute(root: &Path, state: &J, owner: &Owner, args: &J) -> Result<J> {
    let spec: J = serde_json::from_str(&required(args, "spec")?)?;
    if args["action"] == "finalize" {
        return finalize(root, state, owner, &spec);
    }
    let permit = current(root, state, owner)?;
    let mut env = crate::task_env::instance::allocation::restore(owner, &permit)?;
    ensure(
        crate::task_env::isolation::owner(&env).as_ref() == Some(owner),
        "continuation environment changed",
    )?;
    execute_owned(root, &mut env, owner, args, &spec)
}

fn execute_owned(
    root: &Path,
    env: &mut TaskEnvironment,
    owner: &Owner,
    args: &J,
    spec: &J,
) -> Result<J> {
    match args["action"].as_str() {
        Some("run") => run(root, env, owner, spec),
        Some("deliver") => deliver(env, owner, spec),
        Some("stage-complete") => complete_stage(env, owner, spec),
        _ => Err(FlowError::guard("unsupported continuation")),
    }
}

fn complete_stage(env: &mut TaskEnvironment, owner: &Owner, spec: &J) -> Result<J> {
    fields(spec, &["delivery"])?;
    crate::delivery::complete_stage(env, &owner.queue_root, &required(spec, "delivery")?)
}

fn fields(spec: &J, allowed: &[&str]) -> Result<()> {
    let object = spec
        .as_object()
        .ok_or_else(|| FlowError::guard("continuation spec must be an object"))?;
    ensure(
        object.keys().all(|key| allowed.contains(&key.as_str())),
        "unsupported continuation field",
    )
}

fn run(coordinator: &Path, env: &mut TaskEnvironment, owner: &Owner, spec: &J) -> Result<J> {
    let packet = input(owner, spec)?;
    let assignment = required(&packet["binding"], "assignment")?;
    let root = runtime::prepare(env, &assignment)?;
    let launch = launch_spec(coordinator, env, owner, &root, spec, &packet)?;
    // Native journals reserve each external attempt before it is sent. A new
    // tool call cannot silently replay a partially completed run.
    let child = runtime::launch(env, &root, &launch)?;
    util::process::BackgroundChild {
        pid: child.id(),
        child,
        ready: None,
    }
    .reap();
    let mut creation = spec.clone();
    creation["queue_dir"] = json!(owner.queue_root);
    runtime::native::create(env, &root, &creation)?;
    let turn = runtime::native::dispatch(env, &root)?;
    Ok(json!({"runtime":root,"wait":spec["wait"],"turn":turn,"collection":"queue-driver"}))
}

fn input(owner: &Owner, spec: &J) -> Result<J> {
    fields(spec, &["wait", "instruction", "evidence"])?;
    let q = queue::load_queue(&owner.queue_root)?;
    let wait = &q["waits"][required(spec, "wait")?];
    ensure(
        owns(&q, owner, &required(wait, "task")?),
        "foreign activity",
    )?;
    let evidence: Vec<String> =
        serde_json::from_value(spec.get("evidence").cloned().unwrap_or(json!([])))?;
    runtime::native::packet(&q, wait, &required(spec, "instruction")?, &evidence)
}

fn launch_spec(
    coordinator: &Path,
    env: &mut TaskEnvironment,
    owner: &Owner,
    root: &Path,
    spec: &J,
    packet: &J,
) -> Result<J> {
    let upstream = runtime::verify(env, coordinator)?;
    ensure(
        runtime::upstream::selected(&upstream),
        "continuation requires existing upstream attachment",
    )?;
    let socket = runtime::upstream::endpoint(&upstream)?;
    let mut launch = json!({"adapter":"codex","configuration":"upstream","socket":socket,
        "cwd":env.workspace_root.join("checkout"),"model":runtime::role_models::select(&upstream, &packet["binding"]["lineage"]["role"] )?,
        "approval_policy":upstream.get("approval_policy").cloned().unwrap_or(json!("untrusted")),
        "driver_binary":upstream.get("driver_binary").cloned().unwrap_or(json!(std::env::current_exe()?))});
    if let Some(presentation) = upstream.get("native_tui") {
        launch["native_tui"] = presentation.clone();
    }
    if packet["binding"]["lineage"]["role"] == "acceptance" {
        let snapshot = runtime::review::prepare(
            env,
            root,
            &json!({"queue_dir":owner.queue_root,"wait":spec["wait"]}),
        )?;
        launch["cwd"] = snapshot["cwd"].clone();
        launch["review_snapshot"] = snapshot["review_snapshot"].clone();
    }
    Ok(launch)
}

fn deliver(env: &mut TaskEnvironment, owner: &Owner, spec: &J) -> Result<J> {
    fields(
        spec,
        &[
            "adapter",
            "target",
            "ref",
            "expected",
            "authorization",
            "state_version",
        ],
    )?;
    ensure(
        spec["adapter"] == "git",
        "control delivery supports local Git only",
    )?;
    crate::delivery::prepare(env, &owner.queue_root, spec, &crate::delivery::local::Local)?;
    crate::delivery::execute(env, &crate::delivery::local::Local)
}

fn finalize(root: &Path, state: &J, owner: &Owner, spec: &J) -> Result<J> {
    fields(spec, &["task", "state_version", "outcome", "reason"])?;
    ensure(
        spec["task"] == owner.task_id,
        "finalize requires the exact parent",
    )?;
    let permit = current(root, state, owner)?;
    let env = crate::task_env::instance::allocation::restore(owner, &permit)?;
    let mut q = queue::load_queue(&owner.queue_root)?;
    crate::progress::terminal::record(&mut q, spec, owner, &env, util::now())?;
    Ok(json!({"terminal_requested":true,"cleanup":"pending; requires recorded tool response"}))
}

fn commit_terminal(root: &Path, state: &J, owner: &Owner) -> Result<bool> {
    let Some(mut spec) = terminal_request(root)? else {
        return Ok(false);
    };
    spec["queue_id"] = json!(owner.queue_id);
    spec["epoch"] = json!(owner.epoch);
    spec["instance_id"] = json!(owner.instance_id);
    let _lock = queue::locked(&owner.queue_root, false)?;
    let q = queue::load_queue(&owner.queue_root)?;
    // Release the shared lifecycle permit before the existing finalizer takes
    // its exclusive gate, while retaining the queue transaction.
    let permit = bound_permit(&owner.queue_root, &q, root, state)?;
    ensure(permit.owner() == owner, "finalize owner changed")?;
    drop(permit);
    queue::task_action(
        &owner.queue_root,
        "task-finalize",
        &spec,
        state["session"].as_str(),
        &crate::herdr::receiver_factory,
    )?;
    // Cleanup runs on the next existing watcher tick, after this tool response.
    Ok(true)
}

fn terminal_request(root: &Path) -> Result<Option<J>> {
    let journal = util::read_state_json_bounded_if_exists(root, "control-tools.json", 1024 * 1024)?
        .unwrap_or(json!({}));
    let broker = util::read_state_json_bounded_if_exists(root, "broker-tools.json", 1024 * 1024)?
        .unwrap_or(json!({}));
    let entries = journal
        .as_object()
        .ok_or_else(|| FlowError::guard("invalid control journal"))?;
    for (key, entry) in entries {
        if terminal_responded(entry, &broker[key]) {
            return Ok(Some(serde_json::from_str(&required(
                &entry["request"]["params"]["arguments"],
                "spec",
            )?)?));
        }
    }
    Ok(None)
}

fn terminal_responded(entry: &J, broker: &J) -> bool {
    entry["phase"] == "complete"
        && entry["result"]["success"] == true
        && entry["request"]["params"]["arguments"]["action"] == "finalize"
        && broker["phase"] == "responded"
        && broker["request"] == entry["request"]
        && broker["result"] == entry["result"]
}

fn collect_workers(root: &Path, state: &J, owner: &Owner) -> Result<()> {
    let mut journal =
        util::read_state_json_bounded_if_exists(root, "control-tools.json", 1024 * 1024)?
            .unwrap_or(json!({}));
    let entries = journal
        .as_object_mut()
        .ok_or_else(|| FlowError::guard("invalid control journal"))?;
    for entry in entries.values_mut().filter(|e| collecting(e)) {
        entry["collection"] = collect_worker(root, state, owner, entry)?;
    }
    if !journal.as_object().unwrap().is_empty() {
        save(root, &journal)?;
    }
    Ok(())
}

fn collecting(entry: &J) -> bool {
    entry["phase"] == "complete"
        && entry["request"]["params"]["arguments"]["action"] == "run"
        && entry["result"]["success"] == true
        && entry["collection"]["phase"] != "signalled"
}

fn collect_worker(root: &Path, state: &J, owner: &Owner, entry: &J) -> Result<J> {
    let permit = current(root, state, owner)?;
    let env = crate::task_env::instance::allocation::restore(owner, &permit)?;
    let result: J = serde_json::from_str(&required(&entry["result"]["contentItems"][0], "text")?)?;
    let worker = PathBuf::from(required(&result, "runtime")?);
    runtime::native::collect(&env, &worker)
}

pub(super) fn results(root: &Path) -> Result<J> {
    let journal = util::read_state_json_bounded_if_exists(root, "control-tools.json", 1024 * 1024)?
        .unwrap_or(json!({}));
    let entries = journal
        .as_object()
        .ok_or_else(|| FlowError::guard("invalid control journal"))?;
    let results: serde_json::Map<String, J> = entries
        .iter()
        .filter(|(_, e)| !e["collection"].is_null())
        .map(|(key, e)| (key.clone(), e["collection"].clone()))
        .collect();
    Ok(json!(results))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn terminal_requires_exact_completed_response_not_an_unknown_send() {
        let root = util::fs::test_tempdir();
        let entry = json!({"phase":"complete","result":{"success":true},
            "request":{"params":{"arguments":{"action":"finalize","spec":"{\"task\":\"ours\"}"}}}});
        util::atomic_write_json(&root.path().join("control-tools.json"), &json!({"1":entry}))
            .unwrap();
        let mut broker = entry.clone();
        broker["phase"] = json!("sending");
        util::atomic_write_json(&root.path().join("broker-tools.json"), &json!({"1":broker}))
            .unwrap();
        assert!(terminal_request(root.path()).unwrap().is_none());
        broker["phase"] = json!("responded");
        broker["request"]["foreign"] = json!(true);
        util::atomic_write_json(&root.path().join("broker-tools.json"), &json!({"1":broker}))
            .unwrap();
        assert!(terminal_request(root.path()).unwrap().is_none());
        broker["request"] = entry["request"].clone();
        util::atomic_write_json(&root.path().join("broker-tools.json"), &json!({"1":broker}))
            .unwrap();
        assert_eq!(
            terminal_request(root.path()).unwrap(),
            Some(json!({"task":"ours"}))
        );
    }
}
